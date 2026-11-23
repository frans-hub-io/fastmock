//! The capture consumers. Every replica runs [`run`], but only the replica holding the lease
//! `capture:leader` (see [`crate::asyncmock::Leader`]) subscribes, so each message is stored once. The
//! leader reloads the sources every few seconds, starts a consumer per enabled source, restarts it when the
//! source changes and stops it when the source is disabled or deleted. It publishes each source's state to
//! the hot store, where every replica reads it for the API.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use fastmock_brokers::amqp::AmqpCapture;
use fastmock_brokers::mqtt::MqttConnection;
use fastmock_brokers::{
    Backoff, CaptureLimits, CaptureQueue, CaptureSink, LinkStatus, MAX_CAPTURE_META_BYTES,
    MAX_CAPTURE_NAME_BYTES, MAX_CAPTURE_VALUE_BYTES, Offer, Received, capture_channel,
    text_or_hex_capped,
};
use fastmock_core::MessageProtocol;
use fastmock_core::capture::{CaptureSource, CapturedMessage, InsertLimits, StartFrom};
use serde_json::json;
use tokio::sync::watch;
use tokio::time::Instant;
use uuid::Uuid;

use super::{CaptureService, LEASE_KEY, STATUS_PREFIX, SourceStatus, load_sources};
use crate::asyncmock::Leader;
use crate::state::AppState;

/// Messages a consumer buffers between the broker and the store.
const BUFFER: usize = 4096;
/// Bytes a consumer buffers between the broker and the store (at least two messages of the largest size).
const BUFFER_BYTES: usize = 16 * 1024 * 1024;

/// The queue bounds of a consumer.
pub fn queue_limits(max_message_bytes: usize) -> CaptureLimits {
    CaptureLimits {
        max_messages: BUFFER,
        max_bytes: BUFFER_BYTES.max(2 * (max_message_bytes + MAX_CAPTURE_META_BYTES + 1024)),
        max_payload: max_message_bytes,
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeOptions {
    pub lease_ttl: Duration,
    pub renew_every: Duration,
    pub tick: Duration,
    pub reload_every: Duration,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            lease_ttl: Duration::from_secs(15),
            renew_every: Duration::from_secs(5),
            tick: Duration::from_millis(250),
            reload_every: Duration::from_secs(5),
        }
    }
}

#[derive(Default)]
struct Stats {
    received: AtomicU64,
    dropped: AtomicU64,
    last_at: Mutex<Option<DateTime<Utc>>>,
}

enum Link {
    Mqtt(Arc<MqttConnection>),
    Shared(Arc<Mutex<LinkStatus>>),
}

impl Link {
    fn status(&self) -> LinkStatus {
        match self {
            Link::Mqtt(c) => c.status(),
            Link::Shared(s) => s.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        }
    }
}

struct Consumer {
    fingerprint: String,
    stop: watch::Sender<bool>,
    stats: Arc<Stats>,
    /// Messages the queue between the broker and the store dropped.
    queue_dropped: Arc<AtomicU64>,
    link: Link,
    since: DateTime<Utc>,
}

