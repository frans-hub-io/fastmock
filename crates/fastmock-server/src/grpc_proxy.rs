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
    mtls_hosts: EgressPolicy,
    calls: Arc<Semaphore>,
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
            mtls_hosts: EgressPolicy {
                allowed_hosts: cfg.grpc_proxy.client_cert_hosts.clone(),
                denied: vec![],
            },
            calls: Arc::new(Semaphore::new(cfg.grpc_proxy.max_calls)),
        })
    }

    fn tls_for(&self, host: &str) -> Arc<rustls::ClientConfig> {
        match &self.mtls {
            Some(m) if self.mtls_hosts.host_allowed(host) => m.clone(),
            _ => self.tls.clone(),
        }
    }

    /// Opens an HTTP/2 connection to the upstream of `url`, checked against the egress policy.
    async fn connect(&self, policy: &EgressPolicy, url: &reqwest::Url) -> Result<Sender, String> {
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
            "http" => handshake(TokioIo::new(tcp)).await,
            "https" => {
                let name = rustls::pki_types::ServerName::try_from(host.clone())
                    .map_err(|_| format!("{host} is not a valid TLS server name"))?;
                let tls = tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    tokio_rustls::TlsConnector::from(self.tls_for(&host)).connect(name, tcp),
                )
                .await
                .map_err(|_| "the TLS handshake with the upstream timed out".to_string())?
                .map_err(|e| format!("the TLS handshake with the upstream failed: {e}"))?;
                handshake(TokioIo::new(tls)).await
            }
            other => Err(format!(
                "blocked by egress policy: scheme {other} is not allowed"
            )),
        }
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

impl Transcript {
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
            !k.as_str().starts_with("grpc-") && !RESERVED_METADATA.contains(&k.as_str())
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
    let mut sender = match app.grpc_proxy.connect(&app.policy, &url).await {
        Ok(s) => s,
        Err(e) => return unavailable(protocol, req_ct, &log, hit, &path, &e),
    };

    let (req_tx, mut req_rx) = mpsc::channel::<FrameResult>(16);
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
    {
        let transcript = transcript.clone();
        let log = log.clone();
        let method = method.clone();
        let client_failure = client_failure.clone();
        tokio::spawn(
            async move {
                let _held = held;
                for p in raw {
                    if req_tx
                        .send(Ok(Frame::data(crate::grpc::frame(&p, false))))
                        .await
                        .is_err()
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
                            let json = crate::grpc::decode_message(&method, &p);
                            let shown = json
                                .clone()
                                .unwrap_or_else(|e| Value::String(format!("not decoded: {e}")));
                            log.with(|r| r.push_request(&shown));
                            transcript
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push(true, json.as_ref().ok());
                            if req_tx
                                .send(Ok(Frame::data(crate::grpc::frame(&p, false))))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        Ok(None) => return,
                        Err(f) => {
                            let _ = req_tx
                                .send(Err(std::io::Error::other(f.message.clone())))
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

    let (tx, mut rx) = mpsc::channel::<FrameResult>(16);
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
    };
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
                            let json = crate::grpc::decode_output(&method, &p);
                            let shown = json
                                .clone()
                                .unwrap_or_else(|e| Value::String(format!("not decoded: {e}")));
                            transcript
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push(false, json.as_ref().ok());
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
