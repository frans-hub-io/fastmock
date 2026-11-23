//! Message capture: FastMock subscribes to Kafka topics, MQTT topic filters and AMQP exchanges, stores what
//! the application publishes, and lets tests list, wait for and verify those messages.
//!
//! - [`CaptureService`]: the store (memory, or PostgreSQL with that storage), the limits and the local
//!   wake-up of waiting requests.
//! - [`runtime`]: one replica (elected with a lease like the async mock publisher) runs a consumer per
//!   enabled source and writes what it receives.
//! - [`api`]: `/__admin/capture/...`, with the `messages:*` permissions.

pub mod api;
pub mod runtime;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, Utc};
use fastmock_core::capture::{CaptureSource, CaptureStore, CapturedMessage, MemoryCaptureStore};
use fastmock_core::{MessageProtocol, SpaceFilter};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::access::SpaceDef;
use crate::broker_settings::CaptureConfig;
use crate::state::AppState;

/// Data-store namespace of the sources defined through the API.
pub const NAMESPACE: &str = "capture-sources";
/// Hot-store key prefix of the consumer status the leading replica publishes.
pub const STATUS_PREFIX: &str = "capture:status:";
pub const LEASE_KEY: &str = "capture:leader";
/// Characters of the body kept as the preview of a message.
pub const PREVIEW_CHARS: usize = 160;

/// The capture module of a replica.
pub struct CaptureService {
    pub config: CaptureConfig,
    pub store: Arc<dyn CaptureStore>,
    /// Incremented whenever this replica stores messages; waiting requests watch it.
    pub stored: tokio::sync::watch::Sender<u64>,
    /// How often waiting requests also re-read the store: messages may be stored by another replica.
    pub poll: Option<Duration>,
}

impl CaptureService {
    pub fn notify(&self) {
        self.stored.send_modify(|n| *n = n.wrapping_add(1));
    }
}

/// The capture module, or `None` unless `FASTMOCK_CAPTURE_ENABLED=true`. Static sources must name
/// configured spaces and brokers.
pub fn build(
    cfg: &crate::config::Config,
    access: &crate::access::AccessConfig,
    pg: Option<&fastmock_store::PgStore>,
) -> anyhow::Result<Option<Arc<CaptureService>>> {
    let c = &cfg.capture;
    if !c.enabled {
        return Ok(None);
    }
    for s in &c.sources {
        if let Some(space) = &s.space
            && access.space(space).is_none()
        {
            anyhow::bail!(
                "capture source {}: space {space} is not defined in FASTMOCK_ACCESS_FILE",
                s.name
            );
        }
        if let Some(setting) = missing_setting(cfg, s.protocol) {
            anyhow::bail!("capture source {} needs {setting}", s.name);
        }
        let space = s.space.as_deref().and_then(|n| access.space(n));
        if let Err(e) = allowed(c, space, s) {
            anyhow::bail!("capture source {}: {e}", s.name);
        }
    }
    for space in access.spaces() {
        for (field, list) in space.capture.lists() {
            let server = server_list(c, field);
            for e in list.unwrap_or_default() {
                if server.is_none_or(|l| !l.iter().any(|x| meet(x, e).is_some())) {
                    tracing::warn!(
                        space = %space.name,
                        entry = %e,
                        "capture.{field} of the space allows nothing that the server-wide list {} allows; the entry has no effect",
                        server_var(field)
                    );
                }
            }
        }
    }
    let (store, poll): (Arc<dyn CaptureStore>, Option<Duration>) = match pg {
        Some(pg) => (
            Arc::new(fastmock_store::PgCaptureStore::new(pg.pool().clone())),
            Some(Duration::from_millis(500)),
        ),
        None => (Arc::new(MemoryCaptureStore::default()), None),
    };
    let (stored, _) = tokio::sync::watch::channel(0);
    Ok(Some(Arc::new(CaptureService {
        config: c.clone(),
        store,
        stored,
        poll,
    })))
}

/// The setting a protocol needs, when it is missing.
pub fn missing_setting(cfg: &crate::config::Config, p: MessageProtocol) -> Option<&'static str> {
    match p {
        MessageProtocol::Kafka if cfg.kafka_bootstrap.is_none() => Some("FASTMOCK_KAFKA_BOOTSTRAP"),
        MessageProtocol::Mqtt if cfg.mqtt.is_none() => Some("FASTMOCK_MQTT_URL"),
        MessageProtocol::Amqp if cfg.amqp.is_none() => Some("FASTMOCK_AMQP_URL"),
        _ => None,
    }
}

