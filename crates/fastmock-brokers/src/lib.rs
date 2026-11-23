//! MQTT and AMQP 0-9-1 clients for FastMock: publishing from stubs and async mocks, and the subscriptions
//! of message capture.
//!
//! - [`mqtt`]: one MQTT 3.1.1 or 5 connection ([`mqtt::MqttConnection`]) that publishes with
//!   acknowledgements and receives the messages of its subscriptions; it reconnects with a growing pause.
//! - [`amqp`]: a lazily connected publisher with publisher confirms ([`amqp::AmqpPublisher`]) and a capture
//!   consumer on an exclusive, auto-delete queue ([`amqp::consume`]).
//!
//! TLS uses rustls with the ring provider: the Mozilla roots plus an optional CA bundle, and an optional
//! client certificate (the same files and rules as the Kafka client, [`fastmock_kafka::TlsFiles`]).

pub mod amqp;
pub mod mqtt;

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;

pub use fastmock_kafka::{ClientCertFiles, TlsFiles};

const REDACTED: &str = "<redacted>";

/// A user name and password. `Debug` never prints the password.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &REDACTED)
            .finish()
    }
}

/// Whether `name` is allowed by an allow-list of exact names and `prefix*` entries; `None` allows all.
pub fn name_allowed(allowed: Option<&[String]>, name: &str) -> bool {
    allowed.is_none_or(|l| {
        l.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => name == p,
        })
    })
}

/// The pause before the next connection attempt: doubles from 200 ms up to 30 s.
#[derive(Clone, Debug)]
pub struct Backoff {
    next: Duration,
}

pub const MIN_BACKOFF: Duration = Duration::from_millis(200);
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A connection that lasted at least this long resets the pause; one that broke sooner keeps it growing.
pub const STABLE_CONNECTION: Duration = Duration::from_secs(10);

impl Default for Backoff {
    fn default() -> Self {
        Self { next: MIN_BACKOFF }
    }
}

impl Backoff {
    /// The pause to wait now; the following one is twice as long, at most [`MAX_BACKOFF`].
    pub fn next_delay(&mut self) -> Duration {
        let d = self.next;
        self.next = (self.next * 2).min(MAX_BACKOFF);
        d
    }

    pub fn reset(&mut self) {
        self.next = MIN_BACKOFF;
    }

    /// After a connection broke: resets the pause only when the connection had lasted at least
    /// [`STABLE_CONNECTION`], so a broker that accepts and then drops every connection is not retried
    /// at the shortest pause forever.
    pub fn after_disconnect(&mut self, uptime: Option<Duration>) {
        if uptime.is_some_and(|u| u >= STABLE_CONNECTION) {
            self.reset();
        }
    }
}

/// The state of a connection, for the capture source status and the logs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkStatus {
    pub connected: bool,
    /// The last connection or subscription error, cleared by the next successful connection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Successful connections since the start.
    pub connects: u64,
}

/// A message received by a capture subscription, before it is stored.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Received {
    /// Bytes of the payload as received, also after [`Received::bound`] cut it.
    pub size: usize,
    /// [`Received::bound`] cut the payload, the topic, the key, headers or properties.
    pub truncated: bool,
    /// The MQTT topic, the AMQP routing key or the Kafka topic.
    pub topic: String,
    /// AMQP: the exchange the message was published to.
    pub exchange: Option<String>,
    /// Kafka: the record key.
    pub key: Option<Vec<u8>>,
    pub payload: Vec<u8>,
    /// MQTT 5 user properties, AMQP headers (as text) or Kafka headers, in order.
    pub headers: Vec<(String, String)>,
    /// Protocol properties: MQTT `qos`, `retain`, `contentType`, `correlationData`, `responseTopic`,
    /// `messageExpiryInterval`; AMQP `contentType`, `messageId`, `correlationId`, `replyTo`, ...
    pub properties: serde_json::Map<String, serde_json::Value>,
    pub content_type: Option<String>,
}

