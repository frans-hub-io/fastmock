//! AMQP 0-9-1 (RabbitMQ, LavinMQ, ...) on lapin.
//!
//! [`AmqpPublisher`] connects on first use and keeps one connection with one channel in confirm mode; a
//! publication waits for the broker's confirm. After a failure the connection is rebuilt on the next
//! publication, at most once per growing pause ([`crate::Backoff`]); publications in between fail at once.
//!
//! [`consume`] runs a capture subscription: it declares a server-named, exclusive, auto-delete queue, binds
//! it to the exchange with each binding key and consumes without acknowledgements. The queue belongs to
//! FastMock's connection only, so the application's own queues and consumers never lose a message to it; it
//! disappears with the connection.

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use futures_util::StreamExt;
use lapin::options::{
    BasicConsumeOptions, BasicPublishOptions, ConfirmSelectOptions, ExchangeDeclareOptions,
    QueueBindOptions, QueueDeclareOptions,
};
use lapin::types::{AMQPValue, FieldTable, LongString, ShortString};
use lapin::uri::{AMQPScheme, AMQPUri, AMQPUserInfo, SASLMechanism};
use lapin::{
    BasicProperties, Channel, Confirmation, Connection, ConnectionProperties, ExchangeKind,
};
use serde_json::json;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::{Backoff, Credentials, LinkStatus, Received, TlsFiles};

/// Name the allow-list and the API use for the default exchange (`""`).
pub const DEFAULT_EXCHANGE: &str = "amq.default";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AmqpSasl {
    #[default]
    Plain,
    External,
}

impl FromStr for AmqpSasl {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(match s.trim().to_ascii_uppercase().as_str() {
            "PLAIN" => AmqpSasl::Plain,
            "EXTERNAL" => AmqpSasl::External,
            other => bail!("unknown AMQP SASL mechanism `{other}`; use PLAIN or EXTERNAL"),
        })
    }
}

/// Where and how FastMock connects to an AMQP 0-9-1 broker (`FASTMOCK_AMQP_*`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmqpSettings {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    /// The virtual host, decoded (`/` by default).
    pub vhost: String,
    pub credentials: Option<Credentials>,
    pub mechanism: AmqpSasl,
    pub tls_files: TlsFiles,
    /// Declare missing exchanges before publishing (`FASTMOCK_AMQP_DECLARE_EXCHANGES`).
    pub declare_exchanges: bool,
    pub connect_timeout: Duration,
    /// How long a publication waits for the broker's confirm.
    pub confirm_timeout: Duration,
    pub heartbeat: u16,
}

impl AmqpSettings {
    pub fn new(url: &str) -> anyhow::Result<Self> {
        let (host, port, tls, vhost) = parse_url(url)?;
        Ok(Self {
            host,
            port,
            tls,
            vhost,
            credentials: None,
            mechanism: AmqpSasl::Plain,
            tls_files: TlsFiles::default(),
            declare_exchanges: false,
            connect_timeout: Duration::from_secs(10),
            confirm_timeout: Duration::from_secs(10),
            heartbeat: 30,
        })
    }

    /// The broker URL without credentials, for logs.
    pub fn endpoint(&self) -> String {
        let vhost: String = url::form_urlencoded::byte_serialize(self.vhost.as_bytes()).collect();
        format!(
            "{}://{}:{}/{}",
            if self.tls { "amqps" } else { "amqp" },
            self.host,
            self.port,
            vhost
        )
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.tls && self.tls_files != TlsFiles::default() {
            bail!("TLS files are set but the broker URL is not amqps://");
        }
        match self.mechanism {
            AmqpSasl::Plain => {
                if let Some(c) = &self.credentials
                    && c.username.is_empty()
                {
                    bail!("the AMQP user name is empty");
                }
            }
            AmqpSasl::External => {
                if !self.tls || self.tls_files.client_cert.is_none() {
                    bail!(
                        "SASL EXTERNAL needs an amqps:// URL and a client certificate (TLS_CERT_FILE and TLS_KEY_FILE)"
                    );
                }
                if self.credentials.is_some() {
                    bail!("SASL EXTERNAL takes no user name or password");
                }
            }
        }
        if self.tls {
            fastmock_kafka::client_tls_config(&self.tls_files)?;
            self.tls_config()?;
        }
        Ok(())
    }

