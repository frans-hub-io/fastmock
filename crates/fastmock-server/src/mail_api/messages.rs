//! Lists, one message and its renditions, labels and deletes.

use axum::Json;
use axum::body::Body;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fastmock_core::mail::{MAX_PAGE, MailCursor, MailEntry, MailQuery, TranscriptLine};
use fastmock_mail::extract::{CodePattern, MailLink};
use fastmock_mail::labels::normalize_labels;
use fastmock_mail::query::parse_query;
use fastmock_mail::{MailDetail, ParsedMail};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::{blocking, find, service};
use crate::access::Permission;
use crate::admin::{ApiResult, ErrorBody, audit_event_in, bad, not_found};
use crate::auth::Principal;
use crate::spaces::{self, SpaceQuery};
use crate::state::AppState;

/// The `Content-Security-Policy` of `/html`: no scripts, no plugins, no requests, inline styles and
/// `data:` images only; `sandbox` gives the document a unique origin.
pub const HTML_CSP: &str = "sandbox; default-src 'none'; img-src data:; style-src 'unsafe-inline'";
/// `/html?images=allow`: remote images over HTTPS are loaded too.
pub const HTML_CSP_REMOTE_IMAGES: &str =
    "sandbox; default-src 'none'; img-src data: https:; style-src 'unsafe-inline'";
/// Downloads (`/raw`, `/parts/{partId}`) are never rendered with scripts or requests.
const DOWNLOAD_CSP: &str = "sandbox; default-src 'none'";
/// Total size of the `data:` URIs that replace `cid:` references in `/html` (base64 bytes); parts
/// beyond it keep their `cid:` reference.
pub const MAX_INLINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_FILENAME_CHARS: usize = 100;
const DEFAULT_PAGE: usize = 50;

#[derive(Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// Space to list. Without it, every area the caller may read (`mail:read`); the global area only for
    /// callers holding `mail:read` globally.
    pub space: Option<String>,
    /// Search: `from:` `to:` `cc:` `subject:` `has:attachment` `label:` `after:` `before:` (dates as
    /// `YYYY-MM-DD` or RFC 3339) and free text on the subject and text, values with spaces in double
    /// quotes; all terms must hold. At most 1024 bytes and 32 terms.
    #[param(example = "from:billing subject:\"monthly invoice\" has:attachment")]
    pub query: Option<String>,
    /// Page size, 1 to 500 (default 50).
    pub limit: Option<usize>,
    /// `nextCursor` of the previous page.
    pub cursor: Option<String>,
    /// `newest` (default) or `oldest` first.
    pub order: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MailList {
    pub messages: Vec<MailEntry>,
    /// Pass as `cursor` to read the next page; absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[utoipa::path(
    get, path = "/__admin/mail/messages", tag = "mail", operation_id = "listMailMessages",
    summary = "List and search captured messages",
    description = "Summaries of the messages in the areas the caller may read (`mail:read`), newest first unless `order=oldest`, paged with `cursor`.",
    params(ListQuery),
    responses(
        (status = 200, description = "A page of messages", body = MailList),
        (status = 400, description = "Invalid query, limit, order or cursor", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, or an unknown space", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn list(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<MailList>> {
    let svc = service(&app)?;
    let spaces = spaces::select(&app, &p, Permission::MailRead, q.space.as_deref())?;
    let filter = parse_query(q.query.as_deref().unwrap_or_default()).map_err(|e| bad(e.0))?;
    let limit = q.limit.unwrap_or(DEFAULT_PAGE);
    if !(1..=MAX_PAGE).contains(&limit) {
        return Err(bad(format!("limit must be between 1 and {MAX_PAGE}")));
    }
    let oldest_first = match q.order.as_deref().map(str::trim) {
        None | Some("") | Some("newest") => false,
        Some("oldest") => true,
        Some(other) => {
            return Err(bad(format!(
                "order must be newest or oldest, got `{}`",
                other.chars().take(40).collect::<String>()
            )));
        }
    };
    let cursor = match q.cursor.as_deref().filter(|c| !c.is_empty()) {
        Some(c) => Some(MailCursor::decode(c).ok_or_else(|| bad("invalid cursor"))?),
        None => None,
    };
    let page = svc
        .store
        .list(&MailQuery {
            spaces,
            filter,
            limit,
            cursor,
            oldest_first,
        })
        .await?;
    Ok(Json(MailList {
        messages: page.items,
        next_cursor: page.next.map(|c| c.encode()),
    }))
}

/// Admin API paths of a message's renditions.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessageUrls {
    #[schema(example = "/__admin/mail/messages/01936b4e-8c1a-7d2e-9f00-1a2b3c4d5e6f/raw")]
    pub raw: String,
    /// Present when the message has an HTML body; render it only in a sandboxed `<iframe>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub transcript: String,
    pub links: String,
    pub codes: String,
    /// Append the `partId` of an attachment.
    #[schema(example = "/__admin/mail/messages/01936b4e-8c1a-7d2e-9f00-1a2b3c4d5e6f/parts/")]
    pub parts: String,
}

/// A stored message with its parsed headers, bodies and parts.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MailMessageView {
    #[serde(flatten)]
    pub entry: MailEntry,
    #[serde(flatten)]
    pub detail: MailDetail,
    pub urls: MessageUrls,
}