/// Lowercase hex of bytes that are not UTF-8, the text otherwise.
pub fn text_or_hex(b: &[u8]) -> String {
    text_or_hex_capped(b, usize::MAX)
}

/// [`text_or_hex`] of at most `max` bytes of output: longer text is cut on a character boundary, and only
/// the bytes whose hex fits are converted.
pub fn text_or_hex_capped(b: &[u8], max: usize) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => cut(s, max).to_string(),
        Err(_) => b.iter().take(max / 2).map(|x| format!("{x:02x}")).collect(),
    }
}

/// The longest prefix of `s` of at most `max` bytes that ends on a character boundary.
pub fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Bytes of topic, exchange, key, content type, headers and properties one captured message keeps
/// together; what does not fit is dropped and the message is marked `truncated`.
pub const MAX_CAPTURE_META_BYTES: usize = 64 * 1024;
/// Headers (MQTT user properties, AMQP and Kafka headers) one captured message keeps.
pub const MAX_CAPTURE_HEADERS: usize = 100;
/// Bytes of one header value or text property a captured message keeps.
pub const MAX_CAPTURE_VALUE_BYTES: usize = 8192;
/// Bytes of a topic, routing key or record key a captured message keeps.
pub const MAX_CAPTURE_NAME_BYTES: usize = 4096;
/// Bytes counted for a message besides its strings, for the byte bounds of queues.
const RECEIVED_OVERHEAD: usize = 256;

fn value_bytes(v: &Value) -> usize {
    match v {
        Value::String(s) => s.len(),
        Value::Array(a) => a.iter().map(value_bytes).sum::<usize>() + 2,
        Value::Object(o) => o.iter().map(|(k, v)| k.len() + value_bytes(v) + 4).sum(),
        _ => 8,
    }
}

impl Received {
    /// Cuts the payload to `max_payload` bytes and keeps at most [`MAX_CAPTURE_META_BYTES`] of everything
    /// else, before the message is queued for storing.
    pub fn bound(&mut self, max_payload: usize) {
        self.size = self.size.max(self.payload.len());
        if self.payload.len() > max_payload {
            self.payload.truncate(max_payload);
            self.payload.shrink_to_fit();
            self.truncated = true;
        }
        let cut_string = |s: &mut String, max: usize| {
            if s.len() > max {
                let n = cut(s, max).len();
                s.truncate(n);
                true
            } else {
                false
            }
        };
        let mut cut_any = cut_string(&mut self.topic, MAX_CAPTURE_NAME_BYTES);
        if let Some(e) = &mut self.exchange {
            cut_any |= cut_string(e, MAX_CAPTURE_NAME_BYTES);
        }
        if let Some(ct) = &mut self.content_type {
            cut_any |= cut_string(ct, MAX_CAPTURE_VALUE_BYTES);
        }
        if let Some(k) = &mut self.key
            && k.len() > MAX_CAPTURE_NAME_BYTES
        {
            k.truncate(MAX_CAPTURE_NAME_BYTES);
            cut_any = true;
        }
        let mut left = MAX_CAPTURE_META_BYTES.saturating_sub(
            self.topic.len()
                + self.exchange.as_ref().map_or(0, String::len)
                + self.content_type.as_ref().map_or(0, String::len)
                + self.key.as_ref().map_or(0, Vec::len),
        );
        let before = self.headers.len();
        let mut kept = Vec::with_capacity(before.min(MAX_CAPTURE_HEADERS));
        for (mut k, mut v) in std::mem::take(&mut self.headers) {
            if kept.len() >= MAX_CAPTURE_HEADERS {
                cut_any = true;
                break;
            }
            cut_any |= cut_string(&mut k, MAX_CAPTURE_NAME_BYTES);
            cut_any |= cut_string(&mut v, MAX_CAPTURE_VALUE_BYTES);
            let n = k.len() + v.len();
            if n > left {
                cut_any = true;
                break;
            }
            left -= n;
            kept.push((k, v));
        }
        self.headers = kept;
        let mut props = serde_json::Map::new();
        for (k, mut v) in std::mem::take(&mut self.properties) {
            if let Value::String(s) = &mut v {
                cut_any |= cut_string(s, MAX_CAPTURE_VALUE_BYTES);
            }
            let n = k.len() + value_bytes(&v);
            if n > left {
                cut_any = true;
                continue;
            }
            left -= n;
            props.insert(k, v);
        }
        self.properties = props;
        self.truncated |= cut_any;
    }

