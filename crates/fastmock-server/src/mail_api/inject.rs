//! `POST /mail/messages`: put a message into a space as if it had been received.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use fastmock_core::mail::{Envelope, MailEntry, MailSource, MailboxDelivery};
use fastmock_mail::address::mailbox_of;
use fastmock_mail::compose::{ComposeError, ComposeRequest, compose, parse_address};
use fastmock_mail::{AcceptError, Destination, Incoming, ParsedMail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{blocking, service};
use crate::access::Permission;
use crate::admin::{ApiError, ApiResult, ErrorBody, audit_event_in, bad};
use crate::auth::Principal;
use crate::spaces;
use crate::state::AppState;

const MAX_ENVELOPE_RECIPIENTS: usize = 1000;

#[derive(Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct InjectQuery {
    /// Space the message goes to. Without it, the global area, or the caller's only space when it is
    /// bound to spaces.
    pub space: Option<String>,
    /// File the message into this address's INBOX instead of the INBOX of each recipient (for a JSON
    /// body, its `mailbox` field wins).
    pub mailbox: Option<String>,
    /// `message/rfc822` only: the envelope sender (default: the `From` address).
    pub mail_from: Option<String>,
    /// `message/rfc822` only: envelope recipients, comma-separated (default: the `To`, `Cc` and `Bcc`
    /// addresses).
    pub rcpt_to: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Injected {
    /// The stored message (one entry).
    pub messages: Vec<MailEntry>,
}

fn too_large(max: usize) -> ApiError {
    ApiError(
        StatusCode::PAYLOAD_TOO_LARGE,
        format!("the message exceeds {max} bytes (FASTMOCK_MAIL_MAX_MESSAGE_BYTES)"),
    )
}

fn address(raw: &str, what: &str) -> ApiResult<String> {
    parse_address(raw)
        .map(|(_, a)| a)
        .map_err(|e| bad(format!("{what}: {e}")))
}

enum Format {
    Json,
    Raw,
}

fn format_of(headers: &HeaderMap) -> ApiResult<Format> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let essence = ct
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match essence.as_str() {
        "application/json" => Ok(Format::Json),
        e if e.starts_with("application/") && e.ends_with("+json") => Ok(Format::Json),
        "message/rfc822" => Ok(Format::Raw),
        _ => Err(ApiError(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "send the message as application/json (fields to build it from) or message/rfc822 (the raw message)"
                .into(),
        )),
    }
}

#[utoipa::path(
    post, path = "/__admin/mail/messages", tag = "mail", operation_id = "injectMailMessage",
    summary = "Inject a message",
    description = "Stores a message in `space` as if it had been received (`source: inject`) and files it into the IMAP/POP3 INBOX of each recipient (or of `mailbox`). Send `application/json` (`ComposeRequest`: the message is built from the fields; header fields must be single printable lines, and `From`, `To`, `Cc`, `Subject` and the MIME structure headers cannot be set through `headers`) or `message/rfc822` (the raw message, envelope from `mailFrom`/`rcptTo` or its headers). Labels come from plus addresses and the `X-FastMock-Labels` header. The message may not exceed `FASTMOCK_MAIL_MAX_MESSAGE_BYTES`. Publishes `message.received` and records the audit event `mail.inject`.",
    params(InjectQuery),
    request_body(content(
        (ComposeRequest = "application/json", example = json!({"from": "Shop <shop@example.com>", "to": ["alice@example.test"], "subject": "Your code", "text": "Your code is 123456", "headers": {"X-FastMock-Labels": "otp"}})),
        (Vec<u8> = "message/rfc822"),
    )),
    responses(
        (status = 201, description = "Stored", body = Injected),
        (status = 400, description = "Invalid message: addresses, header fields (line breaks are refused), base64, or no recipients", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, or an unknown space", body = ErrorBody),
        (status = 413, description = "The message exceeds FASTMOCK_MAIL_MAX_MESSAGE_BYTES", body = ErrorBody),
        (status = 415, description = "Neither application/json nor message/rfc822", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn inject(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<InjectQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Injected>)> {
    let svc = service(&app)?;
    let area = spaces::target(&app, &p, Permission::MailWrite, q.space.as_deref())?;
    let max = svc.config.max_message_bytes;
    let format = format_of(&headers)?;
    let (raw, envelope, mailbox, kind) = match format {
        Format::Json => {
            if body.len() > max.saturating_mul(2).saturating_add(1 << 20) {
                return Err(too_large(max));
            }
            let req: ComposeRequest =
                serde_json::from_slice(&body).map_err(|e| bad(format!("invalid message: {e}")))?;
            let raw = compose(&req, max).map_err(|e| match e {
                ComposeError::TooLarge(n) => too_large(n),
                other => bad(other.to_string()),
            })?;
            let mut rcpt_to = Vec::with_capacity(req.to.len() + req.cc.len());
            for a in req.to.iter().chain(&req.cc) {
                rcpt_to.push(address(a, "recipient")?);
            }
            let envelope = Envelope {
                mail_from: address(&req.from, "from")?,
                rcpt_to,
                ..Default::default()
            };
            (raw, envelope, req.mailbox.or(q.mailbox.clone()), "json")
        }
        Format::Raw => {
            if body.len() > max {
                return Err(too_large(max));
            }
            if body.is_empty() {
                return Err(bad("the message is empty"));
            }
            let limits = svc.config.parse_limits();
            let raw = body.clone();
            let (from, recipients) = blocking(move || {
                let parsed = ParsedMail::parse(&raw, &limits);
                (
                    parsed.from().first().map(|a| a.address.clone()),
                    parsed.recipients(),
                )
            })
            .await?;
            let rcpt_to = match q.rcpt_to.as_deref().filter(|s| !s.trim().is_empty()) {
                Some(list) => list
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|a| address(a, "rcptTo"))
                    .collect::<ApiResult<Vec<_>>>()?,
                None => recipients,
            };
            if rcpt_to.is_empty() {
                return Err(bad(
                    "the message names no recipients (To, Cc, Bcc); set rcptTo",
                ));
            }
            if rcpt_to.len() > MAX_ENVELOPE_RECIPIENTS {
                return Err(bad(format!("at most {MAX_ENVELOPE_RECIPIENTS} recipients")));
            }
            let mail_from = match q.mail_from.as_deref().map(str::trim) {
                Some("") => String::new(),
                Some(a) => address(a, "mailFrom")?,
                None => from.unwrap_or_default(),
            };
            let envelope = Envelope {
                mail_from,
                rcpt_to,
                ..Default::default()
            };
            (body.to_vec(), envelope, q.mailbox.clone(), "raw")
        }
    };
    let mailboxes = match mailbox.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => Some(vec![MailboxDelivery::inbox(&mailbox_of(&address(
            m, "mailbox",
        )?))]),
        None => None,
    };
    let size = raw.len();
    let mut inc = Incoming::new(MailSource::Inject, envelope, raw);
    inc.destination = Destination::Area(area.clone());
    inc.mailboxes = mailboxes.clone();
    let stored = svc.accept(inc).await.map_err(|e| match e {
        AcceptError::TooLarge(n) => too_large(n),
        AcceptError::Store(e) => ApiError::from(e),
    })?;
    audit_event_in(
        &app,
        &p,
        area.as_deref(),
        "mail.inject",
        json!({
            "ids": stored.iter().map(|e| e.id).collect::<Vec<_>>(),
            "format": kind,
            "size": size,
            "mailbox": mailboxes.and_then(|m| m.first().map(|d| d.address.clone())),
        }),
    )
    .await;
    Ok((StatusCode::CREATED, Json(Injected { messages: stored })))
}
