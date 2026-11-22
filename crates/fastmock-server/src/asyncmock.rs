use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use fastmock_core::MessageProtocol;
use fastmock_core::asyncapi::{self, AsyncImportOptions, AsyncMock, AsyncProtocol};
use fastmock_core::engine::{RenderedAmqp, RenderedMessage, RenderedMqtt};
use fastmock_core::template::context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Semaphore, watch};
use tokio::time::Instant;
use tracing::Instrument;
use uuid::Uuid;

use crate::admin::{ApiError, ApiResult, ErrorBody, audit_event as audit, bad};
use crate::auth::Principal;
use crate::state::AppState;

pub const NAMESPACE: &str = "asyncmocks";
pub const MAX_ASYNC_MOCKS: usize = 1000;
pub const LEASE_KEY: &str = "asyncmock:leader";
const SEQ_PREFIX: &str = "asyncmock:seq:";
const MAX_INFLIGHT: usize = 32;

pub fn parse_spec(spec: &Value) -> ApiResult<Value> {
    match spec {
        Value::String(s) => fastmock_core::openapi::parse_document(s)
            .map_err(|e| bad(format!("the document is neither JSON nor YAML: {e}"))),
        Value::Object(_) => Ok(spec.clone()),
        _ => Err(bad("spec must be a JSON object or a JSON/YAML string")),
    }
}

fn validate_mock(app: &AppState, m: &AsyncMock) -> Result<(), String> {
    let t = app.engine.templates();
    m.validate(|v| t.validate_json(v).map_err(|e| e.to_string()))
}

pub async fn load_all(app: &AppState) -> anyhow::Result<Vec<AsyncMock>> {
    let rows = app
        .stores
        .data
        .list(NAMESPACE, (MAX_ASYNC_MOCKS * 2) as i64)
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for (k, v) in rows {
        match serde_json::from_value::<AsyncMock>(v) {
            Ok(m) => out.push(m),
            Err(e) => {
                metrics::counter!("fastmock_store_errors_total", "store" => "asyncmocks")
                    .increment(1);
                tracing::warn!(key = %k, error = %e, "stored async mock is not valid; ignored");
            }
        }
    }
    out.sort_by_key(|m| m.id);
    Ok(out)
}

async fn get_mock(app: &AppState, id: Uuid) -> ApiResult<AsyncMock> {
    let v = app
        .stores
        .data
        .get(NAMESPACE, &id.to_string())
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "async mock not found".into()))?;
    serde_json::from_value(v).map_err(|e| anyhow::anyhow!("stored async mock {id}: {e}").into())
}

async fn store_mock(app: &AppState, m: &AsyncMock) -> anyhow::Result<()> {
    app.stores
        .data
        .put(NAMESPACE, &m.id.to_string(), &serde_json::to_value(m)?)
        .await
}

pub fn render(app: &AppState, m: &AsyncMock, seq: u64) -> anyhow::Result<RenderedMessage> {
    let idx = (seq.saturating_sub(1) % m.messages.len().max(1) as u64) as usize;
    let ex = m
        .messages
        .get(idx)
        .ok_or_else(|| anyhow::anyhow!("async mock has no messages"))?;
    let t = app.engine.templates();
    let ctx = context(json!({
        "mock": {"id": m.id, "name": m.name, "channel": m.channel},
        "sequence": seq,
        "example": ex.name,
    }));
    let _scope = t.limit_scope(fastmock_core::engine::MAX_MESSAGES_RENDER_BYTES);
    let mut headers = std::collections::BTreeMap::new();
    for (k, v) in &ex.headers {
        let v = if m.templated {
            t.render_in(v, &ctx)?
        } else {
            v.clone()
        };
        headers.insert(k.clone(), v);
    }
    let protocol = message_protocol(m.protocol);
    let mut content_type = None;
    if let Some(ct) = &m.content_type {
        if protocol == MessageProtocol::Kafka {
            if !headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("content-type"))
            {
                headers.insert("content-type".into(), ct.clone());
            }
        } else {
            content_type = Some(ct.clone());
        }
    }
    let value = if m.templated {
        t.render_json_in(&ex.payload, &ctx)?
    } else {
        ex.payload.clone()
    };
    let key = match &ex.key {
        Some(k) if m.templated => Some(t.render_in(k, &ctx)?),
        other => other.clone(),
    };
    Ok(RenderedMessage {
        protocol,
        topic: m.channel.clone(),
        key: key.filter(|_| protocol == MessageProtocol::Kafka),
        value,
        headers,
        avro_schema: m.avro_schema.clone(),
        content_type,
        delay_ms: 0,
        mqtt: m.mqtt.as_ref().map(|o| RenderedMqtt {
            qos: o.qos,
            retain: o.retain,
            message_expiry_seconds: o.message_expiry_seconds,
            ..Default::default()
        }),
        amqp: (protocol == MessageProtocol::Amqp).then(|| {
            let o = m.amqp.clone().unwrap_or_default();
            RenderedAmqp {
                exchange: o.exchange,
                exchange_type: o.exchange_type,
                durable: o.durable.unwrap_or(true),
                expiration: o.expiration,
                persistent: o.persistent,
                priority: o.priority,
                message_type: o.message_type,
                ..Default::default()
            }
        }),
    })
}