    /// Approximate bytes the message holds, for the byte bounds of capture queues.
    pub fn weight(&self) -> usize {
        RECEIVED_OVERHEAD
            + self.payload.len()
            + self.topic.len()
            + self.exchange.as_ref().map_or(0, String::len)
            + self.key.as_ref().map_or(0, Vec::len)
            + self.content_type.as_ref().map_or(0, String::len)
            + self
                .headers
                .iter()
                .map(|(k, v)| k.len() + v.len())
                .sum::<usize>()
            + self
                .properties
                .iter()
                .map(|(k, v)| k.len() + value_bytes(v) + 4)
                .sum::<usize>()
    }
}

/// Bounds of a capture queue between a subscription and the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureLimits {
    /// Messages queued at most.
    pub max_messages: usize,
    /// Bytes ([`Received::weight`]) queued at most.
    pub max_bytes: usize,
    /// Payload bytes kept per message; [`Received::bound`] cuts the rest before queueing.
    pub max_payload: usize,
}

impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            max_messages: 4096,
            max_bytes: 16 * 1024 * 1024,
            max_payload: 1024 * 1024,
        }
    }
}

/// The sending side of a capture queue: bounds each message, then queues it unless the queue holds
/// [`CaptureLimits::max_messages`] messages or [`CaptureLimits::max_bytes`] bytes, in which case the
/// message is dropped and counted.
#[derive(Clone)]
pub struct CaptureSink {
    tx: mpsc::Sender<Received>,
    queued: Arc<AtomicUsize>,
    dropped: Arc<AtomicU64>,
    limits: CaptureLimits,
    protocol: &'static str,
}

/// The receiving side of a capture queue.
pub struct CaptureQueue {
    rx: mpsc::Receiver<Received>,
    queued: Arc<AtomicUsize>,
}

/// What became of an offered message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offer {
    Queued,
    /// The queue was full (by count or bytes); the message is counted as dropped.
    Dropped,
    /// The receiving side is gone.
    Closed,
}

/// A capture queue for one subscription; `protocol` labels the dropped-message metric.
pub fn capture_channel(
    protocol: &'static str,
    limits: CaptureLimits,
) -> (CaptureSink, CaptureQueue) {
    let (tx, rx) = mpsc::channel(limits.max_messages.max(1));
    let queued = Arc::new(AtomicUsize::new(0));
    (
        CaptureSink {
            tx,
            queued: queued.clone(),
            dropped: Arc::new(AtomicU64::new(0)),
            limits,
            protocol,
        },
        CaptureQueue { rx, queued },
    )
}

impl CaptureSink {
    pub fn offer(&self, mut r: Received) -> Offer {
        r.bound(self.limits.max_payload);
        let w = r.weight();
        let before = self.queued.fetch_add(w, Ordering::SeqCst);
        let result = if before + w > self.limits.max_bytes {
            Offer::Dropped
        } else {
            match self.tx.try_send(r) {
                Ok(()) => return Offer::Queued,
                Err(mpsc::error::TrySendError::Full(_)) => Offer::Dropped,
                Err(mpsc::error::TrySendError::Closed(_)) => Offer::Closed,
            }
        };
        self.queued.fetch_sub(w, Ordering::SeqCst);
        if result == Offer::Dropped {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("fastmock_capture_messages_total", "protocol" => self.protocol, "result" => "dropped").increment(1);
        }
        result
    }

