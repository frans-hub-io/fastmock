//! gRPC proxying and recording.
//!
//! A gRPC call matched by a stub with `proxy`, or matched by no stub while a recording runs, is forwarded
//! to an upstream gRPC server over HTTP/2: h2c for `http://` upstreams, TLS with ALPN `h2` for `https://`
//! (Mozilla roots plus `FASTMOCK_OUTBOUND_CA_FILE`, and the client certificate of
//! `FASTMOCK_GRPC_PROXY_CLIENT_CERT_FILE` for the hosts in `FASTMOCK_GRPC_PROXY_CLIENT_CERT_HOSTS`). The
//! upstream must pass the egress policy: allowed hosts, and every address it resolves to outside the denied
//! ranges. Messages are passed on as they are, in both directions and for every kind of call, while the
//! journal keeps them decoded. During a recording the exchange becomes a stub, within
//! `FASTMOCK_GRPC_RECORD_MAX_MESSAGES` and `FASTMOCK_GRPC_RECORD_MAX_BYTES`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use fastmock_core::grpc_model::{
    GrpcCode, GrpcMessage, GrpcReplyRule, GrpcResponseDef, GrpcStatus, MAX_GRPC_MESSAGE_BYTES,
    MAX_GRPC_MESSAGES, MAX_GRPC_RULE_MESSAGES, MAX_GRPC_RULES, code_name,
};
use fastmock_core::outbound::blocked_outbound_header;
use fastmock_core::{BodyMatcher, ProxyDef, RequestPattern, ResponseDef, StringMatcher, Stub};
use http_body_util::StreamBody;
use http_body_util::combinators::BoxBody;
use hyper::body::Frame;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use prost_reflect::MethodDescriptor;
use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::time::Instant;
use tracing::Instrument;
use uuid::Uuid;

use crate::config::Config;
use crate::dataplane::StubHit;
use crate::grpc::{Encoding, Outcome, Protocol};
use crate::grpc_call::{
    CallLog, Failure, GrpcCallLog, Incoming, Kind, Out, RESERVED_METADATA, finished_early,
};
use crate::outbox::EgressPolicy;
use crate::proxy::Recording;
use crate::state::AppState;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Addresses of an upstream tried one after the other.
const MAX_ADDRESSES: usize = 4;

type UpBody = BoxBody<Bytes, std::io::Error>;
type Sender = hyper::client::conn::http2::SendRequest<UpBody>;
type FrameResult = Result<Frame<Bytes>, std::io::Error>;

/// The TLS settings and the call budget of gRPC proxying.
pub struct GrpcProxy {
    tls: Arc<rustls::ClientConfig>,
    mtls: Option<Arc<rustls::ClientConfig>>,
    cert_hosts: CertHosts,
    calls: Arc<Semaphore>,
}

/// Proxy hops a gRPC call may already have made (`x-fastmock-proxy-hops`); a call with more is not
/// proxied again, which ends loops between FastMock instances.
pub const MAX_PROXY_HOPS: u32 = 3;
/// The header FastMock sets on proxied calls with the number of FastMock proxies the call went through.
pub const HOPS_HEADER: &str = "x-fastmock-proxy-hops";

/// The proxy hops a request has made, from [`HOPS_HEADER`] (unparsable values count as the maximum).
pub fn hops(headers: &HeaderMap) -> u32 {
    match headers.get(HOPS_HEADER) {
        None => 0,
        Some(v) => v
            .to_str()
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(MAX_PROXY_HOPS),
    }
}

/// The upstreams (`host:port`) that get FastMock's gRPC client certificate
/// (`FASTMOCK_GRPC_PROXY_CLIENT_CERT_HOSTS`): exact names or IP addresses, or `*.domain` for the names one
/// label below `domain`; the port defaults to 443.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CertHosts {
    entries: Vec<(String, bool, u16)>,
}

fn valid_label(l: &str) -> bool {
    !l.is_empty()
        && l.len() <= 63
        && l.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        && !l.starts_with('-')
        && !l.ends_with('-')
}

