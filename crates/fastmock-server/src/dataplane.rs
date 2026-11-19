use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use chrono::Utc;
use fastmock_core::plugin::TRACEPARENT;
use fastmock_core::{CapturedRequest, Correlation, Fault, JournalEntry};
use futures::{StreamExt, stream};
use serde_json::json;
use tracing::Instrument;
use uuid::Uuid;

use crate::state::AppState;
use crate::telemetry;

#[derive(Clone, Debug)]
pub struct StubHit {
    pub id: Uuid,
    pub name: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct Unmatched;

#[derive(Clone, Copy, Debug)]
struct ResetMarker;

pub const TRACE_HEADER: &str = "x-fastmock-trace-id";
const MAX_JOURNAL_RESPONSE: usize = 16 * 1024 * 1024;

fn status_class(s: u16) -> &'static str {
    match s {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        _ => "5xx",
    }
}

pub async fn data_plane_middleware(
    State(app): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let start = Instant::now();
    let (mut parts, body) = req.into_parts();
    parts.headers.remove(crate::tls::CLIENT_CERT_HEADER);
    if let Some(c) = parts.extensions.get::<crate::tls::ClientCert>()
        && let Ok(v) = HeaderValue::from_str(&c.0)
    {
        parts
            .headers
            .insert(HeaderName::from_static(crate::tls::CLIENT_CERT_HEADER), v);
    }
    let source = app.source_for(parts.uri.path());
    let declared = parts
        .headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|n| n > app.cfg.max_body_bytes) {
        metrics::counter!("fastmock_requests_rejected_total", "reason" => "body_too_large")
            .increment(1);
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            axum::Json(json!({"error": "request body too large"})),
        )
            .into_response();
    }
    let reserve = declared
        .unwrap_or(app.cfg.max_body_bytes)
        .min(app.cfg.max_body_bytes);
    let permits = (reserve.saturating_mul(3) / 1024)
        .clamp(1, (app.cfg.max_inflight_body_bytes / 1024).max(1)) as u32;
    let _budget = match tokio::time::timeout(
        Duration::from_secs(10),
        app.body_budget.clone().acquire_many_owned(permits),
    )
    .await
    {
        Ok(Ok(p)) => p,
        _ => {
            metrics::counter!("fastmock_requests_rejected_total", "reason" => "busy").increment(1);
            let mut r = (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(json!({"error": "server busy: too many large requests in flight"})),
            )
                .into_response();
            r.headers_mut()
                .insert("retry-after", HeaderValue::from_static("1"));
            return r;
        }
    };
    let bytes = match to_bytes(body, app.cfg.max_body_bytes).await {
        Ok(b) => b,
        Err(_) => {
            metrics::counter!("fastmock_requests_rejected_total", "reason" => "body_too_large")
                .increment(1);
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                axum::Json(json!({"error": "request body too large"})),
            )
                .into_response();
        }
    };
    let incoming_tp = parts
        .headers
        .get("traceparent")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let span = tracing::info_span!(
        "fastmock.request",
        otel.name = %format!("{} {}", parts.method, source),
        otel.kind = "server",
        http.request.method = %parts.method,
        url.path = %parts.uri.path(),
        fastmock.source = %source,
        http.response.status_code = tracing::field::Empty,
        fastmock.stub_id = tracing::field::Empty,
        trace_id = tracing::field::Empty,
    );
    telemetry::attach_parent(&span, &parts.headers);
    let ids = telemetry::ids_for(&span, incoming_tp.as_deref());
    span.record("trace_id", ids.trace_id.as_str());
    let journal_headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let method = parts.method.to_string();
    let path = parts.uri.path().to_string();
    let raw_query = parts.uri.query().unwrap_or("").to_string();
    let content_type = parts
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let chaos = crate::extras::chaos(&app, &source, parts.method.as_str(), parts.uri.path()).await;
    let req = Request::from_parts(parts, Body::from(bytes.clone()));
    let tp = Some(ids.traceparent());
    let mut chaos_rule = None;
    let mut resp = match chaos {
        crate::extras::ChaosOutcome::Respond(r, id) => {
            chaos_rule = Some(id);
            r
        }
        crate::extras::ChaosOutcome::Delay(d, id) => {
            chaos_rule = Some(id);
            tokio::time::sleep(d).await;
            TRACEPARENT
                .scope(tp, next.run(req).instrument(span.clone()))
                .await
        }
        crate::extras::ChaosOutcome::None => {
            TRACEPARENT
                .scope(tp, next.run(req).instrument(span.clone()))
                .await
        }
    };
    if let Some(rule) = &chaos_rule
        && let Ok(v) = HeaderValue::from_str(rule)
    {
        resp.headers_mut()
            .insert(HeaderName::from_static("x-fastmock-chaos"), v);
    }
    let status = resp.status().as_u16();
    span.record("http.response.status_code", status);
    let stub = resp.extensions().get::<StubHit>().cloned();
    if let Some(s) = &stub {
        span.record("fastmock.stub_id", s.id.to_string().as_str());
    }
    let matched = resp.extensions().get::<Unmatched>().is_none();
    let correlation = resp.extensions().get::<Correlation>().map(|c| c.0.clone());
    let reset = resp.extensions().get::<ResetMarker>().is_some()
        || resp
            .extensions()
            .get::<crate::extras::Streaming>()
            .is_some();
    let decoded_body = resp
        .extensions()
        .get::<crate::grpc::DecodedBody>()
        .map(|d| d.0.clone());
    if let Ok(v) = HeaderValue::from_str(&ids.trace_id) {
        resp.headers_mut()
            .insert(HeaderName::from_static(TRACE_HEADER), v);
    }
    let exact = axum::body::HttpBody::size_hint(resp.body()).exact();
    let (resp, response_body) = if reset
        || !app.cfg.journal_enabled
        || !exact.is_some_and(|n| n as usize <= MAX_JOURNAL_RESPONSE)
    {
        (resp, Bytes::new())
    } else {
        let (rparts, rbody) = resp.into_parts();
        match to_bytes(rbody, MAX_JOURNAL_RESPONSE).await {
            Ok(b) => (Response::from_parts(rparts, Body::from(b.clone())), b),
            Err(e) => {
                tracing::warn!(error = %e, "reading the response for the journal failed");
                (Response::from_parts(rparts, Body::empty()), Bytes::new())
            }
        }
    };
    let elapsed = start.elapsed();
    let labels = [
        ("source", source.clone()),
        ("matched", matched.to_string()),
        ("status_class", status_class(status).to_string()),
    ];
    metrics::counter!("fastmock_requests_total", &labels).increment(1);
    metrics::histogram!("fastmock_request_duration_seconds", "source" => source.clone())
        .record(elapsed.as_secs_f64());
    if !matched {
        metrics::counter!("fastmock_unmatched_requests_total", "source" => source.clone())
            .increment(1);
    }
    if let Some(s) = &stub {
        let label = s.name.clone().unwrap_or_else(|| s.id.to_string());
        metrics::counter!("fastmock_stub_hits_total", "stub" => label).increment(1);
    }
    let slot = if app.cfg.journal_enabled {
        match app.journal_tx.clone().try_reserve_owned() {
            Ok(p) => Some(p),
            Err(_) => {
                metrics::counter!("fastmock_journal_dropped_total").increment(1);
                None
            }
        }
    } else {
        None
    };
    if let Some(slot) = slot {
        let r = &app.redactor;
        let resp_ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok());
        let entry = JournalEntry {
            id: 0,
            at: Utc::now(),
            source,
            method,
            path,
            query: r.query(&raw_query),
            headers: r.headers(
                journal_headers
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str())),
            ),
            body: match &decoded_body {
                Some(d) => r.body(
                    d.as_bytes(),
                    Some("application/json"),
                    app.cfg.journal_max_body,
                ),
                None => r.body(&bytes, content_type.as_deref(), app.cfg.journal_max_body),
            },
            status,
            response_body: r.body(&response_body, resp_ct, app.cfg.journal_max_body),
            stub_id: stub.map(|s| s.id),
            correlation,
            matched,
            duration_ms: elapsed.as_millis() as u64,
            trace_id: Some(ids.trace_id),
        };
        slot.send(entry);
    }
    resp
}

