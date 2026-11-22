//! Captured broker messages: what FastMock received from the Kafka topics, MQTT topic filters and AMQP
//! exchanges an application publishes to. The model, the source definitions, the [`CaptureStore`]
//! interface with its in-memory implementation, and the [`MessageMatch`] used by find, wait and verify.
//! The consumers and the admin API live in the server crate; the PostgreSQL store in `fastmock-store`.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::matcher::{CompiledBodies, CompiledString};
use crate::model::{BodyMatcher, MessageProtocol, SpaceFilter, StringMatcher};
use crate::storage::StoreResult;

/// Largest page a list returns.
pub const MAX_PAGE: usize = 500;
/// Topics, filters or binding keys per source.
pub const MAX_SOURCE_TOPICS: usize = 20;

fn yes() -> bool {
    true
}

/// Where a Kafka source starts reading.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum StartFrom {
    #[default]
    Latest,
    Earliest,
}

/// A subscription whose messages FastMock stores: Kafka topics, MQTT topic filters or an AMQP exchange.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CaptureSource {
    #[serde(default = "Uuid::now_v7")]
    pub id: Uuid,
    /// 1..64 characters of `a-z 0-9 . _ -`, unique in its space; matchers and filters use it.
    pub name: String,
    /// The space of the source and of its messages; set from `?space=` by the API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    pub protocol: MessageProtocol,
    /// Kafka topics, or MQTT topic filters (`+` and `#` wildcards).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<String>,
    /// AMQP: the exchange to bind FastMock's own queue to (not the default exchange).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exchange: Option<String>,
    /// AMQP: binding keys of FastMock's queue (default `#`, every message of a topic exchange).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub binding_keys: Vec<String>,
    /// MQTT: subscription QoS (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qos: Option<u8>,
    /// Kafka: `latest` (default) or `earliest`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_from: Option<StartFrom>,
    /// Messages kept for this source, oldest dropped first; at most `FASTMOCK_CAPTURE_MAX_MESSAGES`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_messages: Option<usize>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Defined in the server configuration (`FASTMOCK_CAPTURE_SOURCES` or `_FILE`); read-only in the API.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_config: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

fn valid_source_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

fn valid_mqtt_filter(f: &str) -> bool {
    if f.is_empty() || f.len() > 65_535 || f.contains('\0') {
        return false;
    }
    let levels: Vec<&str> = f.split('/').collect();
    levels.iter().enumerate().all(|(i, l)| {
        (!l.contains('+') || *l == "+")
            && (!l.contains('#') || (*l == "#" && i == levels.len() - 1))
    })
}

impl CaptureSource {
    /// Checks the fields against the protocol.
    pub fn validate(&self) -> Result<(), String> {
        if !valid_source_name(&self.name) {
            return Err("name must be 1..64 characters of a-z 0-9 . _ -".into());
        }
        if self.description.as_ref().is_some_and(|d| d.len() > 1024) {
            return Err("description is at most 1024 bytes".into());
        }
        if self.max_messages == Some(0) {
            return Err("maxMessages must be at least 1".into());
        }
        if self.topics.len() > MAX_SOURCE_TOPICS || self.binding_keys.len() > MAX_SOURCE_TOPICS {
            return Err(format!(
                "at most {MAX_SOURCE_TOPICS} topics or binding keys per source"
            ));
        }
        match self.protocol {
            MessageProtocol::Kafka => {
                if self.topics.is_empty() {
                    return Err("a Kafka source needs topics".into());
                }
                if let Some(t) = self
                    .topics
                    .iter()
                    .find(|t| !crate::asyncapi::valid_topic(t))
                {
                    return Err(format!("`{t}` is not a valid Kafka topic name"));
                }
                if self.exchange.is_some() || !self.binding_keys.is_empty() || self.qos.is_some() {
                    return Err("exchange, bindingKeys and qos are not used with Kafka".into());
                }
            }
            MessageProtocol::Mqtt => {
                if self.topics.is_empty() {
                    return Err("an MQTT source needs topic filters in topics".into());
                }
                if let Some(t) = self.topics.iter().find(|t| !valid_mqtt_filter(t)) {
                    return Err(format!(
                        "`{t}` is not a valid MQTT topic filter (+ and # only as whole levels, # last)"
                    ));
                }
                if self.qos.is_some_and(|q| q > 2) {
                    return Err("qos must be 0, 1 or 2".into());
                }
                if self.exchange.is_some() || !self.binding_keys.is_empty() {
                    return Err("exchange and bindingKeys are not used with MQTT".into());
                }
                if self.start_from.is_some() {
                    return Err("startFrom is only used with Kafka".into());
                }
            }
            MessageProtocol::Amqp => {
                let ex = self.exchange.as_deref().unwrap_or_default();
                if ex.is_empty() || ex == "amq.default" {
                    return Err(
                        "an AMQP source needs a named exchange (the default exchange cannot be bound)"
                            .into(),
                    );
                }
                if ex.len() > 255
                    || !ex.chars().all(|c| {
                        c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/')
                    })
                {
                    return Err(format!("`{ex}` is not a valid exchange name"));
                }
                if !self.topics.is_empty() || self.qos.is_some() || self.start_from.is_some() {
                    return Err("topics, qos and startFrom are not used with AMQP; use exchange and bindingKeys".into());
                }
                if self.binding_keys.iter().any(|k| k.len() > 255) {
                    return Err("binding keys are at most 255 bytes".into());
                }
            }
        }
        Ok(())
    }