impl CertHosts {
    /// Parses and checks the entries; refuses schemes, paths, `*`, wildcards over a single label (`*.com`)
    /// and invalid names or ports.
    pub fn parse(list: &[String]) -> Result<Self, String> {
        let mut entries = Vec::new();
        for raw in list {
            let e = raw.trim().to_ascii_lowercase();
            let bad = |why: &str| {
                format!(
                    "FASTMOCK_GRPC_PROXY_CLIENT_CERT_HOSTS: `{raw}` {why}; use host, host:port or *.domain:port"
                )
            };
            if e.contains("://") || e.contains('/') || e.contains('@') {
                return Err(bad("is not a host"));
            }
            let (host, port) = if let Some(rest) = e.strip_prefix('[') {
                let (h, after) = rest.split_once(']').ok_or_else(|| bad("is not a host"))?;
                (h.to_string(), after.strip_prefix(':').map(str::to_string))
            } else if e.matches(':').count() > 1 {
                (e.clone(), None)
            } else {
                match e.rsplit_once(':') {
                    Some((h, p)) => (h.to_string(), Some(p.to_string())),
                    None => (e.clone(), None),
                }
            };
            let port = match port {
                Some(p) => p
                    .parse::<u16>()
                    .ok()
                    .filter(|p| *p > 0)
                    .ok_or_else(|| bad("has an invalid port"))?,
                None => 443,
            };
            let host = host.trim_end_matches('.').to_string();
            if host.parse::<std::net::IpAddr>().is_ok() {
                entries.push((host, false, port));
                continue;
            }
            let (wildcard, name) = match host.strip_prefix("*.") {
                Some(rest) => (true, rest.to_string()),
                None => (false, host.clone()),
            };
            let labels: Vec<&str> = name.split('.').collect();
            if name.is_empty() || !labels.iter().all(|l| valid_label(l)) {
                return Err(bad("is not a valid host name"));
            }
            if wildcard && labels.len() < 2 {
                return Err(bad("covers a whole top-level domain"));
            }
            entries.push((name, wildcard, port));
        }
        Ok(Self { entries })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether `host:port` gets the client certificate.
    pub fn matches(&self, host: &str, port: u16) -> bool {
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim_end_matches('.')
            .to_ascii_lowercase();
        self.entries.iter().any(|(name, wildcard, p)| {
            *p == port
                && match wildcard {
                    false => host == *name,
                    true => host
                        .strip_suffix(name.as_str())
                        .and_then(|l| l.strip_suffix('.'))
                        .is_some_and(|l| !l.is_empty() && !l.contains('.')),
                }
        })
    }

    /// Whether a `baseUrl` points at an upstream that gets the client certificate.
    pub fn matches_url(&self, base_url: &str) -> bool {
        reqwest::Url::parse(base_url).is_ok_and(|u| {
            u.scheme() == "https"
                && u.host_str().is_some()
                && u.port_or_known_default()
                    .is_some_and(|p| self.matches(u.host_str().unwrap_or_default(), p))
        })
    }
}

fn with_h2(c: Arc<rustls::ClientConfig>) -> Arc<rustls::ClientConfig> {
    let mut c = (*c).clone();
    c.alpn_protocols = vec![b"h2".to_vec()];
    Arc::new(c)
}

impl GrpcProxy {
    pub fn new(cfg: &Config) -> anyhow::Result<Self> {
        let files = |client_cert| fastmock_kafka::TlsFiles {
            ca_file: cfg.outbound_ca_file.clone(),
            client_cert,
        };
        let tls = with_h2(
            fastmock_kafka::client_tls_config(&files(None)).context("FASTMOCK_OUTBOUND_CA_FILE")?,
        );
        let mtls = match &cfg.grpc_proxy.client_cert {
            Some(c) => Some(with_h2(
                fastmock_kafka::client_tls_config(&files(Some(c.clone()))).context(
                    "FASTMOCK_GRPC_PROXY_CLIENT_CERT_FILE and FASTMOCK_GRPC_PROXY_CLIENT_KEY_FILE",
                )?,
            )),
            None => None,
        };
        Ok(Self {
            tls,
            mtls,
            cert_hosts: CertHosts::parse(&cfg.grpc_proxy.client_cert_hosts)
                .map_err(|e| anyhow::anyhow!(e))?,
            calls: Arc::new(Semaphore::new(cfg.grpc_proxy.max_calls)),
        })
    }

    fn tls_for(&self, host: &str, port: u16, cert: bool) -> Arc<rustls::ClientConfig> {
        match &self.mtls {
            Some(m) if cert && self.cert_hosts.matches(host, port) => m.clone(),
            _ => self.tls.clone(),
        }
    }

    /// Refuses a stub of a space whose proxy upstream gets FastMock's client certificate: only stubs of the
    /// global area (which need `stubs:write` granted globally) may use that identity.
    pub fn check_stub(&self, stub: &Stub) -> Result<(), String> {
        if stub.space.is_none() || self.mtls.is_none() {
            return Ok(());
        }
        let defs = std::iter::once(&stub.response).chain(stub.responses.iter());
        for r in defs {
            if let Some(p) = &r.proxy
                && self.cert_hosts.matches_url(&p.base_url)
            {
                return Err(format!(
                    "proxy.baseUrl {} is an upstream that gets FastMock's gRPC client certificate (FASTMOCK_GRPC_PROXY_CLIENT_CERT_HOSTS); only stubs of the global area may proxy to it",
                    p.base_url
                ));
            }
        }
        Ok(())
    }

    /// Opens an HTTP/2 connection to the upstream of `url`, checked against the egress policy; `cert`
    /// allows the client certificate for the cert hosts.
    async fn connect(
        &self,
        cfg: &Config,
        policy: &EgressPolicy,
        url: &reqwest::Url,
        cert: bool,
    ) -> Result<Sender, String> {
        let host = url
            .host_str()
            .ok_or("the upstream URL has no host")?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let port = url
            .port_or_known_default()
            .ok_or("the upstream URL has no port")?;
        let addrs: Vec<SocketAddr> = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio::net::lookup_host((host.as_str(), port)),
        )
        .await
        .map_err(|_| format!("resolving {host} timed out"))?
        .map_err(|e| format!("resolving {host} failed: {e}"))?
        .collect();
        let allowed: Vec<SocketAddr> = addrs
            .into_iter()
            .filter(|a| policy.ip_allowed(a.ip()))
            .collect();
        if allowed
            .iter()
            .any(|a| crate::contract::is_own_listener(cfg, *a))
        {
            return Err("proxy loop: the upstream is this FastMock's own listener".into());
        }
        if allowed.is_empty() {
            return Err(format!(
                "blocked by egress policy: all addresses of {host} are denied by FASTMOCK_CALLBACK_DENIED_CIDRS"
            ));
        }
        let mut last = String::new();
        let mut tcp = None;
        for a in allowed.iter().take(MAX_ADDRESSES) {
            match tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(a)).await {
                Ok(Ok(s)) => {
                    tcp = Some(s);
                    break;
                }
                Ok(Err(e)) => last = e.to_string(),
                Err(_) => last = "connect timed out".into(),
            }
        }
        let tcp = tcp.ok_or_else(|| format!("connecting to {host}:{port} failed: {last}"))?;
        let _ = tcp.set_nodelay(true);
        match url.scheme() {
            "http" => handshake(TokioIo::new(H2Guard::new(tcp))).await,
            "https" => {
                let name = rustls::pki_types::ServerName::try_from(host.clone())
                    .map_err(|_| format!("{host} is not a valid TLS server name"))?;
                let tls = tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    tokio_rustls::TlsConnector::from(self.tls_for(&host, port, cert))
                        .connect(name, tcp),
                )
                .await
                .map_err(|_| "the TLS handshake with the upstream timed out".to_string())?
                .map_err(|e| format!("the TLS handshake with the upstream failed: {e}"))?;
                if tls.get_ref().1.alpn_protocol() != Some(b"h2") {
                    return Err("the upstream did not negotiate HTTP/2 (ALPN h2)".into());
                }
                handshake(TokioIo::new(H2Guard::new(tls))).await
            }
            other => Err(format!(
                "blocked by egress policy: scheme {other} is not allowed"
            )),
        }
    }
}

