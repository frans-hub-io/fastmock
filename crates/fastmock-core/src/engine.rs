use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use chrono::Utc;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::matcher::{CompiledStub, NearMiss};
use crate::model::{
    CallbackDef, CallbackJob, CallbackStatus, ChunkedDribbleDelay, Fault, GlobalSettings,
    ResponseDef, ResponseMode, SCENARIO_STARTED, SignatureEncoding, SpaceFilter, Stub,
};
use crate::plugin::current_traceparent;
use crate::request::CapturedRequest;
use crate::storage::{SecretProvider, Stores};
use crate::template::{Templates, hmac_bytes};

pub const STUB_SOURCE: &str = "stubs";
pub const MAX_CALLBACK_DELAY_MS: u64 = 7 * 24 * 3600 * 1000;
pub const MAX_SCENARIO_NAME: usize = 512;
pub const MAX_TTL_SECS: u64 = 365 * 24 * 3600;
pub const DEFAULT_SCENARIO_TTL_SECS: u64 = 7 * 24 * 3600;
pub const MAX_RESPONSES_PER_STUB: usize = 100;
/// Messages one stub publishes at most.
pub const MAX_MESSAGES_PER_STUB: usize = 100;
/// Headers (MQTT user properties) of one stub message at most.
pub const MAX_MESSAGE_HEADERS: usize = 100;
/// Bytes all rendered fields of all messages of one response produce together.
pub const MAX_MESSAGES_RENDER_BYTES: usize = 16 * 1024 * 1024;
/// AMQP 0-9-1 short strings (exchange, routing key, message id, ...) and Kafka topic names.
const AMQP_SHORT: usize = 255;
const KAFKA_TOPIC: usize = 249;
/// MQTT UTF-8 strings and binary data.
const MQTT_STRING: usize = 65_535;
/// Kafka header names.
const MAX_HEADER_NAME_BYTES: usize = 1024;
pub const MAX_DRIBBLE_CHUNKS: u32 = 1000;
pub const MAX_STATUS_MESSAGE: usize = 256;
pub const MAX_BODY_FILE_NAME: usize = 512;
pub const FILES_NAMESPACE: &str = "files";
pub const SETTINGS_NAMESPACE: &str = "settings";
pub const SETTINGS_KEY: &str = "global";
pub const COUNTER_PREFIX: &str = "stub:";
const COUNTER_TTL: Duration = Duration::from_secs(30 * 24 * 3600);
const FILE_CACHE_TTL: Duration = Duration::from_secs(2);
const FILE_CACHE_MAX_ENTRIES: usize = 256;
const FILE_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
const FILE_CACHE_MAX_ENTRY: usize = 16 * 1024 * 1024;
const SETTINGS_TTL: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
#[error("body file is not available")]
pub struct BodyFileMissing(pub String);

#[derive(Debug)]
pub struct BodyFile {
    pub content_type: Option<String>,
    pub bytes: Bytes,
}

type FileCache = HashMap<String, (Instant, Option<Arc<BodyFile>>)>;

pub fn decode_body_file(v: &Value) -> Option<BodyFile> {
    let raw = v.get("base64")?.as_str()?;
    let bytes = B64.decode(raw.trim()).ok()?;
    Some(BodyFile {
        content_type: v
            .get("contentType")
            .and_then(|c| c.as_str())
            .filter(|c| !c.is_empty())
            .map(String::from),
        bytes: Bytes::from(bytes),
    })
}

pub fn is_textual(content_type: Option<&str>, bytes: &[u8]) -> bool {
    match content_type.map(|c| c.to_ascii_lowercase()) {
        Some(c) => [
            "text/",
            "json",
            "xml",
            "javascript",
            "yaml",
            "csv",
            "html",
            "x-www-form-urlencoded",
        ]
        .iter()
        .any(|m| c.contains(m)),
        None => std::str::from_utf8(bytes).is_ok(),
    }
}

fn valid_file_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_BODY_FILE_NAME && !name.chars().any(|c| c.is_control())
}

fn counter_key(kind: &str, stub: &Stub) -> String {
    format!(
        "{COUNTER_PREFIX}{kind}:{}:{}",
        stub.id,
        stub.updated_at.map(|t| t.timestamp_micros()).unwrap_or(0)
    )
}

fn counter_ttl(stub: &Stub) -> Duration {
    match stub.expiry() {
        Some(e) => (e - Utc::now())
            .to_std()
            .unwrap_or(Duration::from_secs(1))
            .clamp(Duration::from_secs(1), COUNTER_TTL),
        None => COUNTER_TTL,
    }
}

#[derive(Debug, Clone)]
pub struct EngineResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    pub stub_id: Uuid,
    pub stub_name: Option<String>,
    pub fault: Option<Fault>,
    pub delay_ms: u64,
    pub correlation: Option<String>,
    pub callbacks: usize,
    pub proxy: Option<crate::model::ProxyDef>,
    pub sse: Option<Vec<crate::model::SseEvent>>,
    pub messages: Vec<RenderedMessage>,
    pub status_message: Option<String>,
    pub chunked_dribble_delay: Option<ChunkedDribbleDelay>,
    pub response_index: usize,
    pub space: Option<String>,
    /// For gRPC calls: the unrendered gRPC answer and the template context it is rendered in.
    pub grpc: Option<Box<GrpcPlan>>,
}

/// What a gRPC call needs to render its messages while they are sent.
#[derive(Debug, Clone)]
pub struct GrpcPlan {
    pub def: Option<crate::grpc_model::GrpcResponseDef>,
    /// The response's `jsonBody`, unrendered.
    pub json_body: Option<Value>,
    pub templated: bool,
    /// The template context of the match (`request`, `state`, ...).
    pub context: Value,
}