    /// Topics, filters or `exchange/binding key` pairs, for allow-lists and the UI.
    pub fn destinations(&self) -> Vec<String> {
        match self.protocol {
            MessageProtocol::Amqp => vec![self.exchange.clone().unwrap_or_default()],
            _ => self.topics.clone(),
        }
    }

    /// What decides the consumer: a changed fingerprint restarts it.
    pub fn fingerprint(&self) -> String {
        serde_json::to_string(&(
            &self.space,
            self.protocol,
            &self.topics,
            &self.exchange,
            &self.binding_keys,
            self.qos,
            self.start_from,
            &self.name,
        ))
        .unwrap_or_default()
    }
}

/// One stored message.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapturedMessage {
    pub id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    pub source_id: Uuid,
    /// The name of the source.
    pub source: String,
    pub protocol: MessageProtocol,
    /// Kafka or MQTT topic; AMQP routing key.
    pub topic: String,
    /// AMQP: the exchange the message was published to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exchange: Option<String>,
    /// Kafka record key as text (lowercase hex when it is not UTF-8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Kafka headers, MQTT 5 user properties or AMQP headers; repeated names are joined with `, `.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Protocol properties: Kafka `partition`, `offset`, `timestamp`; MQTT `qos`, `retain`,
    /// `correlationData`, ...; AMQP `messageId`, `correlationId`, `replyTo`, `deliveryMode`, ...
    #[serde(default)]
    #[schema(value_type = Object)]
    pub properties: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Bytes of the body as received.
    pub size: u64,
    /// The stored body was cut to `FASTMOCK_CAPTURE_MAX_MESSAGE_BYTES`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// The start of a text body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    pub received_at: DateTime<Utc>,
    /// The body, base64 in JSON; the API shows it decoded instead.
    #[serde(default, with = "b64")]
    #[schema(value_type = String, format = Byte)]
    pub body: Bytes,
}

mod b64 {
    use base64::Engine as _;
    use bytes::Bytes;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(b: &Bytes, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(b))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Bytes, D::Error> {
        let s = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(s)
            .map(Bytes::from)
            .map_err(serde::de::Error::custom)
    }
}

impl CapturedMessage {
    /// The body as text, replacing invalid UTF-8.
    pub fn text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }

    /// The body parsed as JSON, when it is JSON.
    pub fn json(&self) -> Option<Value> {
        crate::request::parse_json_bounded(&self.body)
    }

    /// The message without its body (lists).
    pub fn without_body(&self) -> Self {
        Self {
            body: Bytes::new(),
            ..self.clone()
        }
    }

    pub fn body_base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(&self.body)
    }
}

