//! One MQTT connection, version 3.1.1 or 5, on rumqttc.
//!
//! [`MqttConnection::start`] spawns the event loop: it connects (clean session), subscribes to the
//! connection's filters after every CONNACK, hands received messages to a channel, and after an error waits
//! a growing pause ([`crate::Backoff`]) before it connects again. Publications wait for the broker's
//! acknowledgement (PUBACK for QoS 1, PUBCOMP for QoS 2, the write for QoS 0); when the connection breaks,
//! every publication in flight fails and nothing is resent, so a message is published at most once by
//! FastMock (counted as failed when its fate is unknown).
//!
//! TLS (`mqtts://`) runs in FastMock, not in rumqttc: the client connects to a Unix socket in a private
//! directory (mode 0700) of this process, and a bridge task wraps each such connection in rustls towards the
//! broker. rumqttc is used without its TLS features.

use std::collections::{HashMap, VecDeque};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use bytes::Bytes;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};

use crate::{Backoff, Credentials, LinkStatus, Received, TlsFiles};

/// Requests a connection may queue before publications are refused.
pub const REQUEST_CAPACITY: usize = 256;
/// Largest MQTT topic in bytes.
pub const MAX_TOPIC_BYTES: usize = 65_535;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MqttVersion {
    V311,
    #[default]
    V5,
}

impl MqttVersion {
    pub fn as_str(self) -> &'static str {
        match self {
            MqttVersion::V311 => "3.1.1",
            MqttVersion::V5 => "5",
        }
    }
}

impl FromStr for MqttVersion {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(
            match s.trim().to_ascii_lowercase().trim_start_matches('v') {
                "5" | "5.0" => MqttVersion::V5,
                "3.1.1" | "311" | "4" => MqttVersion::V311,
                other => bail!("unknown MQTT version `{other}`; use 5 or 3.1.1"),
            },
        )
    }
}

/// Where and how FastMock connects to an MQTT broker (`FASTMOCK_MQTT_*`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MqttSettings {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub version: MqttVersion,
    /// Client ids are `<prefix>-<random>`, unique per connection.
    pub client_id_prefix: String,
    pub credentials: Option<Credentials>,
    pub tls_files: TlsFiles,
    pub keep_alive: Duration,
    /// Largest packet FastMock sends or accepts.
    pub max_packet_bytes: usize,
    /// How long a publication waits for its acknowledgement.
    pub ack_timeout: Duration,
}

impl MqttSettings {
    /// Settings for a broker URL with the defaults for everything else.
    pub fn new(url: &str) -> anyhow::Result<Self> {
        let (host, port, tls) = parse_url(url)?;
        Ok(Self {
            host,
            port,
            tls,
            version: MqttVersion::default(),
            client_id_prefix: "fastmock".into(),
            credentials: None,
            tls_files: TlsFiles::default(),
            keep_alive: Duration::from_secs(30),
            max_packet_bytes: 16 * 1024 * 1024,
            ack_timeout: Duration::from_secs(10),
        })
    }

    /// `mqtt://host:port` or `mqtts://host:port`, never with credentials.
    pub fn endpoint(&self) -> String {
        format!(
            "{}://{}:{}",
            if self.tls { "mqtts" } else { "mqtt" },
            self.host,
            self.port
        )
    }

    /// Checks the settings and loads the TLS files once.
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.tls && self.tls_files != TlsFiles::default() {
            bail!("TLS files are set but the broker URL is not mqtts://");
        }
        if self.client_id_prefix.is_empty() || self.client_id_prefix.len() > 40 {
            bail!("the client id prefix must be 1..40 characters");
        }
        if let Some(c) = &self.credentials
            && c.username.is_empty()
        {
            bail!("the MQTT user name is empty");
        }
        if self.tls {
            fastmock_kafka::client_tls_config(&self.tls_files)?;
        }
        Ok(())
    }

    /// A new client id: the prefix, `-`, a role and 8 random hex digits.
    pub fn client_id(&self, role: &str) -> String {
        format!(
            "{}-{role}-{:08x}",
            self.client_id_prefix,
            rand::random::<u32>()
        )
    }
}