/// The broker protocol of an async mock (WebSocket mocks are stubs and never published here).
pub fn message_protocol(p: AsyncProtocol) -> MessageProtocol {
    match p {
        AsyncProtocol::Mqtt => MessageProtocol::Mqtt,
        AsyncProtocol::Amqp => MessageProtocol::Amqp,
        AsyncProtocol::Kafka | AsyncProtocol::Ws => MessageProtocol::Kafka,
    }
}

/// The allow-listed name of an async mock's destination: the topic, or the AMQP exchange.
pub fn destination(m: &AsyncMock) -> String {
    match m.protocol {
        AsyncProtocol::Amqp => m
            .amqp
            .as_ref()
            .map(|a| a.exchange.clone())
            .unwrap_or_default(),
        _ => m.channel.clone(),
    }
}

/// Why publishing an async mock cannot work in this configuration: a missing setting or the allow-list.
pub fn publish_problem(app: &AppState, m: &AsyncMock) -> Option<String> {
    let p = message_protocol(m.protocol);
    if let Some(setting) = app.brokers.missing_setting(&app.cfg, p) {
        return Some(format!("{setting} is not configured"));
    }
    let dest = destination(m);
    if !app.brokers.allowed(p, &dest, &m.channel) {
        return Some(format!(
            "{} {} is not in {}",
            if p == MessageProtocol::Amqp {
                "exchange"
            } else {
                "topic"
            },
            fastmock_brokers::amqp::api_exchange(&dest),
            crate::brokers::Brokers::allow_list_name(p)
        ));
    }
    None
}

async fn next_seq(app: &AppState, id: Uuid) -> u64 {
    match app
        .stores
        .hot
        .incr(
            &format!("{SEQ_PREFIX}{id}"),
            Some(Duration::from_secs(30 * 86_400)),
        )
        .await
    {
        Ok(n) => n.max(1) as u64,
        Err(e) => {
            tracing::warn!(error = %e, "async mock sequence counter unavailable");
            1
        }
    }
}

pub async fn publish_mock(
    app: &AppState,
    m: &AsyncMock,
) -> anyhow::Result<crate::brokers::Receipt> {
    let system = match m.protocol {
        AsyncProtocol::Amqp => "rabbitmq",
        other => other.as_str(),
    };
    let span = tracing::info_span!(
        "fastmock.async_publish",
        otel.name = %format!("publish {}", m.channel),
        otel.kind = "producer",
        messaging.system = system,
        messaging.destination.name = %m.channel,
        fastmock.async_mock = %m.id,
    );
    async {
        let seq = next_seq(app, m.id).await;
        let r = async {
            let msg = render(app, m, seq)?;
            app.brokers.publish_one(&msg).await
        }
        .await;
        let id = m.id.to_string();
        match &r {
            Ok(receipt) => {
                metrics::counter!("fastmock_async_messages_total", "mock" => id, "result" => "sent")
                    .increment(1);
                tracing::debug!(topic = %m.channel, protocol = system, partition = receipt.partition, offset = receipt.offset, "async mock message published");
            }
            Err(e) => {
                metrics::counter!("fastmock_async_messages_total", "mock" => id, "result" => "failed")
                    .increment(1);
                tracing::warn!(topic = %m.channel, error = %format!("{e:#}"), "async mock publishing failed");
            }
        }
        r
    }
    .instrument(span)
    .await
}