/// The client connection preface: the magic string, then a frame (SETTINGS) whose length FastMock reads
/// from its header.
const PREFACE: usize = 24;

/// Holds back everything the HTTP/2 client writes after its connection preface until the upstream has
/// sent a SETTINGS frame, so a server that does not speak HTTP/2 never receives request headers or
/// message bytes.
struct H2Guard<T> {
    inner: T,
    written: usize,
    first: Vec<u8>,
    read: Vec<u8>,
    confirmed: bool,
    waiting: Option<std::task::Waker>,
}

impl<T> H2Guard<T> {
    fn new(inner: T) -> Self {
        Self {
            inner,
            written: 0,
            first: Vec::with_capacity(PREFACE + 9),
            read: Vec::with_capacity(9),
            confirmed: false,
            waiting: None,
        }
    }

    /// Bytes the client may write before the upstream's SETTINGS frame arrived.
    fn allowed(&self) -> usize {
        if self.first.len() < PREFACE + 9 {
            return PREFACE + 9;
        }
        let h = &self.first[PREFACE..PREFACE + 9];
        PREFACE + 9 + ((h[0] as usize) << 16 | (h[1] as usize) << 8 | h[2] as usize)
    }
}

impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for H2Guard<T> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &r
            && !self.confirmed
        {
            let new: Vec<u8> = buf.filled()[before..].to_vec();
            let need = 9 - self.read.len();
            let take = need.min(new.len());
            self.read.extend_from_slice(&new[..take]);
            if self.read.len() == 9 {
                if self.read[3] != 0x4 {
                    return std::task::Poll::Ready(Err(std::io::Error::other(
                        "the upstream does not speak HTTP/2: its first frame is not SETTINGS",
                    )));
                }
                self.confirmed = true;
                if let Some(w) = self.waiting.take() {
                    w.wake();
                }
            }
        }
        r
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for H2Guard<T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if self.confirmed {
            return std::pin::Pin::new(&mut self.inner).poll_write(cx, buf);
        }
        let room = self.allowed().saturating_sub(self.written);
        if room == 0 {
            self.waiting = Some(cx.waker().clone());
            return std::task::Poll::Pending;
        }
        let n = room.min(buf.len());
        let r = std::pin::Pin::new(&mut self.inner).poll_write(cx, &buf[..n]);
        if let std::task::Poll::Ready(Ok(done)) = &r {
            let keep = (PREFACE + 9).saturating_sub(self.first.len()).min(*done);
            self.first.extend_from_slice(&buf[..keep]);
            self.written += done;
        }
        r
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

async fn handshake<T>(io: T) -> Result<Sender, String>
where
    T: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let (sender, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .timer(TokioTimer::new())
        .handshake(io)
        .await
        .map_err(|e| format!("the HTTP/2 handshake with the upstream failed: {e}"))?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::debug!(error = %e, "upstream gRPC connection ended");
        }
    });
    Ok(sender)
}

/// Where a call goes: the upstream of a proxy stub, or the target of the running recording.
pub(crate) enum Target {
    Stub { def: ProxyDef, hit: StubHit },
    Recording(Recording),
}

/// A gRPC call whose request messages (all of them, except for bidirectional calls) have been read.
pub(crate) struct ProxyCall {
    pub target: Target,
    pub parts: Parts,
    pub protocol: Protocol,
    pub req_ct: Option<String>,
    pub method: MethodDescriptor,
    pub kind: Kind,
    pub log: Arc<CallLog>,
    pub done: oneshot::Sender<()>,
    pub incoming: Incoming,
    /// The request messages read so far, decompressed.
    pub raw: Vec<Bytes>,
    pub held: Vec<OwnedSemaphorePermit>,
    /// The request as JSON (the message, or the array of a client stream), for the recorded stub.
    pub request_body: Bytes,
}

/// What a recording keeps of a call: the messages in the order FastMock saw them, within the limits.
struct Transcript {
    on: bool,
    events: Vec<(bool, Value)>,
    ins: usize,
    outs: usize,
    bytes: usize,
    max_messages: usize,
    max_bytes: usize,
    /// Why the call cannot be recorded (`too_large`, `invalid`).
    skip: Option<&'static str>,
}

/// Frames a proxied stream may have queued in each direction.
const CHANNEL: usize = 2;

/// The in-flight bytes of one direction of a proxied call, charged to `FASTMOCK_MAX_INFLIGHT_BODY_BYTES`:
/// each message is charged twice (the frame and its decoded copy) and stays charged while it may still be
/// queued.
struct InFlight {
    budget: Arc<Semaphore>,
    held: std::collections::VecDeque<OwnedSemaphorePermit>,
}

impl InFlight {
    fn new(budget: Arc<Semaphore>) -> Self {
        Self {
            budget,
            held: Default::default(),
        }
    }

    /// `false` when the budget is used up.
    fn charge(&mut self, bytes: usize) -> bool {
        let permits = (bytes.saturating_mul(2) / 1024 + 1).min(u32::MAX as usize) as u32;
        match self.budget.clone().try_acquire_many_owned(permits) {
            Ok(p) => {
                self.held.push_back(p);
                while self.held.len() > CHANNEL + 1 {
                    self.held.pop_front();
                }
                true
            }
            Err(_) => {
                metrics::counter!("fastmock_requests_rejected_total", "reason" => "busy")
                    .increment(1);
                false
            }
        }
    }
}

/// Queues a frame for the upstream; `false` when it is gone or read nothing for `wait`.
async fn send_up(tx: &mpsc::Sender<FrameResult>, f: FrameResult, wait: Duration) -> bool {
    matches!(tokio::time::timeout(wait, tx.send(f)).await, Ok(Ok(())))
}