/// Host, port and TLS of `mqtt://`, `mqtts://` (also `tcp://`, `ssl://`, `tls://`) URLs.
pub fn parse_url(url: &str) -> anyhow::Result<(String, u16, bool)> {
    let u = url::Url::parse(url.trim()).with_context(|| format!("`{url}` is not a URL"))?;
    let tls = match u.scheme() {
        "mqtt" | "tcp" => false,
        "mqtts" | "ssl" | "tls" => true,
        other => bail!("unsupported MQTT URL scheme `{other}`; use mqtt:// or mqtts://"),
    };
    if !u.username().is_empty() || u.password().is_some() {
        bail!("the MQTT URL must not contain credentials; use the user name and password settings");
    }
    if !matches!(u.path(), "" | "/") || u.query().is_some() {
        bail!("the MQTT URL takes no path or query");
    }
    let host = u
        .host_str()
        .filter(|h| !h.is_empty())
        .context("the MQTT URL has no host")?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = u.port().unwrap_or(if tls { 8883 } else { 1883 });
    Ok((host, port, tls))
}

/// Whether `t` is a valid topic name to publish to: not empty, no wildcards, no NUL, at most 64 KiB.
pub fn valid_topic(t: &str) -> bool {
    !t.is_empty() && t.len() <= MAX_TOPIC_BYTES && !t.contains(['+', '#', '\0'])
}

/// Whether `f` is a valid topic filter: `+` only as a whole level, `#` only as the whole last level.
pub fn valid_filter(f: &str) -> bool {
    if f.is_empty() || f.len() > MAX_TOPIC_BYTES || f.contains('\0') {
        return false;
    }
    let levels: Vec<&str> = f.split('/').collect();
    levels.iter().enumerate().all(|(i, l)| {
        (!l.contains('+') || *l == "+")
            && (!l.contains('#') || (*l == "#" && i == levels.len() - 1))
    })
}

/// Whether a topic matches a filter (MQTT 4.7); topics starting with `$` match only filters starting with
/// `$`.
pub fn filter_matches(filter: &str, topic: &str) -> bool {
    if topic.starts_with('$') && !filter.starts_with('$') {
        return false;
    }
    let mut f = filter.split('/');
    let mut t = topic.split('/');
    loop {
        match (f.next(), t.next()) {
            (Some("#"), _) => return true,
            (Some("+"), Some(_)) => {}
            (Some(a), Some(b)) if a == b => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// One publication.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MqttPublish {
    pub topic: String,
    pub payload: Vec<u8>,
    pub qos: u8,
    pub retain: bool,
    /// MQTT 5 only; ignored by 3.1.1 connections.
    pub user_properties: Vec<(String, String)>,
    pub content_type: Option<String>,
    pub correlation_data: Option<Vec<u8>>,
    pub response_topic: Option<String>,
    pub message_expiry_interval: Option<u32>,
}

impl MqttPublish {
    /// Whether the publication uses MQTT 5 properties.
    pub fn has_v5_properties(&self) -> bool {
        !self.user_properties.is_empty()
            || self.content_type.is_some()
            || self.correlation_data.is_some()
            || self.response_topic.is_some()
            || self.message_expiry_interval.is_some()
    }
}

type Reply = oneshot::Sender<Result<(), String>>;

#[derive(Default)]
struct Shared {
    /// Publications handed to the client, in order, until the event loop reports their packet id.
    queued: Mutex<VecDeque<Reply>>,
    /// Publications with QoS 1 or 2 waiting for their acknowledgement, by packet id.
    inflight: Mutex<HashMap<u16, Reply>>,
    status: Mutex<LinkStatus>,
    /// Held while a publication is queued and while the event loop drops what a broken connection had
    /// queued, so the order of `queued` always matches the client's request channel.
    order: tokio::sync::Mutex<()>,
    /// The last TLS error of the bridge, reported instead of the closed connection it causes.
    tls_error: Mutex<Option<String>>,
}

impl Shared {
    fn fail_all(&self, error: &str) {
        let queued: Vec<Reply> = self
            .queued
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect();
        let inflight: Vec<Reply> = self
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, r)| r)
            .collect();
        for r in queued.into_iter().chain(inflight) {
            let _ = r.send(Err(error.to_string()));
        }
    }

    fn sent(&self, pkid: u16) {
        let Some(r) = self
            .queued
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
        else {
            return;
        };
        if pkid == 0 {
            let _ = r.send(Ok(()));
        } else {
            self.inflight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(pkid, r);
        }
    }

    fn acked(&self, pkid: u16, result: Result<(), String>) {
        if let Some(r) = self
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&pkid)
        {
            let _ = r.send(result);
        }
    }

    fn set_status(&self, f: impl FnOnce(&mut LinkStatus)) {
        f(&mut self.status.lock().unwrap_or_else(|e| e.into_inner()));
    }
}