#[derive(Clone, Debug)]
pub struct PublisherOptions {
    pub lease_ttl: Duration,
    pub renew_every: Duration,
    pub tick: Duration,
    pub reload_every: Duration,
}

impl Default for PublisherOptions {
    fn default() -> Self {
        Self {
            lease_ttl: Duration::from_secs(15),
            renew_every: Duration::from_secs(5),
            tick: Duration::from_millis(250),
            reload_every: Duration::from_secs(5),
        }
    }
}

pub struct Leader {
    id: String,
    key: &'static str,
    ttl: Duration,
    valid_until: Option<Instant>,
}

impl Leader {
    pub fn new(ttl: Duration) -> Self {
        Self::with_key(LEASE_KEY, ttl)
    }

    /// A lease under another hot-store key (message capture elects its consumer replica the same way).
    pub fn with_key(key: &'static str, ttl: Duration) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            key,
            ttl,
            valid_until: None,
        }
    }

    pub fn is_leader(&self) -> bool {
        self.valid_until.is_some_and(|t| Instant::now() < t)
    }

    pub async fn refresh(&mut self, hot: &dyn fastmock_core::HotStore) -> bool {
        let started = Instant::now();
        let renewed = match hot
            .compare_and_set(self.key, Some(&self.id), &self.id, Some(self.ttl))
            .await
        {
            Ok(true) => Ok(true),
            Ok(false) => {
                hot.compare_and_set(self.key, None, &self.id, Some(self.ttl))
                    .await
            }
            Err(e) => Err(e),
        };
        match renewed {
            Ok(true) => {
                let margin = (self.ttl / 5).min(Duration::from_secs(2));
                self.valid_until = Some(started + self.ttl - margin);
            }
            Ok(false) => self.valid_until = None,
            Err(e) => {
                let store = if self.key == LEASE_KEY {
                    "asyncmock_lease"
                } else {
                    "capture_lease"
                };
                metrics::counter!("fastmock_store_errors_total", "store" => store).increment(1);
                tracing::warn!(error = %e, key = self.key, "renewing a leader lease failed");
            }
        }
        self.is_leader()
    }

    pub async fn release(&mut self, hot: &dyn fastmock_core::HotStore) {
        if self.valid_until.take().is_some() {
            let _ = hot
                .compare_and_set(
                    self.key,
                    Some(&self.id),
                    "released",
                    Some(Duration::from_millis(1)),
                )
                .await;
        }
    }
}

pub async fn run(app: AppState, shutdown: watch::Receiver<bool>) {
    run_with(app, shutdown, PublisherOptions::default()).await
}

