use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::extract::FromRequestParts;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use fastmock_core::matcher::CompiledString;
use fastmock_core::template::context;
use fastmock_core::ws_model::WebSocketDef;
use fastmock_core::{CapturedRequest, EngineResponse};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tracing::Instrument;
use uuid::Uuid;

use crate::state::AppState;

pub const MAX_CONNECTIONS: usize = 1024;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const DEFAULT_SESSION: Duration = Duration::from_secs(3600);
pub const DEFAULT_MAX_MESSAGES: u32 = 10_000;
pub const MAX_INCOMING_MESSAGES: u32 = 10_000;

static CONNECTIONS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONNECTIONS)));

pub fn is_upgrade(headers: &axum::http::HeaderMap) -> bool {
    let has = |name: &str, token: &str| {
        headers.get_all(name).iter().any(|v| {
            v.to_str()
                .is_ok_and(|s| s.split(',').any(|p| p.trim().eq_ignore_ascii_case(token)))
        })
    };
    has("upgrade", "websocket") && has("connection", "upgrade")
}

struct Found {
    def: WebSocketDef,
    templated: bool,
    params: std::collections::BTreeMap<String, String>,
    name: Option<String>,
}

fn lookup(app: &AppState, id: Uuid, cr: Option<&CapturedRequest>) -> Option<Found> {
    let snapshot = app.engine.snapshot();
    let cs = snapshot.iter().find(|c| c.stub.id == id)?;
    let def = cs.stub.response.websocket.clone()?;
    let params = cr
        .and_then(|r| cs.match_request(r, r.json()))
        .unwrap_or_default();
    Some(Found {
        def,
        templated: cs.stub.response.templated,
        params,
        name: cs.stub.name.clone(),
    })
}

pub fn wants_upgrade(app: &AppState, headers: &axum::http::HeaderMap, stub_id: Uuid) -> bool {
    is_upgrade(headers) && lookup(app, stub_id, None).is_some()
}

fn error(status: StatusCode, msg: &str) -> Response {
    (status, axum::Json(json!({"error": msg}))).into_response()
}

pub async fn upgrade(
    app: &AppState,
    parts: &mut Parts,
    cr: &CapturedRequest,
    r: EngineResponse,
) -> Response {
    let hit = crate::dataplane::StubHit {
        id: r.stub_id,
        name: r.stub_name.clone(),
    };
    let Some(found) = lookup(app, r.stub_id, Some(cr)) else {
        return error(
            StatusCode::NOT_FOUND,
            "the WebSocket stub is no longer available",
        );
    };
    let templates = app.engine.templates().clone();
    if let Err(e) = found
        .def
        .validate(|t| templates.validate(t).map_err(|e| e.to_string()))
    {
        metrics::counter!("fastmock_ws_rejected_total", "reason" => "invalid").increment(1);
        tracing::warn!(stub_id = %r.stub_id, error = %e, "WebSocket stub definition is invalid");
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the WebSocket stub definition is invalid",
        );
    }
    let ws = match WebSocketUpgrade::from_request_parts(parts, &()).await {
        Ok(w) => w,
        Err(rej) => {
            metrics::counter!("fastmock_ws_rejected_total", "reason" => "handshake").increment(1);
            return rej.into_response();
        }
    };
    let Ok(permit) = CONNECTIONS.clone().try_acquire_owned() else {
        metrics::counter!("fastmock_ws_rejected_total", "reason" => "connections").increment(1);
        let mut resp = error(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many WebSocket connections",
        );
        resp.headers_mut()
            .insert("retry-after", axum::http::HeaderValue::from_static("1"));
        return resp;
    };
    let delay = Duration::from_millis(r.delay_ms).min(app.cfg.max_delay);
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    let requested: Vec<String> = ws
        .requested_protocols()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(',').map(|p| p.trim().to_string()))
        .filter(|p| !p.is_empty())
        .take(1)
        .collect();
    let ctx = cr.template_context(&found.params, false);
    let span = tracing::info_span!(
        "fastmock.websocket",
        otel.name = "websocket session",
        otel.kind = "server",
        url.path = %cr.path,
        fastmock.stub_id = %r.stub_id,
        fastmock.ws.sent = tracing::field::Empty,
        fastmock.ws.received = tracing::field::Empty,
    );
    let label = found.name.clone().unwrap_or_else(|| r.stub_id.to_string());
    let session = Session {
        templates,
        def: found.def,
        templated: found.templated,
        ctx,
    };
    let mut resp = ws
        .protocols(requested)
        .max_frame_size(MAX_FRAME_BYTES)
        .max_message_size(MAX_FRAME_BYTES)
        .on_failed_upgrade(|e| {
            metrics::counter!("fastmock_ws_rejected_total", "reason" => "upgrade").increment(1);
            tracing::warn!(error = %e, "WebSocket upgrade failed");
        })
        .on_upgrade(move |socket| {
            async move {
                let _permit = permit;
                metrics::gauge!("fastmock_ws_connections").increment(1.0);
                metrics::counter!("fastmock_ws_sessions_total", "stub" => label).increment(1);
                session.run(socket).await;
                metrics::gauge!("fastmock_ws_connections").decrement(1.0);
            }
            .instrument(span)
        });
    resp.extensions_mut().insert(hit);
    resp.extensions_mut().insert(crate::extras::Streaming);
    resp
}