enum Client {
    V4(rumqttc::AsyncClient),
    V5(rumqttc::v5::AsyncClient),
}

/// What the event loop saw, independent of the protocol version.
enum Ev {
    Connected,
    Sent(u16),
    Acked(u16, Result<(), String>),
    SubAck(Vec<bool>),
    Message(Box<Received>),
    Other,
}

fn qos4(q: u8) -> rumqttc::QoS {
    match q {
        0 => rumqttc::QoS::AtMostOnce,
        1 => rumqttc::QoS::AtLeastOnce,
        _ => rumqttc::QoS::ExactlyOnce,
    }
}

fn qos5(q: u8) -> rumqttc::v5::mqttbytes::QoS {
    match q {
        0 => rumqttc::v5::mqttbytes::QoS::AtMostOnce,
        1 => rumqttc::v5::mqttbytes::QoS::AtLeastOnce,
        _ => rumqttc::v5::mqttbytes::QoS::ExactlyOnce,
    }
}

fn map_v4(ev: rumqttc::Event) -> Ev {
    use rumqttc::{Event, Outgoing, Packet};
    match ev {
        Event::Incoming(Packet::ConnAck(_)) => Ev::Connected,
        Event::Outgoing(Outgoing::Publish(pkid)) => Ev::Sent(pkid),
        Event::Incoming(Packet::PubAck(a)) => Ev::Acked(a.pkid, Ok(())),
        Event::Incoming(Packet::PubComp(a)) => Ev::Acked(a.pkid, Ok(())),
        Event::Incoming(Packet::SubAck(s)) => Ev::SubAck(
            s.return_codes
                .iter()
                .map(|c| matches!(c, rumqttc::SubscribeReasonCode::Success(_)))
                .collect(),
        ),
        Event::Incoming(Packet::Publish(p)) => {
            let mut properties = serde_json::Map::new();
            properties.insert("qos".into(), json!(p.qos as u8));
            properties.insert("retain".into(), json!(p.retain));
            Ev::Message(Box::new(Received {
                topic: p.topic,
                payload: p.payload.to_vec(),
                properties,
                ..Default::default()
            }))
        }
        _ => Ev::Other,
    }
}

fn map_v5(ev: rumqttc::v5::Event) -> Ev {
    use rumqttc::Outgoing;
    use rumqttc::v5::Event;
    use rumqttc::v5::mqttbytes::v5::{Packet, PubAckReason, PubCompReason, PubRecReason};
    match ev {
        Event::Incoming(Packet::ConnAck(_)) => Ev::Connected,
        Event::Outgoing(Outgoing::Publish(pkid)) => Ev::Sent(pkid),
        Event::Incoming(Packet::PubAck(a)) => Ev::Acked(
            a.pkid,
            match a.reason {
                PubAckReason::Success | PubAckReason::NoMatchingSubscribers => Ok(()),
                other => Err(format!("the broker refused the message: {other:?}")),
            },
        ),
        Event::Incoming(Packet::PubRec(r)) => match r.reason {
            PubRecReason::Success | PubRecReason::NoMatchingSubscribers => Ev::Other,
            other => Ev::Acked(
                r.pkid,
                Err(format!("the broker refused the message: {other:?}")),
            ),
        },
        Event::Incoming(Packet::PubComp(c)) => Ev::Acked(
            c.pkid,
            match c.reason {
                PubCompReason::Success => Ok(()),
                other => Err(format!(
                    "the broker did not complete the message: {other:?}"
                )),
            },
        ),
        Event::Incoming(Packet::SubAck(s)) => Ev::SubAck(
            s.return_codes
                .iter()
                .map(|c| {
                    matches!(
                        c,
                        rumqttc::v5::mqttbytes::v5::SubscribeReasonCode::Success(_)
                    )
                })
                .collect(),
        ),
        Event::Incoming(Packet::Publish(p)) => {
            let mut properties = serde_json::Map::new();
            properties.insert("qos".into(), json!(p.qos as u8));
            properties.insert("retain".into(), json!(p.retain));
            let mut headers = Vec::new();
            let mut content_type = None;
            if let Some(pp) = p.properties {
                headers = pp.user_properties;
                if let Some(ct) = pp.content_type {
                    properties.insert("contentType".into(), json!(ct));
                    content_type = Some(ct);
                }
                if let Some(cd) = pp.correlation_data {
                    properties.insert("correlationData".into(), json!(crate::text_or_hex(&cd)));
                }
                if let Some(rt) = pp.response_topic {
                    properties.insert("responseTopic".into(), json!(rt));
                }
                if let Some(e) = pp.message_expiry_interval {
                    properties.insert("messageExpiryInterval".into(), json!(e));
                }
                if let Some(f) = pp.payload_format_indicator {
                    properties.insert("payloadFormatIndicator".into(), json!(f));
                }
            }
            Ev::Message(Box::new(Received {
                topic: String::from_utf8_lossy(&p.topic).into_owned(),
                payload: p.payload.to_vec(),
                headers,
                properties,
                content_type,
                ..Default::default()
            }))
        }
        _ => Ev::Other,
    }
}