fn server_var(field: &str) -> &'static str {
    match field {
        "kafkaTopics" => "FASTMOCK_CAPTURE_KAFKA_TOPICS",
        "mqttTopics" => "FASTMOCK_CAPTURE_MQTT_TOPICS",
        _ => "FASTMOCK_CAPTURE_AMQP_EXCHANGES",
    }
}

fn server_list<'a>(c: &'a CaptureConfig, field: &str) -> Option<&'a [String]> {
    match field {
        "kafkaTopics" => c.kafka_topics.as_deref(),
        "mqttTopics" => c.mqtt_topics.as_deref(),
        _ => c.amqp_exchanges.as_deref(),
    }
}

fn field_of(p: MessageProtocol) -> &'static str {
    match p {
        MessageProtocol::Kafka => "kafkaTopics",
        MessageProtocol::Mqtt => "mqttTopics",
        MessageProtocol::Amqp => "amqpExchanges",
    }
}

/// The entry that allows exactly the names both entries allow, if any (entries are names, `prefix*` or
/// `*`).
fn meet(a: &str, b: &str) -> Option<String> {
    match (a.strip_suffix('*'), b.strip_suffix('*')) {
        (None, None) => (a == b).then(|| a.to_string()),
        (Some(p), None) => b.starts_with(p).then(|| b.to_string()),
        (None, Some(q)) => a.starts_with(q).then(|| a.to_string()),
        (Some(p), Some(q)) if p.starts_with(q) => Some(a.to_string()),
        (Some(p), Some(q)) if q.starts_with(p) => Some(b.to_string()),
        _ => None,
    }
}

/// The allow-list sources created through the API in an area get for a protocol: the server-wide list,
/// narrowed by the space's list. `None` while the server-wide list is not set (such sources are refused).
pub fn effective_list(
    c: &CaptureConfig,
    space: Option<&SpaceDef>,
    p: MessageProtocol,
) -> Option<Vec<String>> {
    let field = field_of(p);
    let server = server_list(c, field)?;
    let narrow = space.and_then(|s| {
        s.capture
            .lists()
            .into_iter()
            .find(|(f, _)| *f == field)
            .and_then(|(_, l)| l)
    });
    let Some(narrow) = narrow else {
        return Some(server.to_vec());
    };
    let mut out: Vec<String> = Vec::new();
    for b in narrow {
        for a in server {
            if let Some(m) = meet(a, b)
                && !out.contains(&m)
            {
                out.push(m);
            }
        }
    }
    Some(out)
}

/// Checks a source against the capture allow-lists: the server-wide list of its protocol and, for a
/// source in a space, the space's list (`spaces[].capture` in the access file), which can only narrow it.
/// Sources from the configuration are checked only against the lists that are set; sources created
/// through the API need the server-wide list (`*` allows every name).
pub fn allowed(
    c: &CaptureConfig,
    space: Option<&SpaceDef>,
    s: &CaptureSource,
) -> Result<(), String> {
    let field = field_of(s.protocol);
    let name = server_var(field);
    let list = server_list(c, field);
    if list.is_none() && !s.from_config {
        return Err(format!(
            "capture sources created through the API need {name}: set it to the names or prefix* this server may read ({name}=* allows every one)"
        ));
    }
    let narrow = space.and_then(|sp| {
        sp.capture
            .lists()
            .into_iter()
            .find(|(f, _)| *f == field)
            .and_then(|(_, l)| l)
            .map(|l| (sp.name.as_str(), l))
    });
    for d in s.destinations() {
        if !fastmock_brokers::name_allowed(list, &d) {
            return Err(format!("{d} is not in {name}"));
        }
        if let Some((space, l)) = narrow
            && !fastmock_brokers::name_allowed(Some(l), &d)
        {
            return Err(format!(
                "{d} is not in the capture allow-list of space {space} (spaces[].capture.{field} in the access file)"
            ));
        }
    }
    Ok(())
}

/// [`allowed`] with the space of the source from the access configuration.
pub fn allowed_in(app: &AppState, c: &CaptureConfig, s: &CaptureSource) -> Result<(), String> {
    let space = s.space.as_deref().and_then(|n| app.auth.access().space(n));
    allowed(c, space, s)
}