/// Decodes a message for the journal and the recording, unless it is larger than either keeps.
fn decode_for_journal(
    payload: &[u8],
    limit: usize,
    decode: impl FnOnce(&[u8]) -> Result<Value, String>,
) -> (Value, Result<Value, &'static str>) {
    if payload.len() > limit {
        return (
            Value::String(format!("{} bytes, not decoded", payload.len())),
            Err("too_large"),
        );
    }
    match decode(payload) {
        Ok(v) => (v.clone(), Ok(v)),
        Err(e) => (Value::String(format!("not decoded: {e}")), Err("invalid")),
    }
}

impl Transcript {
    /// Marks the call as not recordable.
    fn refuse(&mut self, reason: &'static str) {
        if self.on && self.skip.is_none() {
            self.skip = Some(reason);
            self.events.clear();
        }
    }

    fn push(&mut self, incoming: bool, v: Option<&Value>) {
        if !self.on || self.skip.is_some() {
            return;
        }
        let Some(v) = v else {
            self.skip = Some("invalid");
            self.events.clear();
            return;
        };
        let n = serde_json::to_vec(v).map_or(usize::MAX, |b| b.len());
        let count = if incoming {
            &mut self.ins
        } else {
            &mut self.outs
        };
        *count += 1;
        if *count > self.max_messages
            || n > MAX_GRPC_MESSAGE_BYTES
            || self.bytes.saturating_add(n) > self.max_bytes
        {
            self.skip = Some("too_large");
            self.events.clear();
            return;
        }
        self.bytes += n;
        self.events.push((incoming, v.clone()));
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && b[i + 1].is_ascii_hexdigit()
            && b[i + 2].is_ascii_hexdigit()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The status in `grpc-status`, `grpc-message` and `grpc-status-details-bin`, with the other metadata.
fn outcome_from(h: &HeaderMap) -> Option<Outcome> {
    let raw = h.get("grpc-status")?;
    let code = raw
        .to_str()
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .map_or(2, |c| if c > 16 { 2 } else { c });
    let message = h
        .get("grpc-message")
        .and_then(|v| v.to_str().ok())
        .map(percent_decode)
        .unwrap_or_default();
    let details = h
        .get("grpc-status-details-bin")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().trim_end_matches('=').to_string());
    let trailers = h
        .iter()
        .filter(|(k, _)| {
            let n = k.as_str();
            !n.starts_with("grpc-")
                && !n.starts_with("x-fastmock-")
                && !RESERVED_METADATA.contains(&n)
        })
        .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
        .collect();
    Some(Outcome {
        code,
        message,
        details,
        trailers,
    })
}

/// The status of an upstream answer that is not HTTP 200, as gRPC clients map it.
fn http_outcome(status: StatusCode) -> Outcome {
    let code = match status.as_u16() {
        400 => 13,
        401 => 16,
        403 => 7,
        404 => 12,
        429 | 502 | 503 | 504 => 14,
        _ => 2,
    };
    Outcome::status(
        code,
        format!("the upstream answered HTTP {}", status.as_u16()),
    )
}

/// Ends a call that could not reach the upstream with `UNAVAILABLE`; the log has the full reason.
fn unavailable(
    protocol: Protocol,
    req_ct: Option<&str>,
    log: &Arc<CallLog>,
    hit: Option<StubHit>,
    path: &str,
    msg: &str,
) -> Response {
    metrics::counter!("fastmock_proxy_errors_total").increment(1);
    tracing::warn!(error = %msg, path = %path, "gRPC proxying failed");
    let reason = if msg.starts_with("blocked by egress policy") {
        "blocked by egress policy"
    } else {
        "upstream unavailable"
    };
    let mut r = finished_early(
        protocol,
        req_ct,
        log,
        Outcome::status(14, format!("proxy failed: {reason}")),
    );
    if let Some(h) = hit {
        r.extensions_mut().insert(h);
    }
    r
}

fn metadata(h: &HeaderMap) -> impl Iterator<Item = (&HeaderName, &HeaderValue)> {
    h.iter().filter(|(k, _)| {
        let n = k.as_str();
        !RESERVED_METADATA.contains(&n)
            && !n.starts_with(':')
            && !n.starts_with("x-fastmock-")
            && !matches!(n, "keep-alive" | "proxy-connection" | "host" | "date")
    })
}

/// Forwards a call to its upstream and streams the answer back to the client.
pub(crate) async fn forward(app: &AppState, c: ProxyCall) -> Response {
    let ProxyCall {
        target,
        parts,
        protocol,
        req_ct,
        method,
        kind,
        log,
        done,
        mut incoming,
        raw,
        held,
        request_body,
    } = c;
    let req_ct = req_ct.as_deref();
    let path = parts.uri.path().to_string();
    let (def, hit, rec) = match target {
        Target::Stub { def, hit } => (def, Some(hit), None),
        Target::Recording(r) => (
            ProxyDef {
                base_url: r
                    .grpc_target_base_url
                    .clone()
                    .unwrap_or_else(|| r.target_base_url.clone()),
                additional_headers: Default::default(),
                remove_headers: vec![],
                strip_prefix: None,
            },
            None,
            Some(r),
        ),
    };
    metrics::counter!("fastmock_grpc_proxied_calls_total", "target" => if rec.is_some() { "recording" } else { "stub" })
        .increment(1);
    let Ok(permit) = app.grpc_proxy.calls.clone().try_acquire_owned() else {
        metrics::counter!("fastmock_requests_rejected_total", "reason" => "busy").increment(1);
        let mut r = finished_early(
            protocol,
            req_ct,
            &log,
            Outcome::status(
                8,
                "too many proxied gRPC calls in flight (FASTMOCK_GRPC_PROXY_MAX_CALLS)",
            ),
        );
        if let Some(h) = hit {
            r.extensions_mut().insert(h);
        }
        return r;
    };
    let url = match crate::proxy::target_url(&def, &path, None).and_then(|u| {
        app.policy
            .check_url(&u)
            .map(|()| u)
            .map_err(|e| format!("blocked by egress policy: {e}"))
    }) {
        Ok(u) => u,
        Err(e) => return unavailable(protocol, req_ct, &log, hit, &path, &e),
    };
    let hop = hops(&parts.headers);
    if hop >= MAX_PROXY_HOPS {
        metrics::counter!("fastmock_proxy_errors_total").increment(1);
        tracing::warn!(path = %path, hops = hop, "gRPC proxy loop refused");
        let mut r = finished_early(
            protocol,
            req_ct,
            &log,
            Outcome::status(
                14,
                format!("proxy failed: the call already went through {hop} FastMock proxies"),
            ),
        );
        if let Some(h) = hit {
            r.extensions_mut().insert(h);
        }
        return r;
    }
    let cert = hit.as_ref().is_none_or(|h| h.space.is_none());
    let mut sender = match app
        .grpc_proxy
        .connect(&app.cfg, &app.policy, &url, cert)
        .await
    {
        Ok(s) => s,
        Err(e) => return unavailable(protocol, req_ct, &log, hit, &path, &e),
    };

    let (req_tx, mut req_rx) = mpsc::channel::<FrameResult>(CHANNEL);
    let body: UpBody = BoxBody::new(StreamBody::new(futures::stream::poll_fn(move |cx| {
        req_rx.poll_recv(cx)
    })));
    let mut req = match http::Request::builder()
        .method(http::Method::POST)
        .uri(url.as_str())
        .version(http::Version::HTTP_2)
        .body(body)
    {
        Ok(r) => r,
        Err(e) => return unavailable(protocol, req_ct, &log, hit, &path, &e.to_string()),
    };
    let removed: Vec<String> = def
        .remove_headers
        .iter()
        .map(|h| h.to_ascii_lowercase())
        .collect();
    let h = req.headers_mut();
    for (k, v) in &parts.headers {
        let n = k.as_str();
        if blocked_outbound_header(n)
            || RESERVED_METADATA.contains(&n)
            || n.starts_with("x-grpc-web")
            || n.starts_with("x-fastmock-")
            || removed.iter().any(|r| r == n)
        {
            continue;
        }
        h.append(k.clone(), v.clone());
    }
    for (k, v) in &def.additional_headers {
        if blocked_outbound_header(k) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(k.to_ascii_lowercase().as_bytes()),
            HeaderValue::from_str(v),
        ) {
            h.insert(n, v);
        }
    }
    h.insert("content-type", HeaderValue::from_static("application/grpc"));
    h.insert("te", HeaderValue::from_static("trailers"));
    h.insert(HOPS_HEADER, HeaderValue::from(hop + 1));
    h.insert(
        "grpc-accept-encoding",
        HeaderValue::from_static(crate::grpc::ACCEPT_ENCODING),
    );