enum Loop {
    V4(Box<rumqttc::EventLoop>),
    V5(Box<rumqttc::v5::EventLoop>),
}

impl Loop {
    async fn poll(&mut self) -> Result<Ev, String> {
        match self {
            Loop::V4(l) => l.poll().await.map(map_v4).map_err(|e| e.to_string()),
            Loop::V5(l) => l.poll().await.map(map_v5).map_err(|e| e.to_string()),
        }
    }

    /// Drops whatever the broken connection still had queued, so that nothing is resent.
    fn drop_queued(&mut self) {
        match self {
            Loop::V4(l) => {
                l.clean();
                l.pending.clear();
            }
            Loop::V5(l) => {
                l.clean();
                l.pending.clear();
            }
        }
    }
}

/// A connection with its event loop task; dropping the last handle stops it.
pub struct MqttConnection {
    client: Client,
    shared: Arc<Shared>,
    stop: watch::Sender<bool>,
    endpoint: String,
    ack_timeout: Duration,
    _bridge: Option<TlsBridge>,
}

/// Wraps the connections of rumqttc to a private Unix socket in TLS towards the broker.
struct TlsBridge {
    dir: std::path::PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TlsBridge {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(unix)]
fn private_dir() -> anyhow::Result<std::path::PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let name = format!("fastmock-mqtt-{:016x}", rand::random::<u64>());
    let mut base = std::env::temp_dir();
    if base.as_os_str().len() + name.len() + 4 > 100 {
        base = std::path::PathBuf::from("/tmp");
    }
    let dir = base.join(name);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

impl TlsBridge {
    #[cfg(unix)]
    fn start(settings: &MqttSettings, shared: Arc<Shared>) -> anyhow::Result<(Self, String)> {
        let config = fastmock_kafka::client_tls_config(&settings.tls_files)?;
        let name = rustls::pki_types::ServerName::try_from(settings.host.clone()).map_err(|e| {
            anyhow::anyhow!("`{}` is not a valid TLS server name: {e}", settings.host)
        })?;
        let dir = private_dir()?;
        let path = dir.join("s");
        let listener = tokio::net::UnixListener::bind(&path)
            .with_context(|| format!("binding {}", path.display()))?;
        let addr = (settings.host.clone(), settings.port);
        let connector = tokio_rustls::TlsConnector::from(config);
        let task = tokio::spawn(async move {
            while let Ok((mut local, _)) = listener.accept().await {
                let connector = connector.clone();
                let name = name.clone();
                let addr = addr.clone();
                let shared = shared.clone();
                tokio::spawn(async move {
                    let upstream = tokio::time::timeout(Duration::from_secs(10), async {
                        let tcp = tokio::net::TcpStream::connect((addr.0.as_str(), addr.1))
                            .await
                            .map_err(|e| format!("I/O: {e}"))?;
                        let _ = tcp.set_nodelay(true);
                        connector
                            .connect(name, tcp)
                            .await
                            .map_err(|e| format!("TLS: {e}"))
                    })
                    .await
                    .unwrap_or_else(|_| Err("TLS: the connection timed out".into()));
                    match upstream {
                        Ok(mut tls) => {
                            *shared.tls_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
                            let _ = tokio::io::copy_bidirectional(&mut local, &mut tls).await;
                        }
                        Err(e) => {
                            *shared.tls_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
                        }
                    }
                });
            }
        });
        let socket = path.to_string_lossy().into_owned();
        Ok((Self { dir, task }, socket))
    }