    fn uri(&self) -> AMQPUri {
        let mut uri = AMQPUri {
            scheme: if self.tls {
                AMQPScheme::AMQPS
            } else {
                AMQPScheme::AMQP
            },
            vhost: self.vhost.clone(),
            ..Default::default()
        };
        uri.authority.host = self.host.clone();
        uri.authority.port = self.port;
        if let Some(c) = &self.credentials {
            uri.authority.userinfo = AMQPUserInfo {
                username: c.username.clone(),
                password: c.password.clone(),
            };
        }
        uri.query.heartbeat = Some(self.heartbeat);
        uri.query.connection_timeout = Some(self.connect_timeout.as_millis() as u64);
        if self.mechanism == AmqpSasl::External {
            uri.query.auth_mechanism = Some(SASLMechanism::External);
        }
        uri
    }

    fn tls_config(&self) -> anyhow::Result<lapin::tcp::OwnedTLSConfig> {
        let mut cfg = lapin::tcp::OwnedTLSConfig::default();
        if let Some(ca) = &self.tls_files.ca_file {
            fastmock_kafka::read_ca_bundle(ca)?;
            cfg.cert_chain = Some(
                std::fs::read_to_string(ca).with_context(|| format!("reading {}", ca.display()))?,
            );
        }
        if let Some(files) = &self.tls_files.client_cert {
            fastmock_kafka::read_client_cert(files)?;
            cfg.identity = Some(lapin::tcp::OwnedIdentity::PKCS8 {
                pem: std::fs::read(&files.cert_file)
                    .with_context(|| format!("reading {}", files.cert_file.display()))?,
                key: std::fs::read(&files.key_file)
                    .with_context(|| format!("reading {}", files.key_file.display()))?,
            });
        }
        Ok(cfg)
    }

    /// Opens a connection; the TLS files are read again each time.
    pub async fn connect(&self, name: &str) -> anyhow::Result<Connection> {
        let props = ConnectionProperties::default().with_connection_name(name.into());
        let tls = if self.tls {
            self.tls_config()?
        } else {
            lapin::tcp::OwnedTLSConfig::default()
        };
        let runtime = lapin::runtime::default_runtime()?;
        let fut = Connection::connect_uri_with_config(self.uri(), props, tls, runtime);
        match tokio::time::timeout(self.connect_timeout + Duration::from_secs(1), fut).await {
            Ok(r) => r.map_err(|e| anyhow::anyhow!("connecting to {}: {e}", self.endpoint())),
            Err(_) => bail!("connecting to {} timed out", self.endpoint()),
        }
    }
}

/// Host, port, TLS and virtual host of an `amqp://` or `amqps://` URL. The virtual host is the URL path
/// without its leading `/`, percent-decoded (`/%2f` is `/`); without a path it is `/`.
pub fn parse_url(url: &str) -> anyhow::Result<(String, u16, bool, String)> {
    let u = url::Url::parse(url.trim()).with_context(|| format!("`{url}` is not a URL"))?;
    let tls = match u.scheme() {
        "amqp" => false,
        "amqps" => true,
        other => bail!("unsupported AMQP URL scheme `{other}`; use amqp:// or amqps://"),
    };
    if !u.username().is_empty() || u.password().is_some() {
        bail!("the AMQP URL must not contain credentials; use the user name and password settings");
    }
    if u.query().is_some() {
        bail!("the AMQP URL takes no query; use the FASTMOCK_AMQP_* settings");
    }
    let host = u
        .host_str()
        .filter(|h| !h.is_empty())
        .context("the AMQP URL has no host")?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = u.port().unwrap_or(if tls { 5671 } else { 5672 });
    let vhost = match u.path() {
        "" | "/" => "/".to_string(),
        p => {
            let raw = &p[1..];
            if raw.contains('/') {
                bail!(
                    "the AMQP virtual host must be one URL-encoded path segment (`/%2f` for `/`)"
                );
            }
            url::form_urlencoded::parse(format!("v={raw}").as_bytes())
                .next()
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default()
        }
    };
    Ok((host, port, tls, vhost))
}

/// The exchange name on the wire for a name of the API (`amq.default` is the default exchange `""`).
pub fn wire_exchange(name: &str) -> &str {
    if name == DEFAULT_EXCHANGE { "" } else { name }
}

