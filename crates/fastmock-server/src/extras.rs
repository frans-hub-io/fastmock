use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use chrono::Utc;
use fastmock_core::engine::RenderedMessage;
use fastmock_core::{ChaosRule, Fault, SseEvent};
use fastmock_kafka::{KafkaPublisher, KafkaSettings, SchemaRegistry, confluent_frame};
use futures::stream;
use serde_json::Value;
use tokio::sync::{OnceCell, RwLock};
use tracing::Instrument;

use crate::state::AppState;

pub const CHAOS_KEY: &str = "chaos:rules";

#[derive(Clone, Copy, Debug)]
pub struct Streaming;

pub fn sse_response(events: Vec<SseEvent>, status: u16, max_delay: Duration) -> Response {
    let s = stream::unfold(events.into_iter(), move |mut it| async move {
        let e = it.next()?;
        if e.delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(e.delay_ms).min(max_delay)).await;
        }
        let mut chunk = String::new();
        if let Some(id) = &e.id {
            chunk.push_str(&format!("id: {}\n", id.replace(['\n', '\r'], " ")));
        }
        if let Some(ev) = &e.event {
            chunk.push_str(&format!("event: {}\n", ev.replace(['\n', '\r'], " ")));
        }
        for line in e.data.split("\r\n").flat_map(|l| l.split(['\r', '\n'])) {
            chunk.push_str(&format!("data: {line}\n"));
        }
        chunk.push('\n');
        Some((Ok::<Bytes, std::io::Error>(Bytes::from(chunk)), it))
    });
    let mut r = Response::new(Body::from_stream(s));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    r.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    r.headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-cache"));
    r.extensions_mut().insert(Streaming);
    r
}

pub struct KafkaHub {
    settings: Option<KafkaSettings>,
    registry: Option<SchemaRegistry>,
    publisher: OnceCell<KafkaPublisher>,
    allowed_topics: Option<Vec<String>>,
}

pub fn topic_allowed(allowed: Option<&[String]>, topic: &str) -> bool {
    allowed.is_none_or(|l| {
        l.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => topic.starts_with(prefix),
            None => topic == p,
        })
    })
}

impl KafkaHub {
    pub fn new(cfg: &crate::config::Config) -> anyhow::Result<Self> {
        Ok(Self {
            settings: cfg.kafka_bootstrap.clone().map(|b| KafkaSettings {
                bootstrap: b,
                client_id: "fastmock".into(),
                security: cfg.kafka_security.clone(),
            }),
            registry: cfg
                .kafka_schema_registry
                .as_deref()
                .map(|r| SchemaRegistry::with_options(r, &cfg.kafka_schema_registry_options))
                .transpose()?,
            publisher: OnceCell::new(),
            allowed_topics: cfg.kafka_allowed_topics.clone(),
        })
    }

    async fn publisher(&self) -> anyhow::Result<&KafkaPublisher> {
        let s = self
            .settings
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("FASTMOCK_KAFKA_BOOTSTRAP is not configured"))?;
        self.publisher
            .get_or_try_init(|| async {
                tokio::time::timeout(Duration::from_secs(5), KafkaPublisher::connect(s))
                    .await
                    .map_err(|_| anyhow::anyhow!("connecting to Kafka timed out"))?
            })
            .await
    }

    async fn encode(&self, m: &RenderedMessage) -> anyhow::Result<Vec<u8>> {
        match &m.avro_schema {
            None => Ok(match &m.value {
                Value::String(s) => s.clone().into_bytes(),
                Value::Null => Vec::new(),
                other => serde_json::to_vec(other)?,
            }),
            Some(schema_json) => {
                let reg = self.registry.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("avroSchema needs FASTMOCK_KAFKA_SCHEMA_REGISTRY_URL")
                })?;
                let schema = apache_avro::Schema::parse(schema_json)?;
                let id = reg
                    .register(&format!("{}-value", m.topic), &schema_json.to_string())
                    .await?;
                let avro_value = fastmock_kafka::json_to_avro(&schema, &m.value)?;
                Ok(confluent_frame(
                    id,
                    &fastmock_kafka::encode(&schema, avro_value)?,
                ))
            }
        }
    }

    pub fn publish_in_background(self: &Arc<Self>, messages: Vec<RenderedMessage>) {
        static INFLIGHT: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
            std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(256)));
        let Ok(permit) = INFLIGHT.clone().try_acquire_owned() else {
            metrics::counter!("fastmock_messages_total", "result" => "dropped")
                .increment(messages.len() as u64);
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

    pub fn topic_allowed(&self, topic: &str) -> bool {
        topic_allowed(self.allowed_topics.as_deref(), topic)
    }

    pub async fn publish_one(&self, m: &RenderedMessage) -> anyhow::Result<(i32, i64)> {
        if !topic_allowed(self.allowed_topics.as_deref(), &m.topic) {
            anyhow::bail!("topic is not in FASTMOCK_KAFKA_ALLOWED_TOPICS");
        }
        let value = self.encode(m).await?;
        let headers: BTreeMap<String, Vec<u8>> = m
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone().into_bytes()))
            .collect();
        self.publisher()
            .await?
            .publish(
                &m.topic,
                m.key.clone().map(String::into_bytes),
                value,
                headers,
            )
            .await
    }

    pub async fn publish(&self, messages: &[RenderedMessage]) {
        for m in messages {
            let r = self.publish_one(m).await;
            match r {
                Ok((partition, offset)) => {
                    metrics::counter!("fastmock_messages_total", "result" => "sent").increment(1);
                    tracing::info!(topic = %m.topic, partition, offset, "message published");
                }
                Err(e) => {
                    metrics::counter!("fastmock_messages_total", "result" => "failed").increment(1);
                    tracing::error!(topic = %m.topic, error = %format!("{e:#}"), "message publishing failed");
                }
            }
        }
    }
}

