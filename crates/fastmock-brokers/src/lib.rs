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
use std::time::Duration;

use serde::Serialize;

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
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
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