/// The name of an exchange in the API, allow-lists and logs (`""` is `amq.default`).
pub fn api_exchange(name: &str) -> &str {
    if name.is_empty() {
        DEFAULT_EXCHANGE
    } else {
        name
    }
}

/// Whether an exchange name is acceptable: at most 255 bytes of letters, digits, `-`, `_`, `.`, `:`.
pub fn valid_exchange(name: &str) -> bool {
    name.len() <= 255
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/'))
}

/// The exchange types FastMock declares.
pub fn exchange_kind(kind: &str) -> anyhow::Result<ExchangeKind> {
    Ok(match kind.to_ascii_lowercase().as_str() {
        "topic" => ExchangeKind::Topic,
        "direct" => ExchangeKind::Direct,
        "fanout" => ExchangeKind::Fanout,
        "headers" => ExchangeKind::Headers,
        other => bail!("unknown exchange type `{other}`; use topic, direct, fanout or headers"),
    })
}

/// How a missing exchange is declared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExchangeDeclaration {
    pub kind: String,
    pub durable: bool,
}

impl Default for ExchangeDeclaration {
    fn default() -> Self {
        Self {
            kind: "topic".into(),
            durable: true,
        }
    }
}

/// One publication.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AmqpMessage {
    /// The exchange as the API names it (`amq.default` or `""` for the default exchange).
    pub exchange: String,
    pub routing_key: String,
    pub payload: Vec<u8>,
    pub headers: Vec<(String, String)>,
    pub content_type: Option<String>,
    pub content_encoding: Option<String>,
    pub message_id: Option<String>,
    pub correlation_id: Option<String>,
    pub reply_to: Option<String>,
    /// Per-message TTL in milliseconds, as text (AMQP 0-9-1 `expiration`).
    pub expiration: Option<String>,
    /// Delivery mode 2 (persistent) instead of 1.
    pub persistent: bool,
    pub priority: Option<u8>,
    pub message_type: Option<String>,
    pub app_id: Option<String>,
    /// Seconds since 1970-01-01.
    pub timestamp: Option<u64>,
    /// Ask the broker to return the message when no queue is bound for it; it then fails.
    pub mandatory: bool,
}

fn short(s: &str) -> anyhow::Result<ShortString> {
    if s.len() > 255 {
        bail!(
            "`{}...` exceeds the 255 bytes of an AMQP short string",
            &s[..32.min(s.len())]
        );
    }
    Ok(s.into())
}

fn properties(m: &AmqpMessage) -> anyhow::Result<BasicProperties> {
    let mut p = BasicProperties::default().with_delivery_mode(if m.persistent { 2 } else { 1 });
    if !m.headers.is_empty() {
        let mut t = FieldTable::default();
        for (k, v) in &m.headers {
            t.insert(
                short(k)?,
                AMQPValue::LongString(LongString::from(v.as_bytes().to_vec())),
            );
        }
        p = p.with_headers(t);
    }
    if let Some(v) = &m.content_type {
        p = p.with_content_type(short(v)?);
    }
    if let Some(v) = &m.content_encoding {
        p = p.with_content_encoding(short(v)?);
    }
    if let Some(v) = &m.message_id {
        p = p.with_message_id(short(v)?);
    }
    if let Some(v) = &m.correlation_id {
        p = p.with_correlation_id(short(v)?);
    }
    if let Some(v) = &m.reply_to {
        p = p.with_reply_to(short(v)?);
    }
    if let Some(v) = &m.expiration {
        if v.is_empty() || !v.chars().all(|c| c.is_ascii_digit()) {
            bail!("expiration must be a number of milliseconds, got `{v}`");
        }
        p = p.with_expiration(short(v)?);
    }
    if let Some(v) = m.priority {
        p = p.with_priority(v);
    }
    if let Some(v) = &m.message_type {
        p = p.with_type(short(v)?);
    }
    if let Some(v) = &m.app_id {
        p = p.with_app_id(short(v)?);
    }
    if let Some(v) = m.timestamp {
        p = p.with_timestamp(v);
    }
    Ok(p)
}

struct PublisherState {
    conn: Option<Arc<Connection>>,
    channel: Option<Channel>,
    declared: HashSet<String>,
    retry_at: Option<Instant>,
    backoff: Backoff,
    last_error: Option<String>,
}