impl Drop for Consumer {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

impl Consumer {
    fn status(&self) -> SourceStatus {
        let l = self.link.status();
        SourceStatus {
            state: if l.connected {
                "running"
            } else if l.last_error.is_some() {
                "error"
            } else {
                "connecting"
            }
            .into(),
            last_error: l.last_error,
            connects: l.connects,
            received: self.stats.received.load(Ordering::Relaxed),
            dropped: self.stats.dropped.load(Ordering::Relaxed)
                + self.queue_dropped.load(Ordering::Relaxed),
            last_message_at: *self.stats.last_at.lock().unwrap_or_else(|e| e.into_inner()),
            since: Some(self.since),
            stored: 0,
        }
    }
}

/// A received message as stored: bounded ([`Received::bound`], already done before queueing), repeated
/// headers joined.
pub fn convert(svc: &CaptureService, src: &CaptureSource, mut r: Received) -> CapturedMessage {
    r.bound(svc.config.max_message_bytes);
    let size = r.size;
    let truncated = r.truncated;
    let body = r.payload;
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in r.headers.into_iter() {
        headers
            .entry(k)
            .and_modify(|e| {
                e.push_str(", ");
                e.push_str(&v);
            })
            .or_insert(v);
    }
    let content_type = r.content_type.or_else(|| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.clone())
    });
    CapturedMessage {
        id: Uuid::now_v7(),
        space: src.space.clone(),
        source_id: src.id,
        source: src.name.clone(),
        protocol: src.protocol,
        topic: r.topic,
        exchange: r.exchange,
        key: r
            .key
            .map(|k| text_or_hex_capped(&k, 2 * MAX_CAPTURE_NAME_BYTES)),
        headers,
        properties: r.properties,
        content_type,
        size: size as u64,
        truncated,
        preview: super::preview(&body),
        received_at: Utc::now(),
        body: Bytes::from(body),
    }
}

async fn store_loop(
    svc: Arc<CaptureService>,
    src: CaptureSource,
    mut rx: CaptureQueue,
    stats: Arc<Stats>,
    mut stopped: watch::Receiver<bool>,
) {
    let limits = InsertLimits {
        keep: src
            .max_messages
            .unwrap_or(svc.config.max_messages)
            .min(svc.config.max_messages),
        area_bytes: svc.config.max_space_bytes,
        total_bytes: svc.config.max_bytes,
    };
    let protocol = src.protocol.as_str();
    let mut batch = Vec::with_capacity(200);
    loop {
        batch.clear();
        let n = tokio::select! {
            _ = stopped.changed() => break,
            n = rx.recv_many(&mut batch, 200) => n,
        };
        if n == 0 {
            break;
        }
        let msgs: Vec<CapturedMessage> = batch.drain(..).map(|r| convert(&svc, &src, r)).collect();
        let truncated = msgs.iter().filter(|m| m.truncated).count() as u64;
        match svc.store.insert(&msgs, &limits).await {
            Ok(evicted) => {
                if evicted > 0 {
                    metrics::counter!("fastmock_capture_messages_total", "protocol" => protocol, "result" => "evicted")
                        .increment(evicted);
                }
                metrics::counter!("fastmock_capture_messages_total", "protocol" => protocol, "result" => "stored")
                    .increment(msgs.len() as u64);
                if truncated > 0 {
                    metrics::counter!("fastmock_capture_messages_total", "protocol" => protocol, "result" => "truncated")
                        .increment(truncated);
                }
                stats
                    .received
                    .fetch_add(msgs.len() as u64, Ordering::Relaxed);
                *stats.last_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Utc::now());
                svc.notify();
            }
            Err(e) => {
                metrics::counter!("fastmock_store_errors_total", "store" => "capture").increment(1);
                metrics::counter!("fastmock_capture_messages_total", "protocol" => protocol, "result" => "dropped")
                    .increment(msgs.len() as u64);
                stats
                    .dropped
                    .fetch_add(msgs.len() as u64, Ordering::Relaxed);
                tracing::error!(source = %src.name, error = %format!("{e:#}"), messages = msgs.len(), "storing captured messages failed");
            }
        }
    }
}