/// A position in a list: the id of the last message of the previous page (ids grow with time).
pub type CaptureCursor = Uuid;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CaptureQuery {
    pub spaces: SpaceFilter,
    pub source_id: Option<Uuid>,
    pub protocol: Option<MessageProtocol>,
    /// Exact topic or routing key.
    pub topic: Option<String>,
    /// Received at or after.
    pub since: Option<DateTime<Utc>>,
    /// Received before.
    pub until: Option<DateTime<Utc>>,
    pub limit: usize,
    pub cursor: Option<CaptureCursor>,
    pub oldest_first: bool,
    /// Load the bodies too.
    pub with_body: bool,
}

impl CaptureQuery {
    pub fn accepts(&self, m: &CapturedMessage) -> bool {
        self.spaces.matches(m.space.as_deref())
            && self.source_id.is_none_or(|s| m.source_id == s)
            && self.protocol.is_none_or(|p| m.protocol == p)
            && self.topic.as_ref().is_none_or(|t| &m.topic == t)
            && self.since.is_none_or(|s| m.received_at >= s)
            && self.until.is_none_or(|u| m.received_at < u)
            && self.cursor.is_none_or(|c| {
                if self.oldest_first {
                    m.id > c
                } else {
                    m.id < c
                }
            })
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CapturePage {
    pub items: Vec<CapturedMessage>,
    /// Where the next page starts; `None` on the last page.
    pub next: Option<CaptureCursor>,
}

/// Retention: messages per source are capped on insert, age by [`CaptureStore::sweep`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureRetention {
    pub max_messages: usize,
    pub max_age: Duration,
}

impl Default for CaptureRetention {
    fn default() -> Self {
        Self {
            max_messages: 1000,
            max_age: Duration::from_secs(86_400),
        }
    }
}

/// Storage of captured messages.
#[async_trait]
pub trait CaptureStore: Send + Sync {
    /// Stores messages of one source and drops its oldest beyond `keep`; returns how many were dropped.
    async fn insert(&self, msgs: &[CapturedMessage], keep: usize) -> StoreResult<u64>;
    async fn get(&self, id: Uuid, spaces: &SpaceFilter) -> StoreResult<Option<CapturedMessage>>;
    /// A page in id order (newest first unless `oldest_first`).
    async fn list(&self, q: &CaptureQuery) -> StoreResult<CapturePage>;
    /// The space of a deleted message: `Some(space)` when it existed.
    async fn delete(&self, id: Uuid, spaces: &SpaceFilter) -> StoreResult<Option<Option<String>>>;
    /// Deletes the messages of `spaces`, of one source when given.
    async fn delete_matching(&self, spaces: &SpaceFilter, source: Option<Uuid>)
    -> StoreResult<u64>;
    /// Deletes messages received before `before`.
    async fn sweep(&self, before: DateTime<Utc>) -> StoreResult<u64>;
    /// Stored messages per source.
    async fn counts(&self) -> StoreResult<HashMap<Uuid, u64>>;
}

/// Captured messages of one process (memory storage).
#[derive(Default)]
pub struct MemoryCaptureStore {
    inner: Mutex<MemoryInner>,
}

#[derive(Default)]
struct MemoryInner {
    messages: BTreeMap<Uuid, CapturedMessage>,
    by_source: HashMap<Uuid, VecDeque<Uuid>>,
}

impl MemoryInner {
    fn remove(&mut self, id: Uuid) -> Option<CapturedMessage> {
        let m = self.messages.remove(&id)?;
        if let Some(q) = self.by_source.get_mut(&m.source_id) {
            q.retain(|x| *x != id);
            if q.is_empty() {
                self.by_source.remove(&m.source_id);
            }
        }
        Some(m)
    }
}

#[async_trait]
impl CaptureStore for MemoryCaptureStore {
    async fn insert(&self, msgs: &[CapturedMessage], keep: usize) -> StoreResult<u64> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut dropped = 0;
        let mut sources = BTreeSet::new();
        for m in msgs {
            g.messages.insert(m.id, m.clone());
            g.by_source.entry(m.source_id).or_default().push_back(m.id);
            sources.insert(m.source_id);
        }
        for s in sources {
            loop {
                let over = g.by_source.get(&s).is_some_and(|q| q.len() > keep.max(1));
                if !over {
                    break;
                }
                let Some(oldest) = g.by_source.get_mut(&s).and_then(|q| q.pop_front()) else {
                    break;
                };
                g.messages.remove(&oldest);
                dropped += 1;
            }
        }
        Ok(dropped)
    }