    let transcript = Arc::new(Mutex::new(Transcript {
        on: rec.is_some(),
        events: Vec::new(),
        ins: 0,
        outs: 0,
        bytes: 0,
        max_messages: app.cfg.grpc_proxy.record_max_messages,
        max_bytes: app.cfg.grpc_proxy.record_max_bytes,
        skip: None,
    }));
    if rec.is_some() && kind != Kind::Bidi {
        let mut t = transcript.lock().unwrap_or_else(|e| e.into_inner());
        match serde_json::from_slice::<Value>(&request_body) {
            Ok(Value::Array(list)) if kind.many_requests() => {
                for v in &list {
                    t.push(true, Some(v));
                }
            }
            Ok(v) => t.push(true, Some(&v)),
            Err(_) => t.push(true, None),
        }
    }
    let client_gzip = incoming.encoding == Some(Encoding::Gzip)
        && Encoding::accepted_by_client(&parts.headers, Encoding::Gzip);
    let deadline = incoming.deadline;
    let client_failure: Arc<Mutex<Option<Failure>>> = Arc::new(Mutex::new(None));
    let write_wait = app.cfg.grpc_stream_idle_timeout;
    let decode_limit = if rec.is_some() {
        app.cfg.journal_max_body.max(MAX_GRPC_MESSAGE_BYTES)
    } else {
        app.cfg.journal_max_body
    };
    {
        let transcript = transcript.clone();
        let log = log.clone();
        let method = method.clone();
        let client_failure = client_failure.clone();
        let mut inflight = InFlight::new(app.body_budget.clone());
        tokio::spawn(
            async move {
                let _held = held;
                for p in raw {
                    if !send_up(
                        &req_tx,
                        Ok(Frame::data(crate::grpc::frame(&p, false))),
                        write_wait,
                    )
                    .await
                    {
                        return;
                    }
                }
                if kind != Kind::Bidi {
                    return;
                }
                loop {
                    match incoming.next().await {
                        Ok(Some(p)) => {
                            metrics::counter!("fastmock_grpc_stream_messages_total", "direction" => "in", "kind" => kind.label())
                                .increment(1);
                            if !inflight.charge(p.len()) {
                                let f = Failure {
                                    code: 8,
                                    message: "server busy: too many large messages in flight".into(),
                                };
                                let _ = send_up(
                                    &req_tx,
                                    Err(std::io::Error::other(f.message.clone())),
                                    write_wait,
                                )
                                .await;
                                *client_failure.lock().unwrap_or_else(|e| e.into_inner()) =
                                    Some(f);
                                return;
                            }
                            let (shown, json) = decode_for_journal(&p, decode_limit, |b| {
                                crate::grpc::decode_message(&method, b)
                            });
                            log.with(|r| r.push_request(&shown));
                            {
                                let mut t = transcript.lock().unwrap_or_else(|e| e.into_inner());
                                match &json {
                                    Ok(v) => t.push(true, Some(v)),
                                    Err(reason) => t.refuse(reason),
                                }
                            }
                            if !send_up(
                                &req_tx,
                                Ok(Frame::data(crate::grpc::frame(&p, false))),
                                write_wait,
                            )
                            .await
                            {
                                return;
                            }
                        }
                        Ok(None) => return,
                        Err(f) => {
                            let _ = send_up(
                                &req_tx,
                                Err(std::io::Error::other(f.message.clone())),
                                write_wait,
                            )
                            .await;
                            *client_failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
                            return;
                        }
                    }
                }
            }
            .instrument(tracing::Span::current()),
        );
    }