#[derive(Clone, Debug)]
pub struct RawFault(pub Bytes);

/// On HTTP/2 the stream is reset before any frame of the response is sent.
#[derive(Clone, Copy, Debug)]
pub struct AbortStream;

#[derive(Clone, Default)]
pub struct FaultSlot(std::sync::Arc<std::sync::Mutex<Option<Bytes>>>);

impl FaultSlot {
    pub fn arm(&self, bytes: Bytes) {
        if let Ok(mut g) = self.0.lock() {
            *g = Some(bytes);
        }
    }

    fn take(&self) -> Option<Bytes> {
        self.0.lock().ok().and_then(|mut g| g.take())
    }

    pub fn arm_from<B>(&self, resp: &http::Response<B>) {
        if let Some(f) = resp.extensions().get::<RawFault>() {
            self.arm(f.0.clone());
        }
    }
}

pub struct FaultIo<I> {
    inner: I,
    slot: FaultSlot,
    pending: Option<(Bytes, usize)>,
    closed: bool,
}

impl<I> FaultIo<I> {
    pub fn new(inner: I, slot: FaultSlot) -> Self {
        Self {
            inner,
            slot,
            pending: None,
            closed: false,
        }
    }
}

impl<I: hyper::rt::Read + Unpin> hyper::rt::Read for FaultIo<I> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.closed {
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<I: hyper::rt::Write + Unpin> FaultIo<I> {
    fn poll_fault(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> Option<std::task::Poll<std::io::Result<usize>>> {
        use std::task::Poll;
        if self.closed {
            return Some(Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "fault injected",
            ))));
        }
        if self.pending.is_none()
            && let Some(b) = self.slot.take()
        {
            self.pending = Some((b, 0));
        }
        let (bytes, offset) = self.pending.as_mut()?;
        while *offset < bytes.len() {
            match std::pin::Pin::new(&mut self.inner).poll_write(cx, &bytes[*offset..]) {
                Poll::Ready(Ok(0)) => break,
                Poll::Ready(Ok(n)) => *offset += n,
                Poll::Ready(Err(e)) => return Some(Poll::Ready(Err(e))),
                Poll::Pending => return Some(Poll::Pending),
            }
        }
        if std::pin::Pin::new(&mut self.inner)
            .poll_shutdown(cx)
            .is_pending()
        {
            return Some(Poll::Pending);
        }
        self.closed = true;
        self.pending = None;
        Some(Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "fault injected",
        ))))
    }
}