fn urls(id: Uuid, detail: &MailDetail) -> MessageUrls {
    let base = format!("/__admin/mail/messages/{id}");
    MessageUrls {
        raw: format!("{base}/raw"),
        html: detail.has_html.then(|| format!("{base}/html")),
        text: detail.text.is_some().then(|| format!("{base}/text")),
        transcript: format!("{base}/transcript"),
        links: format!("{base}/links"),
        codes: format!("{base}/codes"),
        parts: format!("{base}/parts/"),
    }
}

#[utoipa::path(
    get, path = "/__admin/mail/messages/{id}", tag = "mail", operation_id = "getMailMessage",
    summary = "One message",
    description = "The envelope, summary, labels, source and provider fields of a stored message with its header fields in order, decoded addresses, the text body, whether it has an HTML body (served by `/html`, never inline here) and its attachments (`partId` for `/parts/{partId}`).",
    params(("id" = Uuid, Path, description = "Message id"), SpaceQuery),
    responses(
        (status = 200, description = "The message", body = MailMessageView),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, no such message, or one the caller cannot read", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn get_message(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<Json<MailMessageView>> {
    let svc = service(&app)?;
    let msg = find(&app, &svc, &p, id, q.space.as_deref(), Permission::MailRead).await?;
    let limits = svc.config.parse_limits();
    let raw = msg.raw.clone();
    let detail = blocking(move || ParsedMail::parse(&raw, &limits).detail()).await?;
    Ok(Json(MailMessageView {
        urls: urls(id, &detail),
        entry: msg.entry(),
        detail,
    }))
}

fn download(bytes: impl Into<Body>, content_type: &str, disposition: HeaderValue) -> Response {
    let mut r = Response::new(bytes.into());
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    h.insert(header::CONTENT_DISPOSITION, disposition);
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(DOWNLOAD_CSP),
    );
    r
}

#[utoipa::path(
    get, path = "/__admin/mail/messages/{id}/raw", tag = "mail", operation_id = "getMailMessageRaw",
    summary = "The message as received",
    description = "The raw MIME message (`message/rfc822`) as a download named `<id>.eml`.",
    params(("id" = Uuid, Path, description = "Message id"), SpaceQuery),
    responses(
        (status = 200, description = "Raw message", content_type = "message/rfc822", body = Vec<u8>),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, or no such message", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn raw(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<Response> {
    let svc = service(&app)?;
    let msg = find(&app, &svc, &p, id, q.space.as_deref(), Permission::MailRead).await?;
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{id}.eml\""))
        .unwrap_or(HeaderValue::from_static("attachment"));
    Ok(download(msg.raw, "message/rfc822", disposition))
}

#[derive(Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct HtmlQuery {
    /// Space of the message.
    pub space: Option<String>,
    /// `block` (default): only `data:` images (inline `cid:` parts) load. `allow`: remote images over
    /// HTTPS load too (`img-src data: https:`), which tells their senders the message was opened.
    pub images: Option<String>,
}