/// Publishes with publisher confirms over one lazily opened connection.
pub struct AmqpPublisher {
    settings: AmqpSettings,
    name: String,
    state: tokio::sync::Mutex<PublisherState>,
    status: Mutex<LinkStatus>,
}

impl AmqpPublisher {
    pub fn new(settings: AmqpSettings, connection_name: &str) -> Self {
        Self {
            settings,
            name: connection_name.to_string(),
            state: tokio::sync::Mutex::new(PublisherState {
                conn: None,
                channel: None,
                declared: HashSet::new(),
                retry_at: None,
                backoff: Backoff::default(),
                last_error: None,
            }),
            status: Mutex::new(LinkStatus::default()),
        }
    }

    pub fn status(&self) -> LinkStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_status(&self, f: impl FnOnce(&mut LinkStatus)) {
        f(&mut self.status.lock().unwrap_or_else(|e| e.into_inner()));
    }

    /// The channel in confirm mode, connecting when needed.
    async fn channel(&self) -> anyhow::Result<(Channel, Arc<Connection>)> {
        let mut st = self.state.lock().await;
        if let (Some(c), Some(ch)) = (&st.conn, &st.channel)
            && c.status().connected()
            && ch.status().connected()
        {
            return Ok((ch.clone(), c.clone()));
        }
        if let Some(c) = &st.conn
            && c.status().connected()
        {
            let conn = c.clone();
            let ch = conn.create_channel().await?;
            ch.confirm_select(ConfirmSelectOptions::default()).await?;
            st.channel = Some(ch.clone());
            return Ok((ch, conn));
        }
        if let Some(at) = st.retry_at
            && Instant::now() < at
        {
            bail!(
                "the AMQP broker is unavailable ({}); the next connection attempt is in {:?}",
                st.last_error.as_deref().unwrap_or("connection failed"),
                at - Instant::now()
            );
        }
        let attempt = async {
            let conn = Arc::new(self.settings.connect(&self.name).await?);
            let ch = conn.create_channel().await?;
            ch.confirm_select(ConfirmSelectOptions::default()).await?;
            anyhow::Ok((conn, ch))
        }
        .await;
        match attempt {
            Ok((conn, ch)) => {
                tracing::info!(endpoint = %self.settings.endpoint(), "AMQP connected");
                st.backoff.reset();
                st.retry_at = None;
                st.last_error = None;
                st.declared.clear();
                st.conn = Some(conn.clone());
                st.channel = Some(ch.clone());
                self.set_status(|s| {
                    s.connected = true;
                    s.last_error = None;
                    s.connects += 1;
                });
                Ok((ch, conn))
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let delay = st.backoff.next_delay();
                st.retry_at = Some(Instant::now() + delay);
                st.last_error = Some(msg.clone());
                st.conn = None;
                st.channel = None;
                self.set_status(|s| {
                    s.connected = false;
                    s.last_error = Some(msg.clone());
                });
                tracing::warn!(endpoint = %self.settings.endpoint(), error = %msg, retry_in = ?delay, "AMQP connection failed");
                Err(e)
            }
        }
    }