/// The sources of the configuration and of the data store; stored ones with a configured name are
/// ignored.
pub async fn load_sources(app: &AppState) -> anyhow::Result<Vec<CaptureSource>> {
    let Some(svc) = &app.capture else {
        return Ok(vec![]);
    };
    let mut out = svc.config.sources.clone();
    let rows = app
        .stores
        .data
        .list(NAMESPACE, (svc.config.max_sources * 2) as i64)
        .await?;
    for (k, v) in rows {
        match serde_json::from_value::<CaptureSource>(v) {
            Ok(mut s) => {
                s.from_config = false;
                if out.iter().any(|x| x.id == s.id) {
                    continue;
                }
                out.push(s);
            }
            Err(e) => {
                metrics::counter!("fastmock_store_errors_total", "store" => "capture_sources")
                    .increment(1);
                tracing::warn!(key = %k, error = %e, "stored capture source is not valid; ignored");
            }
        }
    }
    out.sort_by(|a, b| (&a.space, &a.name).cmp(&(&b.space, &b.name)));
    Ok(out)
}

/// What the replica running a source's consumer reports about it.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceStatus {
    /// `running` (subscribed), `connecting`, `error` (see `lastError`), `disabled`, or `stopped` (no
    /// replica runs it, for example while no replica holds the capture lease).
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Messages received by the current consumer.
    #[serde(default)]
    pub received: u64,
    /// Messages the consumer dropped because storing could not keep up.
    #[serde(default)]
    pub dropped: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,
    /// Messages stored for the source now.
    #[serde(default)]
    pub stored: u64,
    /// Successful connections of the current consumer; more than 1 means it reconnected.
    #[serde(default)]
    pub connects: u64,
}