pub async fn run_with(app: AppState, mut shutdown: watch::Receiver<bool>, opts: PublisherOptions) {
    let mut leader = Leader::new(opts.lease_ttl);
    let mut last_renew: Option<Instant> = None;
    let mut last_reload: Option<Instant> = None;
    let mut mocks: Arc<Vec<AsyncMock>> = Arc::new(Vec::new());
    let mut next_due: HashMap<Uuid, Instant> = HashMap::new();
    let inflight = Arc::new(Semaphore::new(MAX_INFLIGHT));
    let mut warned_missing: std::collections::HashSet<&'static str> = Default::default();
    metrics::gauge!("fastmock_async_leader").set(0.0);
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = tokio::time::sleep(opts.tick) => {}
        }
        let interval = if leader.is_leader() {
            opts.renew_every
        } else {
            opts.renew_every.min(Duration::from_secs(1))
        };
        if last_renew.is_none_or(|t| t.elapsed() >= interval) {
            let was = leader.is_leader();
            let now = leader.refresh(app.stores.hot.as_ref()).await;
            last_renew = Some(Instant::now());
            if was != now {
                tracing::info!(leader = now, "async mock publisher leadership changed");
                last_reload = None;
            }
            metrics::gauge!("fastmock_async_leader").set(if now { 1.0 } else { 0.0 });
        }
        if !leader.is_leader() {
            next_due.clear();
            continue;
        }
        if last_reload.is_none_or(|t| t.elapsed() >= opts.reload_every) {
            match load_all(&app).await {
                Ok(all) => {
                    let active: Vec<AsyncMock> = all
                        .into_iter()
                        .filter(|m| m.enabled && m.protocol.is_broker())
                        .collect();
                    next_due.retain(|id, _| active.iter().any(|m| m.id == *id));
                    mocks = Arc::new(active);
                    metrics::gauge!("fastmock_async_mocks_active").set(mocks.len() as f64);
                }
                Err(e) => {
                    metrics::counter!("fastmock_store_errors_total", "store" => "asyncmocks")
                        .increment(1);
                    tracing::warn!(error = %e, "loading async mocks failed; keeping the previous set");
                }
            }
            last_reload = Some(Instant::now());
        }
        if mocks.is_empty() {
            continue;
        }
        let now = Instant::now();
        for m in mocks.iter() {
            if let Some(setting) = app
                .brokers
                .missing_setting(&app.cfg, message_protocol(m.protocol))
            {
                if warned_missing.insert(setting) {
                    tracing::warn!(
                        "{} async mocks are configured but {setting} is not set; they are not published",
                        m.protocol.as_str()
                    );
                }
                continue;
            }
            let due = next_due.entry(m.id).or_insert(now);
            if now < *due {
                continue;
            }
            *due = now + Duration::from_millis(m.frequency_ms);
            let Ok(permit) = inflight.clone().try_acquire_owned() else {
                metrics::counter!("fastmock_async_messages_total", "mock" => m.id.to_string(), "result" => "dropped").increment(1);
                continue;
            };
            let app = app.clone();
            let m = m.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let _ = publish_mock(&app, &m).await;
            });
        }
    }
    leader.release(app.stores.hot.as_ref()).await;
    metrics::gauge!("fastmock_async_leader").set(0.0);
}