    async fn declare(
        &self,
        conn: &Connection,
        exchange: &str,
        decl: &ExchangeDeclaration,
    ) -> anyhow::Result<()> {
        if self.state.lock().await.declared.contains(exchange) {
            return Ok(());
        }
        let kind = exchange_kind(&decl.kind)?;
        let probe = conn.create_channel().await?;
        let exists = probe
            .exchange_declare(
                exchange.into(),
                kind.clone(),
                ExchangeDeclareOptions {
                    passive: true,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await
            .is_ok();
        let _ = probe.close(200, "done".into()).await;
        if !exists {
            let ch = conn.create_channel().await?;
            let r = ch
                .exchange_declare(
                    exchange.into(),
                    kind,
                    ExchangeDeclareOptions {
                        durable: decl.durable,
                        ..Default::default()
                    },
                    FieldTable::default(),
                )
                .await;
            let _ = ch.close(200, "done".into()).await;
            r.with_context(|| format!("declaring AMQP exchange {exchange}"))?;
            tracing::info!(exchange, kind = %decl.kind, durable = decl.durable, "declared AMQP exchange");
        }
        self.state
            .lock()
            .await
            .declared
            .insert(exchange.to_string());
        Ok(())
    }

    /// Publishes and waits for the broker's confirm. With `declare`, a missing exchange is declared first.
    pub async fn publish(
        &self,
        m: &AmqpMessage,
        declare: Option<&ExchangeDeclaration>,
    ) -> anyhow::Result<()> {
        let exchange = wire_exchange(&m.exchange).to_string();
        let props = properties(m)?;
        let routing_key = short(&m.routing_key)?;
        let (ch, conn) = self.channel().await?;
        if let Some(d) = declare
            && !exchange.is_empty()
        {
            self.declare(&conn, &exchange, d).await?;
        }
        let result = async {
            let confirm = ch
                .basic_publish(
                    short(&exchange)?,
                    routing_key,
                    BasicPublishOptions {
                        mandatory: m.mandatory,
                        ..Default::default()
                    },
                    &m.payload,
                    props,
                )
                .await?;
            match tokio::time::timeout(self.settings.confirm_timeout, confirm).await {
                Err(_) => bail!(
                    "the AMQP broker did not confirm the message within {:?}",
                    self.settings.confirm_timeout
                ),
                Ok(Err(e)) => Err(e.into()),
                Ok(Ok(Confirmation::Ack(None))) | Ok(Ok(Confirmation::NotRequested)) => Ok(()),
                Ok(Ok(Confirmation::Ack(Some(r)))) => bail!(
                    "the message was returned as unroutable ({} {}): no queue is bound for routing key `{}` on exchange {}",
                    r.reply_code,
                    r.reply_text,
                    m.routing_key,
                    api_exchange(&exchange)
                ),
                Ok(Ok(Confirmation::Nack(_))) => bail!("the AMQP broker rejected the message (nack)"),
            }
        }
        .await;
        if let Err(e) = &result {
            let text = e.to_string();
            if text.contains("NOT_FOUND") && text.contains("exchange") {
                return Err(anyhow::anyhow!(
                    "{text} (create the exchange, or set FASTMOCK_AMQP_DECLARE_EXCHANGES=true)"
                ));
            }
        }
        result
    }

    /// Closes the connection, if any.
    pub async fn close(&self) {
        let mut st = self.state.lock().await;
        st.channel = None;
        if let Some(c) = st.conn.take() {
            let _ = c.close(200, "bye".into()).await;
        }
    }
}

/// What a capture subscription binds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmqpCapture {
    pub exchange: String,
    /// Binding keys; empty means `#`.
    pub binding_keys: Vec<String>,
    /// `x-max-length` of the capture queue: the broker drops the oldest beyond it.
    pub max_queue_length: u32,
    /// For logs and the connection name.
    pub label: String,
}

fn value_text(v: &AMQPValue) -> String {
    match v {
        AMQPValue::Boolean(b) => b.to_string(),
        AMQPValue::ShortShortInt(n) => n.to_string(),
        AMQPValue::ShortShortUInt(n) => n.to_string(),
        AMQPValue::ShortInt(n) => n.to_string(),
        AMQPValue::ShortUInt(n) => n.to_string(),
        AMQPValue::LongInt(n) => n.to_string(),
        AMQPValue::LongUInt(n) => n.to_string(),
        AMQPValue::LongLongInt(n) => n.to_string(),
        AMQPValue::Float(n) => n.to_string(),
        AMQPValue::Double(n) => n.to_string(),
        AMQPValue::ShortString(s) => s.as_str().to_string(),
        AMQPValue::LongString(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
        AMQPValue::Timestamp(t) => t.to_string(),
        AMQPValue::ByteArray(b) => crate::text_or_hex(b.as_slice()),
        AMQPValue::Void => String::new(),
        other => format!("{other:?}"),
    }
}

/// The message of a delivery as capture stores it.
pub fn received(d: &lapin::message::Delivery) -> Received {
    let p = &d.properties;
    let mut props = serde_json::Map::new();
    let mut set = |k: &str, v: Option<String>| {
        if let Some(v) = v {
            props.insert(k.into(), json!(v));
        }
    };
    set(
        "contentType",
        p.content_type().as_ref().map(|s| s.to_string()),
    );
    set(
        "contentEncoding",
        p.content_encoding().as_ref().map(|s| s.to_string()),
    );
    set("messageId", p.message_id().as_ref().map(|s| s.to_string()));
    set(
        "correlationId",
        p.correlation_id().as_ref().map(|s| s.to_string()),
    );
    set("replyTo", p.reply_to().as_ref().map(|s| s.to_string()));
    set("expiration", p.expiration().as_ref().map(|s| s.to_string()));
    set("type", p.kind().as_ref().map(|s| s.to_string()));
    set("appId", p.app_id().as_ref().map(|s| s.to_string()));
    set("userId", p.user_id().as_ref().map(|s| s.to_string()));
    if let Some(m) = p.delivery_mode() {
        props.insert("deliveryMode".into(), json!(m));
    }
    if let Some(m) = p.priority() {
        props.insert("priority".into(), json!(m));
    }
    if let Some(t) = p.timestamp() {
        props.insert("timestamp".into(), json!(t));
    }
    if d.redelivered {
        props.insert("redelivered".into(), json!(true));
    }
    let headers = p
        .headers()
        .as_ref()
        .map(|t| {
            t.inner()
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), value_text(v)))
                .collect()
        })
        .unwrap_or_default();
    Received {
        topic: d.routing_key.as_str().to_string(),
        exchange: Some(api_exchange(d.exchange.as_str()).to_string()),
        key: None,
        payload: d.data.clone(),
        headers,
        content_type: p.content_type().as_ref().map(|s| s.to_string()),
        properties: props,
    }
}