    #[cfg(not(unix))]
    fn start(_: &MqttSettings, _: Arc<Shared>) -> anyhow::Result<(Self, String)> {
        bail!("mqtts:// is supported on Unix platforms only")
    }
}

impl Drop for MqttConnection {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

impl MqttConnection {
    /// Starts connecting in the background. `subscriptions` are `(filter, qos)` pairs subscribed after
    /// every connection; received messages go to `sink` (dropped, with a warning, when it is full).
    pub fn start(
        settings: &MqttSettings,
        client_id: String,
        subscriptions: Vec<(String, u8)>,
        sink: Option<mpsc::Sender<Received>>,
    ) -> anyhow::Result<Arc<Self>> {
        let shared = Arc::new(Shared::default());
        let (bridge, host, port) = if settings.tls {
            let (b, socket) = TlsBridge::start(settings, shared.clone())?;
            (Some(b), socket, 0)
        } else {
            (None, settings.host.clone(), settings.port)
        };
        #[cfg(unix)]
        let transport = bridge.as_ref().map(|_| rumqttc::Transport::Unix);
        #[cfg(not(unix))]
        let transport: Option<rumqttc::Transport> = None;
        let (client, lp) = match settings.version {
            MqttVersion::V311 => {
                let mut o = rumqttc::MqttOptions::new(client_id, host.clone(), port);
                o.set_keep_alive(settings.keep_alive.max(Duration::from_secs(5)))
                    .set_clean_session(true)
                    .set_max_packet_size(settings.max_packet_bytes, settings.max_packet_bytes);
                if let Some(c) = &settings.credentials {
                    o.set_credentials(c.username.clone(), c.password.clone());
                }
                if let Some(t) = transport {
                    o.set_transport(t);
                }
                let (c, l) = rumqttc::AsyncClient::new(o, REQUEST_CAPACITY);
                (Client::V4(c), Loop::V4(Box::new(l)))
            }
            MqttVersion::V5 => {
                let mut o = rumqttc::v5::MqttOptions::new(client_id, host.clone(), port);
                o.set_keep_alive(settings.keep_alive.max(Duration::from_secs(5)))
                    .set_clean_start(true)
                    .set_max_packet_size(Some(settings.max_packet_bytes as u32));
                if let Some(c) = &settings.credentials {
                    o.set_credentials(c.username.clone(), c.password.clone());
                }
                if let Some(t) = transport {
                    o.set_transport(t);
                }
                let (c, l) = rumqttc::v5::AsyncClient::new(o, REQUEST_CAPACITY);
                (Client::V5(c), Loop::V5(Box::new(l)))
            }
        };
        let (stop, stopped) = watch::channel(false);
        let conn = Arc::new(Self {
            client,
            shared,
            stop,
            endpoint: settings.endpoint(),
            ack_timeout: settings.ack_timeout,
            _bridge: bridge,
        });
        let task = EventTask {
            shared: conn.shared.clone(),
            subscriptions,
            sink,
            endpoint: settings.endpoint(),
            subscriber: match &conn.client {
                Client::V4(c) => Client::V4(c.clone()),
                Client::V5(c) => Client::V5(c.clone()),
            },
        };
        tokio::spawn(task.run(lp, stopped));
        Ok(conn)
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn status(&self) -> LinkStatus {
        self.shared
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Publishes and waits for the acknowledgement that its QoS calls for.
    pub async fn publish(&self, m: &MqttPublish) -> anyhow::Result<()> {
        if !valid_topic(&m.topic) {
            bail!("`{}` is not a valid MQTT topic to publish to", m.topic);
        }
        if m.qos > 2 {
            bail!("qos must be 0, 1 or 2");
        }
        let (tx, rx) = oneshot::channel();
        {
            let _order = self.shared.order.lock().await;
            self.shared
                .queued
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(tx);
            let queued = match &self.client {
                Client::V4(c) => c
                    .try_publish(m.topic.clone(), qos4(m.qos), m.retain, m.payload.clone())
                    .map_err(|e| e.to_string()),
                Client::V5(c) => {
                    let props = rumqttc::v5::mqttbytes::v5::PublishProperties {
                        user_properties: m.user_properties.clone(),
                        content_type: m.content_type.clone(),
                        correlation_data: m.correlation_data.clone().map(Bytes::from),
                        response_topic: m.response_topic.clone(),
                        message_expiry_interval: m.message_expiry_interval,
                        ..Default::default()
                    };
                    c.try_publish_with_properties(
                        m.topic.clone(),
                        qos5(m.qos),
                        m.retain,
                        m.payload.clone(),
                        props,
                    )
                    .map_err(|e| e.to_string())
                }
            };
            if let Err(e) = queued {
                self.shared
                    .queued
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .pop_back();
                bail!("the MQTT client cannot take the message ({e}); too many messages in flight");
            }
        }
        match tokio::time::timeout(self.ack_timeout, rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(e))) => bail!("{e}"),
            Ok(Err(_)) => bail!("the MQTT connection was closed"),
            Err(_) => bail!(
                "the MQTT broker did not acknowledge the message within {:?}",
                self.ack_timeout
            ),
        }
    }
}

struct EventTask {
    shared: Arc<Shared>,
    subscriptions: Vec<(String, u8)>,
    sink: Option<mpsc::Sender<Received>>,
    endpoint: String,
    subscriber: Client,
}

impl EventTask {
    fn subscribe(&self) {
        for (f, q) in &self.subscriptions {
            let r = match &self.subscriber {
                Client::V4(c) => c
                    .try_subscribe(f.clone(), qos4(*q))
                    .map_err(|e| e.to_string()),
                Client::V5(c) => c
                    .try_subscribe(f.clone(), qos5(*q))
                    .map_err(|e| e.to_string()),
            };
            if let Err(e) = r {
                tracing::warn!(endpoint = %self.endpoint, filter = %f, error = %e, "MQTT subscription request failed");
            }
        }
    }

