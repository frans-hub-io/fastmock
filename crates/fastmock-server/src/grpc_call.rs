//! gRPC calls on the data plane: unary, server streaming, client streaming and bidirectional streaming,
//! over gRPC (HTTP/2) and gRPC-Web (HTTP/1.1 or HTTP/2), with gzip, rich status details and reflection.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::{Buf, Bytes, BytesMut};
use fastmock_core::engine::GrpcPlan;
use fastmock_core::grpc_model::{
    DEFAULT_GRPC_SENT, GrpcCompression, GrpcMessage, GrpcResponseDef, GrpcStatus,
};
use fastmock_core::matcher::CompiledBodies;
use fastmock_core::template::{Templates, context};
use fastmock_core::{CapturedRequest, Correlation, EngineResponse, Fault};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use prost::Message as _;
use prost_reflect::{DescriptorPool, MethodDescriptor};
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, mpsc, oneshot};
use tokio::time::Instant;
use tracing::Instrument;

use crate::dataplane::{StubHit, Unmatched};
use crate::grpc::{Encoding, Outcome, Protocol};
use crate::state::AppState;

/// Marks a response that must not be journaled (server reflection).
#[derive(Clone, Copy, Debug)]
pub struct SkipJournal;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Unary,
    ServerStreaming,
    ClientStreaming,
    Bidi,
}

impl Kind {
    pub fn of(m: &MethodDescriptor) -> Kind {
        match (m.is_client_streaming(), m.is_server_streaming()) {
            (false, false) => Kind::Unary,
            (false, true) => Kind::ServerStreaming,
            (true, false) => Kind::ClientStreaming,
            (true, true) => Kind::Bidi,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Unary => "unary",
            Kind::ServerStreaming => "serverStreaming",
            Kind::ClientStreaming => "clientStreaming",
            Kind::Bidi => "bidiStreaming",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Kind::Unary => "unary",
            Kind::ServerStreaming => "server_streaming",
            Kind::ClientStreaming => "client_streaming",
            Kind::Bidi => "bidi_streaming",
        }
    }

    fn many_requests(self) -> bool {
        matches!(self, Kind::ClientStreaming | Kind::Bidi)
    }

    fn many_responses(self) -> bool {
        matches!(self, Kind::ServerStreaming | Kind::Bidi)
    }
}

/// What the journal keeps of a call; filled while the call runs.
#[derive(Debug, Default)]
pub struct CallRecord {
    pub kind: Option<Kind>,
    pub protocol: Option<&'static str>,
    pub request: Vec<Value>,
    pub response: Vec<Value>,
    pub request_count: u64,
    pub response_count: u64,
    pub request_truncated: bool,
    pub response_truncated: bool,
    pub status: u32,
    pub message: String,
    request_bytes: usize,
    response_bytes: usize,
    cap: usize,
}

fn keep(list: &mut Vec<Value>, bytes: &mut usize, truncated: &mut bool, v: &Value, cap: usize) {
    if *truncated {
        return;
    }
    let n = serde_json::to_vec(v).map_or(usize::MAX, |s| s.len()) + 1;
    if bytes.saturating_add(n) > cap {
        *truncated = true;
        return;
    }
    *bytes += n;
    list.push(v.clone());
}

impl CallRecord {
    fn push_request(&mut self, v: &Value) {
        self.request_count += 1;
        keep(
            &mut self.request,
            &mut self.request_bytes,
            &mut self.request_truncated,
            v,
            self.cap,
        );
    }

    fn push_response(&mut self, v: &Value) {
        self.response_count += 1;
        keep(
            &mut self.response,
            &mut self.response_bytes,
            &mut self.response_truncated,
            v,
            self.cap,
        );
    }

    /// The journal `body`: the request message, or the JSON array of the messages of a client stream.
    pub fn body(&self) -> String {
        match self.kind {
            Some(k) if k.many_requests() => Value::Array(self.request.clone()).to_string(),
            _ => self
                .request
                .first()
                .map(Value::to_string)
                .unwrap_or_default(),
        }
    }

    /// The journal `responseBody`: the response message, or the JSON array of the messages sent.
    pub fn response_body(&self) -> String {
        match self.kind {
            Some(k) if k.many_responses() => Value::Array(self.response.clone()).to_string(),
            _ => self
                .response
                .first()
                .map(Value::to_string)
                .unwrap_or_default(),
        }
    }

    pub fn info(&self) -> fastmock_core::GrpcCallInfo {
        fastmock_core::GrpcCallInfo {
            kind: self.kind.unwrap_or(Kind::Unary).name().to_string(),
            status: self.status,
            message: (!self.message.is_empty()).then(|| self.message.clone()),
            request_messages: self.request_count,
            response_messages: self.response_count,
            request_truncated: self.request_truncated,
            response_truncated: self.response_truncated,
            protocol: self.protocol.map(String::from),
        }
    }
}

/// The record of a call and a signal that fires when the call is over.
pub struct CallLog {
    pub record: Mutex<CallRecord>,
    pub done: Mutex<Option<oneshot::Receiver<()>>>,
}