/// Reads Kafka topics from `start` (after a reconnection: from the latest offsets), without a consumer
/// group, until `stop` changes.
async fn kafka_consume(
    settings: fastmock_kafka::KafkaSettings,
    src: CaptureSource,
    max_bytes: usize,
    tx: CaptureSink,
    status: Arc<Mutex<LinkStatus>>,
    mut stop: watch::Receiver<bool>,
) {
    let mut backoff = Backoff::default();
    let mut start = match src.start_from.unwrap_or_default() {
        StartFrom::Latest => fastmock_kafka::StartFrom::Latest,
        StartFrom::Earliest => fastmock_kafka::StartFrom::Earliest,
    };
    let record_bytes = i32::try_from(max_bytes.max(64 * 1024)).unwrap_or(i32::MAX);
    loop {
        let r: anyhow::Result<()> = async {
            let client = fastmock_kafka::KafkaPublisher::connect(&settings).await?;
            let mut cursors = Vec::new();
            for t in &src.topics {
                cursors.push(client.cursor(t, start).await?);
            }
            start = fastmock_kafka::StartFrom::Latest;
            {
                let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
                s.connected = true;
                s.last_error = None;
                s.connects += 1;
            }
            backoff.reset();
            tracing::info!(source = %src.name, topics = ?src.topics, "Kafka capture subscribed");
            let wait = Duration::from_millis((1000 / cursors.len().max(1) as u64).max(100));
            loop {
                for (i, c) in cursors.iter_mut().enumerate() {
                    if *stop.borrow() {
                        return Ok(());
                    }
                    let records = client.consume(c, 500, record_bytes, wait).await?;
                    for rec in records {
                        let mut properties = serde_json::Map::new();
                        properties.insert("partition".into(), json!(rec.partition));
                        properties.insert("offset".into(), json!(rec.offset));
                        properties.insert("timestamp".into(), json!(rec.timestamp.to_rfc3339()));
                        let m = Received {
                            topic: src.topics[i].clone(),
                            key: rec.key,
                            payload: rec.value.unwrap_or_default(),
                            headers: rec
                                .headers
                                .into_iter()
                                .take(fastmock_brokers::MAX_CAPTURE_HEADERS)
                                .map(|(k, v)| (k, text_or_hex_capped(&v, MAX_CAPTURE_VALUE_BYTES)))
                                .collect(),
                            properties,
                            ..Default::default()
                        };
                        if tx.offer(m) == Offer::Closed {
                            return Ok(());
                        }
                    }
                }
            }
        }
        .await;
        let was = {
            let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
            let was = s.connected;
            s.connected = false;
            if let Err(e) = &r {
                s.last_error = Some(fastmock_brokers::public_message(e));
            }
            was
        };
        match r {
            Ok(()) => return,
            Err(e) => {
                if was {
                    backoff.reset();
                }
                let delay = backoff.next_delay();
                tracing::warn!(source = %src.name, error = %format!("{e:#}"), retry_in = ?delay, "Kafka capture interrupted");
                tokio::select! {
                    _ = stop.changed() => return,
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

/// Why a source cannot run here, if it cannot.
fn blocker(app: &AppState, svc: &CaptureService, s: &CaptureSource) -> Option<String> {
    if let Some(setting) = super::missing_setting(&app.cfg, s.protocol) {
        return Some(format!("{setting} is not configured"));
    }
    if let Err(e) = super::allowed_in(app, &svc.config, s) {
        return Some(e);
    }
    s.validate().err()
}

fn start(app: &AppState, svc: &Arc<CaptureService>, s: &CaptureSource) -> anyhow::Result<Consumer> {
    let (tx, rx) = capture_channel(
        s.protocol.as_str(),
        queue_limits(svc.config.max_message_bytes),
    );
    let queue_dropped = tx.dropped_counter();
    let (stop, stopped) = watch::channel(false);
    let stats = Arc::new(Stats::default());
    let link =
        match s.protocol {
            MessageProtocol::Mqtt => {
                let mut settings = app.cfg.mqtt.clone().ok_or_else(|| {
                    fastmock_brokers::public("FASTMOCK_MQTT_URL is not configured")
                })?;
                settings.max_packet_bytes =
                    fastmock_brokers::mqtt::capture_packet_limit(svc.config.max_message_bytes);
                let filters = s
                    .topics
                    .iter()
                    .map(|t| (t.clone(), s.qos.unwrap_or(1)))
                    .collect();
                Link::Mqtt(MqttConnection::start(
                    &settings,
                    settings.client_id("cap"),
                    filters,
                    Some(tx),
                )?)
            }
            MessageProtocol::Amqp => {
                let settings = app.cfg.amqp.clone().ok_or_else(|| {
                    fastmock_brokers::public("FASTMOCK_AMQP_URL is not configured")
                })?;
                let status = Arc::new(Mutex::new(LinkStatus::default()));
                tokio::spawn(fastmock_brokers::amqp::consume(
                    settings,
                    AmqpCapture {
                        exchange: s.exchange.clone().unwrap_or_default(),
                        binding_keys: s.binding_keys.clone(),
                        max_queue_length: 10_000,
                        label: s.name.clone(),
                    },
                    tx,
                    status.clone(),
                    stopped.clone(),
                ));
                Link::Shared(status)
            }
            MessageProtocol::Kafka => {
                let mut settings = app.kafka.settings().cloned().ok_or_else(|| {
                    fastmock_brokers::public("FASTMOCK_KAFKA_BOOTSTRAP is not configured")
                })?;
                settings.client_id = "fastmock-capture".into();
                settings.create_topics = None;
                let status = Arc::new(Mutex::new(LinkStatus::default()));
                tokio::spawn(kafka_consume(
                    settings,
                    s.clone(),
                    svc.config.max_message_bytes,
                    tx,
                    status.clone(),
                    stopped.clone(),
                ));
                Link::Shared(status)
            }
        };
    tokio::spawn(store_loop(
        svc.clone(),
        s.clone(),
        rx,
        stats.clone(),
        stopped,
    ));
    Ok(Consumer {
        fingerprint: s.fingerprint(),
        stop,
        stats,
        queue_dropped,
        link,
        since: Utc::now(),
    })
}

pub async fn run(app: AppState, shutdown: watch::Receiver<bool>) {
    run_with(app, shutdown, RuntimeOptions::default()).await
}

pub async fn run_with(app: AppState, mut shutdown: watch::Receiver<bool>, opts: RuntimeOptions) {
    let Some(svc) = app.capture.clone() else {
        return;
    };
    let mut leader = Leader::with_key(LEASE_KEY, opts.lease_ttl);
    let mut last_renew: Option<Instant> = None;
    let mut last_reload: Option<Instant> = None;
    let mut running: HashMap<Uuid, Consumer> = HashMap::new();
    metrics::gauge!("fastmock_capture_leader").set(0.0);
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
                tracing::info!(leader = now, "message capture leadership changed");
                last_reload = None;
            }
            metrics::gauge!("fastmock_capture_leader").set(if now { 1.0 } else { 0.0 });
        }
        if !leader.is_leader() {
            if !running.is_empty() {
                running.clear();
                metrics::gauge!("fastmock_capture_sources_active").set(0.0);
            }
            continue;
        }
        if last_reload.is_some_and(|t| t.elapsed() < opts.reload_every) {
            continue;
        }
        last_reload = Some(Instant::now());
        let sources = match load_sources(&app).await {
            Ok(s) => s,
            Err(e) => {
                metrics::counter!("fastmock_store_errors_total", "store" => "capture_sources")
                    .increment(1);
                tracing::warn!(error = %e, "loading capture sources failed; keeping the consumers");
                continue;
            }
        };
        let mut statuses: Vec<(Uuid, SourceStatus)> = Vec::new();
        let mut wanted: HashMap<Uuid, &CaptureSource> = HashMap::new();
        for s in &sources {
            if !s.enabled {
                statuses.push((
                    s.id,
                    SourceStatus {
                        state: "disabled".into(),
                        ..Default::default()
                    },
                ));
                continue;
            }
            if let Some(why) = blocker(&app, &svc, s) {
                statuses.push((
                    s.id,
                    SourceStatus {
                        state: "error".into(),
                        last_error: Some(why),
                        ..Default::default()
                    },
                ));
                continue;
            }
            wanted.insert(s.id, s);
        }
        running.retain(|id, c| {
            wanted
                .get(id)
                .is_some_and(|s| s.fingerprint() == c.fingerprint)
        });
        for (id, s) in &wanted {
            if running.contains_key(id) {
                continue;
            }
            match start(&app, &svc, s) {
                Ok(c) => {
                    tracing::info!(source = %s.name, protocol = %s.protocol, "capture consumer started");
                    running.insert(*id, c);
                }
                Err(e) => {
                    tracing::warn!(source = %s.name, error = %format!("{e:#}"), "capture consumer could not start");
                    statuses.push((
                        *id,
                        SourceStatus {
                            state: "error".into(),
                            last_error: Some(fastmock_brokers::public_message(&e)),
                            ..Default::default()
                        },
                    ))
                }
            }
        }
        metrics::gauge!("fastmock_capture_sources_active").set(running.len() as f64);
        statuses.extend(running.iter().map(|(id, c)| (*id, c.status())));
        let ttl = Duration::from_secs((opts.reload_every.as_secs() * 4).max(20));
        for (id, st) in statuses {
            if let Ok(v) = serde_json::to_string(&st)
                && let Err(e) = app
                    .stores
                    .hot
                    .set(&format!("{STATUS_PREFIX}{id}"), &v, Some(ttl))
                    .await
            {
                tracing::warn!(error = %e, "writing capture status failed");
                break;
            }
        }
    }
    running.clear();
    leader.release(app.stores.hot.as_ref()).await;
    metrics::gauge!("fastmock_capture_leader").set(0.0);
    metrics::gauge!("fastmock_capture_sources_active").set(0.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker_settings::CaptureConfig;

    #[test]
    fn received_messages_are_bounded() {
        let svc = CaptureService {
            config: CaptureConfig {
                max_message_bytes: 4,
                ..Default::default()
            },
            store: Arc::new(fastmock_core::capture::MemoryCaptureStore::default()),
            stored: watch::channel(0).0,
            poll: None,
        };
        let src: CaptureSource = serde_json::from_value(
            json!({"name": "o", "space": "billing", "protocol": "kafka", "topics": ["t"]}),
        )
        .unwrap();
        let m = convert(
            &svc,
            &src,
            Received {
                topic: "t".into(),
                key: Some(vec![0xff]),
                payload: b"abcdef".to_vec(),
                headers: vec![
                    ("a".into(), "1".into()),
                    ("a".into(), "2".into()),
                    ("Content-Type".into(), "text/plain".into()),
                ],
                ..Default::default()
            },
        );
        assert_eq!(m.body.as_ref(), b"abcd");
        assert!(m.truncated);
        assert_eq!(m.size, 6);
        assert_eq!(m.headers["a"], "1, 2");
        assert_eq!(m.content_type.as_deref(), Some("text/plain"));
        assert_eq!(m.key.as_deref(), Some("ff"));
        assert_eq!(m.space.as_deref(), Some("billing"));
        assert_eq!(m.preview.as_deref(), Some("abcd"));
        let repeated = convert(
            &svc,
            &src,
            Received {
                topic: "t".into(),
                headers: (0..1000)
                    .map(|_| ("a".to_string(), "v".repeat(8192)))
                    .collect(),
                ..Default::default()
            },
        );
        assert!(repeated.headers["a"].len() <= MAX_CAPTURE_META_BYTES);
        assert!(repeated.truncated);
        assert!(repeated.stored_bytes() < 2 * MAX_CAPTURE_META_BYTES as u64);
    }

    #[test]
    fn mqtt_capture_connections_take_smaller_packets() {
        let l = queue_limits(1024 * 1024);
        assert_eq!(l.max_payload, 1024 * 1024);
        assert!(l.max_bytes >= 2 * 1024 * 1024);
        assert_eq!(
            fastmock_brokers::mqtt::capture_packet_limit(1024 * 1024),
            2 * 1024 * 1024
        );
    }
}