    async fn run(self, mut lp: Loop, mut stopped: watch::Receiver<bool>) {
        let mut backoff = Backoff::default();
        let mut dropped_warned = false;
        loop {
            let ev = tokio::select! {
                _ = stopped.changed() => break,
                ev = lp.poll() => ev,
            };
            match ev {
                Ok(Ev::Connected) => {
                    backoff.reset();
                    self.shared.set_status(|s| {
                        s.connected = true;
                        s.last_error = None;
                        s.connects += 1;
                    });
                    tracing::info!(endpoint = %self.endpoint, "MQTT connected");
                    self.subscribe();
                }
                Ok(Ev::Sent(pkid)) => self.shared.sent(pkid),
                Ok(Ev::Acked(pkid, r)) => self.shared.acked(pkid, r),
                Ok(Ev::SubAck(codes)) => {
                    if codes.iter().any(|ok| !ok) {
                        let refused: Vec<&str> = self
                            .subscriptions
                            .iter()
                            .zip(codes.iter())
                            .filter(|(_, ok)| !**ok)
                            .map(|(s, _)| s.0.as_str())
                            .collect();
                        let msg = format!(
                            "the broker refused the subscription to {}",
                            refused.join(", ")
                        );
                        tracing::warn!(endpoint = %self.endpoint, "{msg}");
                        self.shared.set_status(|s| s.last_error = Some(msg));
                    }
                }
                Ok(Ev::Message(m)) => {
                    if let Some(sink) = &self.sink {
                        match sink.try_send(*m) {
                            Ok(()) => dropped_warned = false,
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                metrics::counter!("fastmock_capture_messages_total", "protocol" => "mqtt", "result" => "dropped").increment(1);
                                if !dropped_warned {
                                    tracing::warn!(endpoint = %self.endpoint, "MQTT capture cannot keep up; messages dropped");
                                    dropped_warned = true;
                                }
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => break,
                        }
                    }
                }
                Ok(Ev::Other) => {}
                Err(e) => {
                    let e = self
                        .shared
                        .tls_error
                        .lock()
                        .unwrap_or_else(|x| x.into_inner())
                        .take()
                        .unwrap_or(e);
                    {
                        let _order = self.shared.order.lock().await;
                        lp.drop_queued();
                        self.shared
                            .fail_all(&format!("the MQTT connection failed: {e}"));
                    }
                    let was = self
                        .shared
                        .status
                        .lock()
                        .map(|s| s.connected)
                        .unwrap_or(false);
                    self.shared.set_status(|s| {
                        s.connected = false;
                        s.last_error = Some(e.clone());
                    });
                    let delay = backoff.next_delay();
                    if was {
                        tracing::warn!(endpoint = %self.endpoint, error = %e, "MQTT connection lost; reconnecting");
                    } else {
                        tracing::warn!(endpoint = %self.endpoint, error = %e, retry_in = ?delay, "MQTT connection failed");
                    }
                    tokio::select! {
                        _ = stopped.changed() => break,
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
            }
        }
        self.shared.fail_all("the MQTT connection was closed");
        match &self.subscriber {
            Client::V4(c) => {
                let _ = c.try_disconnect();
            }
            Client::V5(c) => {
                let _ = c.try_disconnect();
            }
        }
    }
}

/// Text of a JSON value for an MQTT user property or header.
pub fn property_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(
            parse_url("mqtt://broker.example.com").unwrap(),
            ("broker.example.com".into(), 1883, false)
        );
        assert_eq!(
            parse_url("mqtts://broker.example.com:8884").unwrap(),
            ("broker.example.com".into(), 8884, true)
        );
        assert_eq!(
            parse_url("ssl://[::1]").unwrap(),
            ("::1".into(), 8883, true)
        );
        assert!(parse_url("http://x").is_err());
        assert!(parse_url("mqtt://u:p@x").is_err());
        assert!(parse_url("mqtt://x/path").is_err());
        assert!(parse_url("not a url").is_err());
    }