/// Response extension: the journal entry of this gRPC call is written when the call ends.
#[derive(Clone)]
pub struct GrpcCallLog(pub Arc<CallLog>);

impl CallLog {
    fn new(protocol: Protocol, cap: usize) -> (Arc<Self>, oneshot::Sender<()>) {
        let (tx, rx) = oneshot::channel();
        let log = Arc::new(Self {
            record: Mutex::new(CallRecord {
                protocol: protocol.journal_name(),
                cap,
                ..Default::default()
            }),
            done: Mutex::new(Some(rx)),
        });
        (log, tx)
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut CallRecord) -> R) -> R {
        let mut g = self.record.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut g)
    }

    /// Waits until the call is over.
    pub async fn finished(&self) {
        let rx = self.done.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(rx) = rx {
            let _ = rx.await;
        }
    }
}

/// Why reading the request stream stopped.
#[derive(Debug)]
pub struct Failure {
    pub code: u32,
    pub message: String,
}

fn limit_hit(code: u32, message: String, limit: &'static str) -> Failure {
    metrics::counter!("fastmock_grpc_stream_limits_total", "limit" => limit).increment(1);
    Failure { code, message }
}

fn failure(code: u32, message: impl Into<String>) -> Failure {
    Failure {
        code,
        message: message.into(),
    }
}

/// Reads the length-prefixed messages of a request body with all limits applied.
pub struct Incoming {
    body: Body,
    buf: BytesMut,
    text: Option<Vec<u8>>,
    encoding: Option<Encoding>,
    max_message: usize,
    max_messages: u64,
    max_bytes: usize,
    idle: Duration,
    deadline: Instant,
    count: u64,
    total: usize,
    ended: bool,
}

fn decode_text(text: &mut Vec<u8>, out: &mut BytesMut) -> Result<(), ()> {
    let n = text.len() / 4 * 4;
    let mut start = 0;
    let mut i = 0;
    while i < n {
        let group = &text[i..i + 4];
        i += 4;
        if group.contains(&b'=') {
            out.extend_from_slice(&B64.decode(&text[start..i]).map_err(|_| ())?);
            start = i;
        }
    }
    if start < n {
        out.extend_from_slice(&B64.decode(&text[start..n]).map_err(|_| ())?);
    }
    text.drain(..n);
    Ok(())
}

impl Incoming {
    pub fn new(
        app: &AppState,
        body: Body,
        protocol: Protocol,
        encoding: Option<Encoding>,
        deadline: Instant,
    ) -> Self {
        Self {
            body,
            buf: BytesMut::new(),
            text: (protocol == Protocol::WebText).then(Vec::new),
            encoding,
            max_message: app.cfg.max_body_bytes,
            max_messages: app.cfg.grpc_max_stream_messages,
            max_bytes: app.cfg.grpc_max_stream_bytes,
            idle: app.cfg.grpc_stream_idle_timeout,
            deadline,
            count: 0,
            total: 0,
            ended: false,
        }
    }

    fn push(&mut self, d: Bytes) -> Result<(), Failure> {
        match &mut self.text {
            None => self.buf.extend_from_slice(&d),
            Some(t) => {
                t.extend(d.iter().filter(|b| !b.is_ascii_whitespace()));
                decode_text(t, &mut self.buf)
                    .map_err(|_| failure(13, "the grpc-web-text body is not valid base64"))?;
            }
        }
        Ok(())
    }

