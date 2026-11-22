//! Publishing stub messages and async mocks to Kafka, MQTT and AMQP.
//!
//! [`Brokers`] owns the Kafka hub of [`crate::extras`], one MQTT connection and one AMQP publisher. The MQTT
//! and AMQP connections open on the first message and reconnect by themselves; an unset or unreachable broker
//! never blocks readiness, the messages to it fail and are counted.
//!
//! Messages without `delayMs` are published at once, at most [`MAX_INFLIGHT`] publications at a time.
//! Delayed messages wait in a separate bounded queue ([`MAX_DELAYED`] messages, [`MAX_DELAYED_BYTES`]
//! bytes, at most [`MAX_DELAYED_PER_STUB`] per stub) and take a publication slot only when they are due.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use fastmock_brokers::amqp::{AmqpMessage, AmqpPublisher, ExchangeDeclaration};
use fastmock_brokers::mqtt::{MqttConnection, MqttPublish, MqttSettings};
use fastmock_brokers::{LinkStatus, name_allowed, public};
use fastmock_core::MessageProtocol;
use fastmock_core::engine::RenderedMessage;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::OnceCell;
use tracing::Instrument;

use crate::extras::KafkaHub;

/// Publications in flight per replica before further ones are dropped.
pub const MAX_INFLIGHT: usize = 256;
/// Delayed messages waiting per replica before further ones are dropped.
pub const MAX_DELAYED: usize = 4096;
/// Bytes of delayed messages waiting per replica.
pub const MAX_DELAYED_BYTES: usize = 64 * 1024 * 1024;
/// Delayed messages of one stub waiting per replica.
pub const MAX_DELAYED_PER_STUB: usize = 1024;
/// How long a due delayed message waits for a publication slot before it is dropped.
const DUE_SLOT_WAIT: Duration = Duration::from_secs(10);

/// Where a message went.
#[derive(Clone, Debug, Default, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    pub protocol: MessageProtocol,
    /// Kafka or MQTT topic; AMQP routing key.
    pub topic: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exchange: Option<String>,
    /// Kafka partition and offset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partition: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
}

struct MqttHub {
    settings: MqttSettings,
    conn: OnceCell<Arc<MqttConnection>>,
    allowed: Option<Vec<String>>,
}

impl MqttHub {
    async fn conn(&self) -> anyhow::Result<&Arc<MqttConnection>> {
        self.conn
            .get_or_try_init(|| async {
                MqttConnection::start(&self.settings, self.settings.client_id("pub"), vec![], None)
            })
            .await
    }
}

struct AmqpHub {
    publisher: AmqpPublisher,
    allowed: Option<Vec<String>>,
    declare: bool,
}

/// The message publishers of this replica.
pub struct Brokers {
    pub kafka: Arc<KafkaHub>,
    mqtt: Option<MqttHub>,
    amqp: Option<AmqpHub>,
    mqtt_allowed: Option<Vec<String>>,
    amqp_allowed: Option<Vec<String>>,
    max_delay: Duration,
    inflight: Arc<tokio::sync::Semaphore>,
    delayed: Arc<Mutex<Delayed>>,
}

/// What the delayed queue holds.
#[derive(Default)]
struct Delayed {
    messages: usize,
    bytes: usize,
    per_stub: HashMap<Option<uuid::Uuid>, usize>,
}

/// A place in the delayed queue, given back when dropped.
struct DelayedSlot {
    queue: Arc<Mutex<Delayed>>,
    stub: Option<uuid::Uuid>,
    bytes: usize,
}

impl Drop for DelayedSlot {
    fn drop(&mut self) {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        q.messages = q.messages.saturating_sub(1);
        q.bytes = q.bytes.saturating_sub(self.bytes);
        if let Some(n) = q.per_stub.get_mut(&self.stub) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                q.per_stub.remove(&self.stub);
            }
        }
    }
}

/// Approximate bytes a rendered message holds while it waits.
pub fn message_bytes(m: &RenderedMessage) -> usize {
    let value = match &m.value {
        Value::String(s) => s.len(),
        Value::Null => 0,
        other => other.to_string().len(),
    };
    256 + value
        + m.topic.len()
        + m.key.as_ref().map_or(0, String::len)
        + m.content_type.as_ref().map_or(0, String::len)
        + m.headers
            .iter()
            .map(|(k, v)| k.len() + v.len())
            .sum::<usize>()
}

