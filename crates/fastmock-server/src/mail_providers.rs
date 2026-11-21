//! Provider HTTP APIs on the data plane (M7): `/__mail/<provider>/...`, or `/__mail/<provider>/@<space>/...`
//! to store into one space. The prefix is reserved while mail is enabled: stubs never answer it and its
//! requests never count as unmatched; they are journaled like other data-plane traffic.

use std::collections::BTreeSet;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{FromRequest, OriginalUri, Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use fastmock_core::mail::{Envelope, MailSource};
use fastmock_mail::chaos::{ChaosEffect, area_key};
use fastmock_mail::providers::{
    self, Fail, FormPart, Op, Parsed, Provider, Reply, failure, path_failure,
};
use fastmock_mail::{AcceptError, Destination, Incoming};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::mail_webhooks;
use crate::state::AppState;

/// The reserved prefix.
pub const PREFIX: &str = "/__mail";
/// Most form fields of one request.
const MAX_FORM_PARTS: usize = 10_000;
/// How long Resend idempotency keys are remembered.
const IDEMPOTENCY_TTL: Duration = Duration::from_secs(24 * 3600);

/// The space a provider request stored into, for the request journal.
#[derive(Clone, Debug)]
pub struct JournalSpace(pub Option<String>);

/// The data-plane service mounted at [`PREFIX`].
pub fn service(app: AppState) -> Router {
    Router::new().fallback(handle).with_state(app)
}

fn to_response(r: Reply, space: Option<String>) -> Response {
    let mut resp = Response::new(Body::from(r.body));
    *resp.status_mut() =
        StatusCode::from_u16(r.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    for (k, v) in r.headers {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(&v),
        ) {
            resp.headers_mut().append(n, v);
        }
    }
    resp.extensions_mut().insert(JournalSpace(space));
    resp
}

fn unknown(provider: &str) -> Response {
    let names: Vec<&str> = Provider::ALL.iter().map(|p| p.name()).collect();
    (
        StatusCode::NOT_FOUND,
        axum::Json(json!({"error": format!(
            "unknown mail provider `{}`; FastMock answers {} under {PREFIX}/<provider>/",
            provider.chars().take(64).collect::<String>(),
            names.join(", ")
        )})),
    )
        .into_response()
}

/// Splits `/<provider>[/@<space>]/<path>`.
fn split(path: &str) -> (String, Option<String>, String) {
    let rest = path.trim_start_matches('/');
    let (provider, after) = match rest.split_once('/') {
        Some((p, a)) => (p.to_string(), format!("/{a}")),
        None => (rest.to_string(), String::new()),
    };
    if let Some(s) = after.strip_prefix("/@") {
        let (space, p) = match s.split_once('/') {
            Some((sp, p)) => (sp.to_string(), format!("/{p}")),
            None => (s.to_string(), String::new()),
        };
        return (provider, Some(space), p);
    }
    (provider, None, after)
}

/// CRC-32 (IEEE) for the gzip trailer.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for b in data {
        crc ^= *b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Decodes a gzip body (`Content-Encoding: gzip`, as SendGrid clients may send) up to `limit` bytes.
pub fn gunzip(data: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let bad = || "the gzip body is not valid".to_string();
    if data.len() < 18 || data[..3] != [0x1f, 0x8b, 8] {
        return Err(bad());
    }
    let flags = data[3];
    let mut pos = 10usize;
    if flags & 4 != 0 {
        let xlen = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2 + xlen;
    }
    for bit in [8u8, 16] {
        if flags & bit != 0 {
            let end = data
                .get(pos..)
                .and_then(|d| d.iter().position(|b| *b == 0))
                .ok_or_else(bad)?;
            pos += end + 1;
        }
    }
    if flags & 2 != 0 {
        pos += 2;
    }
    if pos + 8 > data.len() {
        return Err(bad());
    }
    let out = miniz_oxide::inflate::decompress_to_vec_with_limit(&data[pos..data.len() - 8], limit)
        .map_err(|e| match e.status {
            miniz_oxide::inflate::TINFLStatus::HasMoreOutput => {
                "the decompressed body is too large".to_string()
            }
            _ => bad(),
        })?;
    let t = &data[data.len() - 8..];
    let crc = u32::from_le_bytes([t[0], t[1], t[2], t[3]]);
    let size = u32::from_le_bytes([t[4], t[5], t[6], t[7]]);
    if crc != crc32(&out) || size != out.len() as u32 {
        return Err(bad());
    }
    Ok(out)
}

async fn read_form(
    parts: &axum::http::request::Parts,
    body: &[u8],
    media: Option<&str>,
) -> Result<Vec<FormPart>, Fail> {
    match media {
        Some("application/x-www-form-urlencoded") => {
            let v: Vec<FormPart> = url::form_urlencoded::parse(body)
                .take(MAX_FORM_PARTS + 1)
                .map(|(k, v)| FormPart::text(&k, &v))
                .collect();
            if v.len() > MAX_FORM_PARTS {
                return Err(Fail::invalid(format!(
                    "at most {MAX_FORM_PARTS} form fields"
                )));
            }
            Ok(v)
        }
        Some("multipart/form-data") => {
            let mut req = Request::new(Body::from(body.to_vec()));
            *req.headers_mut() = parts.headers.clone();
            req.headers_mut().remove("content-encoding");
            let mut mp = axum::extract::Multipart::from_request(req, &())
                .await
                .map_err(|e| Fail::invalid(format!("invalid multipart body: {}", e.body_text())))?;
            let mut out = Vec::new();
            loop {
                let field = match mp.next_field().await {
                    Ok(Some(f)) => f,
                    Ok(None) => break,
                    Err(e) => {
                        return Err(Fail::invalid(format!(
                            "invalid multipart body: {}",
                            e.body_text()
                        )));
                    }
                };
                if out.len() >= MAX_FORM_PARTS {
                    return Err(Fail::invalid(format!(
                        "at most {MAX_FORM_PARTS} form fields"
                    )));
                }
                let name = field.name().unwrap_or_default().to_string();
                let filename = field.file_name().map(String::from);
                let content_type = field.content_type().map(String::from);
                let data = field.bytes().await.map_err(|e| {
                    Fail::invalid(format!("invalid multipart body: {}", e.body_text()))
                })?;
                out.push(FormPart {
                    name,
                    filename,
                    content_type,
                    data,
                });
            }
            Ok(out)
        }
        _ => Err(Fail::Media(
            "send multipart/form-data or application/x-www-form-urlencoded".into(),
        )),
    }
}

fn with_chaos(mut r: Response, rule: &str) -> Response {
    if let Ok(v) = HeaderValue::from_str(rule) {
        r.headers_mut()
            .insert(HeaderName::from_static("x-fastmock-chaos"), v);
    }
    r
}

async fn handle(
    State(app): State<AppState>,
    OriginalUri(uri): OriginalUri,
    req: Request,
) -> Response {
    let Some(mail) = app.mail.clone() else {
        return unknown("");
    };
    let full = uri.path().to_string();
    let rel = full.strip_prefix(PREFIX).unwrap_or(&full);
    let (pname, area, path) = split(rel);
    let Some(provider) = Provider::parse(&pname) else {
        return unknown(&pname);
    };
    if let Some(s) = &area
        && !mail.spaces.contains(s)
    {
        return to_response(
            path_failure(
                provider,
                &path,
                &Fail::NotFound(format!("space {s} not found")),
            ),
            None,
        );
    }
    let op = match providers::route(provider, req.method().as_str(), &path) {
        Ok(op) => op,
        Err(f) => return to_response(path_failure(provider, &path, &f), area),
    };
    let (parts, body) = req.into_parts();
    let reply = call(&app, &mail, &op, area.as_deref(), &parts, body).await;
    metrics::counter!(
        "fastmock_mail_provider_requests_total",
        "provider" => provider.name(),
        "status_class" => format!("{}xx", reply.0.status().as_u16() / 100)
    )
    .increment(1);
    reply.0
}

struct Answer(Response);

async fn call(
    app: &AppState,
    mail: &fastmock_mail::MailService,
    op: &Op,
    area: Option<&str>,
    parts: &axum::http::request::Parts,
    body: Body,
) -> Answer {
    let space = area.map(String::from);
    let fail = |f: Fail| Answer(to_response(failure(op, &f), space.clone()));
    let body = match to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => return fail(Fail::invalid("the request body could not be read")),
    };
    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_ascii_lowercase(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let body: Vec<u8> = match providers::header(&headers, "content-encoding")
        .map(|e| e.trim().to_ascii_lowercase())
    {
        None => body.to_vec(),
        Some(e) if e.is_empty() || e == "identity" => body.to_vec(),
        Some(e) if e == "gzip" || e == "x-gzip" => match gunzip(&body, app.cfg.max_body_bytes) {
            Ok(b) => b,
            Err(m) if m.contains("too large") => return fail(Fail::TooLarge),
            Err(m) => return fail(Fail::invalid(m)),
        },
        Some(e) => {
            return fail(Fail::Media(format!(
                "Content-Encoding {} is not supported; use gzip or none",
                e.chars().take(32).collect::<String>()
            )));
        }
    };
    let media = providers::media_type(&headers);
    let form = if op.takes_form() {
        match read_form(parts, &body, media.as_deref()).await {
            Ok(f) => f,
            Err(f) => return fail(f),
        }
    } else {
        vec![]
    };
    let request = providers::Request {
        headers: &headers,
        query: parts.uri.query().unwrap_or(""),
        body: &body,
        form: &form,
        max_bytes: mail.config.max_message_bytes,
    };
    let key = match providers::credential(op, &request) {
        Ok(k) => k,
        Err(f) => return fail(f),
    };
    let mut parsed: Parsed = match providers::parse(op, &request) {
        Ok(p) => p,
        Err(f) => return fail(f),
    };
    let ok: Vec<&providers::OutMessage> = parsed
        .items
        .iter()
        .filter_map(|i| i.as_ref().ok())
        .collect();
    let areas: Vec<Option<String>> = match area {
        Some(a) => vec![Some(a.to_string())],
        None => {
            let mut set: BTreeSet<Option<String>> = BTreeSet::new();
            for m in &ok {
                for r in mail.routes(&m.rcpt_to, None) {
                    set.insert(r.space);
                }
            }
            if set.is_empty() {
                set.insert(None);
            }
            set.into_iter().collect()
        }
    };
    let provider = op.provider();
    match mail_webhooks::key_allowed(&app.stores, &areas, provider, &key).await {
        Ok(true) => {}
        Ok(false) => return fail(Fail::BadKey),
        Err(e) => {
            metrics::counter!("fastmock_store_errors_total", "store" => "mail_webhooks")
                .increment(1);
            tracing::warn!(error = %e, "reading the expected provider keys failed");
            return fail(Fail::Internal("storage".into()));
        }
    }
    let mut chaos_rule = None;
    if let Some(chaos) = &app.mail_chaos {
        let from = ok.first().map(|m| m.mail_from.clone()).unwrap_or_default();
        let rcpts: Vec<String> = ok.iter().flat_map(|m| m.rcpt_to.iter().cloned()).collect();
        for a in &areas {
            if let Some(d) = chaos
                .evaluate_provider(a.as_deref(), provider.name(), &from, &rcpts)
                .await
            {
                if let Some(delay) = d.delay {
                    tokio::time::sleep(delay).await;
                }
                if let ChaosEffect::ProviderError(name) = &d.effect {
                    let r = to_response(failure(op, &Fail::Named(name.clone())), space.clone());
                    return Answer(with_chaos(r, &d.rule_id));
                }
                chaos_rule = Some(d.rule_id);
                break;
            }
        }
    }
    let idem = match op {
        Op::ResendSend => providers::header(&headers, "idempotency-key").map(|k| {
            (
                format!(
                    "mail:idem:resend:{}:{}",
                    area_key(area),
                    hex::encode(Sha256::digest(k.as_bytes()))
                ),
                hex::encode(Sha256::digest(&body)),
            )
        }),
        _ => None,
    };
    if let Some((k, digest)) = &idem {
        match app.stores.hot.get(k).await {
            Ok(Some(v)) => {
                let (d, reply) = v.split_once('|').unwrap_or_default();
                let r = if d == digest {
                    Reply::json(200, &serde_json::from_str(reply).unwrap_or_default())
                } else {
                    failure(op, &Fail::Named("invalid_idempotent_request".into()))
                };
                return Answer(to_response(r, space.clone()));
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "reading an idempotency key failed"),
        }
    }
    let destination = match area {
        Some(a) => Destination::Area(Some(a.to_string())),
        None => Destination::Route,
    };
    let mut stored_spaces: BTreeSet<Option<String>> = BTreeSet::new();
    for item in parsed.items.iter_mut() {
        let Ok(m) = item else {
            continue;
        };
        let mut inc = Incoming::new(
            MailSource::Provider,
            Envelope {
                mail_from: m.mail_from.clone(),
                rcpt_to: m.rcpt_to.clone(),
                ..Default::default()
            },
            m.raw.clone(),
        );
        inc.provider = Some(provider.name().to_string());
        inc.provider_extras = Some(m.extras.clone());
        inc.destination = destination.clone();
        match mail.accept(inc).await {
            Ok(entries) => {
                for e in &entries {
                    stored_spaces.insert(e.space.clone());
                    mail_webhooks::emit_events(&app.stores, e).await;
                }
            }
            Err(AcceptError::TooLarge(_)) => *item = Err(Fail::TooLarge),
            Err(AcceptError::Store(e)) => {
                metrics::counter!("fastmock_store_errors_total", "store" => "mail").increment(1);
                tracing::error!(error = %e, provider = provider.name(), "storing a provider message failed");
                *item = Err(Fail::Internal("storage".into()));
            }
        }
    }
    let reply = providers::success(op, &parsed);
    if let Some((k, digest)) = &idem
        && reply.status == 200
    {
        let v = format!("{digest}|{}", String::from_utf8_lossy(&reply.body));
        if let Err(e) = app.stores.hot.set(k, &v, Some(IDEMPOTENCY_TTL)).await {
            tracing::warn!(error = %e, "storing an idempotency key failed");
        }
    }
    let journal_space = match (area, stored_spaces.len()) {
        (Some(a), _) => Some(a.to_string()),
        (None, 1) => stored_spaces.into_iter().next().flatten(),
        _ => None,
    };
    let r = to_response(reply, journal_space);
    Answer(match chaos_rule {
        Some(rule) => with_chaos(r, &rule),
        None => r,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_provider_space_and_path() {
        let s =
            |a: &str, b: Option<&str>, c: &str| (a.to_string(), b.map(String::from), c.to_string());
        assert_eq!(split("/ses/"), s("ses", None, "/"));
        assert_eq!(split("/ses"), s("ses", None, ""));
        assert_eq!(
            split("/sendgrid/@billing/v3/mail/send"),
            s("sendgrid", Some("billing"), "/v3/mail/send")
        );
        assert_eq!(split("/ses/@billing"), s("ses", Some("billing"), ""));
        assert_eq!(split("/ses/@billing/"), s("ses", Some("billing"), "/"));
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
        out.extend(miniz_oxide::deflate::compress_to_vec(data, 6));
        out.extend(crc32(data).to_le_bytes());
        out.extend((data.len() as u32).to_le_bytes());
        out
    }

    #[test]
    fn gunzip_checks_crc_and_limit() {
        let data = b"{\"hello\":\"world\"}".repeat(100);
        assert_eq!(gunzip(&gzip(&data), 1 << 20).unwrap(), data);
        assert!(gunzip(&gzip(&data), 100).unwrap_err().contains("too large"));
        let mut broken = gzip(&data);
        let n = broken.len();
        broken[n - 6] ^= 1;
        assert!(gunzip(&broken, 1 << 20).is_err());
        assert!(gunzip(b"not gzip at all, no", 1 << 20).is_err());
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