async fn session(
    settings: &AmqpSettings,
    sub: &AmqpCapture,
    sink: &mpsc::Sender<Received>,
    status: &Mutex<LinkStatus>,
    stop: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let conn = settings
        .connect(&format!("fastmock capture {}", sub.label))
        .await?;
    let run = async {
        let ch = conn.create_channel().await?;
        let exchange = wire_exchange(&sub.exchange);
        if exchange.is_empty() {
            bail!("the default exchange cannot be bound; capture from a named exchange");
        }
        ch.exchange_declare(
            exchange.into(),
            ExchangeKind::Topic,
            ExchangeDeclareOptions {
                passive: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .with_context(|| format!("exchange {exchange} does not exist"))?;
        let mut args = FieldTable::default();
        args.insert(
            "x-max-length".into(),
            AMQPValue::LongInt(sub.max_queue_length.min(i32::MAX as u32) as i32),
        );
        let q = ch
            .queue_declare(
                "".into(),
                QueueDeclareOptions {
                    exclusive: true,
                    auto_delete: true,
                    durable: false,
                    ..Default::default()
                },
                args,
            )
            .await?;
        let keys: Vec<String> = if sub.binding_keys.is_empty() {
            vec!["#".into()]
        } else {
            sub.binding_keys.clone()
        };
        for k in &keys {
            ch.queue_bind(
                q.name().clone(),
                exchange.into(),
                short(k)?,
                QueueBindOptions::default(),
                FieldTable::default(),
            )
            .await
            .with_context(|| format!("binding the capture queue to {exchange} with `{k}`"))?;
        }
        let mut consumer = ch
            .basic_consume(
                q.name().clone(),
                "fastmock-capture".into(),
                BasicConsumeOptions {
                    no_ack: true,
                    exclusive: true,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await?;
        {
            let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
            s.connected = true;
            s.last_error = None;
            s.connects += 1;
        }
        tracing::info!(endpoint = %settings.endpoint(), exchange, queue = %q.name(), source = %sub.label, "AMQP capture subscribed");
        let mut dropped_warned = false;
        loop {
            let next = tokio::select! {
                _ = stop.changed() => return Ok(()),
                n = consumer.next() => n,
            };
            match next {
                Some(Ok(d)) => match sink.try_send(received(&d)) {
                    Ok(()) => dropped_warned = false,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        metrics::counter!("fastmock_capture_messages_total", "protocol" => "amqp", "result" => "dropped").increment(1);
                        if !dropped_warned {
                            tracing::warn!(source = %sub.label, "AMQP capture cannot keep up; messages dropped");
                            dropped_warned = true;
                        }
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                },
                Some(Err(e)) => return Err(e.into()),
                None => bail!("the broker cancelled the capture consumer"),
            }
        }
    }
    .await;
    let _ = conn.close(200, "bye".into()).await;
    run
}

/// Runs a capture subscription until `stop` changes, reconnecting with a growing pause.
pub async fn consume(
    settings: AmqpSettings,
    sub: AmqpCapture,
    sink: mpsc::Sender<Received>,
    status: Arc<Mutex<LinkStatus>>,
    mut stop: watch::Receiver<bool>,
) {
    let mut backoff = Backoff::default();
    loop {
        if *stop.borrow() {
            return;
        }
        let r = session(&settings, &sub, &sink, &status, &mut stop).await;
        let was = {
            let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
            let was = s.connected;
            s.connected = false;
            if let Err(e) = &r {
                s.last_error = Some(format!("{e:#}"));
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
                tracing::warn!(endpoint = %settings.endpoint(), source = %sub.label, error = %format!("{e:#}"), retry_in = ?delay, "AMQP capture interrupted");
                tokio::select! {
                    _ = stop.changed() => return,
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_vhosts() {
        assert_eq!(
            parse_url("amqp://rabbit.example.com").unwrap(),
            ("rabbit.example.com".into(), 5672, false, "/".into())
        );
        assert_eq!(
            parse_url("amqps://rabbit.example.com:5673/%2f").unwrap(),
            ("rabbit.example.com".into(), 5673, true, "/".into())
        );
        assert_eq!(parse_url("amqp://h/orders").unwrap().3, "orders");
        assert!(parse_url("amqp://u:p@h").is_err());
        assert!(parse_url("amqp://h/a/b").is_err());
        assert!(parse_url("amqp://h?heartbeat=5").is_err());
        assert!(parse_url("http://h").is_err());
        let s = AmqpSettings::new("amqp://h/orders").unwrap();
        assert_eq!(s.endpoint(), "amqp://h:5672/orders");
        assert_eq!(
            AmqpSettings::new("amqp://h").unwrap().endpoint(),
            "amqp://h:5672/%2F"
        );
    }

    #[test]
    fn default_exchange_names() {
        assert_eq!(wire_exchange("amq.default"), "");
        assert_eq!(wire_exchange("orders"), "orders");
        assert_eq!(api_exchange(""), "amq.default");
        assert!(valid_exchange("orders.v1"));
        assert!(valid_exchange(""));
        assert!(!valid_exchange("has space"));
        assert!(exchange_kind("Topic").is_ok());
        assert!(exchange_kind("x-delayed").is_err());
    }

    #[test]
    fn properties_are_checked() {
        let m = AmqpMessage {
            exchange: "orders".into(),
            routing_key: "order.created".into(),
            headers: vec![("x-event".into(), "created".into())],
            content_type: Some("application/json".into()),
            message_id: Some("m-1".into()),
            correlation_id: Some("c-1".into()),
            reply_to: Some("replies".into()),
            expiration: Some("60000".into()),
            persistent: true,
            priority: Some(5),
            ..Default::default()
        };
        let p = properties(&m).unwrap();
        assert_eq!(*p.delivery_mode(), Some(2));
        assert_eq!(
            p.content_type().as_ref().unwrap().as_str(),
            "application/json"
        );
        assert_eq!(p.expiration().as_ref().unwrap().as_str(), "60000");
        assert!(
            p.headers()
                .as_ref()
                .unwrap()
                .inner()
                .contains_key("x-event")
        );
        let bad = AmqpMessage {
            expiration: Some("1m".into()),
            ..m.clone()
        };
        assert!(properties(&bad).is_err());
        let long = AmqpMessage {
            message_id: Some("x".repeat(300)),
            ..m
        };
        assert!(properties(&long).is_err());
    }

    #[test]
    fn sasl_external_needs_a_client_certificate() {
        let mut s = AmqpSettings::new("amqp://h").unwrap();
        s.mechanism = AmqpSasl::External;
        assert!(s.validate().unwrap_err().to_string().contains("EXTERNAL"));
        assert_eq!("external".parse::<AmqpSasl>().unwrap(), AmqpSasl::External);
        assert!("SCRAM".parse::<AmqpSasl>().is_err());
    }

    #[tokio::test]
    async fn publishing_without_a_broker_backs_off() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut s = AmqpSettings::new("amqp://127.0.0.1:1").unwrap();
        s.connect_timeout = Duration::from_secs(2);
        let p = AmqpPublisher::new(s, "test");
        let m = AmqpMessage {
            exchange: "x".into(),
            routing_key: "k".into(),
            ..Default::default()
        };
        assert!(p.publish(&m, None).await.is_err());
        let e = p.publish(&m, None).await.unwrap_err();
        assert!(e.to_string().contains("next connection attempt"), "{e}");
        assert!(!p.status().connected);
    }
}