    async fn get(&self, id: Uuid, spaces: &SpaceFilter) -> StoreResult<Option<CapturedMessage>> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Ok(g.messages
            .get(&id)
            .filter(|m| spaces.matches(m.space.as_deref()))
            .cloned())
    }

    async fn list(&self, q: &CaptureQuery) -> StoreResult<CapturePage> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let limit = q.limit.clamp(1, MAX_PAGE * 20);
        let iter: Box<dyn Iterator<Item = &CapturedMessage>> = if q.oldest_first {
            Box::new(g.messages.values())
        } else {
            Box::new(g.messages.values().rev())
        };
        let mut items: Vec<CapturedMessage> = Vec::new();
        let mut more = false;
        for m in iter.filter(|m| q.accepts(m)) {
            if items.len() == limit {
                more = true;
                break;
            }
            items.push(if q.with_body {
                m.clone()
            } else {
                m.without_body()
            });
        }
        let next = if more {
            items.last().map(|m| m.id)
        } else {
            None
        };
        Ok(CapturePage { items, next })
    }

    async fn delete(&self, id: Uuid, spaces: &SpaceFilter) -> StoreResult<Option<Option<String>>> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(m) = g.messages.get(&id) else {
            return Ok(None);
        };
        if !spaces.matches(m.space.as_deref()) {
            return Ok(None);
        }
        Ok(g.remove(id).map(|m| m.space))
    }

    async fn delete_matching(
        &self,
        spaces: &SpaceFilter,
        source: Option<Uuid>,
    ) -> StoreResult<u64> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let ids: Vec<Uuid> = g
            .messages
            .values()
            .filter(|m| {
                spaces.matches(m.space.as_deref()) && source.is_none_or(|s| m.source_id == s)
            })
            .map(|m| m.id)
            .collect();
        for id in &ids {
            g.remove(*id);
        }
        Ok(ids.len() as u64)
    }

    async fn sweep(&self, before: DateTime<Utc>) -> StoreResult<u64> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let ids: Vec<Uuid> = g
            .messages
            .values()
            .filter(|m| m.received_at < before)
            .map(|m| m.id)
            .collect();
        for id in &ids {
            g.remove(*id);
        }
        Ok(ids.len() as u64)
    }

    async fn counts(&self) -> StoreResult<HashMap<Uuid, u64>> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Ok(g.by_source
            .iter()
            .map(|(k, v)| (*k, v.len() as u64))
            .collect())
    }
}

/// Conditions on a captured message; all given ones must hold. String conditions are string matchers
/// (`equalTo`, `contains`, `matches`, `caseInsensitive`, `and`/`or`/`not`, `absent`, ...); `body` takes
/// the body matchers of stubs (`equalToJson`, `jsonPath`, `jsonSchema`, `contains`, `matches`, ...).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MessageMatch {
    /// The name of the capture source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<StringMatcher>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<MessageProtocol>,
    /// Kafka or MQTT topic, AMQP routing key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<StringMatcher>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exchange: Option<StringMatcher>,
    /// Kafka record key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<StringMatcher>,
    /// Header name (case-insensitive) to matcher.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, StringMatcher>,
    /// Protocol property (`correlationId`, `messageId`, `qos`, `retain`, `partition`, ...) to matcher.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, StringMatcher>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<StringMatcher>,
    /// Body matchers; JSON matchers see Avro bodies decoded when the schema registry knows the schema.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub body: Vec<BodyMatcher>,
}

impl MessageMatch {
    /// Matchers in the match, for near-miss scores.
    pub fn count(&self) -> usize {
        [
            self.source.is_some(),
            self.protocol.is_some(),
            self.topic.is_some(),
            self.exchange.is_some(),
            self.key.is_some(),
            self.content_type.is_some(),
        ]
        .into_iter()
        .filter(|x| *x)
        .count()
            + self.headers.len()
            + self.properties.len()
            + self.body.len()
    }

    pub fn needs_body(&self) -> bool {
        !self.body.is_empty()
    }
}