impl<I: hyper::rt::Write + Unpin> hyper::rt::Write for FaultIo<I> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = &mut *self;
        if let Some(p) = this.poll_fault(cx) {
            return p;
        }
        std::pin::Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = &mut *self;
        if let Some(p) = this.poll_fault(cx) {
            return p;
        }
        std::pin::Pin::new(&mut this.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
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

pub const MALFORMED_CHUNK: &[u8] =
    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nlskdu018973t09sylgasjkfg1][]'./.sdlv";

pub fn fault_bytes(f: Fault) -> Bytes {
    match f {
        Fault::MalformedResponseChunk => Bytes::from_static(MALFORMED_CHUNK),
        Fault::EmptyResponse => Bytes::new(),
        _ => Bytes::from((0..128).map(|_| rand::random::<u8>()).collect::<Vec<u8>>()),
    }
}

pub fn raw_fault_response(f: Fault) -> Response {
    let raw = fault_bytes(f);
    let fallback = match f {
        Fault::MalformedResponseChunk => {
            Bytes::from_static(b"lskdu018973t09sylgasjkfg1][]'./.sdlv")
        }
        _ => raw.clone(),
    };
    let body = Body::from_stream(
        stream::once(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok::<Bytes, std::io::Error>(fallback)
        })
        .chain(stream::once(async {
            Err(std::io::Error::other("fault: connection closed"))
        })),
    );
    let mut r = Response::new(body);
    if f == Fault::EmptyResponse {
        r.extensions_mut().insert(AbortStream);
    }
    r.extensions_mut().insert(RawFault(raw));
    r.extensions_mut().insert(ResetMarker);
    r
}