    let wait = app
        .cfg
        .grpc_stream_idle_timeout
        .min(deadline.saturating_duration_since(Instant::now()));
    let resp = match tokio::time::timeout(wait, async {
        sender.ready().await?;
        sender.send_request(req).await
    })
    .await
    {
        Err(_) => {
            return unavailable(
                protocol,
                req_ct,
                &log,
                hit,
                &path,
                "the upstream sent no response headers in time",
            );
        }
        Ok(Err(e)) => {
            return unavailable(
                protocol,
                req_ct,
                &log,
                hit,
                &path,
                &format!("the upstream request failed: {e}"),
            );
        }
        Ok(Ok(r)) => r,
    };
    let (rparts, rbody) = resp.into_parts();
    let is_grpc = rparts
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.starts_with("application/grpc"));
    let http_failed = rparts.status != StatusCode::OK;
    let mut first = if http_failed {
        Some(http_outcome(rparts.status))
    } else if let Some(o) = outcome_from(&rparts.headers) {
        Some(o)
    } else if !is_grpc {
        Some(Outcome::status(
            13,
            "the upstream did not answer with a gRPC content type",
        ))
    } else {
        None
    };
    let mut answer_failed = http_failed || (!is_grpc && first.is_some());
    let up_encoding = match Encoding::from_header(&rparts.headers) {
        Ok(e) => e,
        Err(name) => {
            answer_failed = true;
            first.get_or_insert(Outcome::status(
                13,
                format!("the upstream used grpc-encoding {name}, which FastMock does not support"),
            ));
            None
        }
    };

    let (tx, mut rx) = mpsc::channel::<FrameResult>(CHANNEL);
    let stream = futures::stream::poll_fn(move |cx| rx.poll_recv(cx));
    let mut resp = Response::new(Body::new(StreamBody::new(stream)));
    *resp.status_mut() = StatusCode::OK;
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(protocol.content_type(req_ct)) {
        h.insert("content-type", v);
    }
    h.insert(
        "grpc-accept-encoding",
        HeaderValue::from_static(crate::grpc::ACCEPT_ENCODING),
    );
    if client_gzip {
        h.insert("grpc-encoding", HeaderValue::from_static("gzip"));
    }
    if first.is_none() {
        for (k, v) in metadata(&rparts.headers) {
            h.append(k.clone(), v.clone());
        }
    }
    resp.extensions_mut().insert(crate::extras::Streaming);
    resp.extensions_mut().insert(GrpcCallLog(log.clone()));
    if let Some(h) = hit {
        resp.extensions_mut().insert(h);
    }
    let out = Out {
        tx,
        protocol,
        gzip: client_gzip,
        method: method.clone(),
        log: log.clone(),
        kind,
        sent: 0,
        write_timeout: write_wait,
    };
    let mut inflight = InFlight::new(app.body_budget.clone());
    let mut upstream = Incoming::upstream(app, Body::new(rbody), up_encoding, deadline);
    let app = app.clone();
    let request_headers = parts.headers;
    let response_headers = rparts.headers;
    tokio::spawn(
        async move {
            let _permit = permit;
            let _sender = sender;
            metrics::gauge!("fastmock_grpc_streams_active").increment(1.0);
            let mut out = out;
            let mut gone = false;
            let mut failed = false;
            let mut outcome = first;
            if outcome.is_none() {
                loop {
                    match upstream.next().await {
                        Ok(Some(p)) => {
                            if !inflight.charge(p.len()) {
                                failed = true;
                                outcome = Some(Outcome::status(
                                    8,
                                    "server busy: too many large messages in flight",
                                ));
                                break;
                            }
                            let (shown, json) = decode_for_journal(&p, decode_limit, |b| {
                                crate::grpc::decode_output(&method, b)
                            });
                            {
                                let mut t = transcript.lock().unwrap_or_else(|e| e.into_inner());
                                match &json {
                                    Ok(v) => t.push(false, Some(v)),
                                    Err(reason) => t.refuse(reason),
                                }
                            }
                            if !out.forward(&p, &shown).await {
                                gone = true;
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(f) => {
                            failed = true;
                            outcome =
                                Some(Outcome::status(f.code, format!("upstream: {}", f.message)));
                            break;
                        }
                    }
                }
                if outcome.is_none() && !gone {
                    outcome = Some(
                        upstream
                            .trailers
                            .as_ref()
                            .and_then(outcome_from)
                            .unwrap_or_else(|| {
                                failed = true;
                                Outcome::status(13, "the upstream ended the call without a status")
                            }),
                    );
                }
            } else {
                failed = answer_failed;
            }
            if let Some(f) = client_failure
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                failed = true;
                outcome = Some(Outcome::status(f.code, f.message));
            }
            if gone {
                out.cancelled();
            } else {
                let outcome = outcome.unwrap_or_default();
                if let Some(rec) = &rec
                    && !failed
                {
                    let t = std::mem::replace(
                        &mut *transcript.lock().unwrap_or_else(|e| e.into_inner()),
                        Transcript {
                            on: false,
                            events: vec![],
                            ins: 0,
                            outs: 0,
                            bytes: 0,
                            max_messages: 0,
                            max_bytes: 0,
                            skip: None,
                        },
                    );
                    record(
                        &app,
                        rec,
                        Recorded {
                            path: &path,
                            kind,
                            request_headers: &request_headers,
                            response_headers: &response_headers,
                            outcome: &outcome,
                            request_body: &request_body,
                            transcript: t,
                        },
                    )
                    .await;
                }
                out.finish(outcome).await;
            }
            metrics::gauge!("fastmock_grpc_streams_active").decrement(1.0);
            drop(done);
        }
        .instrument(tracing::Span::current()),
    );
    resp
}

struct Recorded<'a> {
    path: &'a str,
    kind: Kind,
    request_headers: &'a HeaderMap,
    response_headers: &'a HeaderMap,
    outcome: &'a Outcome,
    request_body: &'a Bytes,
    transcript: Transcript,
}