/// The bytes of a message value: text as is, `null` as nothing, other JSON as JSON.
pub fn payload(v: &Value) -> Vec<u8> {
    match v {
        Value::String(s) => s.clone().into_bytes(),
        Value::Null => Vec::new(),
        other => serde_json::to_vec(other).unwrap_or_default(),
    }
}

impl Brokers {
    pub fn new(cfg: &crate::config::Config, kafka: Arc<KafkaHub>) -> Self {
        Self {
            kafka,
            mqtt: cfg.mqtt.clone().map(|settings| MqttHub {
                settings,
                conn: OnceCell::new(),
                allowed: cfg.mqtt_allowed_topics.clone(),
            }),
            amqp: cfg.amqp.clone().map(|settings| AmqpHub {
                declare: settings.declare_exchanges,
                publisher: AmqpPublisher::new(settings, "fastmock"),
                allowed: cfg.amqp_allowed_exchanges.clone(),
            }),
            mqtt_allowed: cfg.mqtt_allowed_topics.clone(),
            amqp_allowed: cfg.amqp_allowed_exchanges.clone(),
            max_delay: cfg.max_delay,
            inflight: Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT)),
            delayed: Arc::new(Mutex::new(Delayed::default())),
        }
    }

    /// Delayed messages waiting now.
    pub fn delayed_waiting(&self) -> usize {
        self.delayed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .messages
    }

    /// A place in the delayed queue for a message of `stub`, unless the queue is full.
    fn reserve_delayed(&self, stub: Option<uuid::Uuid>, bytes: usize) -> Option<DelayedSlot> {
        let mut q = self.delayed.lock().unwrap_or_else(|e| e.into_inner());
        let of_stub = q.per_stub.get(&stub).copied().unwrap_or(0);
        if q.messages >= MAX_DELAYED
            || q.bytes + bytes > MAX_DELAYED_BYTES
            || of_stub >= MAX_DELAYED_PER_STUB
        {
            return None;
        }
        q.messages += 1;
        q.bytes += bytes;
        *q.per_stub.entry(stub).or_default() += 1;
        Some(DelayedSlot {
            queue: self.delayed.clone(),
            stub,
            bytes,
        })
    }

    /// The setting that enables a protocol, when it is missing.
    pub fn missing_setting(
        &self,
        cfg: &crate::config::Config,
        p: MessageProtocol,
    ) -> Option<&'static str> {
        match p {
            MessageProtocol::Kafka if cfg.kafka_bootstrap.is_none() => {
                Some("FASTMOCK_KAFKA_BOOTSTRAP")
            }
            MessageProtocol::Mqtt if self.mqtt.is_none() => Some("FASTMOCK_MQTT_URL"),
            MessageProtocol::Amqp if self.amqp.is_none() => Some("FASTMOCK_AMQP_URL"),
            _ => None,
        }
    }

    /// Whether the allow-list of the protocol admits the destination: the topic, or the AMQP exchange
    /// with its routing key. The AMQP default exchange routes to the queue named by the routing key, so an
    /// allow-list admits it only through `amq.default/<queue>` entries (`amq.default/orders.*`).
    pub fn allowed(&self, p: MessageProtocol, destination: &str, routing_key: &str) -> bool {
        match p {
            MessageProtocol::Kafka => self.kafka.topic_allowed(destination),
            MessageProtocol::Mqtt => name_allowed(self.mqtt_allowed.as_deref(), destination),
            MessageProtocol::Amqp => {
                amqp_allowed(self.amqp_allowed.as_deref(), destination, routing_key)
            }
        }
    }

    /// The name of the allow-list of a protocol, for errors.
    pub fn allow_list_name(p: MessageProtocol) -> &'static str {
        match p {
            MessageProtocol::Kafka => "FASTMOCK_KAFKA_ALLOWED_TOPICS",
            MessageProtocol::Mqtt => "FASTMOCK_MQTT_ALLOWED_TOPICS",
            MessageProtocol::Amqp => "FASTMOCK_AMQP_ALLOWED_EXCHANGES",
        }
    }

    /// The state of the MQTT and AMQP publishers (for `/__admin/info`).
    pub fn status(&self) -> (Option<LinkStatus>, Option<LinkStatus>) {
        (
            self.mqtt
                .as_ref()
                .map(|h| h.conn.get().map(|c| c.status()).unwrap_or_default()),
            self.amqp.as_ref().map(|h| h.publisher.status()),
        )
    }

    pub async fn publish_one(&self, m: &RenderedMessage) -> anyhow::Result<Receipt> {
        match m.protocol {
            MessageProtocol::Kafka => {
                let (partition, offset) = self.kafka.publish_one(m).await?;
                Ok(Receipt {
                    protocol: m.protocol,
                    topic: m.topic.clone(),
                    partition: Some(partition),
                    offset: Some(offset),
                    ..Default::default()
                })
            }
            MessageProtocol::Mqtt => {
                let hub = self
                    .mqtt
                    .as_ref()
                    .ok_or_else(|| public("FASTMOCK_MQTT_URL is not configured"))?;
                if !name_allowed(hub.allowed.as_deref(), &m.topic) {
                    return Err(public("topic is not in FASTMOCK_MQTT_ALLOWED_TOPICS"));
                }
                let o = m.mqtt.clone().unwrap_or_default();
                let publish = MqttPublish {
                    topic: m.topic.clone(),
                    payload: payload(&m.value),
                    qos: o.qos,
                    retain: o.retain,
                    user_properties: m
                        .headers
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                    content_type: m.content_type.clone(),
                    correlation_data: o.correlation_data.map(String::into_bytes),
                    response_topic: o.response_topic,
                    message_expiry_interval: o.message_expiry_seconds,
                };
                hub.conn().await?.publish(&publish).await?;
                Ok(Receipt {
                    protocol: m.protocol,
                    topic: m.topic.clone(),
                    ..Default::default()
                })
            }
            MessageProtocol::Amqp => {
                let hub = self
                    .amqp
                    .as_ref()
                    .ok_or_else(|| public("FASTMOCK_AMQP_URL is not configured"))?;
                let o = m.amqp.clone().unwrap_or_default();
                let exchange = fastmock_brokers::amqp::api_exchange(&o.exchange).to_string();
                if !fastmock_brokers::amqp::valid_exchange(fastmock_brokers::amqp::wire_exchange(
                    &exchange,
                )) {
                    return Err(public(format!(
                        "`{}` is not a valid exchange name",
                        fastmock_brokers::cut(&exchange, 64)
                    )));
                }
                if !amqp_allowed(hub.allowed.as_deref(), &exchange, &m.topic) {
                    return Err(public(
                        if exchange == fastmock_brokers::amqp::DEFAULT_EXCHANGE {
                            "the default exchange with this routing key is not in FASTMOCK_AMQP_ALLOWED_EXCHANGES (add amq.default/<queue>)".to_string()
                        } else {
                            format!("exchange {exchange} is not in FASTMOCK_AMQP_ALLOWED_EXCHANGES")
                        },
                    ));
                }
                let msg = AmqpMessage {
                    exchange: exchange.clone(),
                    routing_key: m.topic.clone(),
                    payload: payload(&m.value),
                    headers: m
                        .headers
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                    content_type: m.content_type.clone(),
                    message_id: o.message_id,
                    correlation_id: o.correlation_id,
                    reply_to: o.reply_to,
                    expiration: o.expiration,
                    persistent: o.persistent,
                    priority: o.priority,
                    message_type: o.message_type,
                    app_id: o.app_id,
                    timestamp: Some(chrono::Utc::now().timestamp().max(0) as u64),
                    ..Default::default()
                };
                let decl = (hub.declare && !o.exchange_templated).then(|| ExchangeDeclaration {
                    kind: o.exchange_type.clone().unwrap_or_else(|| "topic".into()),
                    durable: o.durable,
                });
                hub.publisher.publish(&msg, decl.as_ref()).await?;
                Ok(Receipt {
                    protocol: m.protocol,
                    topic: m.topic.clone(),
                    exchange: Some(exchange),
                    ..Default::default()
                })
            }
        }
    }

    /// Publishes in order of `delayMs`, each one that long after the call; logs and counts the outcome.
    pub async fn publish(&self, messages: &[RenderedMessage]) {
        let start = tokio::time::Instant::now();
        let mut order: Vec<&RenderedMessage> = messages.iter().collect();
        order.sort_by_key(|m| m.delay_ms);
        for m in order {
            let delay = Duration::from_millis(m.delay_ms).min(self.max_delay);
            if !delay.is_zero() {
                tokio::time::sleep_until(start + delay).await;
            }
            self.publish_logged(m).await;
        }
    }

    async fn publish_logged(&self, m: &RenderedMessage) {
        let protocol = m.protocol.as_str();
        match self.publish_one(m).await {
            Ok(r) => {
                metrics::counter!("fastmock_messages_total", "protocol" => protocol, "result" => "sent")
                    .increment(1);
                tracing::info!(
                    protocol,
                    topic = %m.destination(),
                    partition = r.partition,
                    offset = r.offset,
                    "message published"
                );
            }
            Err(e) => {
                metrics::counter!("fastmock_messages_total", "protocol" => protocol, "result" => "failed")
                    .increment(1);
                tracing::error!(protocol, topic = %m.destination(), error = %format!("{e:#}"), "message publishing failed");
            }
        }
    }

    /// Publishes in a task, so the response is not delayed. Messages due now need one of
    /// [`MAX_INFLIGHT`] publication slots and are dropped without one. Delayed messages of `stub` wait in
    /// the bounded delayed queue without holding a slot, and are dropped when it is full; when due they
    /// wait up to 10 s for a slot.
    pub fn publish_in_background(
        self: &Arc<Self>,
        messages: Vec<RenderedMessage>,
        stub: Option<uuid::Uuid>,
    ) {
        let dropped = |m: &RenderedMessage, why: &str| {
            metrics::counter!("fastmock_messages_total", "protocol" => m.protocol.as_str(), "result" => "dropped")
                .increment(1);
            tracing::warn!(protocol = m.protocol.as_str(), topic = %m.destination(), "{why}; message dropped");
        };
        let start = tokio::time::Instant::now();
        let mut order = messages;
        order.sort_by_key(|m| m.delay_ms);
        let mut now = Vec::new();
        let mut later = Vec::new();
        for m in order {
            let delay = Duration::from_millis(m.delay_ms).min(self.max_delay);
            if delay.is_zero() {
                now.push(m);
            } else {
                match self.reserve_delayed(stub, message_bytes(&m)) {
                    Some(slot) => later.push((m, start + delay, slot)),
                    None => dropped(&m, "too many delayed messages waiting"),
                }
            }
        }
        let permit = if now.is_empty() {
            None
        } else {
            match self.inflight.clone().try_acquire_owned() {
                Ok(p) => Some(p),
                Err(_) => {
                    for m in &now {
                        dropped(m, "too many message publications in flight");
                    }
                    now.clear();
                    None
                }
            }
        };
        if now.is_empty() && later.is_empty() {
            return;
        }
        let hub = self.clone();
        tokio::spawn(
            async move {
                for m in &now {
                    hub.publish_logged(m).await;
                }
                drop(permit);
                for (m, due, slot) in later {
                    tokio::time::sleep_until(due).await;
                    let permit =
                        tokio::time::timeout(DUE_SLOT_WAIT, hub.inflight.clone().acquire_owned())
                            .await;
                    drop(slot);
                    match permit {
                        Ok(Ok(_permit)) => hub.publish_logged(&m).await,
                        _ => dropped(&m, "no publication slot became free"),
                    }
                }
            }
            .in_current_span(),
        );
    }
}