pub fn dribble_body(
    body: Bytes,
    d: &fastmock_core::ChunkedDribbleDelay,
    max_delay: Duration,
) -> Body {
    let n = (d.number_of_chunks.max(1) as usize).min(body.len().max(1));
    let total = Duration::from_millis(d.total_duration_ms).min(max_delay);
    let interval = total / n as u32;
    let size = body.len().div_ceil(n).max(1);
    let chunks: Vec<Bytes> = (0..n)
        .map(|i| body.slice((i * size).min(body.len())..((i + 1) * size).min(body.len())))
        .collect();
    Body::from_stream(stream::unfold(
        chunks.into_iter(),
        move |mut it| async move {
            let c = it.next()?;
            if !interval.is_zero() {
                tokio::time::sleep(interval).await;
            }
            Some((Ok::<Bytes, std::io::Error>(c), it))
        },
    ))
}

fn reset_response() -> Response {
    let body = Body::from_stream(stream::once(async {
        Err::<Bytes, std::io::Error>(std::io::Error::other("fault: connection reset"))
    }));
    let mut r = Response::new(body);
    r.extensions_mut().insert(ResetMarker);
    r
}

pub async fn stub_handler(State(app): State<AppState>, req: Request) -> Response {
    let (mut parts, body) = req.into_parts();
    let bytes = to_bytes(body, usize::MAX).await.unwrap_or_default();
    let headers = parts
        .headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    if crate::grpc::is_grpc(&parts.headers) {
        return grpc_handler(&app, parts.uri.path(), headers, &bytes).await;
    }
    let cr = CapturedRequest::new(
        parts.method.as_str(),
        parts.uri.path(),
        parts.uri.query(),
        headers,
        bytes.clone(),
    )
    .with_origin(
        parts.uri.scheme_str().unwrap_or(if app.cfg.tls.is_some() {
            "https"
        } else {
            "http"
        }),
        parts.uri.authority().map(|a| a.as_str()),
        Some(app.cfg.listen.port()).filter(|p| *p != 0),
    );
    match app.engine.handle(&cr).await {
        Ok(Some(r)) if crate::websocket::wants_upgrade(&app, &parts.headers, r.stub_id) => {
            crate::websocket::upgrade(&app, &mut parts, &cr, r).await
        }
        Ok(Some(r)) if r.sse.is_some() => {
            if !r.messages.is_empty() {
                app.kafka.publish_in_background(r.messages.clone());
            }
            let delay = Duration::from_millis(r.delay_ms).min(app.cfg.max_delay);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let mut resp = crate::extras::sse_response(
                r.sse.clone().unwrap_or_default(),
                r.status,
                app.cfg.max_delay,
            );
            resp.extensions_mut().insert(StubHit {
                id: r.stub_id,
                name: r.stub_name.clone(),
            });
            if let Some(c) = r.correlation {
                resp.extensions_mut().insert(Correlation(c));
            }
            resp
        }
        Ok(Some(r)) if r.proxy.is_some() && !app.engine.settings().await.proxy_pass_through => {
            metrics::counter!("fastmock_proxy_pass_through_blocked_total").increment(1);
            let mut resp = (
                StatusCode::NOT_FOUND,
                axum::Json(json!({"error": "proxy pass-through is disabled", "method": cr.method, "path": cr.path})),
            )
                .into_response();
            resp.extensions_mut().insert(StubHit {
                id: r.stub_id,
                name: r.stub_name.clone(),
            });
            resp
        }
        Ok(Some(r)) if r.proxy.is_some() => {
            let delay = Duration::from_millis(r.delay_ms).min(app.cfg.max_delay);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let def = r.proxy.clone().unwrap();
            let mut resp = match crate::proxy::forward(
                &app,
                &def,
                parts.method.as_str(),
                parts.uri.path(),
                parts.uri.query(),
                &parts.headers,
                bytes,
            )
            .await
            {
                Ok(u) => crate::proxy::to_response(u),
                Err(e) => crate::proxy::bad_gateway(&e),
            };
            resp.extensions_mut().insert(StubHit {
                id: r.stub_id,
                name: r.stub_name.clone(),
            });
            resp
        }
        Ok(Some(r)) => {
            if !r.messages.is_empty() {
                app.kafka.publish_in_background(r.messages.clone());
            }
            let delay = Duration::from_millis(r.delay_ms).min(app.cfg.max_delay);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if r.callbacks > 0 {
                metrics::counter!("fastmock_callbacks_scheduled_total", "source" => "stubs")
                    .increment(r.callbacks as u64);
            }
            let hit = StubHit {
                id: r.stub_id,
                name: r.stub_name.clone(),
            };
            let mut resp = match r.fault {
                Some(Fault::ConnectionReset) => reset_response(),
                Some(
                    f @ (Fault::RandomDataThenClose
                    | Fault::MalformedResponseChunk
                    | Fault::EmptyResponse),
                ) => raw_fault_response(f),
                Some(Fault::MalformedJson) => {
                    let mut x = Response::new(Body::from("{\"fastmock\": [1, 2,"));
                    x.headers_mut()
                        .insert("content-type", HeaderValue::from_static("application/json"));
                    x
                }
                None => {
                    let mut x = match &r.chunked_dribble_delay {
                        Some(d) => {
                            let mut x =
                                Response::new(dribble_body(r.body.clone(), d, app.cfg.max_delay));
                            x.extensions_mut().insert(crate::extras::Streaming);
                            x
                        }
                        None => Response::new(Body::from(r.body)),
                    };
                    if let Some(m) = &r.status_message
                        && let Ok(p) = hyper::ext::ReasonPhrase::try_from(m.as_bytes().to_vec())
                    {
                        x.extensions_mut().insert(p);
                    }
                    for (k, v) in &r.headers {
                        match (
                            HeaderName::from_bytes(k.as_bytes()),
                            HeaderValue::from_str(v),
                        ) {
                            (Ok(n), Ok(v)) => {
                                x.headers_mut().append(n, v);
                            }
                            _ => {
                                metrics::counter!("fastmock_invalid_response_headers_total")
                                    .increment(1);
                                tracing::warn!(header = %k, stub_id = %r.stub_id, "rendered response header is invalid and was dropped");
                            }
                        }
                    }
                    x
                }
            };
            if !matches!(
                r.fault,
                Some(
                    Fault::ConnectionReset
                        | Fault::RandomDataThenClose
                        | Fault::MalformedResponseChunk
                        | Fault::EmptyResponse
                )
            ) {
                *resp.status_mut() = StatusCode::from_u16(r.status).unwrap_or(StatusCode::OK);
            }
            if let Some(f) = r.fault {
                metrics::counter!("fastmock_faults_total", "fault" => format!("{f:?}"))
                    .increment(1);
            }
            resp.extensions_mut().insert(hit);
            if let Some(c) = r.correlation {
                resp.extensions_mut().insert(Correlation(c));
            }
            resp
        }
        Ok(None) if crate::proxy::active_recording(&app).await.is_some() => {
            let rec = crate::proxy::active_recording(&app).await.unwrap();
            let def = fastmock_core::ProxyDef {
                base_url: rec.target_base_url.clone(),
                additional_headers: Default::default(),
                remove_headers: vec![],
                strip_prefix: None,
            };
            match crate::proxy::forward(
                &app,
                &def,
                parts.method.as_str(),
                parts.uri.path(),
                parts.uri.query(),
                &parts.headers,
                bytes.clone(),
            )
            .await
            {
                Ok(u) => {
                    let stub = crate::proxy::recorded_stub(
                        &app.redactor,
                        &rec,
                        parts.method.as_str(),
                        parts.uri.path(),
                        parts.uri.query(),
                        &parts.headers,
                        &bytes,
                        &u,
                    );
                    let key = format!(
                        "recording:seen:{}",
                        fastmock_core::template::hmac_bytes(
                            "sha256",
                            b"rec",
                            format!(
                                "{}|{}|{}|{}",
                                cr.method,
                                cr.path,
                                cr.raw_query(),
                                cr.body_text()
                            )
                            .as_bytes()
                        )
                        .map(hex::encode)
                        .unwrap_or_default()
                    );
                    let mut hit = None;
                    let fresh = app.stores.hot.get(&key).await.ok().flatten().is_none();
                    let within_limit = fresh
                        && app
                            .stores
                            .hot
                            .incr("recording:count", Some(Duration::from_secs(86_400)))
                            .await
                            .map(|n| n <= app.cfg.recording_max_stubs)
                            .unwrap_or(false);
                    if fresh && !within_limit {
                        metrics::counter!("fastmock_recording_skipped_total", "reason" => "limit")
                            .increment(1);
                    }
                    if within_limit {
                        let saved = match app.engine.validate(&stub) {
                            Err(e) => Err(anyhow::anyhow!(e)),
                            Ok(()) => app.stores.stubs.upsert(&stub).await,
                        };
                        match saved {
                            Ok(()) => {
                                let _ = app
                                    .stores
                                    .hot
                                    .set(
                                        &key,
                                        &stub.id.to_string(),
                                        Some(Duration::from_secs(86_400)),
                                    )
                                    .await;
                                metrics::counter!("fastmock_recorded_stubs_total").increment(1);
                                if let Err(e) = app.engine.refresh_if_changed().await {
                                    tracing::warn!(error = %e, "stub refresh after recording failed");
                                }
                                hit = Some(StubHit {
                                    id: stub.id,
                                    name: stub.name.clone(),
                                });
                            }
                            Err(e) => {
                                metrics::counter!("fastmock_recording_skipped_total", "reason" => "invalid").increment(1);
                                tracing::warn!(error = %e, path = %cr.path, "recorded exchange could not be stored");
                            }
                        }
                    }
                    let mut resp = crate::proxy::to_response(u);
                    if let Some(h) = hit {
                        resp.extensions_mut().insert(h);
                    }
                    resp
                }
                Err(e) => crate::proxy::bad_gateway(&e),
            }
        }
        Ok(None) => {
            let mut body =
                json!({"error": "no stub matched", "method": cr.method, "path": cr.path});
            if app.cfg.unmatched_details {
                body["nearMisses"] =
                    serde_json::to_value(app.engine.near_misses(&cr, 3)).unwrap_or_default();
            }
            let mut resp = (StatusCode::NOT_FOUND, axum::Json(body)).into_response();
            resp.extensions_mut().insert(Unmatched);
            resp
        }
        Err(e)
            if e.downcast_ref::<fastmock_core::engine::BodyFileMissing>()
                .is_some() =>
        {
            tracing::error!(path = %cr.path, file = %e.downcast_ref::<fastmock_core::engine::BodyFileMissing>().map(|f| f.0.as_str()).unwrap_or(""), "response body file is missing");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": "response body file is not available", "detail": "see the server log and the request journal for this trace id"})),
            )
                .into_response()
        }
        Err(e) => {
            metrics::counter!("fastmock_stub_errors_total").increment(1);
            tracing::error!(error = %e, path = %cr.path, "stub evaluation failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": "stub evaluation failed", "detail": "see the server log and the request journal for this trace id"})),
            )
                .into_response()
        }
    }
}

