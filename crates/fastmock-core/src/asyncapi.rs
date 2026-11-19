use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::matcher::value_text;
use crate::model::{RequestPattern, ResponseDef, Stub};
use crate::openapi::{ResolveBudget, example, resolve_budgeted, to_json_schema};
use crate::ws_model::{WebSocketDef, WsMessage};

pub const DEFAULT_FREQUENCY_MS: u64 = 10_000;
pub const MIN_FREQUENCY_MS: u64 = 1_000;
pub const MAX_FREQUENCY_MS: u64 = 86_400_000;
pub const MAX_MESSAGES_PER_MOCK: usize = 100;
pub const MAX_PAYLOAD_BYTES: usize = 256 * 1024;
pub const MAX_OPERATIONS: usize = 2_000;
pub const MAX_HEADERS: usize = 64;
const MAX_AVRO_DEPTH: usize = 24;
pub const MAX_AVRO_EXAMPLE_NODES: usize = 10_000;
pub const MAX_AVRO_EXAMPLE_BYTES: usize = 1024 * 1024;

fn yes() -> bool {
    true
}

fn default_frequency() -> u64 {
    DEFAULT_FREQUENCY_MS
}

fn default_protocol() -> AsyncProtocol {
    AsyncProtocol::Kafka
}

#[derive(
    Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum AsyncProtocol {
    Kafka,
    Ws,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AsyncMock {
    #[serde(default = "Uuid::now_v7")]
    pub id: Uuid,
    pub name: String,
    #[serde(default = "default_protocol")]
    pub protocol: AsyncProtocol,
    pub channel: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    pub messages: Vec<AsyncMessage>,
    #[serde(default = "default_frequency")]
    pub frequency_ms: u64,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "yes")]
    pub templated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avro_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AsyncMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub payload: Value,
}