    /// Messages dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Bytes queued now.
    pub fn queued_bytes(&self) -> usize {
        self.queued.load(Ordering::SeqCst)
    }

    /// The counter of dropped messages, shared with the queue's owner.
    pub fn dropped_counter(&self) -> Arc<AtomicU64> {
        self.dropped.clone()
    }
}

impl CaptureQueue {
    fn release(&self, msgs: &[Received]) {
        let w: usize = msgs.iter().map(Received::weight).sum();
        self.queued.fetch_sub(w, Ordering::SeqCst);
    }

    pub async fn recv(&mut self) -> Option<Received> {
        let r = self.rx.recv().await?;
        self.release(std::slice::from_ref(&r));
        Some(r)
    }

    /// Receives up to `limit` messages into `buf` (appended); 0 when every sink is gone.
    pub async fn recv_many(&mut self, buf: &mut Vec<Received>, limit: usize) -> usize {
        let start = buf.len();
        let n = self.rx.recv_many(buf, limit).await;
        self.release(&buf[start..]);
        n
    }
}

/// An error whose text is safe for API callers: it names no host, port, file, certificate detail or
/// broker response.
#[derive(Debug)]
pub struct PublicError(pub String);

impl fmt::Display for PublicError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PublicError {}

/// A [`PublicError`] as an `anyhow::Error`.
pub fn public(msg: impl Into<String>) -> anyhow::Error {
    PublicError(msg.into()).into()
}

/// What an API caller is told about a broker or schema registry error: the text of a [`PublicError`]
/// anywhere in the chain, otherwise a category such as `broker connection refused` or `TLS handshake
/// failed`. The full error belongs in the server log only.
pub fn public_message(e: &anyhow::Error) -> String {
    if let Some(p) = e.downcast_ref::<PublicError>() {
        return p.0.clone();
    }
    if let Some(p) = e.chain().find_map(|c| c.downcast_ref::<PublicError>()) {
        return p.0.clone();
    }
    error_category(&format!("{e:#}")).to_string()
}