/// A message ready to publish: templates are rendered.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RenderedMessage {
    pub protocol: crate::model::MessageProtocol,
    /// Kafka or MQTT topic; for AMQP the routing key.
    pub topic: String,
    pub key: Option<String>,
    pub value: Value,
    pub headers: std::collections::BTreeMap<String, String>,
    pub avro_schema: Option<Value>,
    /// Schema Registry subject of `avro_schema`; when `None` the publisher derives it from the destination.
    pub avro_subject: Option<String>,
    /// Why the message must not be published (a stub saved before a rule existed); the publisher fails it.
    pub invalid: Option<String>,
    pub content_type: Option<String>,
    /// Publish this long after the match.
    pub delay_ms: u64,
    pub mqtt: Option<RenderedMqtt>,
    pub amqp: Option<RenderedAmqp>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderedMqtt {
    pub qos: u8,
    pub retain: bool,
    pub correlation_data: Option<String>,
    pub response_topic: Option<String>,
    pub message_expiry_seconds: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderedAmqp {
    /// As the API names it: empty or `amq.default` is the default exchange.
    pub exchange: String,
    /// The exchange name was rendered from a template, so it is never declared.
    pub exchange_templated: bool,
    pub exchange_type: Option<String>,
    /// Declare a missing exchange durable (async mocks may ask for a transient one).
    pub durable: bool,
    pub message_id: Option<String>,
    pub correlation_id: Option<String>,
    pub reply_to: Option<String>,
    pub expiration: Option<String>,
    pub persistent: bool,
    pub priority: Option<u8>,
    pub message_type: Option<String>,
    pub app_id: Option<String>,
}

impl RenderedMessage {
    /// The destination for logs: the topic, or `exchange/routing key` for AMQP.
    pub fn destination(&self) -> String {
        match &self.amqp {
            Some(a) => format!(
                "{}/{}",
                if a.exchange.is_empty() {
                    "amq.default"
                } else {
                    a.exchange.as_str()
                },
                self.topic
            ),
            None => self.topic.clone(),
        }
    }
}

pub struct Engine {
    stores: Stores,
    templates: Arc<Templates>,
    secrets: Arc<dyn SecretProvider>,
    cache: RwLock<Arc<Vec<CompiledStub>>>,
    revision: AtomicI64,
    reload_lock: tokio::sync::Mutex<()>,
    files: std::sync::Mutex<FileCache>,
    settings: RwLock<(Instant, Arc<GlobalSettings>)>,
    ownership: RwLock<Arc<crate::ownership::Ownership>>,
    /// Some loaded stub matches MCP calls (`request.mcp`), so paths may serve mocked MCP servers.
    has_mcp: std::sync::atomic::AtomicBool,
}

pub const SCENARIO_PREFIX: &str = "scenario:";
/// Prefix of the scenario keys of spaces: `scenario@{space}:{name}`. Global keys start with
/// `scenario:`, so no global name can reach space data.
pub const SPACE_SCENARIO_PREFIX: &str = "scenario@";

/// Hot-store key of a scenario: `scenario:{name}` globally, `scenario@{space}:{name}` in a space.
pub fn scenario_key(space: Option<&str>, name: &str) -> String {
    match space {
        Some(s) => format!("{SPACE_SCENARIO_PREFIX}{s}:{name}"),
        None => format!("{SCENARIO_PREFIX}{name}"),
    }
}

/// Data-store namespace of body files: `files` globally, `files@{space}` in a space.
pub fn files_namespace(space: Option<&str>) -> String {
    match space {
        Some(s) => format!("{FILES_NAMESPACE}@{s}"),
        None => FILES_NAMESPACE.to_string(),
    }
}

impl Engine {
    pub fn new(
        stores: Stores,
        templates: Arc<Templates>,
        secrets: Arc<dyn SecretProvider>,
    ) -> Self {
        Self {
            stores,
            templates,
            secrets,
            cache: RwLock::new(Arc::new(Vec::new())),
            revision: AtomicI64::new(i64::MIN),
            reload_lock: tokio::sync::Mutex::new(()),
            files: std::sync::Mutex::new(HashMap::new()),
            settings: RwLock::new((
                Instant::now() - SETTINGS_TTL * 2,
                Arc::new(GlobalSettings::default()),
            )),
            ownership: RwLock::new(Arc::default()),
            has_mcp: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn templates(&self) -> &Arc<Templates> {
        &self.templates
    }

    fn validate_response(&self, r: &ResponseDef, at: &str) -> Result<(), String> {
        let t = &self.templates;
        let set = [
            r.body.is_some(),
            r.json_body.is_some(),
            r.base64_body.is_some(),
            r.body_file_name.is_some(),
        ]
        .iter()
        .filter(|x| **x)
        .count();
        if set > 1 {
            return Err(format!(
                "{at}use only one of body, jsonBody, base64Body, bodyFileName"
            ));
        }
        if let Some(ws) = &r.websocket {
            if r.proxy.is_some() || r.sse.is_some() {
                return Err(format!(
                    "{at}a websocket response must not define proxy or sse"
                ));
            }
            ws.validate(|tpl| t.validate(tpl).map_err(|e| e.to_string()))
                .map_err(|e| format!("{at}{e}"))?;
        }
        if let Some(g) = &r.grpc {
            if r.proxy.is_some() || r.sse.is_some() || r.websocket.is_some() {
                return Err(format!(
                    "{at}a gRPC response must not define proxy, sse or websocket"
                ));
            }
            let templated = r.templated;
            g.validate(
                &|tpl| {
                    if templated {
                        t.validate(tpl).map_err(|e| e.to_string())
                    } else {
                        Ok(())
                    }
                },
                &|v| {
                    if templated {
                        t.validate_json(v).map_err(|e| e.to_string())
                    } else {
                        Ok(())
                    }
                },
            )
            .map_err(|e| format!("{at}{e}"))?;
        }
        if let Some(events) = &r.sse {
            if set > 0 || r.proxy.is_some() {
                return Err(format!(
                    "{at}an SSE response must not define a body or proxy"
                ));
            }
            for (i, e) in events.iter().enumerate() {
                t.validate(&e.data)
                    .map_err(|x| format!("{at}sse[{i}].data: {x}"))?;
            }
            if events.len() > 10_000 {
                return Err(format!("{at}at most 10000 SSE events"));
            }
        }
        if let Some(p) = &r.proxy {
            if set > 0 {
                return Err(format!("{at}a proxy response must not define a body"));
            }
            if !(p.base_url.starts_with("http://") || p.base_url.starts_with("https://")) {
                return Err(format!("{at}proxy.baseUrl must be an http(s) URL"));
            }
            if let Some(k) = p
                .additional_headers
                .keys()
                .find(|k| crate::outbound::blocked_outbound_header(k))
            {
                return Err(format!(
                    "{at}proxy.additionalHeaders: {}",
                    crate::outbound::blocked_reason(k)
                ));
            }
        }
        if !(200..=599).contains(&r.status) {
            return Err(format!("{at}invalid status {}", r.status));
        }
        if let Some(b) = &r.base64_body {
            B64.decode(b).map_err(|e| format!("{at}base64Body: {e}"))?;
        }
        if let Some(d) = &r.delay {
            d.validate().map_err(|e| format!("{at}{e}"))?;
        }
        if let Some(d) = &r.chunked_dribble_delay {
            if d.number_of_chunks == 0 || d.number_of_chunks > MAX_DRIBBLE_CHUNKS {
                return Err(format!(
                    "{at}chunkedDribbleDelay.numberOfChunks must be 1..={MAX_DRIBBLE_CHUNKS}"
                ));
            }
            if d.total_duration_ms > crate::model::MAX_DELAY_SPEC_MS {
                return Err(format!(
                    "{at}chunkedDribbleDelay.totalDurationMs is too large"
                ));
            }
            if r.sse.is_some() || r.proxy.is_some() || r.fault.is_some() {
                return Err(format!(
                    "{at}chunkedDribbleDelay cannot be combined with sse, proxy or fault"
                ));
            }
        }
        if let Some(m) = &r.status_message
            && (m.len() > MAX_STATUS_MESSAGE
                || m.bytes().any(|b| b != b'\t' && (b < 0x20 || b == 0x7f)))
        {
            return Err(format!(
                "{at}statusMessage must be at most {MAX_STATUS_MESSAGE} bytes without control characters"
            ));
        }
        if let Some(n) = &r.body_file_name {
            if n.trim().is_empty() || n.len() > MAX_BODY_FILE_NAME {
                return Err(format!(
                    "{at}bodyFileName must be 1..={MAX_BODY_FILE_NAME} bytes"
                ));
            }
            if r.templated {
                t.validate(n)
                    .map_err(|e| format!("{at}bodyFileName: {e}"))?;
            }
        }
        if r.templated {
            if let Some(b) = &r.body {
                t.validate(b).map_err(|e| format!("{at}body: {e}"))?;
            }
            if let Some(b) = &r.json_body {
                t.validate_json(b)
                    .map_err(|e| format!("{at}jsonBody: {e}"))?;
            }
            for (k, v) in &r.headers {
                t.validate(v).map_err(|e| format!("{at}header {k}: {e}"))?;
            }
        }
        for k in r.headers.keys() {
            http::HeaderName::from_bytes(k.as_bytes())
                .map_err(|_| format!("{at}invalid header name {k}"))?;
        }
        Ok(())
    }

    pub fn validate(&self, stub: &Stub) -> Result<(), String> {
        if serde_json::to_string(stub)
            .map_err(|e| e.to_string())?
            .contains("\\u0000")
        {
            return Err("NUL characters are not supported in stubs".into());
        }
        CompiledStub::compile(stub).map_err(|e| e.to_string())?;
        crate::mcp::validate(stub)?;
        let t = &self.templates;
        if stub.responses.is_empty() {
            if stub.response_mode.is_some() {
                return Err("responseMode needs responses".into());
            }
            self.validate_response(&stub.response, "")?;
        } else {
            if stub.response != ResponseDef::default() {
                return Err("use either response or responses, not both".into());
            }
            if stub.responses.len() > MAX_RESPONSES_PER_STUB {
                return Err(format!(
                    "at most {MAX_RESPONSES_PER_STUB} responses per stub"
                ));
            }
            for (i, r) in stub.responses.iter().enumerate() {
                self.validate_response(r, &format!("responses[{i}]: "))?;
            }
        }
        if stub.times == Some(0) {
            return Err("times must be at least 1".into());
        }
        if stub.ttl_seconds.is_some_and(|x| x == 0 || x > MAX_TTL_SECS) {
            return Err("ttlSeconds must be between 1 and 31536000".into());
        }
        if stub.messages.len() > MAX_MESSAGES_PER_STUB {
            return Err(format!("at most {MAX_MESSAGES_PER_STUB} messages per stub"));
        }
        for (i, m) in stub.messages.iter().enumerate() {
            m.check_fields()
                .map_err(|e| format!("messages[{i}]: {e}"))?;
            if m.headers.len() > MAX_MESSAGE_HEADERS {
                return Err(format!(
                    "messages[{i}]: at most {MAX_MESSAGE_HEADERS} headers per message"
                ));
            }
            let name_max = match m.protocol {
                crate::model::MessageProtocol::Amqp => AMQP_SHORT,
                crate::model::MessageProtocol::Mqtt => MQTT_STRING,
                crate::model::MessageProtocol::Kafka => MAX_HEADER_NAME_BYTES,
            };
            if let Some(k) = m
                .headers
                .keys()
                .find(|k| k.is_empty() || k.len() > name_max)
            {
                return Err(format!(
                    "messages[{i}]: header name `{}` must be 1..{name_max} bytes",
                    k.chars().take(40).collect::<String>()
                ));
            }
            for (name, tpl) in m.template_fields() {
                t.validate(tpl)
                    .map_err(|x| format!("messages[{i}].{name}: {x}"))?;
            }
            for (k, v) in &m.headers {
                t.validate(v)
                    .map_err(|x| format!("messages[{i}].headers.{k}: {x}"))?;
            }
            if let Some(v) = &m.value {
                t.validate(v)
                    .map_err(|x| format!("messages[{i}].value: {x}"))?;
            }
            if let Some(v) = &m.json_value {
                t.validate_json(v)
                    .map_err(|x| format!("messages[{i}].jsonValue: {x}"))?;
            }
        }
        if let Some(s) = &stub.scenario {
            if s.name.trim().is_empty() {
                return Err("scenario.name must not be empty".into());
            }
            t.validate(&s.name)
                .map_err(|e| format!("scenario.name: {e}"))?;
            if s.ttl_seconds.is_some_and(|x| x == 0 || x > MAX_TTL_SECS) {
                return Err("scenario.ttlSeconds must be between 1 and 31536000".into());
            }
        }
        for (i, c) in stub.callbacks.iter().enumerate() {
            t.validate(&c.url)
                .map_err(|e| format!("callbacks[{i}].url: {e}"))?;
            if let Some(b) = &c.body {
                t.validate(b)
                    .map_err(|e| format!("callbacks[{i}].body: {e}"))?;
            }
            if let Some(b) = &c.json_body {
                t.validate_json(b)
                    .map_err(|e| format!("callbacks[{i}].jsonBody: {e}"))?;
            }
            for (k, v) in &c.headers {
                if crate::outbound::blocked_outbound_header(k) {
                    return Err(format!(
                        "callbacks[{i}].headers: {}",
                        crate::outbound::blocked_reason(k)
                    ));
                }
                t.validate(v)
                    .map_err(|e| format!("callbacks[{i}].headers.{k}: {e}"))?;
            }
            if c.delay.as_ref().is_some_and(|d| {
                [d.fixed_ms, d.min_ms, d.max_ms]
                    .iter()
                    .flatten()
                    .any(|v| *v > MAX_CALLBACK_DELAY_MS)
            }) {
                return Err(format!("callbacks[{i}].delay is limited to 7 days"));
            }
            if c.max_attempts == 0 || c.max_attempts > 50 {
                return Err(format!("callbacks[{i}].maxAttempts must be 1..=50"));
            }
            http::Method::from_bytes(c.method.as_bytes())
                .map_err(|_| format!("callbacks[{i}].method invalid"))?;
        }
        Ok(())
    }

    pub async fn reload(&self) -> anyhow::Result<usize> {
        let _guard = self.reload_lock.lock().await;
        let revision = self.stores.stubs.revision().await?;
        let list = self.stores.stubs.list().await?;
        let mut compiled = Vec::with_capacity(list.len());
        for s in &list {
            match CompiledStub::compile(s) {
                Ok(c) => compiled.push(c),
                Err(e) => {
                    tracing::warn!(stub_id = %s.id, error = %e, "skipping stub that does not compile")
                }
            }
        }
        compiled.sort_by_key(stub_rank);
        let n = compiled.len();
        let loaded = self.revision.load(Ordering::SeqCst);
        if loaded != i64::MIN && revision < loaded {
            return Ok(self.snapshot().len());
        }
        let has_mcp = compiled.iter().any(|c| c.stub.request.mcp.is_some());
        *self.cache.write().unwrap() = Arc::new(compiled);
        self.has_mcp.store(has_mcp, Ordering::SeqCst);
        self.revision.store(revision, Ordering::SeqCst);
        Ok(n)
    }

    pub async fn refresh_if_changed(&self) -> anyhow::Result<bool> {
        let revision = self.stores.stubs.revision().await?;
        if revision == self.revision.load(Ordering::SeqCst) {
            return Ok(false);
        }
        self.reload().await?;
        Ok(true)
    }

    pub fn snapshot(&self) -> Arc<Vec<CompiledStub>> {
        self.cache.read().unwrap().clone()
    }

    pub fn near_misses(
        &self,
        req: &CapturedRequest,
        limit: usize,
        spaces: &SpaceFilter,
    ) -> Vec<NearMiss> {
        let json = req.json();
        let mut v: Vec<NearMiss> = self
            .snapshot()
            .iter()
            .filter(|c| c.stub.enabled && spaces.matches(c.stub.space.as_deref()))
            .map(|c| c.near_miss(req, json))
            .filter(|n| n.score > 0.0)
            .collect();
        v.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        v.truncate(limit);
        v
    }

    pub async fn settings(&self) -> Arc<GlobalSettings> {
        {
            let r = self.settings.read().unwrap();
            if r.0.elapsed() < SETTINGS_TTL {
                return r.1.clone();
            }
        }
        let loaded = match self.stores.data.get(SETTINGS_NAMESPACE, SETTINGS_KEY).await {
            Ok(Some(v)) => match serde_json::from_value::<GlobalSettings>(v) {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    metrics::counter!("fastmock_store_errors_total", "store" => "settings")
                        .increment(1);
                    tracing::error!(error = %e, "stored global settings are invalid; using defaults");
                    Arc::new(GlobalSettings::default())
                }
            },
            Ok(None) => Arc::new(GlobalSettings::default()),
            Err(e) => {
                metrics::counter!("fastmock_store_errors_total", "store" => "settings")
                    .increment(1);
                tracing::warn!(error = %e, "reading global settings failed; keeping the previous ones");
                let mut w = self.settings.write().unwrap();
                w.0 = Instant::now();
                return w.1.clone();
            }
        };
        *self.settings.write().unwrap() = (Instant::now(), loaded.clone());
        loaded
    }

    pub fn cache_settings(&self, s: GlobalSettings) {
        *self.settings.write().unwrap() = (Instant::now(), Arc::new(s));
    }

    /// Paths and hosts owned by spaces; requests under them skip the stubs of other spaces.
    pub fn set_ownership(&self, o: crate::ownership::Ownership) {
        *self.ownership.write().unwrap() = Arc::new(o);
    }

    /// The one space owning a request path and host, if any.
    pub fn owner(&self, path: &str, host: Option<&str>, port: Option<u16>) -> Option<String> {
        let o = self.ownership.read().unwrap().clone();
        let owners = o.owners(path, host, port);
        match owners.as_slice() {
            [first, rest @ ..] if rest.iter().all(|r| r == first) => Some(first.to_string()),
            _ => None,
        }
    }

    pub fn invalidate_files(&self) {
        self.files.lock().unwrap().clear();
    }

    pub async fn body_file(&self, name: &str) -> anyhow::Result<Option<Arc<BodyFile>>> {
        self.body_file_in(None, name).await
    }

    /// A body file of the global area or of one space.
    pub async fn body_file_in(
        &self,
        space: Option<&str>,
        name: &str,
    ) -> anyhow::Result<Option<Arc<BodyFile>>> {
        let ns = files_namespace(space);
        let cache_key = format!("{ns}\0{name}");
        if let Some((at, f)) = self.files.lock().unwrap().get(&cache_key)
            && at.elapsed() < FILE_CACHE_TTL
        {
            return Ok(f.clone());
        }
        let loaded = match self.stores.data.get(&ns, name).await? {
            Some(v) => match decode_body_file(&v) {
                Some(f) => Some(Arc::new(f)),
                None => {
                    tracing::error!(file = %name, "stored body file is not valid");
                    None
                }
            },
            None => None,
        };
        let mut cache = self.files.lock().unwrap();
        let cached: usize = cache
            .values()
            .filter_map(|(_, f)| f.as_ref().map(|f| f.bytes.len()))
            .sum();
        let size = loaded.as_ref().map(|f| f.bytes.len()).unwrap_or(0);
        if size <= FILE_CACHE_MAX_ENTRY {
            if cache.len() >= FILE_CACHE_MAX_ENTRIES || cached + size > FILE_CACHE_MAX_BYTES {
                cache.clear();
            }
            cache.insert(cache_key, (Instant::now(), loaded.clone()));
        }
        Ok(loaded)
    }

    pub async fn reset_counters(&self) -> anyhow::Result<u64> {
        self.stores.hot.delete_prefix(COUNTER_PREFIX).await
    }

    async fn pick_response(&self, stub: &Stub) -> anyhow::Result<usize> {
        let n = stub.responses.len();
        if n <= 1 {
            return Ok(0);
        }
        Ok(
            match stub.response_mode.unwrap_or(ResponseMode::Sequential) {
                ResponseMode::Random => rand::random_range(0..n),
                mode => {
                    let seen = self
                        .stores
                        .hot
                        .incr(&counter_key("seq", stub), Some(counter_ttl(stub)))
                        .await?;
                    let i = usize::try_from(seen.max(1) - 1).unwrap_or(usize::MAX);
                    if mode == ResponseMode::Cycle {
                        i % n
                    } else {
                        i.min(n - 1)
                    }
                }
            },
        )
    }

    /// The enabled MCP stubs (`request.mcp`) whose URL and origin rules cover the request, in matching
    /// order, or `None` when the request's path serves no mocked MCP server. Costs one atomic load while
    /// no MCP stub is loaded.
    pub fn mcp_endpoint(&self, req: &CapturedRequest) -> Option<Vec<Stub>> {
        if !self.has_mcp.load(Ordering::Relaxed) {
            return None;
        }
        let stubs = self.snapshot();
        let now = Utc::now();
        let ownership = self.ownership.read().unwrap().clone();
        let owners = ownership.owners(&req.path, req.host.as_deref(), req.port);
        let list: Vec<Stub> = stubs
            .iter()
            .filter(|cs| {
                cs.stub.request.mcp.is_some()
                    && cs.stub.enabled
                    && !cs.stub.is_expired(now)
                    && crate::ownership::Ownership::admits(&owners, cs.stub.space.as_deref())
                    && cs.endpoint_matches(req)
            })
            .map(|cs| cs.stub.clone())
            .collect();
        (!list.is_empty()).then_some(list)
    }

    pub async fn handle(&self, req: &CapturedRequest) -> anyhow::Result<Option<EngineResponse>> {
        self.handle_where(req, |_| true).await
    }

    /// Like [`Engine::handle`], considering only the stubs `keep` accepts.
    pub async fn handle_where(
        &self,
        req: &CapturedRequest,
        keep: impl Fn(&CompiledStub) -> bool,
    ) -> anyhow::Result<Option<EngineResponse>> {
        let json = req.json();
        let stubs = self.snapshot();
        let now = Utc::now();
        let ownership = self.ownership.read().unwrap().clone();
        let owners = ownership.owners(&req.path, req.host.as_deref(), req.port);
        'attempt: for _ in 0..8 {
            for cs in stubs.iter() {
                if !cs.stub.enabled
                    || !keep(cs)
                    || cs.stub.is_expired(now)
                    || !crate::ownership::Ownership::admits(&owners, cs.stub.space.as_deref())
                {
                    continue;
                }
                let Some(params) = cs.match_request(req, json) else {
                    continue;
                };
                let mut ctx = crate::template::context(
                    req.template_context(&params, cs.uses_original_request),
                );
                let mut transition = None;
                if let Some(sc) = &cs.stub.scenario {
                    let name = self.templates.render_in(&sc.name, &ctx)?;
                    if name.len() > MAX_SCENARIO_NAME {
                        anyhow::bail!("rendered scenario name exceeds {MAX_SCENARIO_NAME} bytes");
                    }
                    let key = scenario_key(cs.stub.space.as_deref(), &name);
                    let raw = self.stores.hot.get(&key).await?;
                    let current = raw.clone().unwrap_or_else(|| SCENARIO_STARTED.to_string());
                    if sc.required_state.as_ref().is_some_and(|r| *r != current) {
                        continue;
                    }
                    ctx.data_mut()["state"] = json!({"scenario": name, "current": current});
                    if let Some(new_state) = &sc.new_state {
                        let ttl = Some(Duration::from_secs(
                            sc.ttl_seconds.unwrap_or(DEFAULT_SCENARIO_TTL_SECS),
                        ));
                        transition = Some((key, raw, new_state.clone(), ttl));
                    }
                }
                let mut times_key = None;
                if let Some(limit) = cs.stub.times {
                    let key = counter_key("times", &cs.stub);
                    let used = self
                        .stores
                        .hot
                        .incr(&key, Some(counter_ttl(&cs.stub)))
                        .await?;
                    if used > i64::try_from(limit).unwrap_or(i64::MAX) {
                        let label = cs
                            .stub
                            .name
                            .clone()
                            .unwrap_or_else(|| cs.stub.id.to_string());
                        metrics::counter!("fastmock_stub_exhausted_total", "stub" => label)
                            .increment(1);
                        continue;
                    }
                    times_key = Some(key);
                }
                if let Some((key, raw, new_state, ttl)) = transition
                    && !self
                        .stores
                        .hot
                        .compare_and_set(&key, raw.as_deref(), &new_state, ttl)
                        .await?
                {
                    if let Some(k) = times_key
                        && let Err(e) = self.stores.hot.incr_by(&k, -1, None).await
                    {
                        tracing::warn!(error = %e, "returning an unused times slot failed");
                    }
                    continue 'attempt;
                }
                let index = self.pick_response(&cs.stub).await?;
                return Ok(Some(
                    crate::plugin::SPACE
                        .scope(
                            cs.stub.space.clone(),
                            self.respond(cs, index, &ctx, req.grpc_call),
                        )
                        .await?,
                ));
            }
            return Ok(None);
        }
        anyhow::bail!("scenario state kept changing concurrently; giving up after 8 attempts")
    }

    async fn respond(
        &self,
        cs: &CompiledStub,
        index: usize,
        ctx: &crate::template::TemplateContext,
        grpc_call: bool,
    ) -> anyhow::Result<EngineResponse> {
        let stub = &cs.stub;
        let r = stub.response_at(index);
        let t = &self.templates;
        let mut headers = Vec::new();
        for (k, v) in &r.headers {
            for line in v.split(crate::model::header_values::SEPARATOR) {
                headers.push((
                    k.clone(),
                    if r.templated {
                        t.render_in(line, ctx)?
                    } else {
                        line.to_string()
                    },
                ));
            }
        }
        let body = if grpc_call {
            Bytes::new()
        } else if let Some(b) = &r.base64_body {
            Bytes::from(B64.decode(b)?)
        } else if let Some(j) = &r.json_body {
            if !headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            {
                headers.push(("content-type".into(), "application/json".into()));
            }
            let v = if r.templated {
                t.render_json_in(j, ctx)?
            } else {
                j.clone()
            };
            let out = serde_json::to_vec(&v)?;
            if out.len() > crate::template::MAX_OUTPUT_BYTES {
                anyhow::bail!(
                    "rendered jsonBody exceeds {} bytes",
                    crate::template::MAX_OUTPUT_BYTES
                );
            }
            Bytes::from(out)
        } else if let Some(b) = &r.body {
            Bytes::from(if r.templated {
                t.render_in(b, ctx)?
            } else {
                b.clone()
            })
        } else if let Some(n) = &r.body_file_name {
            let name = if r.templated {
                t.render_in(n, ctx)?
            } else {
                n.clone()
            };
            let file = if !valid_file_name(&name) {
                None
            } else if let Some(space) = &stub.space {
                match self.body_file_in(Some(space), &name).await? {
                    Some(f) => Some(f),
                    None => self.body_file_in(None, &name).await?,
                }
            } else {
                self.body_file_in(None, &name).await?
            };
            let Some(file) = file else {
                metrics::counter!("fastmock_body_file_missing_total").increment(1);
                return Err(BodyFileMissing(name).into());
            };
            if let Some(ct) = &file.content_type
                && !headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            {
                headers.push(("content-type".into(), ct.clone()));
            }
            match std::str::from_utf8(&file.bytes) {
                Ok(text)
                    if r.templated
                        && text.contains("{{")
                        && is_textual(file.content_type.as_deref(), &file.bytes) =>
                {
                    Bytes::from(t.render_in(text, ctx)?)
                }
                _ => file.bytes.clone(),
            }
        } else {
            Bytes::new()
        };
        let sse = match &r.sse {
            None => None,
            Some(events) => {
                let mut out = Vec::with_capacity(events.len());
                for e in events {
                    out.push(crate::model::SseEvent {
                        event: e.event.as_ref().map(|x| t.render_in(x, ctx)).transpose()?,
                        id: e.id.as_ref().map(|x| t.render_in(x, ctx)).transpose()?,
                        data: t.render_in(&e.data, ctx)?,
                        delay_ms: e.delay_ms,
                    });
                }
                Some(out)
            }
        };
        let messages = self.render_messages(stub, ctx)?;
        let mut correlation = None;
        for c in &stub.callbacks {
            let mut job = self.build_callback(c, ctx).await;
            job.space = stub.space.clone();
            if correlation.is_none() {
                correlation = job.correlation.clone();
            }
            self.stores.outbox.enqueue(&job).await?;
        }
        let global = self.settings().await;
        let base_delay = r
            .delay
            .as_ref()
            .map(|d| d.sample_ms())
            .or(global.fixed_delay_ms)
            .unwrap_or(0);
        let extra_delay = global.delay.as_ref().map(|d| d.sample_ms()).unwrap_or(0);
        Ok(EngineResponse {
            status: r.status,
            headers,
            body,
            stub_id: stub.id,
            stub_name: stub.name.clone(),
            fault: r.fault,
            delay_ms: base_delay.saturating_add(extra_delay),
            correlation,
            callbacks: stub.callbacks.len(),
            proxy: r.proxy.clone(),
            sse,
            messages,
            status_message: r.status_message.clone(),
            chunked_dribble_delay: r.chunked_dribble_delay.clone(),
            response_index: index,
            space: stub.space.clone(),
            grpc: (grpc_call || r.grpc.is_some()).then(|| {
                Box::new(GrpcPlan {
                    def: r.grpc.clone(),
                    json_body: r.json_body.clone(),
                    templated: r.templated,
                    context: ctx.data().clone(),
                })
            }),
        })
    }

    /// Renders the messages of a stub with one shared budget for all their fields, each field cut off at
    /// the length its protocol allows.
    fn render_messages(
        &self,
        stub: &Stub,
        ctx: &crate::template::TemplateContext,
    ) -> anyhow::Result<Vec<RenderedMessage>> {
        use crate::model::MessageProtocol;
        let t = &self.templates;
        let _scope = t.limit_scope(MAX_MESSAGES_RENDER_BYTES);
        let mut messages = Vec::with_capacity(stub.messages.len().min(MAX_MESSAGES_PER_STUB));
        for (i, m) in stub.messages.iter().enumerate() {
            let field = |name: &str, tpl: &str, cap: usize| {
                t.render_in_capped(tpl, ctx, cap)
                    .map_err(|e| anyhow::anyhow!("messages[{i}].{name}: {e}"))
            };
            let opt = |name: &str, x: &Option<String>, cap: usize| {
                x.as_ref().map(|v| field(name, v, cap)).transpose()
            };
            let protocol = m.protocol;
            let (short, header_cap, topic_cap) = match protocol {
                MessageProtocol::Kafka => (usize::MAX, usize::MAX, KAFKA_TOPIC),
                MessageProtocol::Mqtt => (MQTT_STRING, MQTT_STRING, MQTT_STRING),
                MessageProtocol::Amqp => (AMQP_SHORT, usize::MAX, AMQP_SHORT),
            };
            let mut headers = std::collections::BTreeMap::new();
            for (k, v) in m.headers.iter().take(MAX_MESSAGE_HEADERS) {
                headers.insert(k.clone(), field(&format!("headers.{k}"), v, header_cap)?);
            }
            let value = match (&m.json_value, &m.value) {
                (Some(j), _) => t
                    .render_json_in(j, ctx)
                    .map_err(|e| anyhow::anyhow!("messages[{i}].jsonValue: {e}"))?,
                (None, Some(v)) => Value::String(field("value", v, usize::MAX)?),
                (None, None) => Value::Null,
            };
            let (topic, mqtt, amqp) = match protocol {
                MessageProtocol::Kafka => (field("topic", &m.topic, topic_cap)?, None, None),
                MessageProtocol::Mqtt => (
                    field("topic", &m.topic, topic_cap)?,
                    Some(RenderedMqtt {
                        qos: m.qos.unwrap_or(0),
                        retain: m.retain.unwrap_or(false),
                        correlation_data: opt("correlationData", &m.correlation_data, short)?,
                        response_topic: opt("responseTopic", &m.response_topic, short)?,
                        message_expiry_seconds: m.message_expiry_seconds,
                    }),
                    None,
                ),
                MessageProtocol::Amqp => (
                    opt("routingKey", &m.routing_key, short)?.unwrap_or_default(),
                    None,
                    Some(RenderedAmqp {
                        exchange: opt("exchange", &m.exchange, short)?.unwrap_or_default(),
                        exchange_templated: m.exchange.as_ref().is_some_and(|e| e.contains("{{")),
                        exchange_type: m.exchange_type.clone(),
                        durable: true,
                        message_id: opt("messageId", &m.message_id, short)?,
                        correlation_id: opt("correlationId", &m.correlation_id, short)?,
                        reply_to: opt("replyTo", &m.reply_to, short)?,
                        expiration: opt("expiration", &m.expiration, short)?,
                        persistent: m.persistent.unwrap_or(false),
                        priority: m.priority,
                        message_type: opt("messageType", &m.message_type, short)?,
                        app_id: opt("appId", &m.app_id, short)?,
                    }),
                ),
            };
            messages.push(RenderedMessage {
                protocol,
                topic,
                key: opt("key", &m.key, usize::MAX)?,
                value,
                headers,
                avro_schema: m.avro_schema.clone(),
                avro_subject: m.avro_subject.clone(),
                invalid: m.needs_avro_subject().then(|| {
                    "avroSubject is required when the topic, exchange or routingKey is a template".to_string()
                }),
                content_type: opt("contentType", &m.content_type, short)?,
                delay_ms: m.delay_ms.unwrap_or(0),
                mqtt,
                amqp,
            });
        }
        Ok(messages)
    }

    async fn build_callback(
        &self,
        c: &CallbackDef,
        ctx: &crate::template::TemplateContext,
    ) -> CallbackJob {
        let t = &self.templates;
        let delay = c
            .delay
            .as_ref()
            .map(|d| d.sample_ms())
            .unwrap_or(0)
            .min(MAX_CALLBACK_DELAY_MS);
        let due = Utc::now() + chrono::Duration::milliseconds(delay as i64);
        let mut job = CallbackJob::new(
            STUB_SOURCE,
            String::new(),
            c.method.to_ascii_uppercase(),
            Default::default(),
            String::new(),
            due,
            c.max_attempts,
        );
        job.traceparent = current_traceparent();
        let built: anyhow::Result<()> = (|| {
            job.url = t.render_in(&c.url, ctx)?;
            for (k, v) in &c.headers {
                job.headers.insert(k.clone(), t.render_in(v, ctx)?);
            }
            job.body = if let Some(j) = &c.json_body {
                job.headers
                    .entry("content-type".into())
                    .or_insert_with(|| "application/json".into());
                serde_json::to_string(&t.render_json_in(j, ctx)?)?
            } else if let Some(b) = &c.body {
                t.render_in(b, ctx)?
            } else {
                String::new()
            };
            if let Some(corr) = &c.correlation {
                job.correlation = Some(t.render_in(corr, ctx)?);
            }
            if let Some(h) = &c.hmac {
                let secret = self.secrets.get(&h.secret_ref).ok_or_else(|| {
                    anyhow::anyhow!("secret `{}` is not configured", h.secret_ref)
                })?;
                let algo = match h.algorithm {
                    crate::model::HmacAlgorithm::Sha256 => "sha256",
                    crate::model::HmacAlgorithm::Sha512 => "sha512",
                };
                let raw = hmac_bytes(algo, secret.as_bytes(), job.body.as_bytes())?;
                let sig = match h.encoding {
                    SignatureEncoding::Hex => hex::encode(raw),
                    SignatureEncoding::Base64 => B64.encode(raw),
                };
                job.headers.insert(
                    h.header.clone(),
                    format!("{}{}", h.prefix.clone().unwrap_or_default(), sig),
                );
            }
            Ok(())
        })();
        if let Err(e) = built {
            job.status = CallbackStatus::Failed;
            job.last_error = Some(format!("callback could not be built: {e}"));
        }
        job
    }

    /// Stored scenario states as `(space, name, state)`.
    pub async fn scenario_states(&self) -> anyhow::Result<Vec<(Option<String>, String, String)>> {
        let mut out: Vec<(Option<String>, String, String)> = self
            .stores
            .hot
            .scan(SCENARIO_PREFIX, 10_000)
            .await?
            .into_iter()
            .filter_map(|(k, v)| Some((None, k.strip_prefix(SCENARIO_PREFIX)?.to_string(), v)))
            .collect();
        out.extend(
            self.stores
                .hot
                .scan(SPACE_SCENARIO_PREFIX, 10_000)
                .await?
                .into_iter()
                .filter_map(|(k, v)| {
                    let (space, name) = k.strip_prefix(SPACE_SCENARIO_PREFIX)?.split_once(':')?;
                    Some((Some(space.to_string()), name.to_string(), v))
                }),
        );
        Ok(out)
    }

    pub async fn set_scenario_state(
        &self,
        space: Option<&str>,
        name: &str,
        state: &str,
    ) -> anyhow::Result<()> {
        self.stores
            .hot
            .set(&scenario_key(space, name), state, None)
            .await
    }

    pub async fn reset_scenarios(&self) -> anyhow::Result<u64> {
        Ok(self.stores.hot.delete_prefix(SCENARIO_PREFIX).await?
            + self.stores.hot.delete_prefix(SPACE_SCENARIO_PREFIX).await?)
    }

    /// Resets the scenarios of the global area only.
    pub async fn reset_global_scenarios(&self) -> anyhow::Result<u64> {
        self.stores.hot.delete_prefix(SCENARIO_PREFIX).await
    }

    /// Resets the scenarios of the given spaces only.
    pub async fn reset_scenarios_in(&self, spaces: &[String]) -> anyhow::Result<u64> {
        let mut n = 0;
        for s in spaces {
            n += self
                .stores
                .hot
                .delete_prefix(&format!("{SPACE_SCENARIO_PREFIX}{s}:"))
                .await?;
        }
        Ok(n)
    }
}

/// Sort key of a stub: priority first; at equal priority, stubs with an `order` (imported from WireMock or
/// MockServer) come first in that order, and the remaining ones by URL specificity.
pub fn stub_rank(c: &CompiledStub) -> (i32, bool, i64, (u8, usize)) {
    match c.stub.order {
        Some(o) => (c.stub.priority, false, o, (0, 0)),
        None => (c.stub.priority, true, 0, c.specificity()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::memory_stores;
    use crate::storage::NoSecrets;
    use std::collections::HashMap;

    struct Secrets(HashMap<String, String>);
    impl SecretProvider for Secrets {
        fn get(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }
    }

    fn engine() -> (Engine, Stores) {
        let stores = memory_stores(1000);
        let secrets = Secrets(HashMap::from([("hook".to_string(), "key".to_string())]));
        (
            Engine::new(
                stores.clone(),
                Arc::new(Templates::new()),
                Arc::new(secrets),
            ),
            stores,
        )
    }

    async fn add(e: &Engine, s: &Stores, v: Value) -> Stub {
        let stub: Stub = serde_json::from_value(v).unwrap();
        e.validate(&stub).unwrap();
        s.stubs.upsert(&stub).await.unwrap();
        e.reload().await.unwrap();
        stub
    }

    #[test]
    fn websocket_definitions_are_validated_on_save() {
        let (e, _) = engine();
        let ok: Stub = serde_json::from_value(json!({
            "request": {"urlPath": "/ws"},
            "response": {"websocket": {"messages": [{"data": "hi {{uuid}}"}]}}
        }))
        .unwrap();
        assert!(e.validate(&ok).is_ok());
        let bad_template: Stub = serde_json::from_value(json!({
            "request": {"urlPath": "/ws"},
            "response": {"websocket": {"messages": [{"data": "{{> partial}}"}]}}
        }))
        .unwrap();
        assert!(e.validate(&bad_template).is_err());
        let with_sse: Stub = serde_json::from_value(json!({
            "request": {"urlPath": "/ws"},
            "response": {"websocket": {"messages": [{"data": "x"}]}, "sse": [{"data": "y"}]}
        }))
        .unwrap();
        assert!(e.validate(&with_sse).is_err());
        let repeat_without_delay: Stub = serde_json::from_value(json!({
            "request": {"urlPath": "/ws"},
            "response": {"websocket": {"repeat": true, "messages": [{"data": "x"}]}}
        }))
        .unwrap();
        assert!(e.validate(&repeat_without_delay).is_err());
    }

    fn post(path: &str, body: Value) -> CapturedRequest {
        CapturedRequest::new("POST", path, None, vec![], Bytes::from(body.to_string()))
    }

    #[tokio::test]
    async fn grpc_answers_are_validated_and_returned_unrendered() {
        let (e, s) = engine();
        let bad = |v: Value| {
            let stub: Stub =
                serde_json::from_value(json!({"request": {"urlPath": "/x.Y/Z"}, "response": v}))
                    .unwrap();
            e.validate(&stub).is_err()
        };
        assert!(bad(
            json!({"grpc": {"messages": [{"jsonBody": {"a": "{{> p}}"}}]}})
        ));
        assert!(bad(json!({"grpc": {"status": {"code": "NOPE"}}})));
        assert!(bad(json!({"grpc": {}, "sse": [{"data": "x"}]})));
        assert!(bad(
            json!({"grpc": {"onMessage": [{"match": [{"matches": "("}], "status": {"code": 1}}]}})
        ));
        add(
            &e,
            &s,
            json!({"request": {"urlPath": "/shop.v1.S/M", "grpc": {"messageCount": {"min": 2}}},
                   "response": {"jsonBody": {"n": "{{size request.json}}"},
                                "grpc": {"messages": [{"jsonBody": {"i": "{{stream.index}}"}}]}}}),
        )
        .await;
        let mut one = post("/shop.v1.S/M", json!([{"a": 1}]));
        one.grpc_call = true;
        assert!(
            e.handle(&one).await.unwrap().is_none(),
            "messageCount min 2"
        );
        let mut two = post("/shop.v1.S/M", json!([{"a": 1}, {"a": 2}]));
        two.grpc_call = true;
        let r = e.handle(&two).await.unwrap().unwrap();
        assert!(
            r.body.is_empty(),
            "gRPC bodies are rendered while they are sent"
        );
        let plan = r.grpc.unwrap();
        assert_eq!(plan.json_body.unwrap()["n"], "{{size request.json}}");
        assert_eq!(plan.context["request"]["json"][1]["a"], 2);
        assert_eq!(plan.def.unwrap().messages.len(), 1);
    }

    #[tokio::test]
    async fn stored_avro_stubs_with_templated_destinations_publish_nothing() {
        let (e, s) = engine();
        let stub: Stub = serde_json::from_value(json!({"request": {"urlPath": "/old"}, "response": {"status": 202},
            "messages": [{"topic": "orders.{{request.body}}", "avroSchema": {"type": "string"}, "value": "x"},
                         {"topic": "fixed", "avroSchema": {"type": "string"}, "value": "x"}]}))
        .unwrap();
        assert!(e.validate(&stub).is_err());
        s.stubs.upsert(&stub).await.unwrap();
        e.reload().await.unwrap();
        let r = e
            .handle(&CapturedRequest::new(
                "POST",
                "/old",
                None,
                vec![],
                Bytes::from("a"),
            ))
            .await
            .unwrap()
            .unwrap();
        assert!(
            r.messages[0]
                .invalid
                .as_ref()
                .unwrap()
                .contains("avroSubject")
        );
        assert!(r.messages[1].invalid.is_none());
    }

    #[tokio::test]
    async fn message_fields_share_one_render_budget_and_protocol_limits() {
        let (e, s) = engine();
        add(&e, &s, json!({"request": {"urlPath": "/amqp"}, "response": {"status": 202},
            "messages": [{"protocol": "amqp", "exchange": "orders", "routingKey": "{{request.body}}"}]})).await;
        let r = e
            .handle(&CapturedRequest::new(
                "POST",
                "/amqp",
                None,
                vec![],
                Bytes::from("k".repeat(300)),
            ))
            .await;
        let err = format!("{:#}", r.expect_err("a routing key over 255 bytes fails"));
        assert!(
            err.contains("messages[0].routingKey") && err.contains("255"),
            "{err}"
        );
        let many: Vec<Value> = (0..12)
            .map(|i| json!({"topic": format!("t{i}"), "value": "{{request.body}}", "headers": {"a": "{{request.body}}", "b": "{{request.body}}"}}))
            .collect();
        add(
            &e,
            &s,
            json!({"request": {"urlPath": "/many"}, "response": {"status": 202}, "messages": many}),
        )
        .await;
        let big = "x".repeat(600 * 1024);
        let r = e
            .handle(&CapturedRequest::new(
                "POST",
                "/many",
                None,
                vec![],
                Bytes::from(big),
            ))
            .await;
        let err = format!(
            "{:#}",
            r.expect_err("36 fields of 600 KiB exceed the shared budget")
        );
        assert!(err.contains("budget"), "{err}");
        let ok = e
            .handle(&CapturedRequest::new(
                "POST",
                "/many",
                None,
                vec![],
                Bytes::from("y"),
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ok.messages.len(), 12);
        let too_many: Vec<Value> = (0..=MAX_MESSAGES_PER_STUB)
            .map(|_| json!({"topic": "t"}))
            .collect();
        let stub: Stub =
            serde_json::from_value(json!({"request": {"urlPath": "/x"}, "messages": too_many}))
                .unwrap();
        assert!(e.validate(&stub).unwrap_err().contains("messages per stub"));
        let headers: serde_json::Map<String, Value> = (0..=MAX_MESSAGE_HEADERS)
            .map(|i| (format!("h{i}"), json!("v")))
            .collect();
        let stub: Stub = serde_json::from_value(
            json!({"request": {"urlPath": "/x"}, "messages": [{"topic": "t", "headers": headers}]}),
        )
        .unwrap();
        assert!(
            e.validate(&stub)
                .unwrap_err()
                .contains("headers per message")
        );
        let mut long_name = serde_json::Map::new();
        long_name.insert("h".repeat(256), json!("v"));
        let stub: Stub = serde_json::from_value(json!({"request": {"urlPath": "/x"}, "messages": [{"protocol": "amqp", "routingKey": "k", "headers": long_name}]})).unwrap();
        assert!(e.validate(&stub).unwrap_err().contains("header name"));
    }

    #[tokio::test]
    async fn mqtt_and_amqp_messages_are_rendered() {
        let (e, s) = engine();
        add(&e, &s, json!({"request": {"urlPathTemplate": "/ship/{id}"}, "response": {"status": 202},
            "messages": [
                {"protocol": "mqtt", "topic": "orders/{{request.pathParams.id}}", "qos": 2, "retain": true,
                 "correlationData": "c-{{request.pathParams.id}}", "responseTopic": "replies/{{request.pathParams.id}}",
                 "messageExpirySeconds": 60, "contentType": "application/json", "headers": {"k": "{{request.pathParams.id}}"},
                 "jsonValue": {"id": "{{request.pathParams.id}}"}, "delayMs": 50},
                {"protocol": "amqp", "exchange": "ex-{{request.pathParams.id}}", "routingKey": "order.{{request.pathParams.id}}",
                 "exchangeType": "direct", "messageId": "m-{{request.pathParams.id}}", "correlationId": "c", "replyTo": "r",
                 "expiration": "1000", "persistent": true, "priority": 3, "messageType": "t", "appId": "a", "value": "plain"}
            ]})).await;
        let r = e
            .handle(&post("/ship/A-1", json!({})))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.messages.len(), 2);
        let m = &r.messages[0];
        assert_eq!(m.protocol, crate::model::MessageProtocol::Mqtt);
        assert_eq!(m.topic, "orders/A-1");
        assert_eq!(m.value, json!({"id": "A-1"}));
        assert_eq!(m.headers["k"], "A-1");
        assert_eq!(m.content_type.as_deref(), Some("application/json"));
        assert_eq!(m.delay_ms, 50);
        assert_eq!(
            m.mqtt,
            Some(RenderedMqtt {
                qos: 2,
                retain: true,
                correlation_data: Some("c-A-1".into()),
                response_topic: Some("replies/A-1".into()),
                message_expiry_seconds: Some(60),
            })
        );
        let a = &r.messages[1];
        assert_eq!(a.topic, "order.A-1", "the routing key");
        assert_eq!(a.destination(), "ex-A-1/order.A-1");
        let o = a.amqp.as_ref().unwrap();
        assert_eq!(o.exchange, "ex-A-1");
        assert_eq!(o.exchange_type.as_deref(), Some("direct"));
        assert!(o.durable && o.persistent);
        assert_eq!(o.message_id.as_deref(), Some("m-A-1"));
        assert_eq!(o.expiration.as_deref(), Some("1000"));
        assert_eq!(o.priority, Some(3));
        assert_eq!(a.value, json!("plain"));
        let bad: Stub = serde_json::from_value(json!({"request": {"urlPath": "/x"},
            "messages": [{"protocol": "mqtt", "topic": "{{#if}}"}]}))
        .unwrap();
        assert!(e.validate(&bad).unwrap_err().contains("messages[0].topic"));
        let bad: Stub = serde_json::from_value(json!({"request": {"urlPath": "/x"},
            "messages": [{"protocol": "amqp", "routingKey": "k", "qos": 1}]}))
        .unwrap();
        assert!(
            e.validate(&bad)
                .unwrap_err()
                .contains("qos not used with protocol amqp")
        );
    }

    #[tokio::test]
    async fn priority_and_templated_json() {
        let (e, s) = engine();
        add(&e, &s, json!({"priority": 5, "request": {"urlPathTemplate": "/send/{id}"}, "response": {"status": 200, "jsonBody": {"id": "{{request.pathParams.id}}", "seq": "{{json request.json.seq}}"}}})).await;
        add(&e, &s, json!({"priority": 1, "request": {"urlPathTemplate": "/send/{id}", "body": [{"jsonPath": "$.seq", "value": {"equalTo": "0"}}]}, "response": {"status": 400}})).await;
        let r = e
            .handle(&post("/send/a", json!({"seq": 3})))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(
            serde_json::from_slice::<Value>(&r.body).unwrap(),
            json!({"id": "a", "seq": 3})
        );
        assert!(
            r.headers
                .iter()
                .any(|(k, v)| k == "content-type" && v == "application/json")
        );
        let r = e
            .handle(&post("/send/a", json!({"seq": 0})))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.status, 400);
        assert!(
            e.handle(&post("/other", json!({})))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn scenarios_are_keyed_by_template() {
        let (e, s) = engine();
        add(&e, &s, json!({"request": {"urlPath": "/order"}, "scenario": {"name": "order-{{request.json.id}}", "requiredState": "Started", "newState": "paid"}, "response": {"body": "created"}})).await;
        add(&e, &s, json!({"request": {"urlPath": "/order"}, "scenario": {"name": "order-{{request.json.id}}", "requiredState": "paid"}, "response": {"body": "already {{state.current}}"}})).await;
        let b = |r: Option<EngineResponse>| String::from_utf8(r.unwrap().body.to_vec()).unwrap();
        assert_eq!(
            b(e.handle(&post("/order", json!({"id": 1}))).await.unwrap()),
            "created"
        );
        assert_eq!(
            b(e.handle(&post("/order", json!({"id": 1}))).await.unwrap()),
            "already paid"
        );
        assert_eq!(
            b(e.handle(&post("/order", json!({"id": 2}))).await.unwrap()),
            "created"
        );
        assert_eq!(e.reset_scenarios().await.unwrap(), 2);
        assert_eq!(
            b(e.handle(&post("/order", json!({"id": 1}))).await.unwrap()),
            "created"
        );
    }

    #[tokio::test]
    async fn callbacks_are_enqueued_with_signature() {
        let (e, s) = engine();
        add(&e, &s, json!({"request": {"urlPath": "/msg"}, "response": {"status": 202},
            "callbacks": [{"url": "http://hook.example.com/cb", "jsonBody": {"id": "{{request.json.id}}"}, "correlation": "{{request.json.id}}",
                "delay": {"fixedMs": 1000}, "hmac": {"header": "X-Sig", "secretRef": "hook", "prefix": "sha256="}},
                {"url": "http://hook.example.com/missing", "hmac": {"header": "X-Sig", "secretRef": "absent"}}]})).await;
        let r = e
            .handle(&post("/msg", json!({"id": "m1"})))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.callbacks, 2);
        assert_eq!(r.correlation.as_deref(), Some("m1"));
        let jobs = s.outbox.list(&Default::default()).await.unwrap();
        let ok = jobs.iter().find(|j| j.url.ends_with("/cb")).unwrap();
        assert_eq!(ok.body, "{\"id\":\"m1\"}");
        let expected = hex::encode(hmac_bytes("sha256", b"key", ok.body.as_bytes()).unwrap());
        assert_eq!(ok.headers["X-Sig"], format!("sha256={expected}"));
        assert!(ok.due_at > Utc::now() + chrono::Duration::milliseconds(500));
        let bad = jobs.iter().find(|j| j.url.ends_with("/missing")).unwrap();
        assert_eq!(bad.status, CallbackStatus::Failed);
        assert!(bad.last_error.as_ref().unwrap().contains("absent"));
        let _ = NoSecrets;
    }

    fn body(r: Option<EngineResponse>) -> String {
        String::from_utf8(r.unwrap().body.to_vec()).unwrap()
    }

    fn get(path: &str) -> CapturedRequest {
        CapturedRequest::new("GET", path, None, vec![], Bytes::new())
    }

    #[tokio::test]
    async fn response_sequences() {
        let (e, s) = engine();
        add(&e, &s, json!({"request": {"urlPath": "/seq"}, "responses": [{"body": "a"}, {"body": "b"}, {"status": 503, "body": "c"}]})).await;
        add(&e, &s, json!({"request": {"urlPath": "/cycle"}, "responseMode": "cycle", "responses": [{"body": "1"}, {"body": "2"}]})).await;
        add(&e, &s, json!({"request": {"urlPath": "/rnd"}, "responseMode": "random", "responses": [{"body": "x"}, {"body": "y"}]})).await;
        let mut seq = vec![];
        for _ in 0..5 {
            seq.push(body(e.handle(&get("/seq")).await.unwrap()));
        }
        assert_eq!(seq, ["a", "b", "c", "c", "c"]);
        let mut cyc = vec![];
        for _ in 0..5 {
            cyc.push(body(e.handle(&get("/cycle")).await.unwrap()));
        }
        assert_eq!(cyc, ["1", "2", "1", "2", "1"]);
        for _ in 0..5 {
            assert!(["x", "y"].contains(&body(e.handle(&get("/rnd")).await.unwrap()).as_str()));
        }
        assert_eq!(e.reset_counters().await.unwrap(), 2);
        assert_eq!(body(e.handle(&get("/seq")).await.unwrap()), "a");
    }

    #[tokio::test]
    async fn times_and_expiry_fall_through() {
        let (e, s) = engine();
        add(
            &e,
            &s,
            json!({"priority": 9, "request": {"urlPath": "/t"}, "response": {"body": "fallback"}}),
        )
        .await;
        let limited = add(&e, &s, json!({"priority": 1, "times": 2, "request": {"urlPath": "/t"}, "response": {"body": "limited"}})).await;
        add(&e, &s, json!({"priority": 1, "expiresAt": "2001-01-01T00:00:00Z", "request": {"urlPath": "/t"}, "response": {"body": "expired"}})).await;
        let got: Vec<String> = futures_lite(&e, "/t", 4).await;
        assert_eq!(got, ["limited", "limited", "fallback", "fallback"]);
        let mut updated = limited.clone();
        updated.updated_at = None;
        s.stubs.upsert(&updated).await.unwrap();
        e.reload().await.unwrap();
        assert_eq!(
            body(e.handle(&get("/t")).await.unwrap()),
            "limited",
            "an update resets the counter"
        );
        add(&e, &s, json!({"priority": 0, "ttlSeconds": 1, "request": {"urlPath": "/t"}, "response": {"body": "ttl"}})).await;
        assert_eq!(body(e.handle(&get("/t")).await.unwrap()), "ttl");
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_ne!(body(e.handle(&get("/t")).await.unwrap()), "ttl");
    }

    async fn futures_lite(e: &Engine, path: &str, n: usize) -> Vec<String> {
        let mut v = vec![];
        for _ in 0..n {
            v.push(body(e.handle(&get(path)).await.unwrap()));
        }
        v
    }

    #[tokio::test]
    async fn global_settings_add_delays() {
        let (e, s) = engine();
        add(
            &e,
            &s,
            json!({"request": {"urlPath": "/a"}, "response": {"body": "a"}}),
        )
        .await;
        add(&e, &s, json!({"request": {"urlPath": "/b"}, "response": {"body": "b", "delay": {"fixedMs": 5}}})).await;
        assert_eq!(e.handle(&get("/a")).await.unwrap().unwrap().delay_ms, 0);
        s.data
            .put(
                SETTINGS_NAMESPACE,
                SETTINGS_KEY,
                &json!({"fixedDelayMs": 100, "delay": {"minMs": 10, "maxMs": 10}}),
            )
            .await
            .unwrap();
        e.cache_settings(GlobalSettings::default());
        assert_eq!(
            e.handle(&get("/a")).await.unwrap().unwrap().delay_ms,
            0,
            "cached"
        );
        *e.settings.write().unwrap() = (
            Instant::now() - SETTINGS_TTL * 2,
            Arc::new(GlobalSettings::default()),
        );
        assert_eq!(e.handle(&get("/a")).await.unwrap().unwrap().delay_ms, 110);
        assert_eq!(
            e.handle(&get("/b")).await.unwrap().unwrap().delay_ms,
            15,
            "stub delay replaces the global fixed delay"
        );
    }

    #[tokio::test]
    async fn body_files_are_served_and_templated() {
        let (e, s) = engine();
        s.data
            .put(FILES_NAMESPACE, "users/7.json", &json!({"contentType": "application/json", "base64": B64.encode("{\"path\":\"{{request.path}}\"}"), "size": 25, "updatedAt": "2026-01-01T00:00:00Z"}))
            .await
            .unwrap();
        s.data
            .put(FILES_NAMESPACE, "logo.bin", &json!({"contentType": "application/octet-stream", "base64": B64.encode([0u8, 1, 2, 123, 123]), "size": 5}))
            .await
            .unwrap();
        add(&e, &s, json!({"request": {"urlPathTemplate": "/users/{id}"}, "response": {"bodyFileName": "users/{{request.pathParams.id}}.json"}})).await;
        add(&e, &s, json!({"request": {"urlPath": "/logo"}, "response": {"bodyFileName": "logo.bin", "statusMessage": "Fine"}})).await;
        add(&e, &s, json!({"request": {"urlPath": "/raw"}, "response": {"bodyFileName": "users/7.json", "templated": false, "chunkedDribbleDelay": {"numberOfChunks": 3, "totalDurationMs": 30}}})).await;
        let r = e.handle(&get("/users/7")).await.unwrap().unwrap();
        assert_eq!(
            String::from_utf8(r.body.to_vec()).unwrap(),
            "{\"path\":\"/users/7\"}"
        );
        assert!(
            r.headers
                .iter()
                .any(|(k, v)| k == "content-type" && v == "application/json")
        );
        let r = e.handle(&get("/logo")).await.unwrap().unwrap();
        assert_eq!(r.body.as_ref(), &[0u8, 1, 2, 123, 123]);
        assert_eq!(r.status_message.as_deref(), Some("Fine"));
        let r = e.handle(&get("/raw")).await.unwrap().unwrap();
        assert!(
            String::from_utf8(r.body.to_vec())
                .unwrap()
                .contains("{{request.path}}")
        );
        assert_eq!(r.chunked_dribble_delay.unwrap().number_of_chunks, 3);
        let err = e.handle(&get("/users/8")).await.unwrap_err();
        assert!(err.downcast_ref::<BodyFileMissing>().is_some());
        s.data.delete(FILES_NAMESPACE, "logo.bin").await.unwrap();
        assert!(
            e.handle(&get("/logo")).await.is_ok(),
            "cached for a short time"
        );
        e.invalidate_files();
        assert!(e.handle(&get("/logo")).await.is_err());
    }

    #[tokio::test]
    async fn validation_of_new_fields() {
        let (e, _) = engine();
        for bad in [
            json!({"request": {}, "response": {"body": "x"}, "responses": [{"body": "y"}]}),
            json!({"request": {}, "responseMode": "cycle"}),
            json!({"request": {}, "times": 0}),
            json!({"request": {}, "ttlSeconds": 0}),
            json!({"request": {}, "response": {"body": "x", "bodyFileName": "a"}}),
            json!({"request": {}, "response": {"statusMessage": "a\r\nb: c"}}),
            json!({"request": {}, "response": {"chunkedDribbleDelay": {"numberOfChunks": 0, "totalDurationMs": 1}}}),
            json!({"request": {}, "response": {"fault": "EMPTY_RESPONSE", "chunkedDribbleDelay": {"numberOfChunks": 2, "totalDurationMs": 1}}}),
            json!({"request": {}, "response": {"delay": {"lognormal": {"medianMs": 10, "sigma": 99.0}}}}),
            json!({"request": {}, "response": {"delay": {"fixedMs": 1, "lognormal": {"medianMs": 10, "sigma": 0.1}}}}),
            json!({"request": {}, "responses": (0..101).map(|_| json!({})).collect::<Vec<_>>()}),
        ] {
            let stub: Stub = serde_json::from_value(bad.clone()).unwrap();
            assert!(e.validate(&stub).is_err(), "{bad}");
        }
        let ok: Stub = serde_json::from_value(json!({"request": {}, "responses": [{"status": 201}], "response": {}, "times": 3, "ttlSeconds": 60,
            "expiresAt": "2099-01-01T00:00:00Z"})).unwrap();
        e.validate(&ok).unwrap();
        let ln = crate::model::LogNormalDelay {
            median_ms: 100,
            sigma: 0.5,
        };
        let samples: Vec<u64> = (0..2000).map(|_| ln.sample_ms()).collect();
        let mut sorted = samples.clone();
        sorted.sort();
        let median = sorted[1000] as f64;
        assert!((70.0..140.0).contains(&median), "{median}");
        assert!(samples.iter().any(|x| *x != samples[0]));
    }

    #[tokio::test]
    async fn validation_rejects_bad_stubs() {
        let (e, _) = engine();
        let bad: Stub =
            serde_json::from_value(json!({"request": {}, "response": {"body": "{{#if}}"}}))
                .unwrap();
        assert!(e.validate(&bad).is_err());
        let bad: Stub =
            serde_json::from_value(json!({"request": {}, "response": {"status": 99}})).unwrap();
        assert!(e.validate(&bad).is_err());
        let bad: Stub = serde_json::from_value(
            json!({"request": {}, "response": {"headers": {"bad header": "x"}}}),
        )
        .unwrap();
        assert!(e.validate(&bad).is_err());
        assert!(
            serde_json::from_value::<Stub>(json!({"request": {"unknown": 1}, "response": {}}))
                .is_err()
        );
    }
}