#[utoipa::path(
    get, path = "/__admin/mail/messages/{id}/html", tag = "mail", operation_id = "getMailMessageHtml",
    summary = "The HTML body, sandboxed",
    description = "The HTML body with `cid:` references to parts of the message replaced by `data:` URIs (4 MiB of encoded parts in total; references beyond it stay `cid:`). Served with `Content-Security-Policy: sandbox; default-src 'none'; img-src data:; style-src 'unsafe-inline'` (`img-src data: https:` with `images=allow`) and `X-Content-Type-Options: nosniff`, so scripts, forms, frames and requests are blocked. Show it in an `<iframe sandbox>` without `allow-scripts` and `allow-same-origin`.",
    params(("id" = Uuid, Path, description = "Message id"), HtmlQuery),
    responses(
        (status = 200, description = "HTML body", content_type = "text/html", body = String),
        (status = 400, description = "Invalid `images` value", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, no such message, or the message has no HTML body", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn html(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(q): Query<HtmlQuery>,
) -> ApiResult<Response> {
    let csp = match q.images.as_deref().map(str::trim) {
        None | Some("") | Some("block") => HTML_CSP,
        Some("allow") => HTML_CSP_REMOTE_IMAGES,
        Some(_) => return Err(bad("images must be block or allow")),
    };
    let svc = service(&app)?;
    let msg = find(&app, &svc, &p, id, q.space.as_deref(), Permission::MailRead).await?;
    let limits = svc.config.parse_limits();
    let raw = msg.raw.clone();
    let html =
        blocking(move || ParsedMail::parse(&raw, &limits).html_with_inline_parts(MAX_INLINE_BYTES))
            .await?
            .ok_or_else(|| not_found("HTML body"))?;
    let mut r = Response::new(Body::from(html));
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(csp),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(r)
}

#[utoipa::path(
    get, path = "/__admin/mail/messages/{id}/text", tag = "mail", operation_id = "getMailMessageText",
    summary = "The text body",
    description = "The text body; for a message with only an HTML body, the HTML converted to text.",
    params(("id" = Uuid, Path, description = "Message id"), SpaceQuery),
    responses(
        (status = 200, description = "Text body", content_type = "text/plain", body = String),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, no such message, or the message has no body", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn text(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<Response> {
    let svc = service(&app)?;
    let msg = find(&app, &svc, &p, id, q.space.as_deref(), Permission::MailRead).await?;
    let limits = svc.config.parse_limits();
    let raw = msg.raw.clone();
    let text = blocking(move || ParsedMail::parse(&raw, &limits).text())
        .await?
        .ok_or_else(|| not_found("text body"))?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        text,
    )
        .into_response())
}