async fn record(app: &AppState, rec: &Recording, r: Recorded<'_>) {
    if let Some(reason) = r.transcript.skip {
        metrics::counter!("fastmock_recording_skipped_total", "reason" => reason).increment(1);
        tracing::warn!(path = %r.path, reason, "gRPC call not recorded");
        return;
    }
    let identity = if r.kind == Kind::Bidi {
        let ins: Vec<Value> = r
            .transcript
            .events
            .iter()
            .filter(|(i, _)| *i)
            .map(|(_, v)| v.clone())
            .collect();
        format!("grpc|{}|{}", r.path, Value::Array(ins))
    } else {
        format!(
            "grpc|{}|{}",
            r.path,
            String::from_utf8_lossy(r.request_body)
        )
    };
    let pool = app.grpc.pool().await;
    match recorded_stub(&app.redactor, &pool, rec, &r) {
        Ok(stub) => {
            crate::proxy::store_recorded(app, &identity, stub).await;
        }
        Err(reason) => {
            metrics::counter!("fastmock_recording_skipped_total", "reason" => "too_large")
                .increment(1);
            tracing::warn!(path = %r.path, reason = %reason, "gRPC call not recorded");
        }
    }
}

fn message(v: &Value) -> GrpcMessage {
    GrpcMessage {
        json_body: v.clone(),
        delay_ms: 0,
    }
}

fn json_matcher(redactor: &crate::redact::Redactor, v: &Value) -> BodyMatcher {
    let mut v = v.clone();
    let had_secrets = redactor.strip_sensitive(&mut v);
    BodyMatcher {
        equal_to_json: Some(v),
        ignore_extra_elements: had_secrets,
        ..Default::default()
    }
}