pub struct ChaosCache {
    rules: RwLock<(Instant, Arc<Vec<ChaosRule>>)>,
}

impl Default for ChaosCache {
    fn default() -> Self {
        Self {
            rules: RwLock::new((
                Instant::now() - Duration::from_secs(60),
                Arc::new(Vec::new()),
            )),
        }
    }
}

impl ChaosCache {
    pub async fn rules(&self, app: &AppState) -> Arc<Vec<ChaosRule>> {
        {
            let r = self.rules.read().await;
            if r.0.elapsed() < Duration::from_secs(2) {
                return r.1.clone();
            }
        }
        let loaded: Vec<ChaosRule> = match app.stores.hot.get(CHAOS_KEY).await {
            Ok(Some(raw)) => match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(e) => {
                    metrics::counter!("fastmock_store_errors_total", "store" => "chaos")
                        .increment(1);
                    tracing::error!(error = %e, "stored chaos rules are not valid; ignoring them");
                    Vec::new()
                }
            },
            Ok(None) => Vec::new(),
            Err(e) => {
                metrics::counter!("fastmock_store_errors_total", "store" => "chaos").increment(1);
                tracing::warn!(error = %e, "reading chaos rules failed; keeping the previous set");
                let mut w = self.rules.write().await;
                w.0 = Instant::now();
                return w.1.clone();
            }
        };
        let arc = Arc::new(loaded);
        *self.rules.write().await = (Instant::now(), arc.clone());
        arc
    }

    pub async fn invalidate(&self) {
        self.rules.write().await.0 = Instant::now() - Duration::from_secs(60);
    }
}

pub enum ChaosOutcome {
    None,
    Delay(Duration, String),
    Respond(Response, String),
}

pub async fn chaos(app: &AppState, source: &str, method: &str, path: &str) -> ChaosOutcome {
    let rules = app.chaos.rules(app).await;
    if rules.is_empty() {
        return ChaosOutcome::None;
    }
    let now = Utc::now();
    for r in rules.iter() {
        if !r.applies(source, method, path, now) || rand::random::<f64>() >= r.probability {
            continue;
        }
        metrics::counter!("fastmock_chaos_injections_total", "rule" => r.id.clone()).increment(1);
        let delay = r
            .delay
            .as_ref()
            .map(|d| Duration::from_millis(d.sample_ms()).min(app.cfg.max_delay));
        if r.status.is_none() && r.fault.is_none() {
            return ChaosOutcome::Delay(delay.unwrap_or_default(), r.id.clone());
        }
        if let Some(d) = delay {
            tokio::time::sleep(d).await;
        }
        let resp = match r.fault {
            Some(Fault::ConnectionReset) => {
                let body = Body::from_stream(stream::once(async {
                    Err::<Bytes, std::io::Error>(std::io::Error::other("chaos: connection reset"))
                }));
                let mut x = Response::new(body);
                x.extensions_mut().insert(Streaming);
                x
            }
            Some(
                f @ (Fault::RandomDataThenClose
                | Fault::MalformedResponseChunk
                | Fault::EmptyResponse),
            ) => crate::dataplane::raw_fault_response(f),
            Some(Fault::MalformedJson) => {
                ([("content-type", "application/json")], "{\"chaos\": [").into_response()
            }
            None => (
                StatusCode::from_u16(r.status.unwrap_or(500))
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                r.body.clone().unwrap_or_default(),
            )
                .into_response(),
        };
        return ChaosOutcome::Respond(resp, r.id.clone());
    }
    ChaosOutcome::None
}