    /// The next message payload (decompressed), `None` at the end of the request stream.
    pub async fn next(&mut self) -> Result<Option<Bytes>, Failure> {
        loop {
            if self.buf.len() >= 5 {
                let flag = self.buf[0];
                let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]])
                    as usize;
                if flag & !1 != 0 {
                    return Err(failure(13, "invalid gRPC message flags"));
                }
                if len > self.max_message {
                    return Err(limit_hit(
                        8,
                        format!(
                            "request message of {len} bytes exceeds the limit of {} bytes",
                            self.max_message
                        ),
                        "message_bytes",
                    ));
                }
                if self.buf.len() >= 5 + len {
                    self.buf.advance(5);
                    let raw = self.buf.split_to(len).freeze();
                    let payload = if flag & 1 == 1 {
                        if self.encoding != Some(Encoding::Gzip) {
                            return Err(failure(
                                13,
                                "compressed request message without grpc-encoding: gzip",
                            ));
                        }
                        match crate::mail_providers::gunzip(&raw, self.max_message) {
                            Ok(v) => Bytes::from(v),
                            Err(e) if e.contains("too large") => {
                                return Err(limit_hit(
                                    8,
                                    format!(
                                        "decompressed request message exceeds the limit of {} bytes",
                                        self.max_message
                                    ),
                                    "message_bytes",
                                ));
                            }
                            Err(_) => {
                                return Err(failure(13, "request message is not valid gzip"));
                            }
                        }
                    } else {
                        raw
                    };
                    self.count += 1;
                    if self.count > self.max_messages {
                        return Err(limit_hit(
                            8,
                            format!("more than {} request messages", self.max_messages),
                            "messages",
                        ));
                    }
                    self.total = self.total.saturating_add(payload.len());
                    if self.total > self.max_bytes {
                        return Err(limit_hit(
                            8,
                            format!("request messages exceed {} bytes in total", self.max_bytes),
                            "stream_bytes",
                        ));
                    }
                    return Ok(Some(payload));
                }
            }
            if self.ended {
                let pending =
                    !self.buf.is_empty() || self.text.as_ref().is_some_and(|t| !t.is_empty());
                return if pending {
                    Err(failure(13, "truncated gRPC frame"))
                } else {
                    Ok(None)
                };
            }
            let now = Instant::now();
            if now >= self.deadline {
                return Err(limit_hit(
                    4,
                    "the call reached the maximum stream duration".into(),
                    "duration",
                ));
            }
            let wait = self.idle.min(self.deadline - now);
            match tokio::time::timeout(wait, self.body.frame()).await {
                Err(_) if Instant::now() >= self.deadline => {
                    return Err(limit_hit(
                        4,
                        "the call reached the maximum stream duration".into(),
                        "duration",
                    ));
                }
                Err(_) => {
                    return Err(limit_hit(
                        4,
                        format!(
                            "no request message within the idle timeout of {} ms",
                            self.idle.as_millis()
                        ),
                        "idle",
                    ));
                }
                Ok(None) => self.ended = true,
                Ok(Some(Err(_))) => return Err(failure(1, "the request stream was cancelled")),
                Ok(Some(Ok(f))) => {
                    if let Ok(d) = f.into_data() {
                        self.push(d)?;
                    }
                }
            }
        }
    }
}

type FrameTx = mpsc::Sender<Result<Frame<Bytes>, std::io::Error>>;

/// Pause before a stream reset so the frames sent before it reach the client (a reset discards frames
/// still queued for sending).
const FAULT_FLUSH: Duration = Duration::from_millis(50);

/// Writes response frames and keeps the journal record.
struct Out {
    tx: FrameTx,
    protocol: Protocol,
    gzip: bool,
    method: MethodDescriptor,
    log: Arc<CallLog>,
    kind: Kind,
    sent: u32,
}

impl Out {
    async fn data(&self, bytes: Bytes) -> bool {
        let b = if self.protocol == Protocol::WebText {
            Bytes::from(B64.encode(&bytes))
        } else {
            bytes
        };
        self.tx.send(Ok(Frame::data(b))).await.is_ok()
    }

    /// Sends a message; `Err` when it does not fit the output type, `Ok(false)` when the client is gone.
    async fn message(&mut self, json: &Value) -> Result<bool, String> {
        let payload = crate::grpc::encode_message(&self.method, json)?;
        self.log.with(|r| r.push_response(json));
        self.sent += 1;
        metrics::counter!("fastmock_grpc_stream_messages_total", "direction" => "out", "kind" => self.kind.label())
            .increment(1);
        Ok(self.data(crate::grpc::frame(&payload, self.gzip)).await)
    }

    async fn finish(self, outcome: Outcome) {
        self.log.with(|r| {
            r.status = outcome.code;
            r.message = outcome.message.clone();
        });
        match self.protocol {
            Protocol::Grpc => {
                let _ = self
                    .tx
                    .send(Ok(Frame::trailers(outcome.header_map())))
                    .await;
            }
            _ => {
                self.data(outcome.web_frame()).await;
            }
        }
    }

    fn cancelled(&self) {
        self.log.with(|r| {
            r.status = 1;
            r.message = "cancelled by the client".into();
        });
    }

    async fn fault(self, f: Fault) {
        metrics::counter!("fastmock_faults_total", "fault" => format!("{f:?}")).increment(1);
        self.log.with(|r| {
            r.status = 14;
            r.message = format!("fault injected: {f:?}");
        });
        if matches!(
            f,
            Fault::RandomDataThenClose | Fault::MalformedResponseChunk | Fault::MalformedJson
        ) {
            self.data(crate::dataplane::fault_bytes(Fault::RandomDataThenClose))
                .await;
        }
        if self.sent > 0 || f != Fault::ConnectionReset {
            tokio::time::sleep(FAULT_FLUSH).await;
        }
        let _ = self
            .tx
            .send(Err(std::io::Error::other("fault: stream reset")))
            .await;
    }
}

/// Renders the templates of messages, status messages, details and trailers.
struct Renderer {
    templates: Arc<Templates>,
    ctx: Value,
    templated: bool,
}

impl Renderer {
    fn context(&self, extra: &[(&str, Value)]) -> fastmock_core::template::TemplateContext {
        let mut ctx = self.ctx.clone();
        for (k, v) in extra {
            ctx[*k] = v.clone();
        }
        context(ctx)
    }