/// The stub that replays a recorded call.
fn recorded_stub(
    redactor: &crate::redact::Redactor,
    pool: &prost_reflect::DescriptorPool,
    rec: &Recording,
    r: &Recorded<'_>,
) -> Result<Stub, String> {
    let mut req = RequestPattern {
        method: Some("POST".into()),
        url_path: Some(r.path.to_string()),
        ..Default::default()
    };
    for name in &rec.request_headers {
        let n = name.to_ascii_lowercase();
        if n == "content-type" {
            continue;
        }
        if let Some(v) = r
            .request_headers
            .get(n.as_str())
            .and_then(|v| v.to_str().ok())
        {
            let sensitive = redactor.sensitive_header(&n);
            req.headers.insert(
                n,
                StringMatcher {
                    equal_to: (!sensitive).then(|| v.to_string()),
                    matches: sensitive.then(|| ".+".to_string()),
                    ..Default::default()
                },
            );
        }
    }
    let ins: Vec<&Value> = r
        .transcript
        .events
        .iter()
        .filter(|(i, _)| *i)
        .map(|(_, v)| v)
        .collect();
    let outs: Vec<&Value> = r
        .transcript
        .events
        .iter()
        .filter(|(i, _)| !*i)
        .map(|(_, v)| v)
        .collect();
    match r.kind {
        Kind::Unary | Kind::ServerStreaming => {
            if let Some(v) = ins.first() {
                req.body.push(json_matcher(redactor, v));
            }
        }
        Kind::ClientStreaming => {
            let all = Value::Array(ins.iter().map(|v| (*v).clone()).collect());
            req.body.push(json_matcher(redactor, &all));
        }
        Kind::Bidi => {}
    }
    let keep: Vec<String> = rec
        .capture_headers
        .iter()
        .map(|h| h.to_ascii_lowercase())
        .collect();
    let mut headers = BTreeMap::new();
    for (k, v) in metadata(r.response_headers) {
        if keep.iter().any(|x| x == k.as_str())
            && let Ok(v) = v.to_str()
        {
            headers.insert(k.as_str().to_string(), v.to_string());
        }
    }
    let mut def = GrpcResponseDef::default();
    for (k, v) in &r.outcome.trailers {
        if keep.iter().any(|x| x == k) {
            def.trailers.insert(k.clone(), v.clone());
        }
    }
    let code = r.outcome.code;
    if code != 0 || !r.outcome.message.is_empty() {
        let details = r
            .outcome
            .details
            .as_deref()
            .and_then(|b| crate::grpc::decode_status_details(pool, b))
            .and_then(|v| v.get("details").cloned())
            .and_then(|d| d.as_array().cloned())
            .unwrap_or_default();
        def.status = Some(GrpcStatus {
            code: GrpcCode::Name(code_name(code).to_string()),
            message: (!r.outcome.message.is_empty()).then(|| r.outcome.message.clone()),
            details,
        });
    }
    let mut json_body = None;
    match r.kind {
        Kind::Unary | Kind::ClientStreaming if code == 0 && outs.len() == 1 => {
            json_body = Some(outs[0].clone());
        }
        Kind::Bidi => {
            let mut seen_in = false;
            let mut current: Option<usize> = None;
            let mut rules: Vec<GrpcReplyRule> = Vec::new();
            for (incoming, v) in &r.transcript.events {
                if *incoming {
                    seen_in = true;
                    let m = json_matcher(redactor, v);
                    current = if rules.iter().any(|x| x.matcher.first() == Some(&m)) {
                        None
                    } else {
                        rules.push(GrpcReplyRule {
                            matcher: vec![m],
                            ..Default::default()
                        });
                        Some(rules.len() - 1)
                    };
                } else if let Some(i) = current {
                    rules[i].messages.push(message(v));
                } else if !seen_in {
                    def.messages.push(message(v));
                }
            }
            rules.retain(|x| !x.messages.is_empty());
            if rules.len() > MAX_GRPC_RULES
                || rules
                    .iter()
                    .any(|x| x.messages.len() > MAX_GRPC_RULE_MESSAGES)
            {
                return Err(format!(
                    "a recorded bidirectional call needs at most {MAX_GRPC_RULES} distinct request messages with at most {MAX_GRPC_RULE_MESSAGES} replies each"
                ));
            }
            def.on_message = rules;
        }
        _ => def.messages = outs.iter().map(|v| message(v)).collect(),
    }
    if def.messages.len() > MAX_GRPC_MESSAGES {
        return Err(format!("more than {MAX_GRPC_MESSAGES} response messages"));
    }
    let grpc = (def != GrpcResponseDef::default()).then_some(def);
    Ok(Stub {
        id: Uuid::now_v7(),
        name: Some(format!("recorded gRPC {}", r.path)),
        priority: 5,
        request: req,
        response: ResponseDef {
            status: 200,
            templated: false,
            headers,
            json_body,
            grpc,
            ..Default::default()
        },
        tags: vec![rec.tag.clone(), "recorded".into()],
        enabled: true,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_come_from_headers_and_http() {
        let mut h = HeaderMap::new();
        h.insert("grpc-status", HeaderValue::from_static("5"));
        h.insert(
            "grpc-message",
            HeaderValue::from_static("order%20Z-9 not found%"),
        );
        h.insert("x-shard", HeaderValue::from_static("7"));
        h.insert("grpc-status-details-bin", HeaderValue::from_static("CAU="));
        let o = outcome_from(&h).unwrap();
        assert_eq!(o.code, 5);
        assert_eq!(o.message, "order Z-9 not found%");
        assert_eq!(o.details.as_deref(), Some("CAU"));
        assert_eq!(o.trailers, vec![("x-shard".to_string(), "7".to_string())]);
        h.insert("grpc-status", HeaderValue::from_static("99"));
        assert_eq!(outcome_from(&h).unwrap().code, 2);
        assert!(outcome_from(&HeaderMap::new()).is_none());
        assert_eq!(http_outcome(StatusCode::NOT_FOUND).code, 12);
        assert_eq!(http_outcome(StatusCode::SERVICE_UNAVAILABLE).code, 14);
        assert_eq!(http_outcome(StatusCode::IM_A_TEAPOT).code, 2);
    }

    #[test]
    fn cert_hosts_match_host_and_port_and_refuse_broad_entries() {
        let list = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let h = CertHosts::parse(&list(&[
            "Orders.Example.com",
            "*.svc.example.com:8443",
            "198.51.100.7:50051",
            "[2001:db8::1]",
        ]))
        .unwrap();
        assert!(h.matches("orders.example.com.", 443));
        assert!(
            !h.matches("orders.example.com", 9443),
            "the port must match"
        );
        assert!(h.matches("a.svc.example.com", 8443));
        assert!(!h.matches("a.b.svc.example.com", 8443), "one label only");
        assert!(!h.matches("svc.example.com", 8443));
        assert!(!h.matches("a.svc.example.com", 443));
        assert!(h.matches("198.51.100.7", 50051));
        assert!(h.matches("[2001:db8::1]", 443));
        assert!(h.matches_url("https://orders.example.com/"));
        assert!(
            !h.matches_url("http://orders.example.com:443/"),
            "plain h2c never gets it"
        );
        assert!(!h.matches_url("https://orders.example.com:9443/"));
        for bad in [
            "*",
            "*.com",
            "https://x.example.com",
            "x.example.com/p",
            "x.example.com:0",
            "x.example.com:99999",
            "a_b.example.com",
            "",
            "u@x.example.com",
        ] {
            assert!(CertHosts::parse(&list(&[bad])).is_err(), "{bad}");
        }
    }

    #[test]
    fn hop_counts_come_from_the_header() {
        let mut h = HeaderMap::new();
        assert_eq!(hops(&h), 0);
        h.insert(HOPS_HEADER, HeaderValue::from_static("2"));
        assert_eq!(hops(&h), 2);
        h.insert(HOPS_HEADER, HeaderValue::from_static("x"));
        assert_eq!(hops(&h), MAX_PROXY_HOPS);
    }

    #[tokio::test]
    async fn the_guard_holds_back_writes_until_the_server_sent_settings() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (client, mut server) = tokio::io::duplex(4096);
        let mut g = H2Guard::new(client);
        let mut preface = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
        preface.extend_from_slice(&[0, 0, 6, 4, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 100]);
        g.write_all(&preface).await.unwrap();
        let blocked =
            tokio::time::timeout(Duration::from_millis(100), g.write(b"HEADERS-AND-DATA")).await;
        assert!(
            blocked.is_err(),
            "nothing else before the server's SETTINGS"
        );
        server
            .write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        let mut buf = [0u8; 9];
        g.read_exact(&mut buf).await.unwrap();
        assert_eq!(g.write(b"HEADERS").await.unwrap(), 7);
        let mut got = vec![0u8; preface.len() + 7];
        server.read_exact(&mut got).await.unwrap();
        assert_eq!(&got[preface.len()..], b"HEADERS");

        let (client, mut server) = tokio::io::duplex(4096);
        let mut g = H2Guard::new(client);
        server.write_all(b"+OK\r\n-ERR x\r\n").await.unwrap();
        let mut buf = [0u8; 9];
        assert!(
            g.read_exact(&mut buf).await.is_err(),
            "a first frame that is not SETTINGS ends it"
        );
    }

    #[test]
    fn transcripts_stop_at_the_limits() {
        let mut t = Transcript {
            on: true,
            events: vec![],
            ins: 0,
            outs: 0,
            bytes: 0,
            max_messages: 2,
            max_bytes: 64,
            skip: None,
        };
        t.push(true, Some(&serde_json::json!({"id": "a"})));
        t.push(false, Some(&serde_json::json!({"id": "b"})));
        t.push(false, Some(&serde_json::json!({"id": "c"})));
        assert!(t.skip.is_none());
        t.push(false, Some(&serde_json::json!({"id": "d"})));
        assert_eq!(t.skip, Some("too_large"));
        assert!(t.events.is_empty());
        let mut t = Transcript {
            skip: None,
            events: vec![],
            ins: 0,
            outs: 0,
            bytes: 0,
            on: true,
            max_messages: 10,
            max_bytes: 16,
        };
        t.push(true, Some(&serde_json::json!({"text": "a long message"})));
        assert_eq!(t.skip, Some("too_large"));
        let mut off = Transcript {
            on: false,
            skip: None,
            events: vec![],
            ins: 0,
            outs: 0,
            bytes: 0,
            max_messages: 1,
            max_bytes: 1,
        };
        off.push(true, None);
        assert!(off.skip.is_none());
    }
}