/// A compiled [`MessageMatch`].
pub struct CompiledMessageMatch {
    source: Option<CompiledString>,
    protocol: Option<MessageProtocol>,
    topic: Option<CompiledString>,
    exchange: Option<CompiledString>,
    key: Option<CompiledString>,
    headers: Vec<(String, CompiledString)>,
    properties: Vec<(String, CompiledString)>,
    content_type: Option<CompiledString>,
    body: CompiledBodies,
}

/// The body as a match sees it: the text, the bytes and the JSON value (for Avro: decoded).
pub struct BodyView<'a> {
    pub text: &'a str,
    pub raw: &'a [u8],
    pub json: Option<&'a Value>,
}

fn property_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

impl CompiledMessageMatch {
    pub fn compile(m: &MessageMatch) -> Result<Self, String> {
        let s = |x: &Option<StringMatcher>| {
            x.as_ref()
                .map(CompiledString::compile)
                .transpose()
                .map_err(|e| e.to_string())
        };
        if m.headers.len() > 64 || m.properties.len() > 64 || m.body.len() > 32 {
            return Err("at most 64 header or property matchers and 32 body matchers".into());
        }
        Ok(Self {
            source: s(&m.source)?,
            protocol: m.protocol,
            topic: s(&m.topic)?,
            exchange: s(&m.exchange)?,
            key: s(&m.key)?,
            headers: m
                .headers
                .iter()
                .map(|(k, v)| CompiledString::compile(v).map(|c| (k.to_ascii_lowercase(), c)))
                .collect::<Result<_, _>>()
                .map_err(|e| e.to_string())?,
            properties: m
                .properties
                .iter()
                .map(|(k, v)| CompiledString::compile(v).map(|c| (k.clone(), c)))
                .collect::<Result<_, _>>()
                .map_err(|e| e.to_string())?,
            content_type: s(&m.content_type)?,
            body: CompiledBodies::compile(&m.body).map_err(|e| e.to_string())?,
        })
    }

    /// The names of the failing conditions (empty when the message matches): `source`, `topic`,
    /// `headers.x-event`, `properties.correlationId`, `body[0]`, ... `body` is needed only when the
    /// match has body matchers; without it they fail.
    pub fn failures(&self, m: &CapturedMessage, body: Option<&BodyView<'_>>) -> Vec<String> {
        let mut out = Vec::new();
        let mut check = |name: &str, c: &Option<CompiledString>, v: Option<&str>| {
            if let Some(c) = c
                && !c.matches(v)
            {
                out.push(name.to_string());
            }
        };
        check("source", &self.source, Some(&m.source));
        check("topic", &self.topic, Some(&m.topic));
        check("exchange", &self.exchange, m.exchange.as_deref());
        check("key", &self.key, m.key.as_deref());
        check("contentType", &self.content_type, m.content_type.as_deref());
        if let Some(p) = self.protocol
            && p != m.protocol
        {
            out.push("protocol".into());
        }
        for (name, c) in &self.headers {
            let v = m
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str());
            if !c.matches(v) {
                out.push(format!("headers.{name}"));
            }
        }
        for (name, c) in &self.properties {
            let v = m.properties.get(name).map(property_text);
            if !c.matches(v.as_deref()) {
                out.push(format!("properties.{name}"));
            }
        }
        if !self.body.is_empty() {
            match body {
                Some(b) => {
                    for i in self.body.failures(b.text, b.raw, b.json) {
                        out.push(format!("body[{i}]"));
                    }
                }
                None => {
                    for i in 0..self.body.len() {
                        out.push(format!("body[{i}]"));
                    }
                }
            }
        }
        out
    }
}

/// A behaviour check shared by the store implementations' tests.
#[doc(hidden)]
pub mod conformance {
    use super::*;
    use serde_json::json;

    pub fn msg(source: Uuid, n: u8) -> CapturedMessage {
        CapturedMessage {
            id: Uuid::now_v7(),
            space: None,
            source_id: source,
            source: "orders".into(),
            protocol: MessageProtocol::Kafka,
            topic: "orders.events".into(),
            exchange: None,
            key: Some(format!("k{n}")),
            headers: BTreeMap::from([("X-Event".to_string(), "created".to_string())]),
            properties: Map::from_iter([("partition".to_string(), json!(0))]),
            content_type: Some("application/json".into()),
            size: 10,
            truncated: false,
            preview: None,
            received_at: Utc::now(),
            body: Bytes::from(format!("{{\"n\":{n},\"status\":\"NEW\"}}")),
        }
    }

