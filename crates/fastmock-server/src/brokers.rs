//! Publishing stub messages and async mocks to Kafka, MQTT and AMQP.
//!
//! [`Brokers`] owns the Kafka hub of [`crate::extras`], one MQTT connection and one AMQP publisher. The MQTT
//! and AMQP connections open on the first message and reconnect by themselves; an unset or unreachable broker
//! never blocks readiness, the messages to it fail and are counted.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use fastmock_brokers::amqp::{AmqpMessage, AmqpPublisher, ExchangeDeclaration};
use fastmock_brokers::mqtt::{MqttConnection, MqttPublish, MqttSettings};
use fastmock_brokers::{LinkStatus, name_allowed};
use fastmock_core::MessageProtocol;
use fastmock_core::engine::RenderedMessage;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::OnceCell;
use tracing::Instrument;

use crate::extras::KafkaHub;

/// Publications in flight per replica before further ones are dropped.
pub const MAX_INFLIGHT: usize = 256;

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
        }
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

    /// Whether the allow-list of the protocol admits the destination (topic, or AMQP exchange).
    pub fn allowed(&self, p: MessageProtocol, destination: &str) -> bool {
        match p {
            MessageProtocol::Kafka => self.kafka.topic_allowed(destination),
            MessageProtocol::Mqtt => name_allowed(self.mqtt_allowed.as_deref(), destination),
            MessageProtocol::Amqp => name_allowed(
                self.amqp_allowed.as_deref(),
                fastmock_brokers::amqp::api_exchange(destination),
            ),
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
                    .ok_or_else(|| anyhow::anyhow!("FASTMOCK_MQTT_URL is not configured"))?;
                if !name_allowed(hub.allowed.as_deref(), &m.topic) {
                    anyhow::bail!("topic is not in FASTMOCK_MQTT_ALLOWED_TOPICS");
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
                    .ok_or_else(|| anyhow::anyhow!("FASTMOCK_AMQP_URL is not configured"))?;
                let o = m.amqp.clone().unwrap_or_default();
                let exchange = fastmock_brokers::amqp::api_exchange(&o.exchange).to_string();
                if !fastmock_brokers::amqp::valid_exchange(fastmock_brokers::amqp::wire_exchange(
                    &exchange,
                )) {
                    anyhow::bail!("`{exchange}` is not a valid exchange name");
                }
                if !name_allowed(hub.allowed.as_deref(), &exchange) {
                    anyhow::bail!("exchange {exchange} is not in FASTMOCK_AMQP_ALLOWED_EXCHANGES");
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
                let decl = hub.declare.then(|| ExchangeDeclaration {
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
    }

    /// Publishes in a task, so the response is not delayed; drops the messages when
    /// [`MAX_INFLIGHT`] publications are already running.
    pub fn publish_in_background(self: &Arc<Self>, messages: Vec<RenderedMessage>) {
        let Ok(permit) = self.inflight.clone().try_acquire_owned() else {
            for m in &messages {
                metrics::counter!("fastmock_messages_total", "protocol" => m.protocol.as_str(), "result" => "dropped")
                    .increment(1);
            }
            tracing::warn!(
                messages = messages.len(),
                "too many message publications in flight; dropped"
            );
            return;
        };
        let hub = self.clone();
        tokio::spawn(
            async move {
                let _permit = permit;
                hub.publish(&messages).await;
            }
            .in_current_span(),
        );
    }
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
        c.amqp_allowed_exchanges = Some(vec!["orders".into(), "amq.default".into()]);
        c
    }

    #[test]
    fn payloads_and_allow_lists() {
        assert_eq!(payload(&json!("text")), b"text");
        assert_eq!(payload(&Value::Null), b"");
        assert_eq!(payload(&json!({"a": 1})), br#"{"a":1}"#);
        let c = cfg();
        let b = Brokers::new(&c, Arc::new(KafkaHub::new(&c).unwrap()));
        assert!(b.allowed(MessageProtocol::Mqtt, "devices/d1/state"));
        assert!(!b.allowed(MessageProtocol::Mqtt, "audit"));
        assert!(b.allowed(MessageProtocol::Amqp, "orders"));
        assert!(b.allowed(MessageProtocol::Amqp, ""));
        assert!(!b.allowed(MessageProtocol::Amqp, "audit"));
        assert!(b.allowed(MessageProtocol::Kafka, "anything"));
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