/// The allow-list check of AMQP: exchanges by name; the default exchange only through
/// `amq.default/<routing key>` entries (exact or `prefix*`). `None` allows everything.
pub fn amqp_allowed(list: Option<&[String]>, exchange: &str, routing_key: &str) -> bool {
    let exchange = fastmock_brokers::amqp::api_exchange(exchange);
    if exchange == fastmock_brokers::amqp::DEFAULT_EXCHANGE {
        return name_allowed(list, &format!("{exchange}/{routing_key}"));
    }
    name_allowed(list, exchange)
}

/// The body of a message as bytes (for tests and capture).
pub fn payload_bytes(v: &Value) -> Bytes {
    Bytes::from(payload(v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastmock_core::engine::{RenderedAmqp, RenderedMqtt};
    use serde_json::json;

    fn cfg() -> crate::config::Config {
        let mut c = crate::config::Config::for_tests(crate::config::StorageKind::Memory, vec![]);
        c.mqtt = Some(MqttSettings::new("mqtt://127.0.0.1:1").unwrap());
        c.mqtt_allowed_topics = Some(vec!["devices/*".into()]);
        c.amqp_allowed_exchanges = Some(vec!["orders".into(), "amq.default/replies.*".into()]);
        c
    }

    #[test]
    fn payloads_and_allow_lists() {
        assert_eq!(payload(&json!("text")), b"text");
        assert_eq!(payload(&Value::Null), b"");
        assert_eq!(payload(&json!({"a": 1})), br#"{"a":1}"#);
        let c = cfg();
        let b = Brokers::new(&c, Arc::new(KafkaHub::new(&c).unwrap()));
        assert!(b.allowed(MessageProtocol::Mqtt, "devices/d1/state", ""));
        assert!(!b.allowed(MessageProtocol::Mqtt, "audit", ""));
        assert!(b.allowed(MessageProtocol::Amqp, "orders", "any.key"));
        assert!(b.allowed(MessageProtocol::Amqp, "", "replies.a"));
        assert!(b.allowed(MessageProtocol::Amqp, "amq.default", "replies.b"));
        assert!(!b.allowed(MessageProtocol::Amqp, "", "payments"));
        assert!(!b.allowed(MessageProtocol::Amqp, "audit", "x"));
        assert!(b.allowed(MessageProtocol::Kafka, "anything", ""));
        assert_eq!(
            b.missing_setting(&c, MessageProtocol::Amqp),
            Some("FASTMOCK_AMQP_URL")
        );
        assert_eq!(b.missing_setting(&c, MessageProtocol::Mqtt), None);
        assert_eq!(
            b.missing_setting(&c, MessageProtocol::Kafka),
            Some("FASTMOCK_KAFKA_BOOTSTRAP")
        );
    }

    #[test]
    fn the_default_exchange_needs_allowed_queues() {
        let plain = vec!["amq.default".to_string()];
        assert!(
            !amqp_allowed(Some(&plain), "", "any-queue"),
            "allowing amq.default alone admits no queue"
        );
        assert!(!amqp_allowed(Some(&plain), "amq.default", "payments"));
        let queues = vec!["amq.default/orders.*".to_string()];
        assert!(amqp_allowed(Some(&queues), "", "orders.created"));
        assert!(!amqp_allowed(Some(&queues), "", "payments"));
        assert!(amqp_allowed(None, "", "anything"));
    }

    #[tokio::test]
    async fn delayed_messages_wait_without_publication_slots() {
        let c = cfg();
        let b = Arc::new(Brokers::new(&c, Arc::new(KafkaHub::new(&c).unwrap())));
        let delayed = |n: usize| -> Vec<RenderedMessage> {
            (0..n)
                .map(|_| RenderedMessage {
                    protocol: MessageProtocol::Mqtt,
                    topic: "devices/d1/state".into(),
                    delay_ms: 5_000,
                    mqtt: Some(RenderedMqtt::default()),
                    ..Default::default()
                })
                .collect()
        };
        let a = uuid::Uuid::now_v7();
        b.publish_in_background(delayed(MAX_DELAYED_PER_STUB + 500), Some(a));
        assert_eq!(
            b.delayed_waiting(),
            MAX_DELAYED_PER_STUB,
            "one stub gets a share"
        );
        assert_eq!(
            b.inflight.available_permits(),
            MAX_INFLIGHT,
            "waiting messages hold no publication slot"
        );
        for _ in 0..4 {
            b.publish_in_background(delayed(MAX_DELAYED_PER_STUB), Some(uuid::Uuid::now_v7()));
        }
        assert_eq!(b.delayed_waiting(), MAX_DELAYED);
        let big = RenderedMessage {
            value: json!("x".repeat(1024 * 1024)),
            ..delayed(1).remove(0)
        };
        let before = b.delayed_waiting();
        b.publish_in_background(vec![big], Some(uuid::Uuid::now_v7()));
        assert_eq!(b.delayed_waiting(), before, "the queue is full");
        assert!(b.inflight.clone().try_acquire_owned().is_ok());
    }

    #[test]
    fn delayed_messages_are_bounded_by_bytes() {
        let c = cfg();
        let b = Brokers::new(&c, Arc::new(KafkaHub::new(&c).unwrap()));
        let mut slots = Vec::new();
        for _ in 0..100 {
            if let Some(s) = b.reserve_delayed(Some(uuid::Uuid::now_v7()), 1024 * 1024) {
                slots.push(s);
            }
        }
        assert_eq!(slots.len(), MAX_DELAYED_BYTES / (1024 * 1024));
        slots.clear();
        assert_eq!(b.delayed_waiting(), 0, "slots are given back");
    }

    #[tokio::test]
    async fn refusals_are_errors() {
        let c = cfg();
        let b = Brokers::new(&c, Arc::new(KafkaHub::new(&c).unwrap()));
        let e = b
            .publish_one(&RenderedMessage {
                protocol: MessageProtocol::Mqtt,
                topic: "audit/x".into(),
                mqtt: Some(RenderedMqtt::default()),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(e.to_string().contains("FASTMOCK_MQTT_ALLOWED_TOPICS"));
        let e = b
            .publish_one(&RenderedMessage {
                protocol: MessageProtocol::Amqp,
                topic: "k".into(),
                amqp: Some(RenderedAmqp {
                    exchange: "orders".into(),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(e.to_string().contains("FASTMOCK_AMQP_URL"));
    }
}