pub fn valid_topic(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 249
        && t != "."
        && t != ".."
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl AsyncMock {
    pub fn validate(&self, template: impl Fn(&Value) -> Result<(), String>) -> Result<(), String> {
        if self.name.trim().is_empty() || self.name.len() > 256 {
            return Err("name must be 1..256 characters".into());
        }
        match self.protocol {
            AsyncProtocol::Kafka => {
                if !valid_topic(&self.channel) {
                    return Err(format!(
                        "channel `{}` is not a valid Kafka topic name ([a-zA-Z0-9._-], at most 249 characters)",
                        self.channel
                    ));
                }
            }
            AsyncProtocol::Ws => {
                if !self.channel.starts_with('/') || self.channel.len() > 2048 {
                    return Err("a WebSocket channel must be a path starting with /".into());
                }
            }
        }
        if self.messages.is_empty() || self.messages.len() > MAX_MESSAGES_PER_MOCK {
            return Err(format!(
                "messages must contain 1..{MAX_MESSAGES_PER_MOCK} examples"
            ));
        }
        if !(MIN_FREQUENCY_MS..=MAX_FREQUENCY_MS).contains(&self.frequency_ms) {
            return Err(format!(
                "frequencyMs must be between {MIN_FREQUENCY_MS} and {MAX_FREQUENCY_MS}"
            ));
        }
        if self.tags.len() > 32 || self.tags.iter().any(|t| t.len() > 128) {
            return Err("at most 32 tags of up to 128 characters".into());
        }
        if let Some(s) = &self.avro_schema
            && serde_json::to_vec(s).map(|v| v.len()).unwrap_or(usize::MAX) > MAX_PAYLOAD_BYTES
        {
            return Err(format!("avroSchema exceeds {MAX_PAYLOAD_BYTES} bytes"));
        }
        if let Some(s) = &self.payload_schema
            && serde_json::to_vec(s).map(|v| v.len()).unwrap_or(usize::MAX) > 4 * MAX_PAYLOAD_BYTES
        {
            return Err(format!(
                "payloadSchema exceeds {} bytes",
                4 * MAX_PAYLOAD_BYTES
            ));
        }
        for (i, m) in self.messages.iter().enumerate() {
            let size = serde_json::to_vec(&m.payload)
                .map(|v| v.len())
                .unwrap_or(usize::MAX);
            if size > MAX_PAYLOAD_BYTES {
                return Err(format!(
                    "messages[{i}].payload exceeds {MAX_PAYLOAD_BYTES} bytes"
                ));
            }
            if m.headers.len() > MAX_HEADERS {
                return Err(format!("messages[{i}] has more than {MAX_HEADERS} headers"));
            }
            if m.key.as_ref().is_some_and(|k| k.len() > 4096) {
                return Err(format!("messages[{i}].key exceeds 4096 bytes"));
            }
            if m.headers
                .iter()
                .any(|(k, v)| k.is_empty() || k.len() > 256 || v.len() > 8192)
            {
                return Err(format!("messages[{i}] has an invalid header"));
            }
            if self.templated {
                template(&m.payload).map_err(|e| format!("messages[{i}].payload: {e}"))?;
                if let Some(k) = &m.key {
                    template(&Value::String(k.clone()))
                        .map_err(|e| format!("messages[{i}].key: {e}"))?;
                }
                for (k, v) in &m.headers {
                    template(&Value::String(v.clone()))
                        .map_err(|e| format!("messages[{i}].headers.{k}: {e}"))?;
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct AsyncImportOptions {
    pub tag: String,
    pub frequency_ms: Option<u64>,
    pub path_prefix: String,
}

#[derive(Debug, Default)]
pub struct AsyncImportResult {
    pub title: String,
    pub version: String,
    pub mocks: Vec<AsyncMock>,
    pub warnings: Vec<String>,
}

fn protocol_kind(p: &str) -> Option<AsyncProtocol> {
    match p.to_ascii_lowercase().as_str() {
        "kafka" | "kafka-secure" => Some(AsyncProtocol::Kafka),
        "ws" | "wss" => Some(AsyncProtocol::Ws),
        _ => None,
    }
}

fn binding_protocols(bindings: &[Option<&Value>]) -> BTreeSet<AsyncProtocol> {
    let mut out = BTreeSet::new();
    for b in bindings.iter().flatten() {
        if let Some(o) = b.as_object() {
            if o.contains_key("kafka") {
                out.insert(AsyncProtocol::Kafka);
            }
            if o.contains_key("ws") {
                out.insert(AsyncProtocol::Ws);
            }
        }
    }
    out
}

fn server_protocol(server: &Value) -> Option<AsyncProtocol> {
    server
        .get("protocol")
        .and_then(|p| p.as_str())
        .and_then(protocol_kind)
}

fn param_value(p: &Value) -> Option<String> {
    if let Some(Value::Array(a)) = p.get("examples")
        && let Some(first) = a.first()
    {
        return Some(value_text(first));
    }
    if let Some(d) = p.get("default") {
        return Some(value_text(d));
    }
    if let Some(Value::Array(e)) = p.get("enum")
        && let Some(first) = e.first()
    {
        return Some(value_text(first));
    }
    if let Some(s) = p.get("schema") {
        let ex = example(s, 0);
        if !ex.is_null() {
            return Some(value_text(&ex));
        }
    }
    None
}

fn sanitize_topic(address: &str, params: Option<&Value>, warnings: &mut Vec<String>) -> String {
    let mut out = address.to_string();
    if let Some(Value::Object(ps)) = params {
        for (name, p) in ps {
            let v = param_value(p).unwrap_or_else(|| name.clone());
            out = out.replace(&format!("{{{name}}}"), &v);
        }
    }
    let cleaned: String = out
        .trim_matches('/')
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned: String = cleaned.chars().take(249).collect();
    if cleaned != address {
        warnings.push(format!(
            "channel `{address}` was mapped to Kafka topic `{cleaned}`"
        ));
    }
    cleaned
}

fn ws_path(prefix: &str, address: &str) -> String {
    format!(
        "{}/{}",
        prefix.trim_end_matches('/'),
        address.trim_start_matches('/')
    )
}

fn is_avro(format: Option<&str>) -> bool {
    format.is_some_and(|f| {
        f.to_ascii_lowercase()
            .starts_with("application/vnd.apache.avro")
    })
}

fn avro_full_name(o: &Map<String, Value>, ns: Option<&str>) -> Option<(String, String)> {
    let name = o.get("name")?.as_str()?.to_string();
    if name.contains('.') {
        let short = name.rsplit('.').next().unwrap_or(&name).to_string();
        return Some((name, short));
    }
    let ns = o.get("namespace").and_then(|n| n.as_str()).or(ns);
    Some(match ns {
        Some(n) if !n.is_empty() => (format!("{n}.{name}"), name),
        _ => (name.clone(), name),
    })
}

pub fn avro_example(schema: &Value) -> Value {
    let mut names = AvroNames {
        types: BTreeMap::new(),
        expanding: Vec::new(),
        nodes: MAX_AVRO_EXAMPLE_NODES,
        bytes: MAX_AVRO_EXAMPLE_BYTES,
    };
    avro_example_in(schema, &mut names, None, 0)
}

struct AvroNames {
    types: BTreeMap<String, Value>,
    expanding: Vec<String>,
    nodes: usize,
    bytes: usize,
}

impl AvroNames {
    fn get(&self, k: &str) -> Option<&Value> {
        self.types.get(k)
    }

    fn insert(&mut self, k: String, v: &Value) {
        self.types.entry(k).or_insert_with(|| v.clone());
    }

    fn charge(&mut self, bytes: usize) -> bool {
        if self.nodes == 0 || bytes > self.bytes {
            self.nodes = 0;
            return false;
        }
        self.nodes -= 1;
        self.bytes -= bytes;
        true
    }
}

fn avro_example_in(schema: &Value, names: &mut AvroNames, ns: Option<&str>, depth: usize) -> Value {
    if depth > MAX_AVRO_DEPTH || !names.charge(8) {
        return Value::Null;
    }
    match schema {
        Value::String(t) => match t.as_str() {
            "null" => Value::Null,
            "boolean" => json!(true),
            "int" | "long" => json!(0),
            "float" | "double" => json!(0.0),
            "bytes" => json!(""),
            "string" => json!("string"),
            other => {
                let full = match ns {
                    Some(n) if !other.contains('.') => format!("{n}.{other}"),
                    _ => other.to_string(),
                };
                let key = if names.get(&full).is_some() {
                    full
                } else {
                    other.to_string()
                };
                if names.expanding.contains(&key) {
                    return Value::Null;
                }
                match names.get(&key).cloned() {
                    Some(s) => {
                        names.expanding.push(key);
                        let v = avro_example_in(&s, names, ns, depth + 1);
                        names.expanding.pop();
                        v
                    }
                    None => Value::Null,
                }
            }
        },
        Value::Array(branches) => {
            let pick = branches
                .iter()
                .find(|b| b.as_str() != Some("null"))
                .or_else(|| branches.first());
            pick.map(|b| avro_example_in(b, names, ns, depth + 1))
                .unwrap_or(Value::Null)
        }
        Value::Object(o) => {
            if let Some(d) = o.get("default")
                && depth > 0
                && !o.contains_key("fields")
            {
                return d.clone();
            }
            if let Some(lt) = o.get("logicalType").and_then(|l| l.as_str()) {
                match lt {
                    "date" => return json!("2026-01-01"),
                    "timestamp-millis"
                    | "timestamp-micros"
                    | "timestamp-nanos"
                    | "local-timestamp-millis"
                    | "local-timestamp-micros"
                    | "local-timestamp-nanos" => return json!("2026-01-01T00:00:00Z"),
                    "decimal" => return json!("0"),
                    "uuid" => return json!("3fa85f64-5717-4562-b3fc-2c963f66afa6"),
                    "time-millis" | "time-micros" => return json!(0),
                    _ => {}
                }
            }
            let ty = o.get("type");
            match ty.and_then(|t| t.as_str()) {
                Some("record") | Some("error") => {
                    let full = avro_full_name(o, ns);
                    if let Some((f, s)) = &full {
                        names.insert(f.clone(), schema);
                        names.insert(s.clone(), schema);
                    }
                    let pushed = full.as_ref().map(|(f, s)| {
                        names.expanding.push(f.clone());
                        names.expanding.push(s.clone());
                    });
                    let inner_ns = full
                        .as_ref()
                        .and_then(|(f, _)| f.rsplit_once('.').map(|(n, _)| n.to_string()));
                    let mut m = Map::new();
                    if let Some(Value::Array(fields)) = o.get("fields") {
                        for f in fields {
                            let Some(name) = f.get("name").and_then(|n| n.as_str()) else {
                                continue;
                            };
                            let v = match f.get("default") {
                                Some(d) => d.clone(),
                                None => avro_example_in(
                                    f.get("type").unwrap_or(&Value::Null),
                                    names,
                                    inner_ns.as_deref().or(ns),
                                    depth + 1,
                                ),
                            };
                            m.insert(name.to_string(), v);
                        }
                    }
                    if pushed.is_some() {
                        names.expanding.pop();
                        names.expanding.pop();
                    }
                    Value::Object(m)
                }
                Some("enum") => {
                    if let Some((f, s)) = avro_full_name(o, ns) {
                        names.insert(f, schema);
                        names.insert(s, schema);
                    }
                    o.get("symbols")
                        .and_then(|s| s.as_array())
                        .and_then(|s| s.first())
                        .cloned()
                        .unwrap_or(Value::Null)
                }
                Some("fixed") => {
                    if let Some((f, s)) = avro_full_name(o, ns) {
                        names.insert(f, schema);
                        names.insert(s, schema);
                    }
                    let size = o
                        .get("size")
                        .and_then(|s| s.as_u64())
                        .unwrap_or(0)
                        .min(1024);
                    if !names.charge(size as usize) {
                        return Value::Null;
                    }
                    Value::String("0".repeat(size as usize))
                }
                Some("array") => Value::Array(vec![avro_example_in(
                    o.get("items").unwrap_or(&Value::Null),
                    names,
                    ns,
                    depth + 1,
                )]),
                Some("map") => json!({"key": avro_example_in(
                    o.get("values").unwrap_or(&Value::Null),
                    names,
                    ns,
                    depth + 1,
                )}),
                Some(_) | None => match ty {
                    Some(t) => avro_example_in(t, names, ns, depth + 1),
                    None => Value::Null,
                },
            }
        }
        _ => Value::Null,
    }
}

fn header_values(v: &Value) -> BTreeMap<String, String> {
    v.as_object()
        .map(|o| {
            o.iter()
                .take(MAX_HEADERS)
                .map(|(k, v)| (k.clone(), value_text(v)))
                .collect()
        })
        .unwrap_or_default()
}

fn payload_text_size(v: &Value) -> usize {
    serde_json::to_vec(v).map(|b| b.len()).unwrap_or(usize::MAX)
}

struct MessageInfo {
    name: String,
    content_type: Option<String>,
    avro_schema: Option<Value>,
    payload_schema: Option<Value>,
    examples: Vec<AsyncMessage>,
}

fn message_info(
    msg: &Value,
    fallback_name: &str,
    default_ct: Option<&str>,
    warnings: &mut Vec<String>,
) -> MessageInfo {
    let name = msg
        .get("name")
        .or_else(|| msg.get("messageId"))
        .and_then(|n| n.as_str())
        .unwrap_or(fallback_name)
        .to_string();
    let content_type = msg
        .get("contentType")
        .and_then(|c| c.as_str())
        .or(default_ct)
        .map(String::from);
    let mut payload = msg.get("payload").cloned();
    let mut format = msg
        .get("schemaFormat")
        .and_then(|f| f.as_str())
        .map(String::from);
    if let Some(p) = &payload
        && let Some(f) = p.get("schemaFormat").and_then(|f| f.as_str())
        && p.get("schema").is_some()
    {
        format = Some(f.to_string());
        payload = p.get("schema").cloned();
    }
    let avro = is_avro(format.as_deref());
    let mut avro_schema = None;
    let mut payload_schema = None;
    if let Some(p) = &payload {
        if avro {
            let parsed = match p {
                Value::String(s) => serde_json::from_str::<Value>(s).ok(),
                other => Some(other.clone()),
            };
            match parsed {
                Some(s) => avro_schema = Some(s),
                None => warnings.push(format!(
                    "{name}: the Avro payload schema is not valid JSON; messages are sent as JSON"
                )),
            }
        } else if format.as_deref().is_none_or(|f| {
            f.contains("asyncapi")
                || f.contains("schema+json")
                || f.contains("schema+yaml")
                || f.contains("openapi")
        }) {
            payload_schema = Some(to_json_schema(p));
        } else if let Some(f) = &format {
            warnings.push(format!(
                "{name}: schema format `{f}` is not supported; payloads are not generated from it"
            ));
        }
    }
    let key_schema = msg.pointer("/bindings/kafka/key").cloned();
    let key_example = key_schema.as_ref().map(|k| {
        let e = if avro { avro_example(k) } else { example(k, 0) };
        value_text(&e)
    });
    let header_example = msg
        .get("headers")
        .map(|h| header_values(&example(h, 0)))
        .unwrap_or_default();
    let generate = |avro_schema: &Option<Value>, payload: &Option<Value>| -> Value {
        match (avro_schema, payload) {
            (Some(a), _) => avro_example(a),
            (None, Some(p)) if !avro => example(p, 0),
            _ => Value::Null,
        }
    };
    let mut examples = Vec::new();
    if let Some(Value::Array(list)) = msg.get("examples") {
        for (i, ex) in list.iter().enumerate().take(MAX_MESSAGES_PER_MOCK) {
            let payload_v = ex
                .get("payload")
                .cloned()
                .unwrap_or_else(|| generate(&avro_schema, &payload));
            let headers = ex
                .get("headers")
                .map(header_values)
                .unwrap_or_else(|| header_example.clone());
            examples.push(AsyncMessage {
                name: Some(
                    ex.get("name")
                        .and_then(|n| n.as_str())
                        .map(String::from)
                        .unwrap_or_else(|| format!("example-{}", i + 1)),
                ),
                key: key_example.clone(),
                headers,
                payload: payload_v,
            });
        }
    }
    if examples.is_empty()
        && let Some(p) = &payload
        && !avro
    {
        let mut list: Vec<Value> = match p.get("examples") {
            Some(Value::Array(a)) => a.iter().take(MAX_MESSAGES_PER_MOCK).cloned().collect(),
            _ => vec![],
        };
        if list.is_empty()
            && let Some(e) = p.get("example")
        {
            list.push(e.clone());
        }
        for (i, e) in list.into_iter().enumerate() {
            examples.push(AsyncMessage {
                name: Some(format!("example-{}", i + 1)),
                key: key_example.clone(),
                headers: header_example.clone(),
                payload: e,
            });
        }
    }
    if examples.is_empty() {
        let generated = generate(&avro_schema, &payload);
        if generated.is_null() && payload.is_some() {
            warnings.push(format!(
                "{name}: no example and none could be generated; the message payload is null"
            ));
        }
        examples.push(AsyncMessage {
            name: Some("generated".into()),
            key: key_example.clone(),
            headers: header_example,
            payload: generated,
        });
    }
    examples.retain(|e| {
        let ok = payload_text_size(&e.payload) <= MAX_PAYLOAD_BYTES;
        if !ok {
            warnings.push(format!(
                "{name}: an example exceeds {MAX_PAYLOAD_BYTES} bytes and was skipped"
            ));
        }
        ok
    });
    MessageInfo {
        name,
        content_type,
        avro_schema,
        payload_schema,
        examples,
    }
}

fn message_list(m: Option<&Value>) -> Vec<Value> {
    match m {
        None | Some(Value::Null) => vec![],
        Some(Value::Array(a)) => a.clone(),
        Some(Value::Object(o)) => match o.get("oneOf") {
            Some(Value::Array(a)) => a.clone(),
            _ => vec![Value::Object(o.clone())],
        },
        Some(_) => vec![],
    }
}

fn ref_name(r: Option<&Value>) -> Option<String> {
    let s = r?.get("$ref")?.as_str()?;
    let last = s.rsplit('/').next()?;
    Some(last.replace("~1", "/").replace("~0", "~"))
}

struct OpSpec {
    op_name: String,
    address: String,
    params: Option<Value>,
    protocols: BTreeSet<AsyncProtocol>,
    messages: Vec<Value>,
    kafka_topic: Option<String>,
}

pub fn parse_version(raw: &Value) -> anyhow::Result<u8> {
    let v = raw.get("asyncapi").and_then(|v| v.as_str()).unwrap_or("");
    if v.starts_with("2.") {
        Ok(2)
    } else if v.starts_with("3.") {
        Ok(3)
    } else {
        anyhow::bail!(
            "only AsyncAPI 2.x and 3.x documents are supported (got `{}`)",
            if v.is_empty() { "no asyncapi field" } else { v }
        )
    }
}

fn bounded_resolve(doc: &Value, v: &Value, total: &mut ResolveBudget) -> anyhow::Result<Value> {
    resolve_budgeted(doc, v, 0, total)
}

fn all_server_protocols(doc: &Value, total: &mut ResolveBudget) -> BTreeMap<String, AsyncProtocol> {
    doc.get("servers")
        .and_then(|s| s.as_object())
        .map(|o| {
            o.iter()
                .filter_map(|(k, s)| {
                    let s = if s.get("$ref").is_some() {
                        bounded_resolve(doc, s, total).ok()?
                    } else {
                        s.clone()
                    };
                    server_protocol(&s).map(|p| (k.clone(), p))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn collect_v2(
    doc: &Value,
    servers: &BTreeMap<String, AsyncProtocol>,
    total: &mut ResolveBudget,
    warnings: &mut Vec<String>,
) -> anyhow::Result<Vec<OpSpec>> {
    let mut out = Vec::new();
    let Some(channels) = doc.get("channels").and_then(|c| c.as_object()) else {
        anyhow::bail!("the document has no channels");
    };
    if channels.len() > MAX_OPERATIONS {
        anyhow::bail!("the document has more than {MAX_OPERATIONS} channels");
    }
    for (name, raw) in channels {
        let ch = bounded_resolve(doc, raw, total)?;
        let Some(op) = ch.get("subscribe") else {
            if ch.get("publish").is_some() {
                warnings.push(format!(
                    "{name}: only `publish` (the application receives); nothing to mock"
                ));
            }
            continue;
        };
        let op_name = op
            .get("operationId")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| format!("subscribe {name}"));
        let messages = message_list(op.get("message"));
        let mut protocols = binding_protocols(&[op.get("bindings"), ch.get("bindings")]);
        for m in &messages {
            protocols.extend(binding_protocols(&[m.get("bindings")]));
        }
        if protocols.is_empty() {
            let restricted: Vec<String> = ch
                .get("servers")
                .and_then(|s| s.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            for (srv, p) in servers {
                if restricted.is_empty() || restricted.contains(srv) {
                    protocols.insert(*p);
                }
            }
        }
        out.push(OpSpec {
            op_name,
            address: name.clone(),
            params: ch.get("parameters").cloned(),
            protocols,
            messages,
            kafka_topic: ch
                .pointer("/bindings/kafka/topic")
                .and_then(|t| t.as_str())
                .map(String::from),
        });
    }
    Ok(out)
}

fn collect_v3(
    doc: &Value,
    servers: &BTreeMap<String, AsyncProtocol>,
    total: &mut ResolveBudget,
    warnings: &mut Vec<String>,
) -> anyhow::Result<Vec<OpSpec>> {
    let mut out = Vec::new();
    let Some(ops) = doc.get("operations").and_then(|c| c.as_object()) else {
        anyhow::bail!("the document has no operations");
    };
    if ops.len() > MAX_OPERATIONS {
        anyhow::bail!("the document has more than {MAX_OPERATIONS} operations");
    }
    for (op_id, raw_op) in ops {
        let raw_op = if raw_op.get("$ref").is_some() {
            bounded_resolve(doc, raw_op, total)?
        } else {
            raw_op.clone()
        };
        let action = raw_op.get("action").and_then(|a| a.as_str()).unwrap_or("");
        if action != "send" {
            if action == "receive" {
                warnings.push(format!(
                    "{op_id}: `receive` (the application consumes); nothing to mock"
                ));
            }
            continue;
        }
        let channel_key = ref_name(raw_op.get("channel")).unwrap_or_else(|| op_id.clone());
        let op = bounded_resolve(doc, &raw_op, total)?;
        let ch = op.get("channel").cloned().unwrap_or(Value::Null);
        let address = match ch.get("address") {
            Some(Value::String(a)) => a.clone(),
            _ => channel_key.clone(),
        };
        let mut messages = message_list(op.get("messages"));
        if messages.is_empty()
            && let Some(Value::Object(m)) = ch.get("messages")
        {
            messages = m.values().cloned().collect();
        }
        let mut protocols = binding_protocols(&[op.get("bindings"), ch.get("bindings")]);
        for m in &messages {
            protocols.extend(binding_protocols(&[m.get("bindings")]));
        }
        if protocols.is_empty() {
            match ch.get("servers").and_then(|s| s.as_array()) {
                Some(list) if !list.is_empty() => {
                    for s in list {
                        if let Some(p) = server_protocol(s) {
                            protocols.insert(p);
                        }
                    }
                }
                _ => protocols.extend(servers.values().copied()),
            }
        }
        out.push(OpSpec {
            op_name: op_id.clone(),
            address,
            params: ch.get("parameters").cloned(),
            protocols,
            messages,
            kafka_topic: ch
                .pointer("/bindings/kafka/topic")
                .and_then(|t| t.as_str())
                .map(String::from),
        });
    }
    Ok(out)
}

/// How Kafka topics are named: after the channel (default) or as Microcks does,
/// `<service name without spaces and dashes>-<version>-<channel with / replaced by ->`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TopicNaming {
    #[default]
    Channel,
    Microcks,
}

fn microcks_topic(
    title: &str,
    version: &str,
    doc_version: u8,
    spec: &OpSpec,
    warnings: &mut Vec<String>,
) -> String {
    let service = title.replace([' ', '-'], "");
    let part = if doc_version == 2 || spec.address.contains('{') {
        let mut a = spec.address.clone();
        if let Some(Value::Object(ps)) = &spec.params {
            for (name, p) in ps {
                let v = param_value(p).unwrap_or_else(|| name.clone());
                a = a.replace(&format!("{{{name}}}"), &v);
            }
        }
        a.replace('/', "-")
    } else {
        spec.op_name.clone()
    };
    let raw = format!("{service}-{}-{part}", version.replace(' ', ""));
    let topic: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(249)
        .collect();
    if topic != raw {
        warnings.push(format!(
            "Microcks topic `{raw}` was mapped to Kafka topic `{topic}`"
        ));
    }
    topic
}

pub fn import(raw: &Value, opts: &AsyncImportOptions) -> anyhow::Result<AsyncImportResult> {
    import_named(raw, opts, TopicNaming::Channel)
}

pub fn import_named(
    raw: &Value,
    opts: &AsyncImportOptions,
    naming: TopicNaming,
) -> anyhow::Result<AsyncImportResult> {
    let version = parse_version(raw)?;
    let mut out = AsyncImportResult {
        title: raw
            .pointer("/info/title")
            .and_then(|t| t.as_str())
            .unwrap_or("asyncapi")
            .to_string(),
        version: raw
            .pointer("/info/version")
            .map(value_text)
            .unwrap_or_default(),
        ..Default::default()
    };
    let frequency = opts
        .frequency_ms
        .unwrap_or(DEFAULT_FREQUENCY_MS)
        .clamp(MIN_FREQUENCY_MS, MAX_FREQUENCY_MS);
    let mut total = ResolveBudget::default();
    let servers = all_server_protocols(raw, &mut total);
    let default_ct = raw.get("defaultContentType").and_then(|c| c.as_str());
    let specs = if version == 2 {
        collect_v2(raw, &servers, &mut total, &mut out.warnings)?
    } else {
        collect_v3(raw, &servers, &mut total, &mut out.warnings)?
    };
    let service = format!("{}:{}", out.title, out.version);
    for spec in specs {
        if spec.protocols.is_empty() {
            out.warnings.push(format!(
                "{}: no Kafka or WebSocket binding or server; skipped",
                spec.op_name
            ));
            continue;
        }
        if spec.messages.is_empty() {
            out.warnings.push(format!(
                "{}: the operation has no messages; skipped",
                spec.op_name
            ));
            continue;
        }
        let multi = spec.messages.len() > 1;
        for (i, msg) in spec.messages.iter().enumerate() {
            let fallback = format!("message-{}", i + 1);
            let info = message_info(msg, &fallback, default_ct, &mut out.warnings);
            if info.examples.is_empty() {
                continue;
            }
            for proto in &spec.protocols {
                let channel = match proto {
                    AsyncProtocol::Kafka => match (&spec.kafka_topic, naming) {
                        (_, TopicNaming::Microcks) => microcks_topic(
                            &out.title,
                            &out.version,
                            version,
                            &spec,
                            &mut out.warnings,
                        ),
                        (Some(t), _) => t.clone(),
                        (None, _) => {
                            sanitize_topic(&spec.address, spec.params.as_ref(), &mut out.warnings)
                        }
                    },
                    AsyncProtocol::Ws => ws_path(&opts.path_prefix, &spec.address),
                };
                let name = if multi {
                    format!("{} {}", spec.op_name, info.name)
                } else {
                    spec.op_name.clone()
                };
                out.mocks.push(AsyncMock {
                    id: Uuid::now_v7(),
                    name,
                    protocol: *proto,
                    channel,
                    operation: Some(spec.op_name.clone()),
                    service: Some(service.clone()),
                    messages: info.examples.clone(),
                    frequency_ms: frequency,
                    enabled: true,
                    templated: true,
                    content_type: info.content_type.clone(),
                    avro_schema: if *proto == AsyncProtocol::Kafka {
                        info.avro_schema.clone()
                    } else {
                        None
                    },
                    payload_schema: info.payload_schema.clone(),
                    tags: if opts.tag.is_empty() {
                        vec![]
                    } else {
                        vec![opts.tag.clone()]
                    },
                    updated_at: None,
                });
            }
        }
    }
    if out.mocks.is_empty() {
        out.warnings.push("no mockable operations found".into());
    }
    Ok(out)
}

pub fn schema_errors(schema: &Value, instance: &Value, max: usize) -> Result<Vec<String>, String> {
    let v = crate::matcher::compile_schema(schema).map_err(|e| e.to_string())?;
    Ok(v.0
        .iter_errors(instance)
        .take(max)
        .map(|e| {
            let path = e.instance_path().to_string();
            let mut msg = format!("{}: {e}", if path.is_empty() { "$" } else { path.as_str() });
            if msg.len() > 512 {
                let mut cut = 512;
                while !msg.is_char_boundary(cut) {
                    cut -= 1;
                }
                msg.truncate(cut);
                msg.push('…');
            }
            msg
        })
        .collect())
}

pub fn payload_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub fn websocket_stub(m: &AsyncMock) -> Stub {
    let ws = WebSocketDef {
        messages: m
            .messages
            .iter()
            .map(|x| WsMessage {
                data: payload_text(&x.payload),
                delay_ms: m.frequency_ms,
            })
            .collect(),
        repeat: true,
        ..Default::default()
    };
    Stub {
        id: Uuid::now_v7(),
        name: Some(m.name.clone()),
        priority: crate::model::DEFAULT_PRIORITY,
        request: RequestPattern {
            method: Some("GET".into()),
            url_path_template: Some(m.channel.clone()),
            ..Default::default()
        },
        response: ResponseDef {
            status: 426,
            templated: m.templated,
            headers: BTreeMap::from([("content-type".to_string(), "application/json".to_string())]),
            json_body: Some(
                json!({"error": "this endpoint is a WebSocket; send an Upgrade request"}),
            ),
            websocket: Some(ws),
            ..Default::default()
        },
        tags: m.tags.clone(),
        metadata: Some(json!({"asyncapi": {"operation": m.operation, "service": m.service}})),
        enabled: m.enabled,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openapi::parse_document;

    fn opts() -> AsyncImportOptions {
        AsyncImportOptions {
            tag: "events".into(),
            frequency_ms: None,
            path_prefix: "/ws".into(),
        }
    }

    const V26: &str = r##"
asyncapi: "2.6.0"
info: {title: Accounts, version: "1.2"}
defaultContentType: application/json
servers:
  broker: {url: "kafka.example.com:9092", protocol: kafka}
  sockets: {url: "wss://ws.example.com", protocol: wss}
channels:
  user/signedup:
    servers: [broker]
    subscribe:
      operationId: userSignedUp
      message: {$ref: "#/components/messages/UserSignedUp"}
  user/{userId}/updates:
    servers: [sockets]
    parameters:
      userId: {schema: {type: string, example: u42}}
    subscribe:
      message:
        payload: {type: object, properties: {status: {type: string, enum: [active, banned]}}}
  orders:
    bindings: {kafka: {topic: orders.v1}}
    subscribe:
      operationId: orderPlaced
      message:
        oneOf:
          - {$ref: "#/components/messages/OrderPlaced"}
          - name: OrderCancelled
            payload: {type: object, properties: {id: {type: integer}}, example: {id: 5}}
  commands:
    publish:
      message: {payload: {type: string}}
components:
  messages:
    UserSignedUp:
      name: UserSignedUp
      headers: {type: object, properties: {x-source: {type: string, example: signup}}}
      bindings: {kafka: {key: {type: string, format: uuid}}}
      payload: {$ref: "#/components/schemas/User"}
      examples:
        - name: alice
          headers: {x-source: web}
          payload: {id: "{{uuid}}", email: alice@example.com}
        - name: bob
          payload: {id: "2", email: bob@example.com}
    OrderPlaced:
      name: OrderPlaced
      schemaFormat: "application/vnd.apache.avro;version=1.9.0"
      payload:
        type: record
        name: Order
        namespace: com.example
        fields:
          - {name: id, type: long}
          - {name: amount, type: {type: bytes, logicalType: decimal, precision: 10, scale: 2}}
          - {name: status, type: {type: enum, name: Status, symbols: [NEW, PAID]}}
          - {name: note, type: ["null", "string"], default: null}
          - {name: lines, type: {type: array, items: {type: record, name: Line, fields: [{name: sku, type: string}]}}}
          - {name: first, type: Line}
  schemas:
    User:
      type: object
      required: [id, email]
      properties:
        id: {type: string}
        email: {type: string, format: email}
        friend: {$ref: "#/components/schemas/User"}
"##;

    #[test]
    fn imports_asyncapi_2_6() {
        let doc = parse_document(V26).unwrap();
        let r = import(&doc, &opts()).unwrap();
        let by = |n: &str| {
            r.mocks.iter().find(|m| m.name == n).unwrap_or_else(|| {
                panic!(
                    "{n}: {:?}",
                    r.mocks.iter().map(|m| &m.name).collect::<Vec<_>>()
                )
            })
        };
        let signed = by("userSignedUp");
        assert_eq!(signed.protocol, AsyncProtocol::Kafka);
        assert_eq!(signed.channel, "user-signedup");
        assert_eq!(signed.messages.len(), 2);
        assert_eq!(signed.messages[0].name.as_deref(), Some("alice"));
        assert_eq!(signed.messages[0].headers["x-source"], "web");
        assert_eq!(signed.messages[1].headers["x-source"], "signup");
        assert_eq!(
            signed.messages[0].key.as_deref(),
            Some("3fa85f64-5717-4562-b3fc-2c963f66afa6")
        );
        assert_eq!(signed.content_type.as_deref(), Some("application/json"));
        assert_eq!(signed.frequency_ms, DEFAULT_FREQUENCY_MS);
        assert_eq!(signed.tags, vec!["events".to_string()]);
        assert_eq!(signed.service.as_deref(), Some("Accounts:1.2"));
        let schema = signed.payload_schema.as_ref().unwrap();
        assert_eq!(schema["required"], json!(["id", "email"]));
        let ws = r
            .mocks
            .iter()
            .find(|m| m.protocol == AsyncProtocol::Ws)
            .unwrap();
        assert_eq!(ws.channel, "/ws/user/{userId}/updates");
        assert_eq!(ws.messages[0].payload, json!({"status": "active"}));
        let placed = by("orderPlaced OrderPlaced");
        assert_eq!(placed.channel, "orders.v1");
        let avro = placed.avro_schema.as_ref().unwrap();
        assert_eq!(avro["name"], "Order");
        assert_eq!(
            placed.messages[0].payload,
            json!({"id": 0, "amount": "0", "status": "NEW", "note": null, "lines": [{"sku": "string"}], "first": {"sku": "string"}})
        );
        let cancelled = by("orderPlaced OrderCancelled");
        assert_eq!(cancelled.messages[0].payload, json!({"id": 5}));
        assert!(r.warnings.iter().any(|w| w.contains("commands")));
        assert!(r.warnings.iter().any(|w| w.contains("user-signedup")));
        for m in &r.mocks {
            m.validate(|_| Ok(())).unwrap();
        }
    }

    const V30: &str = r##"{
  "asyncapi": "3.0.0",
  "info": {"title": "Lights", "version": "2"},
  "servers": {
    "prod": {"host": "broker.example.com:9092", "protocol": "kafka"},
    "socket": {"host": "ws.example.com", "protocol": "ws"}
  },
  "channels": {
    "lightingMeasured": {
      "address": "smartylighting.streetlights.1.0.event.{streetlightId}.lighting.measured",
      "servers": [{"$ref": "#/servers/prod"}],
      "parameters": {"streetlightId": {"enum": ["s1", "s2"]}},
      "messages": {"lightMeasured": {"$ref": "#/components/messages/lightMeasured"}}
    },
    "live": {
      "address": "/live",
      "servers": [{"$ref": "#/servers/socket"}],
      "messages": {"tick": {"payload": {"type": "object", "properties": {"n": {"type": "integer", "minimum": 3}}}}}
    },
    "turnOn": {
      "address": "turn.on",
      "messages": {"cmd": {"payload": {"type": "object"}}}
    }
  },
  "operations": {
    "sendLightMeasured": {
      "action": "send",
      "channel": {"$ref": "#/channels/lightingMeasured"},
      "messages": [{"$ref": "#/channels/lightingMeasured/messages/lightMeasured"}]
    },
    "streamTicks": {"action": "send", "channel": {"$ref": "#/channels/live"}},
    "turnOnCommand": {"action": "receive", "channel": {"$ref": "#/channels/turnOn"}}
  },
  "components": {
    "messages": {
      "lightMeasured": {
        "name": "lightMeasured",
        "contentType": "application/json",
        "bindings": {"kafka": {"key": {"type": "string", "examples": ["s1"]}}},
        "payload": {"$ref": "#/components/schemas/lightMeasuredPayload"}
      }
    },
    "schemas": {
      "lightMeasuredPayload": {
        "type": "object",
        "properties": {
          "lumens": {"type": "integer", "minimum": 0, "examples": [12]},
          "sentAt": {"type": "string", "format": "date-time"}
        },
        "examples": [{"lumens": 7, "sentAt": "{{now}}"}, {"lumens": 9, "sentAt": "2026-01-01T00:00:00Z"}]
      }
    }
  }
}"##;

    #[test]
    fn imports_asyncapi_3_0() {
        let doc = parse_document(V30).unwrap();
        let r = import(
            &doc,
            &AsyncImportOptions {
                frequency_ms: Some(10),
                ..opts()
            },
        )
        .unwrap();
        assert_eq!(r.mocks.len(), 2, "{:?}", r.mocks);
        let k = &r.mocks[0];
        assert_eq!(k.name, "sendLightMeasured");
        assert_eq!(k.protocol, AsyncProtocol::Kafka);
        assert_eq!(
            k.channel,
            "smartylighting.streetlights.1.0.event.s1.lighting.measured"
        );
        assert_eq!(k.frequency_ms, MIN_FREQUENCY_MS);
        assert_eq!(k.messages.len(), 2);
        assert_eq!(k.messages[0].payload["sentAt"], "{{now}}");
        assert_eq!(k.messages[0].key.as_deref(), Some("s1"));
        let w = &r.mocks[1];
        assert_eq!(w.protocol, AsyncProtocol::Ws);
        assert_eq!(w.channel, "/ws/live");
        assert_eq!(w.messages[0].payload, json!({"n": 3}));
        assert!(r.warnings.iter().any(|w| w.contains("turnOnCommand")));
        let stub = websocket_stub(w);
        assert_eq!(stub.request.url_path_template.as_deref(), Some("/ws/live"));
        let def = stub.response.websocket.as_ref().unwrap();
        assert!(def.repeat);
        assert_eq!(def.messages[0].data, "{\"n\":3}");
        assert_eq!(def.messages[0].delay_ms, MIN_FREQUENCY_MS);
        assert!(def.validate(|_| Ok(())).is_ok());
    }

    #[test]
    fn multi_format_avro_payload_and_errors() {
        let doc = json!({"asyncapi": "3.0.0", "info": {"title": "x", "version": "1"},
            "channels": {"c": {"address": "c", "bindings": {"kafka": {}}, "messages": {"m": {
                "payload": {"schemaFormat": "application/vnd.apache.avro+json;version=1.9.0",
                    "schema": {"type": "record", "name": "R", "fields": [{"name": "at", "type": {"type": "long", "logicalType": "timestamp-millis"}},
                        {"name": "tags", "type": {"type": "map", "values": "int"}}]}}}}}},
            "operations": {"o": {"action": "send", "channel": {"$ref": "#/channels/c"}}}});
        let r = import(&doc, &opts()).unwrap();
        assert_eq!(r.mocks.len(), 1);
        assert!(r.mocks[0].avro_schema.is_some());
        assert_eq!(
            r.mocks[0].messages[0].payload,
            json!({"at": "2026-01-01T00:00:00Z", "tags": {"key": 0}})
        );
        assert!(import(&json!({"openapi": "3.0.0"}), &opts()).is_err());
        assert!(import(&json!({"asyncapi": "1.2.0"}), &opts()).is_err());
        let none = import(
            &json!({"asyncapi": "2.6.0", "info": {"title": "x", "version": "1"}, "servers": {"m": {"url": "x", "protocol": "mqtt"}},
            "channels": {"a": {"subscribe": {"message": {"payload": {"type": "string"}}}}}}),
            &opts(),
        )
        .unwrap();
        assert!(none.mocks.is_empty());
        assert!(
            none.warnings
                .iter()
                .any(|w| w.contains("no Kafka or WebSocket"))
        );
    }

    #[test]
    fn recursive_avro_records_are_bounded() {
        let fields: Vec<Value> = (0..4)
            .map(|i| json!({"name": format!("f{i}"), "type": "R"}))
            .chain([json!({"name": "pad", "type": {"type": "fixed", "name": "P", "size": 1024}})])
            .collect();
        let schema = json!({"type": "record", "name": "R", "fields": fields});
        let started = std::time::Instant::now();
        let v = avro_example(&schema);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(v["f0"], Value::Null);
        assert_eq!(v["pad"].as_str().map(str::len), Some(1024));
        let list = json!({"type": "record", "name": "Node", "namespace": "x", "fields": [
            {"name": "v", "type": "int"}, {"name": "next", "type": ["null", "x.Node"]}]});
        assert_eq!(avro_example(&list), json!({"v": 0, "next": null}));
        let mut types = vec![json!({"type": "fixed", "name": "T0", "size": 1024})];
        for i in 1..20 {
            types.push(json!({"type": "record", "name": format!("T{i}"), "fields": [
                {"name": "a", "type": format!("T{}", i - 1)}, {"name": "b", "type": format!("T{}", i - 1)}]}));
        }
        let defs: Vec<Value> = types
            .into_iter()
            .enumerate()
            .map(|(i, t)| json!({"name": format!("d{i}"), "type": t}))
            .collect();
        let wide = json!({"type": "record", "name": "Top", "fields": defs});
        let size = serde_json::to_vec(&avro_example(&wide)).unwrap().len();
        assert!(size <= MAX_AVRO_EXAMPLE_BYTES + 64 * 1024, "{size}");
        let doc = json!({"asyncapi": "2.6.0", "info": {"title": "t", "version": "1"},
            "servers": {"k": {"url": "k:9092", "protocol": "kafka"}},
            "channels": {"t": {"subscribe": {"message": {"schemaFormat": "application/vnd.apache.avro;version=1.9.0",
                "payload": schema}}}}});
        let r = import(&doc, &opts()).unwrap();
        assert_eq!(r.mocks.len(), 1);
    }

    #[test]
    fn diamond_refs_hit_the_budget() {
        let mut schemas = Map::new();
        for i in 0..30 {
            schemas.insert(
                format!("S{i}"),
                json!({"type": "object", "properties": {
                "a": {"$ref": format!("#/components/schemas/S{}", i + 1)},
                "b": {"$ref": format!("#/components/schemas/S{}", i + 1)}}}),
            );
        }
        schemas.insert("S30".into(), json!({"type": "string"}));
        let doc = json!({"asyncapi": "2.6.0", "info": {"title": "x", "version": "1"},
            "components": {"schemas": schemas},
            "channels": {"t": {"bindings": {"kafka": {}}, "subscribe": {"message": {"payload": {"$ref": "#/components/schemas/S0"}}}}}});
        let err = import(&doc, &opts()).unwrap_err().to_string();
        assert!(err.contains("$ref expansion"), "{err}");
    }

    #[test]
    fn schema_errors_are_reported_with_paths() {
        let schema = json!({"type": "object", "required": ["id"], "properties": {"id": {"type": "integer"}}});
        assert!(
            schema_errors(&schema, &json!({"id": 1}), 10)
                .unwrap()
                .is_empty()
        );
        let errs = schema_errors(&schema, &json!({"id": "x"}), 10).unwrap();
        assert_eq!(errs.len(), 1);
        assert!(errs[0].starts_with("/id"), "{errs:?}");
        assert!(schema_errors(&json!({"type": 5}), &json!(1), 10).is_err());
    }

    #[test]
    fn validation_limits() {
        let base = AsyncMock {
            id: Uuid::now_v7(),
            name: "m".into(),
            protocol: AsyncProtocol::Kafka,
            channel: "t".into(),
            operation: None,
            service: None,
            messages: vec![AsyncMessage::default()],
            frequency_ms: 1000,
            enabled: true,
            templated: true,
            content_type: None,
            avro_schema: None,
            payload_schema: None,
            tags: vec![],
            updated_at: None,
        };
        assert!(base.validate(|_| Ok(())).is_ok());
        let mut m = base.clone();
        m.frequency_ms = 999;
        assert!(m.validate(|_| Ok(())).is_err());
        let mut m = base.clone();
        m.channel = "bad topic".into();
        assert!(m.validate(|_| Ok(())).is_err());
        let mut m = base.clone();
        m.messages = vec![];
        assert!(m.validate(|_| Ok(())).is_err());
        let mut m = base.clone();
        m.messages[0].payload = json!("{{#if}}");
        assert!(m.validate(|_| Err("bad".into())).is_err());
        m.templated = false;
        assert!(m.validate(|_| Err("bad".into())).is_ok());
        let parsed: AsyncMock = serde_json::from_value(
            json!({"name": "x", "channel": "t", "messages": [{"payload": 1}]}),
        )
        .unwrap();
        assert_eq!(parsed.frequency_ms, DEFAULT_FREQUENCY_MS);
        assert!(parsed.enabled);
        assert!(
            serde_json::from_value::<AsyncMock>(
                json!({"name": "x", "channel": "t", "messages": [], "bogus": 1})
            )
            .is_err()
        );
    }
}