    fn json(&self, v: &Value, extra: &[(&str, Value)]) -> Result<Value, String> {
        if !self.templated {
            return Ok(v.clone());
        }
        self.templates
            .render_json_in(v, &self.context(extra))
            .map_err(|e| format!("rendering a response message failed: {e}"))
    }

    fn text(&self, s: &str, extra: &[(&str, Value)]) -> Result<String, String> {
        if !self.templated {
            return Ok(s.to_string());
        }
        self.templates
            .render_in(s, &self.context(extra))
            .map_err(|e| format!("rendering failed: {e}"))
    }

    fn outcome(
        &self,
        pool: &DescriptorPool,
        status: Option<&GrpcStatus>,
        trailers: &std::collections::BTreeMap<String, String>,
        extra: &[(&str, Value)],
    ) -> Outcome {
        let mut out = Outcome::default();
        if let Some(s) = status {
            out.code = s.code.value().unwrap_or(2);
            match s
                .message
                .as_deref()
                .map(|m| self.text(m, extra))
                .transpose()
            {
                Ok(m) => out.message = m.unwrap_or_default(),
                Err(e) => return Outcome::status(13, e),
            }
            if !s.details.is_empty() {
                let mut rendered = Vec::with_capacity(s.details.len());
                for d in &s.details {
                    match self.json(d, extra) {
                        Ok(v) => rendered.push(v),
                        Err(e) => return Outcome::status(13, e),
                    }
                }
                match crate::grpc::status_details(pool, out.code, &out.message, &rendered) {
                    Ok(b) => out.details = Some(b),
                    Err(e) => return Outcome::status(13, e),
                }
            }
        }
        for (k, v) in trailers {
            match self.text(v, extra) {
                Ok(v) => out.trailers.push((k.clone(), v)),
                Err(e) => tracing::warn!(trailer = %k, error = %e, "gRPC trailer skipped"),
            }
        }
        out
    }
}

/// A gRPC answer before any message: status (and message) only.
fn trailers_only(protocol: Protocol, req_ct: Option<&str>, outcome: Outcome) -> Response {
    let mut r = match protocol {
        Protocol::Grpc => {
            let frames: Vec<Result<Frame<Bytes>, std::io::Error>> =
                vec![Ok(Frame::trailers(outcome.header_map()))];
            Response::new(Body::new(StreamBody::new(futures::stream::iter(frames))))
        }
        Protocol::Web => Response::new(Body::from(outcome.web_frame())),
        Protocol::WebText => Response::new(Body::from(B64.encode(outcome.web_frame()))),
    };
    *r.status_mut() = StatusCode::OK;
    if let Ok(v) = HeaderValue::from_str(protocol.content_type(req_ct)) {
        r.headers_mut().insert("content-type", v);
    }
    r.headers_mut().insert(
        "grpc-accept-encoding",
        HeaderValue::from_static(crate::grpc::ACCEPT_ENCODING),
    );
    r.extensions_mut().insert(crate::extras::Streaming);
    r
}

fn finished_early(
    protocol: Protocol,
    req_ct: Option<&str>,
    log: &Arc<CallLog>,
    outcome: Outcome,
) -> Response {
    log.with(|r| {
        r.status = outcome.code;
        r.message = outcome.message.clone();
    });
    let mut r = trailers_only(protocol, req_ct, outcome);
    r.extensions_mut().insert(GrpcCallLog(log.clone()));
    r
}

const RESERVED_METADATA: [&str; 12] = [
    "content-type",
    "content-length",
    "grpc-status",
    "grpc-message",
    "grpc-status-details-bin",
    "grpc-encoding",
    "grpc-accept-encoding",
    "connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
];

fn legacy_status(r: &EngineResponse) -> Option<Outcome> {
    let code = r
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("grpc-status"))
        .and_then(|(_, v)| v.trim().parse::<u32>().ok())
        .map(|c| if c > 16 { 2 } else { c })?;
    let message = r
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("grpc-message"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    Some(Outcome::status(code, message))
}