#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportAsyncApiRequest {
    #[schema(value_type = Object)]
    pub spec: Value,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub frequency_ms: Option<u64>,
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// Destination names: `channel` (default, the channel address) or `microcks`
    /// (`<service>-<version>-<channel>`, the Kafka topic, MQTT topic or AMQP exchange Microcks uses).
    #[serde(default)]
    pub topic_naming: asyncapi::TopicNaming,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImportAsyncApiResponse {
    pub dry_run: bool,
    pub tag: String,
    pub async_mocks: Vec<AsyncMock>,
    pub websocket_stubs: Vec<fastmock_core::Stub>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AsyncMockList {
    pub async_mocks: Vec<AsyncMock>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TriggerResult {
    pub id: Uuid,
    pub protocol: MessageProtocol,
    /// Kafka or MQTT topic; AMQP routing key.
    pub topic: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exchange: Option<String>,
    /// Kafka only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partition: Option<i32>,
    /// Kafka only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    pub tag: Option<String>,
}

fn default_tag(title: &str) -> String {
    let slug: String = title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    format!(
        "asyncapi-{}",
        if slug.is_empty() {
            "import".to_string()
        } else {
            slug.chars().take(64).collect()
        }
    )
}

#[utoipa::path(
    post,
    path = "/__admin/import/asyncapi",
    tag = "asyncapi",
    operation_id = "importAsyncApi",
    summary = "Import an AsyncAPI document",
    request_body = ImportAsyncApiRequest,
    responses(
        (status = 201, description = "Async mocks and WebSocket stubs created", body = ImportAsyncApiResponse),
        (status = 200, description = "Dry run: what would be created", body = ImportAsyncApiResponse),
        (status = 400, description = "The document is not a supported AsyncAPI document", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "The token lacks the editor role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn import_asyncapi(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Json(req): Json<ImportAsyncApiRequest>,
) -> ApiResult<Response> {
    let doc = parse_spec(&req.spec)?;
    let dry_run = req.dry_run;
    let out = import_document(&app, &p, doc.clone(), &req).await?;
    if !dry_run {
        crate::services::record_service(
            &app,
            &p.name,
            crate::services::ServiceInfo::asyncapi(&doc, &out.tag),
        )
        .await;
    }
    let status = if dry_run {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(out)).into_response())
}

pub async fn import_document(
    app: &AppState,
    p: &Principal,
    doc: Value,
    req: &ImportAsyncApiRequest,
) -> ApiResult<ImportAsyncApiResponse> {
    if req
        .frequency_ms
        .is_some_and(|f| !(asyncapi::MIN_FREQUENCY_MS..=asyncapi::MAX_FREQUENCY_MS).contains(&f))
    {
        return Err(bad(format!(
            "frequencyMs must be between {} and {}",
            asyncapi::MIN_FREQUENCY_MS,
            asyncapi::MAX_FREQUENCY_MS
        )));
    }
    if req
        .tag
        .as_ref()
        .is_some_and(|t| t.is_empty() || t.len() > 128)
    {
        return Err(bad("tag must be 1..128 characters"));
    }
    let prefix = req.path_prefix.clone().unwrap_or_default();
    if !prefix.is_empty() && (!prefix.starts_with('/') || prefix.len() > 256) {
        return Err(bad(
            "pathPrefix must start with / and be at most 256 characters",
        ));
    }
    let title = doc
        .pointer("/info/title")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let tag = req.tag.clone().unwrap_or_else(|| default_tag(&title));
    let opts = AsyncImportOptions {
        tag: tag.clone(),
        frequency_ms: req.frequency_ms,
        path_prefix: prefix,
    };
    let naming = req.topic_naming;
    let result = tokio::task::spawn_blocking(move || asyncapi::import_named(&doc, &opts, naming))
        .await
        .map_err(|e| anyhow::anyhow!("import task failed: {e}"))?
        .map_err(|e| bad(format!("{e:#}")))?;
    let mut warnings = result.warnings;
    let mut kafka = Vec::new();
    let mut ws_stubs = Vec::new();
    for mut m in result.mocks {
        if let Err(e) = validate_mock(app, &m) {
            if m.templated {
                m.templated = false;
                warnings.push(format!(
                    "{}: examples are not valid templates ({e}); they are sent verbatim",
                    m.name
                ));
            }
            if let Err(e) = validate_mock(app, &m) {
                warnings.push(format!("{}: skipped: {e}", m.name));
                continue;
            }
        }
        match m.protocol {
            AsyncProtocol::Kafka | AsyncProtocol::Mqtt | AsyncProtocol::Amqp => {
                let p = message_protocol(m.protocol);
                let dest = destination(&m);
                if !app.brokers.allowed(p, &dest, &m.channel) {
                    warnings.push(format!(
                        "{}: {} {} is not in {}; publishing will fail",
                        m.name,
                        if p == MessageProtocol::Amqp {
                            "exchange"
                        } else {
                            "topic"
                        },
                        fastmock_brokers::amqp::api_exchange(&dest),
                        crate::brokers::Brokers::allow_list_name(p)
                    ));
                }
                m.updated_at = Some(Utc::now());
                kafka.push(m);
            }
            AsyncProtocol::Ws => {
                let stub = asyncapi::websocket_stub(&m);
                match app.engine.validate(&stub) {
                    Ok(()) => ws_stubs.push(stub),
                    Err(e) => warnings.push(format!("{}: WebSocket stub skipped: {e}", m.name)),
                }
            }
        }
    }
    if kafka.is_empty() && ws_stubs.is_empty() {
        let mut msg = String::from(
            "the document produced no async mocks or WebSocket stubs; nothing was changed",
        );
        for w in warnings.iter().take(5) {
            msg.push_str("; ");
            msg.push_str(w);
        }
        return Err(bad(msg));
    }
    let mut missing: Vec<(&str, &str)> = Vec::new();
    for m in &kafka {
        let p = message_protocol(m.protocol);
        if let Some(setting) = app.brokers.missing_setting(&app.cfg, p)
            && !missing.iter().any(|(s, _)| *s == setting)
        {
            missing.push((setting, m.protocol.as_str()));
        }
    }
    for (setting, proto) in missing {
        let name = match proto {
            "kafka" => "Kafka",
            "mqtt" => "MQTT",
            _ => "AMQP",
        };
        warnings.push(format!(
            "{setting} is not set; {name} async mocks are stored but not published"
        ));
    }
    if kafka.iter().any(|m| {
        m.protocol == AsyncProtocol::Amqp
            && m.amqp.as_ref().is_some_and(|a| a.durable == Some(false))
    }) && app.cfg.amqp.as_ref().is_some_and(|a| !a.declare_exchanges)
    {
        warnings.push(
            "Microcks exchange names are declared only with FASTMOCK_AMQP_DECLARE_EXCHANGES=true; create the exchanges or set it".into(),
        );
    }
    if !req.dry_run {
        let existing = load_all(app).await?;
        let replaced: Vec<Uuid> = existing
            .iter()
            .filter(|m| m.tags.contains(&tag))
            .map(|m| m.id)
            .collect();
        if existing.len() - replaced.len() + kafka.len() > MAX_ASYNC_MOCKS {
            return Err(bad(format!(
                "at most {MAX_ASYNC_MOCKS} async mocks can be stored"
            )));
        }
        for id in &replaced {
            app.stores.data.delete(NAMESPACE, &id.to_string()).await?;
        }
        for m in &kafka {
            store_mock(app, m).await?;
        }
        app.stores
            .stubs
            .replace_tag(Some(&tag), &fastmock_core::SpaceFilter::Global, &ws_stubs)
            .await?;
        app.engine.reload().await?;
        audit(
            app,
            p,
            "import.asyncapi",
            json!({"tag": tag, "asyncMocks": kafka.len(), "websocketStubs": ws_stubs.len(), "replaced": replaced.len()}),
        )
        .await;
    }
    Ok(ImportAsyncApiResponse {
        dry_run: req.dry_run,
        tag,
        async_mocks: kafka,
        websocket_stubs: ws_stubs,
        warnings,
    })
}

#[utoipa::path(
    get,
    path = "/__admin/async-mocks",
    tag = "asyncapi",
    operation_id = "listAsyncMocks",
    summary = "List async mocks",
    params(ListQuery),
    responses(
        (status = 200, description = "Async mocks", body = AsyncMockList),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn list_mocks(
    State(app): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<AsyncMockList>> {
    let mut all = load_all(&app).await?;
    if let Some(t) = &q.tag {
        all.retain(|m| m.tags.contains(t));
    }
    Ok(Json(AsyncMockList { async_mocks: all }))
}

#[utoipa::path(
    get,
    path = "/__admin/async-mocks/{id}",
    tag = "asyncapi",
    operation_id = "getAsyncMock",
    summary = "One async mock",
    params(("id" = Uuid, Path, description = "Async mock id")),
    responses(
        (status = 200, description = "The async mock", body = AsyncMock),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn get_one(
    State(app): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<AsyncMock>> {
    Ok(Json(get_mock(&app, id).await?))
}

#[utoipa::path(
    put,
    path = "/__admin/async-mocks/{id}",
    tag = "asyncapi",
    operation_id = "putAsyncMock",
    summary = "Create or replace an async mock",
    params(("id" = Uuid, Path, description = "Async mock id")),
    request_body = AsyncMock,
    responses(
        (status = 200, description = "Created or replaced", body = AsyncMock),
        (status = 400, description = "Invalid async mock", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "The token lacks the editor role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn put_one(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
    Json(body): Json<Value>,
) -> ApiResult<Json<AsyncMock>> {
    let mut m: AsyncMock =
        serde_json::from_value(body).map_err(|e| bad(format!("invalid async mock: {e}")))?;
    m.id = id;
    if !m.protocol.is_broker() {
        return Err(bad(
            "only Kafka, MQTT and AMQP async mocks are stored here; WebSocket mocks are stubs with response.websocket",
        ));
    }
    validate_mock(&app, &m).map_err(bad)?;
    let exists = app
        .stores
        .data
        .get(NAMESPACE, &id.to_string())
        .await?
        .is_some();
    if !exists && load_all(&app).await?.len() >= MAX_ASYNC_MOCKS {
        return Err(bad(format!(
            "at most {MAX_ASYNC_MOCKS} async mocks can be stored"
        )));
    }
    m.updated_at = Some(Utc::now());
    store_mock(&app, &m).await?;
    audit(
        &app,
        &p,
        "asyncmock.put",
        json!({"id": id, "name": m.name, "channel": m.channel}),
    )
    .await;
    Ok(Json(m))
}

#[utoipa::path(
    delete,
    path = "/__admin/async-mocks/{id}",
    tag = "asyncapi",
    operation_id = "deleteAsyncMock",
    summary = "Delete an async mock",
    params(("id" = Uuid, Path, description = "Async mock id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "The token lacks the editor role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn delete_one(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    if !app.stores.data.delete(NAMESPACE, &id.to_string()).await? {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "async mock not found".into(),
        ));
    }
    let _ = app.stores.hot.delete(&format!("{SEQ_PREFIX}{id}")).await;
    audit(&app, &p, "asyncmock.delete", json!({"id": id})).await;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/__admin/async-mocks/{id}/trigger",
    tag = "asyncapi",
    operation_id = "triggerAsyncMock",
    summary = "Publish the next example now",
    params(("id" = Uuid, Path, description = "Async mock id")),
    responses(
        (status = 200, description = "One message published", body = TriggerResult),
        (status = 400, description = "The broker is not configured, or the topic or exchange is not allowed", body = ErrorBody),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 502, description = "Publishing to the broker failed", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "The token lacks the editor role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn trigger(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<TriggerResult>> {
    let m = get_mock(&app, id).await?;
    if let Some(problem) = publish_problem(&app, &m) {
        return Err(bad(problem));
    }
    audit(&app, &p, "asyncmock.trigger", json!({"id": id})).await;
    match publish_mock(&app, &m).await {
        Ok(r) => Ok(Json(TriggerResult {
            id,
            protocol: r.protocol,
            topic: r.topic,
            exchange: r.exchange,
            partition: r.partition,
            offset: r.offset,
        })),
        Err(e) => Err(ApiError(
            StatusCode::BAD_GATEWAY,
            format!(
                "publishing to {} failed: {} (details in the server log)",
                match m.protocol {
                    AsyncProtocol::Mqtt => "MQTT",
                    AsyncProtocol::Amqp => "AMQP",
                    _ => "Kafka",
                },
                crate::redact::truncate(fastmock_brokers::public_message(&e), 512)
            ),
        )),
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/import/asyncapi", post(import_asyncapi))
        .route("/async-mocks", get(list_mocks))
        .route(
            "/async-mocks/{id}",
            get(get_one).put(put_one).delete(delete_one),
        )
        .route("/async-mocks/{id}/trigger", post(trigger))
}

#[derive(utoipa::OpenApi)]
#[openapi(
    tags((name = "asyncapi", description = "AsyncAPI import and async mocks that publish to Kafka, MQTT and AMQP")),
    paths(import_asyncapi, list_mocks, get_one, put_one, delete_one, trigger),
    components(schemas(
        ImportAsyncApiRequest,
        ImportAsyncApiResponse,
        AsyncMockList,
        TriggerResult,
        AsyncMock,
        fastmock_core::asyncapi::AsyncMessage,
        fastmock_core::asyncapi::AsyncProtocol,
        fastmock_core::asyncapi::AsyncMqtt,
        fastmock_core::asyncapi::AsyncAmqp,
        fastmock_core::ws_model::WebSocketDef,
        fastmock_core::ws_model::WsMessage,
        fastmock_core::ws_model::WsReply,
    ))
)]
pub struct AsyncMockApi;

#[cfg(test)]
mod tests {
    use super::*;
    use fastmock_core::memory::memory_stores;

    #[tokio::test]
    async fn only_one_leader_holds_the_lease() {
        let stores = memory_stores(10);
        let hot = stores.hot.as_ref();
        let mut a = Leader::new(Duration::from_millis(300));
        let mut b = Leader::new(Duration::from_millis(300));
        assert!(a.refresh(hot).await);
        assert!(!b.refresh(hot).await);
        assert!(a.refresh(hot).await, "renewal keeps the lease");
        assert!(!b.refresh(hot).await);
        a.release(hot).await;
        assert!(!a.is_leader());
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(b.refresh(hot).await, "a released lease is taken over");
        assert!(!a.refresh(hot).await);
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert!(!b.is_leader(), "local validity ends before the lease");
        assert!(a.refresh(hot).await, "an expired lease is taken over");
    }

    #[test]
    fn default_tags_are_slugs() {
        assert_eq!(default_tag("Account Service!"), "asyncapi-account-service");
        assert_eq!(default_tag(""), "asyncapi-import");
    }
}