/// A file name that is safe in a `Content-Disposition` header and on any file system: the last path
/// segment, letters, digits, `.`, `-`, `_` and spaces only (others become `_`), at most 100
/// characters, no leading or trailing dots or spaces; `fallback` when nothing is left.
pub fn safe_filename(name: Option<&str>, fallback: &str) -> String {
    let last = name
        .and_then(|n| n.rsplit(['/', '\\']).next())
        .unwrap_or_default();
    let mapped: String = last
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .take(MAX_FILENAME_CHARS)
        .collect();
    let trimmed = mapped.trim_matches(['.', ' ']);
    if trimmed.chars().all(|c| c == '_') {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

/// `attachment; filename="<safe>"`, plus `filename*=UTF-8''...` with the original name (last path
/// segment, control characters removed) when it differs.
pub fn attachment_disposition(name: Option<&str>, fallback: &str) -> HeaderValue {
    let safe = safe_filename(name, fallback);
    let mut v = format!("attachment; filename=\"{safe}\"");
    let original: String = name
        .and_then(|n| n.rsplit(['/', '\\']).next())
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect();
    if !original.is_empty() && original != safe {
        v.push_str("; filename*=UTF-8''");
        for b in original.bytes() {
            if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'~') {
                v.push(b as char);
            } else {
                v.push_str(&format!("%{b:02X}"));
            }
        }
    }
    HeaderValue::from_str(&v).unwrap_or(HeaderValue::from_static("attachment"))
}

/// A media type made of RFC 2045 token characters (`type/subtype`, lowercase), else
/// `application/octet-stream`.
pub fn safe_content_type(ct: &str) -> String {
    let token = |s: &str| {
        !s.is_empty()
            && s.len() <= 127
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&-^_.+".contains(&b))
    };
    match ct.split_once('/') {
        Some((t, s)) if token(t) && token(s) => ct.to_ascii_lowercase(),
        _ => "application/octet-stream".into(),
    }
}

#[utoipa::path(
    get, path = "/__admin/mail/messages/{id}/parts/{partId}", tag = "mail", operation_id = "getMailMessagePart",
    summary = "Download a part",
    description = "The decoded content of an attachment or inline part (`partId` from the message's `attachments`), as a download (`Content-Disposition: attachment` with a sanitised file name), with its content type when well-formed (else `application/octet-stream`), `X-Content-Type-Options: nosniff` and `Content-Security-Policy: sandbox; default-src 'none'`.",
    params(("id" = Uuid, Path, description = "Message id"), ("partId" = String, Path, description = "Part id"), SpaceQuery),
    responses(
        (status = 200, description = "Part content", content_type = "application/octet-stream", body = Vec<u8>),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, no such message or part", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn part(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path((id, part_id)): Path<(Uuid, String)>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<Response> {
    let svc = service(&app)?;
    let msg = find(&app, &svc, &p, id, q.space.as_deref(), Permission::MailRead).await?;
    let limits = svc.config.parse_limits();
    let raw = msg.raw.clone();
    let pid = part_id.clone();
    let content = blocking(move || ParsedMail::parse(&raw, &limits).part(&pid))
        .await?
        .ok_or_else(|| not_found("part"))?;
    let fallback = format!(
        "part-{}",
        safe_filename(Some(&part_id), "x").replace(' ', "_")
    );
    Ok(download(
        content.bytes,
        &safe_content_type(&content.content_type),
        attachment_disposition(content.name.as_deref(), &fallback),
    ))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct MailTranscript {
    /// SMTP commands (credentials masked, DATA replaced by its size) and replies; empty for messages
    /// that did not come over SMTP.
    pub transcript: Vec<TranscriptLine>,
}

#[utoipa::path(
    get, path = "/__admin/mail/messages/{id}/transcript", tag = "mail", operation_id = "getMailMessageTranscript",
    summary = "The SMTP session of a message",
    params(("id" = Uuid, Path, description = "Message id"), SpaceQuery),
    responses(
        (status = 200, description = "Session transcript", body = MailTranscript),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, or no such message", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn transcript(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<Json<MailTranscript>> {
    let svc = service(&app)?;
    let msg = find(&app, &svc, &p, id, q.space.as_deref(), Permission::MailRead).await?;
    Ok(Json(MailTranscript {
        transcript: msg.transcript,
    }))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct MailLinks {
    pub links: Vec<MailLink>,
}

#[utoipa::path(
    get, path = "/__admin/mail/messages/{id}/links", tag = "mail", operation_id = "getMailMessageLinks",
    summary = "Links in a message",
    description = "Every http(s) URL of the text and HTML bodies, de-duplicated in order of appearance: HTML anchors (with their text) first, then URLs written in the text. At most 1000; 2 MiB of each body are scanned. FastMock never follows them.",
    params(("id" = Uuid, Path, description = "Message id"), SpaceQuery),
    responses(
        (status = 200, description = "Links", body = MailLinks),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, or no such message", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn links(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<Json<MailLinks>> {
    let svc = service(&app)?;
    let msg = find(&app, &svc, &p, id, q.space.as_deref(), Permission::MailRead).await?;
    let limits = svc.config.parse_limits();
    let links = blocking(move || {
        let parsed = ParsedMail::parse(&msg.raw, &limits);
        let html = parsed.html();
        let text = if msg.summary.has_text {
            parsed.text()
        } else {
            None
        };
        fastmock_mail::extract::links(text.as_deref(), html.as_deref())
    })
    .await?;
    Ok(Json(MailLinks { links }))
}

#[derive(Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CodesQuery {
    /// Space of the message.
    pub space: Option<String>,
    /// Regular expression (Rust `regex` syntax, at most 256 bytes, compiled within a size limit); with a
    /// capture group the first group is the code. Default `\b\d{4,8}\b` (4 to 8 digit codes).
    #[param(example = "code:\\s*([A-Z0-9]{6})")]
    pub pattern: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct MailCodes {
    /// Distinct codes in order of appearance (subject first, then the text), at most 100.
    pub codes: Vec<String>,
    /// The pattern used.
    #[schema(example = "\\b\\d{4,8}\\b")]
    pub pattern: String,
}

#[utoipa::path(
    get, path = "/__admin/mail/messages/{id}/codes", tag = "mail", operation_id = "getMailMessageCodes",
    summary = "Codes in a message",
    description = "One-time codes found in the subject and the text body (HTML converted when there is no text part). The `regex` crate matches in linear time; 2 MiB of text are scanned.",
    params(("id" = Uuid, Path, description = "Message id"), CodesQuery),
    responses(
        (status = 200, description = "Codes", body = MailCodes),
        (status = 400, description = "The pattern is too long, invalid or too large once compiled", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, or no such message", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn codes(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(q): Query<CodesQuery>,
) -> ApiResult<Json<MailCodes>> {
    let pattern_text = q
        .pattern
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fastmock_mail::extract::DEFAULT_CODE_PATTERN.to_string());
    let pattern = CodePattern::new(Some(&pattern_text)).map_err(|e| bad(e.to_string()))?;
    let svc = service(&app)?;
    let msg = find(&app, &svc, &p, id, q.space.as_deref(), Permission::MailRead).await?;
    let limits = svc.config.parse_limits();
    let codes = blocking(move || {
        let parsed = ParsedMail::parse(&msg.raw, &limits);
        let mut text = parsed.subject().unwrap_or_default();
        if let Some(t) = parsed.text() {
            text.push('\n');
            text.push_str(&t);
        }
        pattern.codes(&text)
    })
    .await?;
    Ok(Json(MailCodes {
        codes,
        pattern: pattern_text,
    }))
}

#[utoipa::path(
    delete, path = "/__admin/mail/messages/{id}", tag = "mail", operation_id = "deleteMailMessage",
    summary = "Delete a message",
    description = "Removes the message and its IMAP/POP3 mailbox entries, publishes `message.deleted` and records the audit event `mail.message.delete`.",
    params(("id" = Uuid, Path, description = "Message id"), SpaceQuery),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "The caller may read the message but lacks mail:delete", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, or no such message", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn delete_message(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<StatusCode> {
    let svc = service(&app)?;
    let msg = find(
        &app,
        &svc,
        &p,
        id,
        q.space.as_deref(),
        Permission::MailDelete,
    )
    .await?;
    let area = fastmock_core::SpaceFilter::area(msg.space.as_deref());
    let Some(r) = svc.store.delete(id, &area).await? else {
        return Err(not_found("message"));
    };
    svc.publish_deleted(std::slice::from_ref(&r)).await;
    audit_event_in(
        &app,
        &p,
        msg.space.as_deref(),
        "mail.message.delete",
        json!({"id": id}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DeleteQuery {
    /// Space to clear. Without it, every area where the caller holds `mail:delete`.
    pub space: Option<String>,
    /// Only messages matching this search (syntax of `GET /mail/messages`); all without it.
    pub query: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct MailDeleted {
    pub deleted: usize,
    pub ids: Vec<Uuid>,
}

#[utoipa::path(
    delete, path = "/__admin/mail/messages", tag = "mail", operation_id = "deleteMailMessages",
    summary = "Delete messages",
    description = "Deletes the messages matching `query` (all without it) in `space`, or in every area where the caller holds `mail:delete`; publishes `message.deleted` per area and records the audit event `mail.messages.delete`.",
    params(DeleteQuery),
    responses(
        (status = 200, description = "Deleted messages", body = MailDeleted),
        (status = 400, description = "Invalid query", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, or an unknown space", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn delete_matching(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<DeleteQuery>,
) -> ApiResult<Json<MailDeleted>> {
    let svc = service(&app)?;
    let spaces = spaces::select(&app, &p, Permission::MailDelete, q.space.as_deref())?;
    let filter = parse_query(q.query.as_deref().unwrap_or_default()).map_err(|e| bad(e.0))?;
    let refs = svc.store.delete_matching(&spaces, &filter).await?;
    svc.publish_deleted(&refs).await;
    let ids: Vec<Uuid> = refs.iter().map(|r| r.id).collect();
    audit_event_in(
        &app,
        &p,
        q.space.as_deref(),
        "mail.messages.delete",
        json!({"deleted": ids.len(), "query": q.query}),
    )
    .await;
    Ok(Json(MailDeleted {
        deleted: ids.len(),
        ids,
    }))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LabelsBody {
    /// The new labels (replacing the current ones): 1 to 64 letters, digits or `- _ . : /` each,
    /// case-insensitive, at most 32.
    #[schema(example = json!(["otp", "signup"]))]
    pub labels: Vec<String>,
}

#[utoipa::path(
    put, path = "/__admin/mail/messages/{id}/labels", tag = "mail", operation_id = "setMailMessageLabels",
    summary = "Replace the labels of a message",
    description = "Labels are stored lowercase, sorted and without duplicates. Records the audit event `mail.message.labels`.",
    params(("id" = Uuid, Path, description = "Message id"), SpaceQuery),
    request_body(content = LabelsBody, example = json!({"labels": ["otp", "signup"]})),
    responses(
        (status = 200, description = "The message with its new labels", body = MailEntry),
        (status = 400, description = "Invalid label, or more than 32", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "The caller may read the message but lacks mail:write", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, or no such message", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn set_labels(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(q): Query<SpaceQuery>,
    Json(b): Json<LabelsBody>,
) -> ApiResult<Json<MailEntry>> {
    let labels = normalize_labels(&b.labels).map_err(bad)?;
    let svc = service(&app)?;
    let msg = find(
        &app,
        &svc,
        &p,
        id,
        q.space.as_deref(),
        Permission::MailWrite,
    )
    .await?;
    let area = fastmock_core::SpaceFilter::area(msg.space.as_deref());
    let entry = svc
        .store
        .set_labels(id, &area, &labels)
        .await?
        .ok_or_else(|| not_found("message"))?;
    audit_event_in(
        &app,
        &p,
        msg.space.as_deref(),
        "mail.message.labels",
        json!({"id": id, "before": msg.labels, "after": entry.labels}),
    )
    .await;
    Ok(Json(entry))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_made_safe() {
        assert_eq!(safe_filename(Some("invoice.pdf"), "f"), "invoice.pdf");
        assert_eq!(safe_filename(Some("../../etc/passwd"), "f"), "passwd");
        assert_eq!(safe_filename(Some("a\\b\\c.txt"), "f"), "c.txt");
        assert_eq!(
            safe_filename(Some("x\"; filename=evil.html\r\nSet-Cookie: a=b"), "f"),
            "x__ filename_evil.html__Set-Cookie_ a_b"
        );
        assert_eq!(safe_filename(Some("..."), "f"), "f");
        assert_eq!(safe_filename(Some("ÄÖÜ"), "f"), "f");
        assert_eq!(safe_filename(None, "part-2"), "part-2");
        assert_eq!(safe_filename(Some(&"a".repeat(300)), "f").len(), 100);
        let d = attachment_disposition(Some("Grüße 1.pdf"), "f");
        assert_eq!(
            d.to_str().unwrap(),
            "attachment; filename=\"Gr__e 1.pdf\"; filename*=UTF-8''Gr%C3%BC%C3%9Fe%201.pdf"
        );
        let d = attachment_disposition(Some("plain.txt"), "f");
        assert_eq!(d.to_str().unwrap(), "attachment; filename=\"plain.txt\"");
        let d = attachment_disposition(Some("a\r\nb.txt"), "f");
        assert!(!d.to_str().unwrap().contains('\r'));
    }

    #[test]
    fn content_types_are_checked() {
        assert_eq!(safe_content_type("Image/PNG"), "image/png");
        assert_eq!(
            safe_content_type("text/html; charset=utf-8"),
            "application/octet-stream"
        );
        assert_eq!(safe_content_type("nonsense"), "application/octet-stream");
        assert_eq!(
            safe_content_type("application/vnd.ms-excel"),
            "application/vnd.ms-excel"
        );
    }
}