/// Serves one gRPC or gRPC-Web call; `body` is the unread request body.
pub async fn handle(app: &AppState, parts: Parts, body: Body) -> Response {
    let protocol = Protocol::detect(&parts.headers);
    let path = parts.uri.path().to_string();
    let req_ct = parts
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let req_ct = req_ct.as_deref();
    let started = Instant::now();
    let deadline = started + app.cfg.grpc_max_stream_duration;
    let (log, done) = CallLog::new(protocol, app.cfg.journal_max_body);
    let encoding = match Encoding::from_header(&parts.headers) {
        Ok(e) => e,
        Err(name) => {
            drop(done);
            return finished_early(
                protocol,
                req_ct,
                &log,
                Outcome::status(
                    12,
                    format!("grpc-encoding {name} is not supported; use gzip or identity"),
                ),
            );
        }
    };
    let incoming = Incoming::new(app, body, protocol, encoding, deadline);
    if app.cfg.grpc_reflection && crate::grpc_reflection::is_reflection(&path) {
        drop(done);
        return reflection(app, protocol, req_ct, incoming).await;
    }
    let Some(method) = app.grpc.method(&path).await else {
        drop(done);
        let mut r = finished_early(
            protocol,
            req_ct,
            &log,
            Outcome::status(
                12,
                format!("unknown gRPC method {path}: upload its descriptor set first"),
            ),
        );
        r.extensions_mut().insert(Unmatched);
        return r;
    };
    metrics::counter!("fastmock_grpc_calls_total", "method" => path.clone()).increment(1);
    let kind = Kind::of(&method);
    log.with(|r| r.kind = Some(kind));
    call(
        app, parts, protocol, req_ct, method, kind, log, done, incoming,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn call(
    app: &AppState,
    parts: Parts,
    protocol: Protocol,
    req_ct: Option<&str>,
    method: MethodDescriptor,
    kind: Kind,
    log: Arc<CallLog>,
    done: oneshot::Sender<()>,
    mut incoming: Incoming,
) -> Response {
    let path = parts.uri.path().to_string();
    let early = |outcome: Outcome| finished_early(protocol, req_ct, &log, outcome);
    let received = |n: u64| {
        metrics::counter!("fastmock_grpc_stream_messages_total", "direction" => "in", "kind" => kind.label())
            .increment(n);
    };
    let mut held: Vec<OwnedSemaphorePermit> = Vec::new();
    let body = match kind {
        Kind::Unary | Kind::ServerStreaming => {
            let first = match incoming.next().await {
                Ok(Some(p)) => p,
                Ok(None) => return early(Outcome::status(13, "the call has no request message")),
                Err(f) => return early(Outcome::status(f.code, f.message)),
            };
            match incoming.next().await {
                Ok(None) => {}
                Ok(Some(_)) => {
                    return early(Outcome::status(
                        13,
                        "this method takes exactly one request message",
                    ));
                }
                Err(f) => return early(Outcome::status(f.code, f.message)),
            }
            received(1);
            let json = match crate::grpc::decode_message(&method, &first) {
                Ok(j) => j,
                Err(e) => return early(Outcome::status(13, e)),
            };
            log.with(|r| r.push_request(&json));
            let text = json.to_string();
            if text.len() > app.cfg.max_body_bytes.saturating_mul(2) {
                return early(Outcome::status(8, "decoded request is too large"));
            }
            text
        }
        Kind::ClientStreaming => {
            let mut all = Vec::new();
            let mut size = 2usize;
            loop {
                let payload = match incoming.next().await {
                    Ok(Some(p)) => p,
                    Ok(None) => break,
                    Err(f) => return early(Outcome::status(f.code, f.message)),
                };
                received(1);
                let json = match crate::grpc::decode_message(&method, &payload) {
                    Ok(j) => j,
                    Err(e) => return early(Outcome::status(13, e)),
                };
                let n = serde_json::to_vec(&json).map_or(0, |v| v.len()) + 1;
                size += n;
                let permits = (n.saturating_mul(2) / 1024 + 1) as u32;
                match app.body_budget.clone().try_acquire_many_owned(permits) {
                    Ok(p) => held.push(p),
                    Err(_) => {
                        metrics::counter!("fastmock_requests_rejected_total", "reason" => "busy")
                            .increment(1);
                        return early(Outcome::status(
                            8,
                            "server busy: too many large requests in flight",
                        ));
                    }
                }
                log.with(|r| r.push_request(&json));
                all.push(json);
            }
            if size > app.cfg.grpc_max_stream_bytes.saturating_mul(2) {
                return early(Outcome::status(8, "decoded request stream is too large"));
            }
            Value::Array(all).to_string()
        }
        Kind::Bidi => "[]".to_string(),
    };
    let mut hdrs: Vec<(String, String)> = parts
        .headers
        .iter()
        .filter(|(k, _)| *k != "content-type")
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    hdrs.push(("content-type".into(), "application/json".into()));
    let mut cr = CapturedRequest::new("POST", &path, None, hdrs, Bytes::from(body)).with_origin(
        parts.uri.scheme_str().unwrap_or(if app.cfg.tls.is_some() {
            "https"
        } else {
            "http"
        }),
        parts.uri.authority().map(|a| a.as_str()),
        Some(app.cfg.listen.port()).filter(|p| *p != 0),
    );
    cr.grpc_call = true;
    let r = match app.engine.handle(&cr).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            let mut resp = early(Outcome::status(12, format!("no stub matched {path}")));
            resp.extensions_mut().insert(Unmatched);
            return resp;
        }
        Err(e) => {
            metrics::counter!("fastmock_stub_errors_total").increment(1);
            tracing::error!(error = %e, path = %path, "stub evaluation failed");
            return early(Outcome::status(13, format!("stub evaluation failed: {e}")));
        }
    };
    drop(held);
    if !r.messages.is_empty() {
        app.kafka.publish_in_background(r.messages.clone());
    }
    if r.callbacks > 0 {
        metrics::counter!("fastmock_callbacks_scheduled_total", "source" => "stubs")
            .increment(r.callbacks as u64);
    }
    let delay = Duration::from_millis(r.delay_ms).min(app.cfg.max_delay);
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    let hit = StubHit {
        id: r.stub_id,
        name: r.stub_name.clone(),
        space: r.space.clone(),
    };
    let plan = r.grpc.clone().map(|p| *p).unwrap_or(GrpcPlan {
        def: None,
        json_body: None,
        templated: true,
        context: json!({}),
    });
    let gzip = match plan.def.as_ref().and_then(|d| d.compression) {
        Some(GrpcCompression::Gzip) => Encoding::accepted_by_client(&parts.headers, Encoding::Gzip),
        Some(GrpcCompression::Identity) => false,
        None => {
            incoming.encoding == Some(Encoding::Gzip)
                && Encoding::accepted_by_client(&parts.headers, Encoding::Gzip)
        }
    };
    let (tx, mut rx) = mpsc::channel::<Result<Frame<Bytes>, std::io::Error>>(16);
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
    if gzip {
        h.insert("grpc-encoding", HeaderValue::from_static("gzip"));
    }
    for (k, v) in &r.headers {
        let lower = k.to_ascii_lowercase();
        if RESERVED_METADATA.contains(&lower.as_str()) || lower.starts_with(':') {
            continue;
        }
        match (
            HeaderName::from_bytes(lower.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            (Ok(n), Ok(v)) => {
                h.append(n, v);
            }
            _ => {
                metrics::counter!("fastmock_invalid_response_headers_total").increment(1);
            }
        }
    }
    resp.extensions_mut().insert(crate::extras::Streaming);
    resp.extensions_mut().insert(hit);
    if let Some(c) = r.correlation.clone() {
        resp.extensions_mut().insert(Correlation(c));
    }
    resp.extensions_mut().insert(GrpcCallLog(log.clone()));
    let out = Out {
        tx,
        protocol,
        gzip,
        method,
        log,
        kind,
        sent: 0,
    };
    let driver = Driver {
        renderer: Renderer {
            templates: app.engine.templates().clone(),
            ctx: plan.context.clone(),
            templated: plan.templated,
        },
        pool: app.grpc.pool().await,
        legacy: legacy_status(&r),
        fault: r.fault,
        def: plan.def.unwrap_or_default(),
        json_body: plan.json_body,
        max_delay: app.cfg.max_delay,
        deadline: incoming.deadline,
    };
    tokio::spawn(
        async move {
            metrics::gauge!("fastmock_grpc_streams_active").increment(1.0);
            match kind {
                Kind::Unary | Kind::ClientStreaming => driver.single(out).await,
                Kind::ServerStreaming => driver.server_stream(out).await,
                Kind::Bidi => driver.bidi(out, incoming).await,
            }
            metrics::gauge!("fastmock_grpc_streams_active").decrement(1.0);
            drop(done);
        }
        .instrument(tracing::Span::current()),
    );
    resp
}

struct Driver {
    renderer: Renderer,
    pool: DescriptorPool,
    legacy: Option<Outcome>,
    fault: Option<Fault>,
    def: GrpcResponseDef,
    json_body: Option<Value>,
    max_delay: Duration,
    deadline: Instant,
}

enum Sent {
    Ok,
    Gone,
    Failed(String),
}

impl Driver {
    fn delay(&self, ms: u64) -> Duration {
        Duration::from_millis(ms).min(self.max_delay)
    }

    fn end_deadline(&self) -> Instant {
        match self.def.close_after_ms {
            Some(ms) => self
                .deadline
                .min(Instant::now() + Duration::from_millis(ms)),
            None => self.deadline,
        }
    }

    fn final_outcome(&self, extra: &[(&str, Value)]) -> Outcome {
        let mut o = self.renderer.outcome(
            &self.pool,
            self.def.status.as_ref(),
            &self.def.trailers,
            extra,
        );
        if self.def.status.is_none()
            && let Some(l) = &self.legacy
        {
            o.code = l.code;
            o.message = l.message.clone();
        }
        o
    }

    /// Whether the call ends with an error before any message (a non-OK unary status, or the
    /// `grpc-status` response header).
    fn error_first(&self, many: bool) -> bool {
        if self.def.status.is_none() {
            return self.legacy.as_ref().is_some_and(|l| l.code != 0);
        }
        !many
            && self
                .def
                .status
                .as_ref()
                .and_then(|s| s.code.value())
                .is_some_and(|c| c != 0)
    }

    async fn send(&self, out: &mut Out, m: &GrpcMessage, extra: &[(&str, Value)]) -> Sent {
        let wait = self.delay(m.delay_ms);
        if !wait.is_zero() {
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = out.tx.closed() => return Sent::Gone,
            }
        }
        let json = match self.renderer.json(&m.json_body, extra) {
            Ok(j) => j,
            Err(e) => return Sent::Failed(e),
        };
        match out.message(&json).await {
            Ok(true) => Sent::Ok,
            Ok(false) => Sent::Gone,
            Err(e) => Sent::Failed(e),
        }
    }

    async fn end(&self, out: Out, extra: &[(&str, Value)]) {
        match self.fault {
            Some(f) => out.fault(f).await,
            None => {
                let o = self.final_outcome(extra);
                out.finish(o).await
            }
        }
    }

    /// Unary and client-streaming calls: one message, then the status.
    async fn single(self, mut out: Out) {
        if let Some(f) = self.fault {
            return out.fault(f).await;
        }
        if self.error_first(false) {
            let o = self.final_outcome(&[]);
            return out.finish(o).await;
        }
        let m = self
            .def
            .messages
            .first()
            .cloned()
            .unwrap_or_else(|| GrpcMessage {
                json_body: self.json_body.clone().unwrap_or_else(|| json!({})),
                delay_ms: 0,
            });
        match self.send(&mut out, &m, &[]).await {
            Sent::Ok => {
                let o = self.final_outcome(&[]);
                out.finish(o).await
            }
            Sent::Gone => out.cancelled(),
            Sent::Failed(e) => out.finish(Outcome::status(13, e)).await,
        }
    }

    async fn server_stream(self, mut out: Out) {
        if self.error_first(true) {
            let o = self.final_outcome(&[]);
            return out.finish(o).await;
        }
        let script: Vec<GrpcMessage> = if self.def.messages.is_empty() {
            self.json_body
                .iter()
                .map(|j| GrpcMessage {
                    json_body: j.clone(),
                    delay_ms: 0,
                })
                .collect()
        } else {
            self.def.messages.clone()
        };
        let max = self.def.max_messages.unwrap_or(DEFAULT_GRPC_SENT);
        let end_at = self.end_deadline();
        let mut idx = 0usize;
        while idx < script.len() && out.sent < max {
            let extra = [("stream", json!({"index": out.sent}))];
            let sent = tokio::select! {
                s = self.send(&mut out, &script[idx], &extra) => s,
                _ = tokio::time::sleep_until(end_at) => break,
            };
            match sent {
                Sent::Ok => {}
                Sent::Gone => return out.cancelled(),
                Sent::Failed(e) => return out.finish(Outcome::status(13, e)).await,
            }
            idx += 1;
            if idx == script.len() && self.def.repeat {
                idx = 0;
            }
        }
        self.end(out, &[]).await
    }

    async fn bidi(self, mut out: Out, mut incoming: Incoming) {
        if self.error_first(true) {
            let o = self.final_outcome(&[]);
            return out.finish(o).await;
        }
        let mut rules = Vec::with_capacity(self.def.on_message.len());
        for r in &self.def.on_message {
            match CompiledBodies::compile(&r.matcher) {
                Ok(c) => rules.push((c, r)),
                Err(e) => {
                    return out
                        .finish(Outcome::status(13, format!("invalid onMessage rule: {e}")))
                        .await;
                }
            }
        }
        let script = &self.def.messages;
        let max = self.def.max_messages.unwrap_or(DEFAULT_GRPC_SENT);
        let end_at = self.end_deadline();
        let mut idx = 0usize;
        let mut next_at = script
            .first()
            .map(|m| Instant::now() + self.delay(m.delay_ms));
        let mut client_open = true;
        let mut index = 0u64;
        let kind = out.kind;
        enum Event {
            End,
            Gone,
            Tick,
            Incoming(Result<Option<Bytes>, Failure>),
        }
        loop {
            if out.sent >= max || (!client_open && (next_at.is_none() || self.def.repeat)) {
                break;
            }
            let event = if next_at.is_some_and(|t| t <= Instant::now()) {
                Event::Tick
            } else {
                let tick = async {
                    match next_at {
                        Some(t) => tokio::time::sleep_until(t).await,
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    biased;
                    _ = tokio::time::sleep_until(end_at) => Event::End,
                    _ = out.tx.closed() => Event::Gone,
                    _ = tick => Event::Tick,
                    m = incoming.next(), if client_open => Event::Incoming(m),
                }
            };
            match event {
                Event::End => break,
                Event::Gone => return out.cancelled(),
                Event::Tick => {
                    let m = GrpcMessage {
                        json_body: script[idx].json_body.clone(),
                        delay_ms: 0,
                    };
                    let extra = [("stream", json!({"index": out.sent}))];
                    match self.send(&mut out, &m, &extra).await {
                        Sent::Ok => {}
                        Sent::Gone => return out.cancelled(),
                        Sent::Failed(e) => return out.finish(Outcome::status(13, e)).await,
                    }
                    idx += 1;
                    if idx >= script.len() {
                        if self.def.repeat {
                            idx = 0;
                        } else {
                            next_at = None;
                            continue;
                        }
                    }
                    next_at = Some(Instant::now() + self.delay(script[idx].delay_ms));
                }
                Event::Incoming(m) => {
                    let payload = match m {
                        Ok(Some(p)) => p,
                        Ok(None) => {
                            client_open = false;
                            continue;
                        }
                        Err(f) => return out.finish(Outcome::status(f.code, f.message)).await,
                    };
                    metrics::counter!("fastmock_grpc_stream_messages_total", "direction" => "in", "kind" => kind.label()).increment(1);
                    let json = match crate::grpc::decode_message(&out.method, &payload) {
                        Ok(j) => j,
                        Err(e) => return out.finish(Outcome::status(13, e)).await,
                    };
                    out.log.with(|r| r.push_request(&json));
                    let extra = [("message", json!({"json": json, "index": index}))];
                    index += 1;
                    match rules.iter().find(|(c, _)| c.matches_json(&json)) {
                        Some((_, rule)) => {
                            for m in &rule.messages {
                                if out.sent >= max {
                                    break;
                                }
                                match self.send(&mut out, m, &extra).await {
                                    Sent::Ok => {}
                                    Sent::Gone => return out.cancelled(),
                                    Sent::Failed(e) => {
                                        return out.finish(Outcome::status(13, e)).await;
                                    }
                                }
                            }
                            if let Some(s) = &rule.status {
                                let o = self.renderer.outcome(
                                    &self.pool,
                                    Some(s),
                                    &self.def.trailers,
                                    &extra,
                                );
                                return out.finish(o).await;
                            }
                        }
                        None => {
                            if let Some(j) = &self.json_body {
                                let m = GrpcMessage {
                                    json_body: j.clone(),
                                    delay_ms: 0,
                                };
                                match self.send(&mut out, &m, &extra).await {
                                    Sent::Ok => {}
                                    Sent::Gone => return out.cancelled(),
                                    Sent::Failed(e) => {
                                        return out.finish(Outcome::status(13, e)).await;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        self.end(out, &[]).await
    }
}

/// Server reflection: a bidirectional stream answering each request from the loaded descriptors.
async fn reflection(
    app: &AppState,
    protocol: Protocol,
    req_ct: Option<&str>,
    mut incoming: Incoming,
) -> Response {
    let pool = app.grpc.pool().await;
    let (tx, mut rx) = mpsc::channel::<Result<Frame<Bytes>, std::io::Error>>(16);
    let stream = futures::stream::poll_fn(move |cx| rx.poll_recv(cx));
    let mut resp = Response::new(Body::new(StreamBody::new(stream)));
    if let Ok(v) = HeaderValue::from_str(protocol.content_type(req_ct)) {
        resp.headers_mut().insert("content-type", v);
    }
    resp.extensions_mut().insert(crate::extras::Streaming);
    resp.extensions_mut().insert(SkipJournal);
    metrics::counter!("fastmock_grpc_reflection_calls_total").increment(1);
    tokio::spawn(async move {
        let send = |b: Bytes| {
            let tx = tx.clone();
            async move {
                let b = if protocol == Protocol::WebText {
                    Bytes::from(B64.encode(&b))
                } else {
                    b
                };
                tx.send(Ok(Frame::data(b))).await.is_ok()
            }
        };
        let outcome = loop {
            match incoming.next().await {
                Ok(None) => break Outcome::status(0, ""),
                Err(f) => break Outcome::status(f.code, f.message),
                Ok(Some(p)) => {
                    let Ok(req) = crate::grpc_reflection::ServerReflectionRequest::decode(&p[..])
                    else {
                        break Outcome::status(13, "invalid reflection request");
                    };
                    let answer = crate::grpc_reflection::answer(&pool, req);
                    if !send(crate::grpc::frame(&answer.encode_to_vec(), false)).await {
                        return;
                    }
                }
            }
        };
        match protocol {
            Protocol::Grpc => {
                let _ = tx.send(Ok(Frame::trailers(outcome.header_map()))).await;
            }
            _ => {
                send(outcome.web_frame()).await;
            }
        }
    });
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_padded_text_segments() {
        let mut text = Vec::new();
        text.extend_from_slice(B64.encode(b"ab").as_bytes());
        text.extend_from_slice(B64.encode(b"cde").as_bytes());
        text.extend_from_slice(b"Zm");
        let mut out = BytesMut::new();
        decode_text(&mut text, &mut out).unwrap();
        assert_eq!(&out[..], b"abcde");
        assert_eq!(text, b"Zm");
        text.extend_from_slice(b"9v");
        decode_text(&mut text, &mut out).unwrap();
        assert_eq!(&out[..], b"abcdefoo");
    }

    #[test]
    fn records_are_bounded() {
        let mut r = CallRecord {
            kind: Some(Kind::ClientStreaming),
            cap: 30,
            ..Default::default()
        };
        for i in 0..10 {
            r.push_request(&json!({"i": i}));
        }
        assert_eq!(r.request_count, 10);
        assert!(r.request_truncated);
        assert_eq!(r.request.len(), 3);
        assert_eq!(r.body(), r#"[{"i":0},{"i":1},{"i":2}]"#);
        assert_eq!(r.info().kind, "clientStreaming");
    }
}