async fn grpc_handler(
    app: &AppState,
    path: &str,
    headers: Vec<(String, String)>,
    body: &Bytes,
) -> Response {
    let Some(method) = app.grpc.method(path).await else {
        let mut r = crate::grpc::response(
            None,
            12,
            &format!("unknown gRPC method {path}: upload its descriptor set first"),
        );
        r.extensions_mut().insert(Unmatched);
        return r;
    };
    let json = match crate::grpc::decode_request(&method, body) {
        Ok(j) => j,
        Err((code, msg)) => return crate::grpc::response(None, code, &msg),
    };
    let decoded = json.to_string();
    if decoded.len() > app.cfg.max_body_bytes.saturating_mul(2) {
        return crate::grpc::response(None, 8, "decoded request is too large");
    }
    let mut hdrs = headers;
    hdrs.retain(|(k, _)| !k.eq_ignore_ascii_case("content-type"));
    hdrs.push(("content-type".into(), "application/json".into()));
    let cr = CapturedRequest::new("POST", path, None, hdrs, Bytes::from(decoded.clone()));
    let mut resp = match app.engine.handle(&cr).await {
        Ok(Some(r)) => {
            if !r.messages.is_empty() {
                app.kafka.publish_in_background(r.messages.clone());
            }
            let delay = Duration::from_millis(r.delay_ms).min(app.cfg.max_delay);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let code = r
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("grpc-status"))
                .and_then(|(_, v)| v.parse::<u32>().ok())
                .map(|c| if c > 16 { 2 } else { c })
                .unwrap_or(0);
            let message = r
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("grpc-message"))
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            let mut out = if code != 0 {
                crate::grpc::response(None, code, &message)
            } else {
                match serde_json::from_slice::<serde_json::Value>(&r.body)
                    .map_err(|e| e.to_string())
                    .and_then(|j| crate::grpc::encode_response(&method, &j))
                {
                    Ok(frame) => crate::grpc::response(Some(frame), 0, ""),
                    Err(e) => crate::grpc::response(None, 13, &e),
                }
            };
            out.extensions_mut().insert(StubHit {
                id: r.stub_id,
                name: r.stub_name.clone(),
            });
            if let Some(c) = r.correlation {
                out.extensions_mut().insert(Correlation(c));
            }
            out
        }
        Ok(None) => {
            let mut out = crate::grpc::response(None, 12, &format!("no stub matched {path}"));
            out.extensions_mut().insert(Unmatched);
            out
        }
        Err(e) => crate::grpc::response(None, 13, &format!("stub evaluation failed: {e}")),
    };
    metrics::counter!("fastmock_grpc_calls_total", "method" => path.to_string()).increment(1);
    resp.extensions_mut()
        .insert(crate::grpc::DecodedBody(decoded));
    resp
}