/// The category of an error text (see [`public_message`]).
pub fn error_category(text: &str) -> String {
    let t = text.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| t.contains(w));
    if let Some(i) = t.find("schema registry returned ") {
        let code: String = t[i + 25..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !code.is_empty() {
            return format!("schema registry returned {code}");
        }
    }
    if has(&["schema registry"]) {
        return "schema registry unavailable".into();
    }
    if has(&["avro"]) {
        return "the value does not match the Avro schema".into();
    }
    if has(&["refused the subscription"]) {
        return "the broker refused the subscription".into();
    }
    if has(&["fatal alert"])
        && has(&[
            "unknownca",
            "badcertificate",
            "certificaterequired",
            "certificateunknown",
            "certificaterevoked",
            "certificateexpired",
            "unsupportedcertificate",
            "accessdenied",
        ])
    {
        return "TLS handshake failed: the broker refused FastMock's client certificate".into();
    }
    if has(&["certificate", "tls", "handshake", "ssl", "fatal alert"]) {
        return "TLS handshake failed".into();
    }
    if has(&["connection refused"]) {
        return "broker connection refused".into();
    }
    if has(&[
        "failed to lookup",
        "name or service not known",
        "nodename nor servname",
        "no such host",
        "dns",
    ]) {
        return "broker host not found".into();
    }
    if has(&["access_refused", "access refused"]) && !has(&["login"]) {
        return "the broker refused access (check the permissions of FastMock's user)".into();
    }
    if has(&["kafka topic"]) && has(&["does not exist"]) {
        return "the Kafka topic does not exist".into();
    }
    if has(&["unknowntopicorpartition", "unknown topic"]) {
        return "the Kafka topic does not exist".into();
    }
    if has(&[
        "access_refused",
        "not authorized",
        "notauthorized",
        "bad user name",
        "badusernamepassword",
        "authentication",
        "sasl",
        "credentials",
    ]) {
        return "the broker refused the credentials".into();
    }
    if has(&["not_found"]) && has(&["exchange"]) {
        return "the exchange does not exist".into();
    }
    if has(&["unroutable"]) {
        return "the message was returned as unroutable: no queue is bound for it".into();
    }
    if has(&["nack", "refused the message", "rejected"]) {
        return "the broker rejected the message".into();
    }
    if has(&[
        "timed out",
        "timeout",
        "did not acknowledge",
        "did not confirm",
    ]) {
        return "the broker did not answer in time".into();
    }
    if has(&["unavailable", "next connection attempt"]) {
        return "broker unavailable; retrying".into();
    }
    if has(&[
        "connection reset",
        "broken pipe",
        "connection was closed",
        "connection closed",
        "connection failed",
        "connection lost",
        "eof",
    ]) {
        return "broker connection lost".into();
    }
    "broker error (details in the server log)".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_lists_take_names_and_prefixes() {
        let l = vec!["orders/*".to_string(), "audit".to_string()];
        assert!(name_allowed(Some(&l), "orders/1/created"));
        assert!(name_allowed(Some(&l), "audit"));
        assert!(!name_allowed(Some(&l), "audit/x"));
        assert!(!name_allowed(Some(&[]), "x"));
        assert!(name_allowed(None, "anything"));
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let mut b = Backoff::default();
        assert_eq!(b.next_delay(), Duration::from_millis(200));
        assert_eq!(b.next_delay(), Duration::from_millis(400));
        for _ in 0..20 {
            b.next_delay();
        }
        assert_eq!(b.next_delay(), MAX_BACKOFF);
        b.reset();
        assert_eq!(b.next_delay(), MIN_BACKOFF);
    }

    #[test]
    fn a_connection_that_breaks_at_once_keeps_the_pause_growing() {
        let mut b = Backoff::default();
        b.next_delay();
        b.next_delay();
        b.after_disconnect(Some(Duration::from_millis(5)));
        assert_eq!(b.next_delay(), Duration::from_millis(800));
        b.after_disconnect(None);
        assert_eq!(b.next_delay(), Duration::from_millis(1600));
        b.after_disconnect(Some(STABLE_CONNECTION));
        assert_eq!(b.next_delay(), MIN_BACKOFF);
    }

    #[test]
    fn received_messages_are_bounded_before_queueing() {
        let mut r = Received {
            topic: "t".repeat(10_000),
            key: Some(vec![0xff; 10_000]),
            payload: vec![b'x'; 1000],
            headers: (0..300)
                .map(|i| (format!("h{i}"), "v".repeat(20_000)))
                .collect(),
            ..Default::default()
        };
        r.properties
            .insert("correlationData".into(), Value::String("c".repeat(200_000)));
        r.bound(100);
        assert_eq!(r.size, 1000);
        assert_eq!(r.payload.len(), 100);
        assert!(r.truncated);
        assert_eq!(r.topic.len(), MAX_CAPTURE_NAME_BYTES);
        assert_eq!(r.key.as_ref().unwrap().len(), MAX_CAPTURE_NAME_BYTES);
        assert!(r.headers.len() < MAX_CAPTURE_HEADERS);
        assert!(
            r.headers
                .iter()
                .all(|(_, v)| v.len() <= MAX_CAPTURE_VALUE_BYTES)
        );
        assert!(r.weight() <= 100 + MAX_CAPTURE_META_BYTES + 1024);
        let mut small = Received {
            topic: "a/b".into(),
            payload: b"ok".to_vec(),
            headers: vec![("a".into(), "1".into()), ("a".into(), "2".into())],
            ..Default::default()
        };
        small.bound(100);
        assert!(!small.truncated);
        assert_eq!(small.size, 2);
        assert_eq!(small.headers.len(), 2);
        assert_eq!(text_or_hex_capped(&[0xff; 100], 10), "ffffffffff");
        assert_eq!(text_or_hex_capped("€€".as_bytes(), 4), "€");
    }

    #[tokio::test]
    async fn capture_queues_are_bounded_by_bytes() {
        let (sink, mut queue) = capture_channel(
            "mqtt",
            CaptureLimits {
                max_messages: 100,
                max_bytes: 4096,
                max_payload: 1024,
            },
        );
        let msg = || Received {
            topic: "t".into(),
            payload: vec![0; 10_000],
            ..Default::default()
        };
        assert_eq!(sink.offer(msg()), Offer::Queued);
        assert_eq!(sink.offer(msg()), Offer::Queued);
        assert_eq!(sink.offer(msg()), Offer::Queued);
        assert_eq!(sink.offer(msg()), Offer::Dropped);
        assert_eq!(sink.dropped(), 1);
        assert!(sink.queued_bytes() <= 4096);
        let first = queue.recv().await.unwrap();
        assert_eq!(first.payload.len(), 1024);
        assert_eq!(first.size, 10_000);
        assert!(first.truncated);
        assert_eq!(sink.offer(msg()), Offer::Queued);
        let mut buf = Vec::new();
        assert_eq!(queue.recv_many(&mut buf, 10).await, 3);
        assert_eq!(sink.queued_bytes(), 0);
        drop(queue);
        assert_eq!(sink.offer(msg()), Offer::Closed);
    }

    #[test]
    fn errors_for_api_callers_name_no_endpoint() {
        let cases = [
            (
                "connecting to amqp://rabbit.example.com:5672/%2F: IO error: Connection refused (os error 111)",
                "broker connection refused",
            ),
            (
                "the MQTT connection failed: TLS: invalid peer certificate: UnknownIssuer",
                "TLS handshake failed",
            ),
            (
                "connecting to amqps://rabbit.example.com:5671/%2F: IO error: received fatal alert: UnknownCA",
                "TLS handshake failed: the broker refused FastMock's client certificate",
            ),
            (
                "connecting to amqps://rabbit.example.com:5671/%2F: IO error: received fatal alert: HandshakeFailure",
                "TLS handshake failed",
            ),
            (
                "connecting to amqps://198.51.100.7:5671/%2F: protocol error: AMQP soft error: ACCESS-REFUSED: ACCESS_REFUSED - Login was refused using authentication mechanism EXTERNAL. For details see the broker logfile.",
                "the broker refused the credentials",
            ),
            (
                "schema registry returned 404 Not Found for schema id 7: {\"error_code\":40403,\"message\":\"secret detail\"}",
                "schema registry returned 404",
            ),
            (
                "connecting to amqp://198.51.100.7:5672/orders: ACCESS_REFUSED - Login was refused",
                "the broker refused the credentials",
            ),
            (
                "failed to lookup address information: Name or service not known",
                "broker host not found",
            ),
            (
                "something else at 198.51.100.7:1883",
                "broker error (details in the server log)",
            ),
        ];
        for (text, want) in cases {
            let got = public_message(&anyhow::anyhow!("{text}"));
            assert_eq!(got, want, "{text}");
            assert!(!got.contains("198.51.100.7") && !got.contains("example.com"));
        }
        let e = anyhow::Error::from(PublicError(
            "topic is not in FASTMOCK_MQTT_ALLOWED_TOPICS".into(),
        ))
        .context("publishing to mqtt://198.51.100.7:1883");
        assert_eq!(
            public_message(&e),
            "topic is not in FASTMOCK_MQTT_ALLOWED_TOPICS"
        );
        let e = anyhow::anyhow!("NOT_FOUND - no exchange 'billing' in vhost 'private'")
            .context(PublicError("exchange billing does not exist".into()));
        assert_eq!(public_message(&e), "exchange billing does not exist");
    }

    #[test]
    fn passwords_are_not_printed() {
        let c = Credentials {
            username: "u".into(),
            password: "secret".into(),
        };
        assert!(!format!("{c:?}").contains("secret"));
        assert_eq!(text_or_hex(b"ab"), "ab");
        assert_eq!(text_or_hex(&[0xff, 0x01]), "ff01");
    }
}