pub async fn statuses(app: &AppState) -> HashMap<Uuid, SourceStatus> {
    let mut out = HashMap::new();
    match app.stores.hot.scan(STATUS_PREFIX, 2048).await {
        Ok(rows) => {
            for (k, v) in rows {
                let id = k.rsplit(':').next().and_then(|x| Uuid::parse_str(x).ok());
                if let (Some(id), Ok(s)) = (id, serde_json::from_str::<SourceStatus>(&v)) {
                    out.insert(id, s);
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "reading capture status failed"),
    }
    out
}

/// A message body for people and tests.
#[derive(Clone, Debug, Serialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DecodedBody {
    /// `json`, `text`, `avro` (Confluent-framed, decoded with the registered schema), `base64` (other
    /// binary data) or `empty`.
    pub encoding: String,
    /// The JSON value (`json`, `avro`), the text (`text`) or the base64 of the bytes (`base64`).
    #[schema(value_type = Object)]
    pub value: Value,
    /// Avro: the schema id of the frame.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_id: Option<i32>,
    /// Why an Avro body could not be decoded (it is then shown as base64).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl DecodedBody {
    /// The JSON matchers see: the JSON or Avro value.
    pub fn json(&self) -> Option<&Value> {
        match self.encoding.as_str() {
            "json" | "avro" => Some(&self.value),
            _ => None,
        }
    }
}

/// The schema a Confluent-framed body needs: `None` for other bodies, otherwise the schema or why there is
/// none. Answers the registry has given (also failures, for a while) cost nothing; other lookups take one of
/// `lookups`, and when none is left the body stays undecoded. Errors name no registry address or response.
pub async fn resolve_schema(
    app: &AppState,
    m: &CapturedMessage,
    lookups: &mut usize,
) -> Option<Result<Arc<apache_avro::Schema>, String>> {
    let id = fastmock_kafka::confluent_schema_id(&m.body)?;
    let Some(reg) = app.kafka.registry() else {
        return Some(Err(
            "Confluent-framed Avro; set FASTMOCK_KAFKA_SCHEMA_REGISTRY_URL to decode it".into(),
        ));
    };
    if m.truncated {
        return Some(Err(
            "the stored body is truncated; Avro cannot be decoded".into()
        ));
    }
    if let Some(r) = reg.cached(id).await {
        return Some(r);
    }
    if *lookups == 0 {
        return Some(Err(
            "not decoded: this search looked up its limit of schema ids; narrow it".into(),
        ));
    }
    *lookups -= 1;
    Some(reg.schema(id).await.map_err(|e| {
        tracing::warn!(schema_id = id, error = %format!("{e:#}"), "schema lookup for a captured message failed");
        e.to_string()
    }))
}

/// Decodes a body: Confluent Avro with `schema` (from [`resolve_schema`]), then JSON, text, or base64.
/// Pure CPU work, so large bodies are decoded off the async workers.
pub fn decode_with(
    m: &CapturedMessage,
    schema: Option<Result<Arc<apache_avro::Schema>, String>>,
) -> DecodedBody {
    let b64 = |error: Option<String>, schema_id: Option<i32>| DecodedBody {
        encoding: "base64".into(),
        value: Value::String(base64::engine::general_purpose::STANDARD.encode(&m.body)),
        schema_id,
        error,
    };
    if m.body.is_empty() {
        return DecodedBody {
            encoding: "empty".into(),
            value: Value::String(String::new()),
            schema_id: None,
            error: None,
        };
    }
    if let Some(id) = fastmock_kafka::confluent_schema_id(&m.body) {
        return match schema {
            Some(Ok(schema)) => match fastmock_kafka::decode_avro(&schema, &m.body) {
                Ok(v) => DecodedBody {
                    encoding: "avro".into(),
                    value: v,
                    schema_id: Some(id),
                    error: None,
                },
                Err(e) => b64(Some(e.to_string()), Some(id)),
            },
            Some(Err(e)) => b64(Some(e), Some(id)),
            None => b64(None, Some(id)),
        };
    }
    match std::str::from_utf8(&m.body) {
        Ok(text) => match m.json() {
            Some(v) => DecodedBody {
                encoding: "json".into(),
                value: v,
                schema_id: None,
                error: None,
            },
            None => DecodedBody {
                encoding: "text".into(),
                value: Value::String(text.to_string()),
                schema_id: None,
                error: None,
            },
        },
        Err(_) => b64(None, None),
    }
}

/// Decodes one body for the API: [`resolve_schema`] with one lookup, then [`decode_with`].
pub async fn decode(app: &AppState, m: &CapturedMessage) -> DecodedBody {
    let schema = resolve_schema(app, m, &mut 1).await;
    let m = m.clone();
    tokio::task::spawn_blocking(move || decode_with(&m, schema))
        .await
        .unwrap_or_else(|_| DecodedBody {
            encoding: "empty".into(),
            value: Value::String(String::new()),
            schema_id: None,
            error: Some("decoding failed".into()),
        })
}

/// Spaces a principal may read messages in, for the source lists of other modules.
pub fn readable(spaces: &SpaceFilter, space: Option<&str>) -> bool {
    spaces.matches(space)
}

/// The text preview of a body: up to [`PREVIEW_CHARS`] characters of UTF-8, otherwise none.
pub fn preview(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    if body.first() == Some(&0) {
        return None;
    }
    Some(text.chars().take(PREVIEW_CHARS).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_are_short_text() {
        assert_eq!(preview(b"{\"a\":1}").as_deref(), Some("{\"a\":1}"));
        assert_eq!(preview(&[0xff, 0xfe]), None);
        assert_eq!(preview(&[0, 0, 0, 0, 1, 2]), None);
        assert_eq!(
            preview("é".repeat(500).as_bytes()).unwrap().chars().count(),
            160
        );
    }

    #[test]
    fn allow_lists_apply_to_destinations() {
        let mut c = CaptureConfig {
            mqtt_topics: Some(vec!["devices/*".into()]),
            amqp_exchanges: Some(vec!["orders".into()]),
            ..Default::default()
        };
        let src = |v: Value| serde_json::from_value::<CaptureSource>(v).unwrap();
        assert!(allowed(&c, None, &src(serde_json::json!({"name": "a", "protocol": "mqtt", "topics": ["devices/+/state"]}))).is_ok());
        assert!(
            allowed(
                &c,
                None,
                &src(serde_json::json!({"name": "a", "protocol": "mqtt", "topics": ["#"]}))
            )
            .is_err()
        );
        assert!(
            allowed(
                &c,
                None,
                &src(serde_json::json!({"name": "a", "protocol": "amqp", "exchange": "orders"}))
            )
            .is_ok()
        );
        assert!(
            allowed(
                &c,
                None,
                &src(serde_json::json!({"name": "a", "protocol": "kafka", "topics": ["x"]}))
            )
            .is_err(),
            "no Kafka allow-list: API sources are refused"
        );
        c.kafka_topics = Some(vec![]);
        assert!(
            allowed(
                &c,
                None,
                &src(serde_json::json!({"name": "a", "protocol": "kafka", "topics": ["x"]}))
            )
            .is_err()
        );
    }

    #[test]
    fn api_sources_need_an_allow_list() {
        let c = CaptureConfig::default();
        let mut s: CaptureSource = serde_json::from_value(
            serde_json::json!({"name": "a", "protocol": "kafka", "topics": ["orders"]}),
        )
        .unwrap();
        let e = allowed(&c, None, &s).unwrap_err();
        assert!(e.contains("FASTMOCK_CAPTURE_KAFKA_TOPICS"), "{e}");
        s.from_config = true;
        assert!(
            allowed(&c, None, &s).is_ok(),
            "configured sources need no allow-list"
        );
        s.from_config = false;
        let c = CaptureConfig {
            kafka_topics: Some(vec!["*".into()]),
            ..Default::default()
        };
        assert!(allowed(&c, None, &s).is_ok());
    }

    fn space(capture: Value) -> SpaceDef {
        SpaceDef {
            name: "payments".into(),
            description: None,
            path_prefixes: vec![],
            hosts: vec![],
            mail_domains: vec![],
            capture: serde_json::from_value(capture).unwrap(),
        }
    }

    #[test]
    fn space_lists_narrow_and_never_widen() {
        let c = CaptureConfig {
            kafka_topics: Some(vec!["orders.*".into(), "billing.events".into()]),
            mqtt_topics: Some(vec!["*".into()]),
            ..Default::default()
        };
        let src = |v: Value| serde_json::from_value::<CaptureSource>(v).unwrap();
        let kafka =
            |t: &str| src(serde_json::json!({"name": "a", "protocol": "kafka", "topics": [t]}));
        let wide = space(serde_json::json!({"kafkaTopics": ["*"]}));
        assert!(allowed(&c, Some(&wide), &kafka("orders.created")).is_ok());
        let e = allowed(&c, Some(&wide), &kafka("audit.log")).unwrap_err();
        assert!(e.contains("FASTMOCK_CAPTURE_KAFKA_TOPICS"), "{e}");
        let narrow = space(serde_json::json!({"kafkaTopics": ["orders.eu.*"]}));
        assert!(allowed(&c, Some(&narrow), &kafka("orders.eu.created")).is_ok());
        let e = allowed(&c, Some(&narrow), &kafka("orders.us.created")).unwrap_err();
        assert!(
            e.contains("space payments") && e.contains("kafkaTopics"),
            "{e}"
        );
        assert!(allowed(&c, Some(&narrow), &kafka("billing.events")).is_err());
        let none = space(serde_json::json!({"mqttTopics": []}));
        assert!(
            allowed(
                &c,
                Some(&none),
                &src(serde_json::json!({"name": "a", "protocol": "mqtt", "topics": ["x/y"]}))
            )
            .is_err(),
            "an empty space list allows nothing"
        );
        assert!(
            allowed(&c, Some(&none), &kafka("orders.x")).is_ok(),
            "other protocols keep the server-wide list"
        );
        let no_server = CaptureConfig::default();
        assert!(
            allowed(&no_server, Some(&wide), &kafka("orders.x")).is_err(),
            "a space list does not replace a missing server-wide list"
        );
        let mut configured = kafka("audit.log");
        configured.from_config = true;
        assert!(allowed(&no_server, None, &configured).is_ok());
        assert!(
            allowed(&no_server, Some(&narrow), &configured).is_err(),
            "configured sources must match the space list when it is set"
        );
    }

    #[test]
    fn effective_lists_are_intersections() {
        let c = CaptureConfig {
            kafka_topics: Some(vec![
                "orders.*".into(),
                "billing.events".into(),
                "a*".into(),
            ]),
            amqp_exchanges: Some(vec!["*".into()]),
            ..Default::default()
        };
        let sp = space(serde_json::json!({
            "kafkaTopics": ["*", "orders.eu.*", "billing.*", "ab", "x"],
            "amqpExchanges": ["billing"]
        }));
        assert_eq!(
            effective_list(&c, Some(&sp), MessageProtocol::Kafka).unwrap(),
            vec!["orders.*", "billing.events", "a*", "orders.eu.*", "ab"]
        );
        assert_eq!(
            effective_list(&c, Some(&sp), MessageProtocol::Amqp).unwrap(),
            vec!["billing"]
        );
        assert_eq!(effective_list(&c, Some(&sp), MessageProtocol::Mqtt), None);
        assert_eq!(
            effective_list(&c, None, MessageProtocol::Kafka).unwrap(),
            vec!["orders.*", "billing.events", "a*"]
        );
        for name in [
            "orders.eu.1",
            "orders.us",
            "billing.events",
            "billing.x",
            "ab",
            "ac",
            "x",
        ] {
            let both = fastmock_brokers::name_allowed(c.kafka_topics.as_deref(), name)
                && fastmock_brokers::name_allowed(sp.capture.kafka_topics.as_deref(), name);
            let eff = effective_list(&c, Some(&sp), MessageProtocol::Kafka).unwrap();
            assert_eq!(
                fastmock_brokers::name_allowed(Some(&eff), name),
                both,
                "{name}"
            );
        }
    }
}