    /// Checks a store: per-source caps, spaces, pages, deletes and sweeps.
    pub async fn run(s: &dyn CaptureStore) {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let first: Vec<CapturedMessage> = (0..5).map(|n| msg(a, n)).collect();
        assert_eq!(s.insert(&first, 3).await.unwrap(), 2);
        let mut other = msg(b, 9);
        other.space = Some("billing".into());
        s.insert(std::slice::from_ref(&other), 3).await.unwrap();
        let counts = s.counts().await.unwrap();
        assert_eq!(counts[&a], 3);
        assert_eq!(counts[&b], 1);
        assert!(
            s.get(first[0].id, &SpaceFilter::Any)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            s.get(first[4].id, &SpaceFilter::Any)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            s.get(other.id, &SpaceFilter::Global)
                .await
                .unwrap()
                .is_none()
        );
        let page = s
            .list(&CaptureQuery {
                limit: 2,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.items[0].id, other.id);
        assert!(page.items[0].body.is_empty());
        let next = s
            .list(&CaptureQuery {
                limit: 2,
                cursor: page.next,
                with_body: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(next.items.len(), 2);
        assert!(next.next.is_none());
        assert!(!next.items[0].body.is_empty());
        let global = s
            .list(&CaptureQuery {
                spaces: SpaceFilter::Global,
                limit: 10,
                oldest_first: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(global.items[0].id, first[2].id);
        assert_eq!(
            s.delete(other.id, &SpaceFilter::Global).await.unwrap(),
            None
        );
        assert_eq!(
            s.delete(other.id, &SpaceFilter::Any).await.unwrap(),
            Some(Some("billing".into()))
        );
        assert_eq!(
            s.delete_matching(&SpaceFilter::Any, Some(a)).await.unwrap(),
            3
        );
        s.insert(&[msg(a, 1)], 3).await.unwrap();
        assert_eq!(
            s.sweep(Utc::now() + chrono::Duration::seconds(1))
                .await
                .unwrap(),
            1
        );
        assert!(s.counts().await.unwrap().is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use conformance::msg;

    #[tokio::test]
    async fn the_memory_store_keeps_the_newest_per_source() {
        conformance::run(&MemoryCaptureStore::default()).await;
    }

    #[test]
    fn matches_report_what_failed() {
        let m = msg(Uuid::now_v7(), 1);
        let mm: MessageMatch = serde_json::from_value(json!({
            "source": {"equalTo": "orders"},
            "topic": {"contains": "orders"},
            "key": {"equalTo": "k1"},
            "headers": {"x-event": {"equalTo": "created"}},
            "properties": {"partition": {"equalTo": "0"}},
            "body": [{"jsonPath": "$.status", "value": {"equalTo": "NEW"}}, {"equalToJson": {"n": 1}, "ignoreExtraElements": true}]
        }))
        .unwrap();
        assert_eq!(mm.count(), 7);
        let c = CompiledMessageMatch::compile(&mm).unwrap();
        let json = m.json();
        let text = m.text();
        let view = BodyView {
            text: &text,
            raw: &m.body,
            json: json.as_ref(),
        };
        assert!(c.failures(&m, Some(&view)).is_empty());
        assert_eq!(c.failures(&m, None), vec!["body[0]", "body[1]"]);
        let bad: MessageMatch = serde_json::from_value(json!({
            "protocol": "mqtt",
            "key": {"equalTo": "k2"},
            "headers": {"x-other": {"equalTo": "1"}},
            "body": [{"contains": "PAID"}]
        }))
        .unwrap();
        let c = CompiledMessageMatch::compile(&bad).unwrap();
        assert_eq!(
            c.failures(&m, Some(&view)),
            vec!["key", "protocol", "headers.x-other", "body[0]"]
        );
        assert!(
            CompiledMessageMatch::compile(
                &serde_json::from_value(json!({"topic": {"matches": "("}})).unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn sources_are_checked_per_protocol() {
        let src = |v: Value| {
            serde_json::from_value::<CaptureSource>(v)
                .unwrap()
                .validate()
        };
        assert!(
            src(json!({"name": "orders", "protocol": "kafka", "topics": ["orders.events"]}))
                .is_ok()
        );
        assert!(src(json!({"name": "orders", "protocol": "kafka", "topics": []})).is_err());
        assert!(src(json!({"name": "Orders", "protocol": "kafka", "topics": ["a"]})).is_err());
        assert!(src(json!({"name": "d", "protocol": "mqtt", "topics": ["devices/+/state", "#"], "qos": 2})).is_ok());
        assert!(src(json!({"name": "d", "protocol": "mqtt", "topics": ["devices/#/x"]})).is_err());
        assert!(
            src(json!({"name": "d", "protocol": "mqtt", "topics": ["a"], "startFrom": "earliest"}))
                .is_err()
        );
        assert!(src(json!({"name": "o", "protocol": "amqp", "exchange": "orders", "bindingKeys": ["order.*"]})).is_ok());
        assert!(src(json!({"name": "o", "protocol": "amqp", "exchange": "amq.default"})).is_err());
        assert!(
            src(json!({"name": "o", "protocol": "amqp", "exchange": "orders", "topics": ["x"]}))
                .is_err()
        );
        assert!(
            serde_json::from_value::<CaptureSource>(json!({"name": "o", "protocol": "nats"}))
                .is_err()
        );
        let s: CaptureSource =
            serde_json::from_value(json!({"name": "o", "protocol": "amqp", "exchange": "x"}))
                .unwrap();
        assert!(s.enabled);
        let mut t = s.clone();
        t.description = Some("changed".into());
        assert_eq!(s.fingerprint(), t.fingerprint());
        t.binding_keys = vec!["a".into()];
        assert_ne!(s.fingerprint(), t.fingerprint());
    }

    #[test]
    fn stub_messages_check_fields_per_protocol() {
        use crate::model::MessageDef;
        let def = |v: Value| {
            serde_json::from_value::<MessageDef>(v)
                .unwrap()
                .check_fields()
        };
        assert!(def(json!({"topic": "orders", "key": "k"})).is_ok());
        assert!(def(json!({"topic": ""})).unwrap_err().contains("topic"));
        assert!(
            def(json!({"topic": "t", "qos": 1}))
                .unwrap_err()
                .contains("qos")
        );
        assert!(def(json!({"protocol": "mqtt", "topic": "a/b", "qos": 2, "retain": true, "correlationData": "c", "contentType": "application/json"})).is_ok());
        assert!(def(json!({"protocol": "mqtt", "topic": "a", "qos": 3})).is_err());
        assert!(
            def(json!({"protocol": "mqtt", "topic": "a", "key": "k"}))
                .unwrap_err()
                .contains("key")
        );
        assert!(def(json!({"protocol": "amqp", "exchange": "orders", "routingKey": "order.created", "persistent": true, "messageId": "{{uuid}}"})).is_ok());
        assert!(def(json!({"protocol": "amqp", "topic": "t"})).is_err());
        assert!(
            def(json!({"protocol": "amqp"}))
                .unwrap_err()
                .contains("exchange")
        );
        assert!(
            def(json!({"protocol": "amqp", "routingKey": "q", "exchangeType": "x-delayed"}))
                .is_err()
        );
        assert!(
            def(json!({"protocol": "amqp", "routingKey": "q", "avroSchema": {"type": "string"}}))
                .is_err()
        );
        assert!(def(json!({"topic": "t", "delayMs": 4_000_000})).is_err());
        assert!(serde_json::from_value::<MessageDef>(json!({"topic": "t", "qoss": 1})).is_err());
        let m: MessageDef = serde_json::from_value(
            json!({"protocol": "amqp", "exchange": "orders", "routingKey": "k"}),
        )
        .unwrap();
        assert_eq!(m.destination(), "orders/k");
        let v = serde_json::to_value(MessageDef {
            topic: "t".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(v, json!({"topic": "t"}));
    }

    #[test]
    fn messages_serialize_the_body_as_base64() {
        let m = msg(Uuid::now_v7(), 2);
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["body"], m.body_base64());
        let back: CapturedMessage = serde_json::from_value(v).unwrap();
        assert_eq!(back, m);
    }
}