pub async fn health() -> impl IntoResponse {
    axum::Json(json!({"status": "ok"}))
}

pub async fn ready(State(app): State<AppState>) -> Response {
    if !app.ready.load(Ordering::SeqCst) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"status": "starting"})),
        )
            .into_response();
    }
    let hot = app.stores.hot.ping().await;
    let data = app.stores.data.ping().await;
    let ok = hot.is_ok() && data.is_ok();
    let state = |r: &anyhow::Result<()>, name: &str| match r {
        Ok(()) => "ok",
        Err(e) => {
            tracing::warn!(store = name, error = %e, "readiness check failed");
            "unavailable"
        }
    };
    let body = json!({
        "status": if ok { "ready" } else { "degraded" },
        "valkey": state(&hot, "valkey"),
        "database": state(&data, "database"),
    });
    (
        if ok {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        axum::Json(body),
    )
        .into_response()
}

#[cfg(test)]
mod fault_io_tests {
    use super::*;
    use std::io::IoSlice;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    #[derive(Default)]
    struct VecWriter {
        out: Vec<u8>,
        vectored_calls: usize,
        shut: bool,
    }

    impl hyper::rt::Write for VecWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.out.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_write_vectored(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bufs: &[IoSlice<'_>],
        ) -> Poll<std::io::Result<usize>> {
            self.vectored_calls += 1;
            let mut n = 0;
            for b in bufs {
                self.out.extend_from_slice(b);
                n += b.len();
            }
            Poll::Ready(Ok(n))
        }

        fn is_write_vectored(&self) -> bool {
            true
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            self.shut = true;
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn fault_io_keeps_vectored_writes() {
        use hyper::rt::Write;
        let slot = FaultSlot::default();
        let mut io = FaultIo::new(VecWriter::default(), slot.clone());
        assert!(io.is_write_vectored());
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let bufs = [IoSlice::new(b"head"), IoSlice::new(b"body")];
        let r = Pin::new(&mut io).poll_write_vectored(&mut cx, &bufs);
        assert!(matches!(r, Poll::Ready(Ok(8))));
        assert_eq!(io.inner.out, b"headbody");
        assert_eq!(io.inner.vectored_calls, 1);

        slot.arm(Bytes::from_static(b"garbage"));
        let r = Pin::new(&mut io).poll_write_vectored(&mut cx, &bufs);
        assert!(matches!(r, Poll::Ready(Err(_))));
        assert_eq!(io.inner.out, b"headbodygarbage");
        assert!(io.inner.shut);
        let r = Pin::new(&mut io).poll_write_vectored(&mut cx, &bufs);
        assert!(matches!(r, Poll::Ready(Err(_))));
        assert_eq!(io.inner.vectored_calls, 1);
    }
}