struct Session {
    templates: Arc<fastmock_core::template::Templates>,
    def: WebSocketDef,
    templated: bool,
    ctx: Value,
}

enum Flow {
    Continue,
    Close(u16, &'static str),
    Gone,
}

impl Session {
    fn render(&self, tpl: &str, ctx: &Value) -> Option<String> {
        if !self.templated {
            return Some(tpl.to_string());
        }
        match self.templates.render_in(tpl, &context(ctx.clone())) {
            Ok(s) => Some(s),
            Err(e) => {
                metrics::counter!("fastmock_ws_messages_total", "direction" => "out", "result" => "template_error").increment(1);
                tracing::warn!(error = %e, "rendering a WebSocket message failed");
                None
            }
        }
    }

    async fn send(&self, socket: &mut WebSocket, msg: Message, sent: &mut u32) -> bool {
        let ok = socket.send(msg).await.is_ok();
        if ok {
            *sent += 1;
            metrics::counter!("fastmock_ws_messages_total", "direction" => "out", "result" => "sent")
                .increment(1);
        }
        ok
    }

    async fn run(self, mut socket: WebSocket) {
        let rules: Vec<(Option<CompiledString>, &fastmock_core::ws_model::WsReply)> = self
            .def
            .on_message
            .iter()
            .filter_map(|r| match &r.matcher {
                None => Some((None, r)),
                Some(m) => CompiledString::compile(m).ok().map(|c| (Some(c), r)),
            })
            .collect();
        let deadline = Instant::now()
            + self
                .def
                .close_after_ms
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_SESSION);
        let max_out = self.def.max_messages.unwrap_or(DEFAULT_MAX_MESSAGES);
        let scripted = &self.def.messages;
        let mut idx = 0usize;
        let mut next_at = scripted
            .first()
            .map(|m| Instant::now() + Duration::from_millis(m.delay_ms));
        let (mut sent, mut received) = (0u32, 0u32);
        let flow = loop {
            if sent >= max_out {
                break Flow::Close(1000, "maxMessages reached");
            }
            let tick = async {
                match next_at {
                    Some(t) => tokio::time::sleep_until(t).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break Flow::Close(1000, "closeAfterMs reached"),
                _ = tick => {
                    if let Some(text) = self.render(&scripted[idx].data, &self.ctx)
                        && !self.send(&mut socket, Message::text(text), &mut sent).await
                    {
                        break Flow::Gone;
                    }
                    idx += 1;
                    if idx >= scripted.len() {
                        if self.def.repeat {
                            idx = 0;
                        } else {
                            next_at = None;
                            continue;
                        }
                    }
                    next_at = Some(Instant::now() + Duration::from_millis(scripted[idx].delay_ms));
                }
                incoming = socket.recv() => {
                    let (text, echo) = match incoming {
                        None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break Flow::Gone,
                        Some(Ok(Message::Text(t))) => (t.as_str().to_string(), Message::Text(t)),
                        Some(Ok(Message::Binary(b))) => (String::from_utf8_lossy(&b).into_owned(), Message::Binary(b)),
                        Some(Ok(_)) => continue,
                    };
                    received += 1;
                    metrics::counter!("fastmock_ws_messages_total", "direction" => "in", "result" => "received").increment(1);
                    if received > MAX_INCOMING_MESSAGES {
                        break Flow::Close(1008, "too many messages");
                    }
                    match self.on_message(&mut socket, &rules, &text, echo, &mut sent).await {
                        Flow::Continue => {}
                        other => break other,
                    }
                }
            }
        };
        let span = tracing::Span::current();
        span.record("fastmock.ws.sent", sent);
        span.record("fastmock.ws.received", received);
        if let Flow::Close(code, reason) = flow {
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code,
                    reason: reason.into(),
                })))
                .await;
        }
        tracing::debug!(sent, received, "WebSocket session ended");
    }

    async fn on_message(
        &self,
        socket: &mut WebSocket,
        rules: &[(Option<CompiledString>, &fastmock_core::ws_model::WsReply)],
        text: &str,
        echo: Message,
        sent: &mut u32,
    ) -> Flow {
        let rule = rules
            .iter()
            .find(|(m, _)| m.as_ref().is_none_or(|m| m.matches(Some(text))));
        if let Some((_, r)) = rule {
            if let Some(reply) = &r.reply {
                let mut ctx = self.ctx.clone();
                ctx["message"] = json!({
                    "text": text,
                    "json": fastmock_core::request::parse_json_bounded(text.as_bytes()).unwrap_or(Value::Null),
                });
                if let Some(out) = self.render(reply, &ctx)
                    && !self.send(socket, Message::text(out), sent).await
                {
                    return Flow::Gone;
                }
            }
            if r.close {
                return Flow::Close(1000, "closed by onMessage rule");
            }
            return Flow::Continue;
        }
        if self.def.echo && !self.send(socket, echo, sent).await {
            return Flow::Gone;
        }
        Flow::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_upgrade_requests() {
        let mut h = axum::http::HeaderMap::new();
        assert!(!is_upgrade(&h));
        h.insert("upgrade", "WebSocket".parse().unwrap());
        h.insert("connection", "keep-alive, Upgrade".parse().unwrap());
        assert!(is_upgrade(&h));
        h.insert("upgrade", "h2c".parse().unwrap());
        assert!(!is_upgrade(&h));
    }
}