    #[test]
    fn versions() {
        assert_eq!("5".parse::<MqttVersion>().unwrap(), MqttVersion::V5);
        assert_eq!("v3.1.1".parse::<MqttVersion>().unwrap(), MqttVersion::V311);
        assert!("3.1".parse::<MqttVersion>().is_err());
    }

    #[test]
    fn topics_and_filters() {
        assert!(valid_topic("devices/d1/state"));
        assert!(!valid_topic("devices/+/state"));
        assert!(!valid_topic(""));
        assert!(valid_filter("devices/+/state"));
        assert!(valid_filter("devices/#"));
        assert!(valid_filter("#"));
        assert!(!valid_filter("devices/#/x"));
        assert!(!valid_filter("devices/a+"));
        assert!(filter_matches("devices/+/state", "devices/d1/state"));
        assert!(!filter_matches("devices/+/state", "devices/d1/x/state"));
        assert!(filter_matches("devices/#", "devices"));
        assert!(filter_matches("devices/#", "devices/a/b"));
        assert!(!filter_matches("#", "$SYS/uptime"));
        assert!(filter_matches("$SYS/#", "$SYS/uptime"));
        assert!(!filter_matches("a/b", "a/b/c"));
    }

    #[test]
    fn settings_check_tls_files() {
        let mut s = MqttSettings::new("mqtt://broker.example.com").unwrap();
        s.validate().unwrap();
        s.tls_files.ca_file = Some("/nonexistent/ca.pem".into());
        assert!(s.validate().unwrap_err().to_string().contains("mqtts://"));
        s.tls = true;
        assert!(s.validate().is_err());
        assert!(s.client_id("pub").starts_with("fastmock-pub-"));
        assert_eq!(s.endpoint(), "mqtts://broker.example.com:1883");
    }

    #[tokio::test]
    async fn publishing_without_a_broker_fails_quickly() {
        let mut s = MqttSettings::new("mqtt://127.0.0.1:1").unwrap();
        s.ack_timeout = Duration::from_secs(5);
        let c = MqttConnection::start(&s, s.client_id("t"), vec![], None).unwrap();
        let started = std::time::Instant::now();
        let e = c
            .publish(&MqttPublish {
                topic: "a/b".into(),
                payload: b"x".to_vec(),
                qos: 1,
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(e.to_string().contains("connection failed"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(!c.status().connected);
        assert!(c.status().last_error.is_some());
    }
}
