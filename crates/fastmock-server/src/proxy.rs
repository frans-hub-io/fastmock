use std::collections::BTreeMap;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use fastmock_core::outbound::blocked_outbound_header;
use fastmock_core::{ProxyDef, RequestPattern, ResponseDef, StringMatcher, Stub};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::state::AppState;

const HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];
const MAX_UPSTREAM_BODY: usize = 16 * 1024 * 1024;
pub const RECORDING_KEY: &str = "recording:active";

pub struct Upstream {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

/// The upstream URL of a proxied request: `baseUrl` + the path without `stripPrefix` + the query.
pub fn target_url(def: &ProxyDef, path: &str, query: Option<&str>) -> Result<reqwest::Url, String> {
    let rel = match &def.strip_prefix {
        Some(p) => {
            let p = p.trim_end_matches('/');
            match path.strip_prefix(p) {
                Some(rest) if rest.is_empty() || rest.starts_with('/') => rest,
                _ => path,
            }
        }
        None => path,
    };
    let mut url = format!(
        "{}{}",
        def.base_url.trim_end_matches('/'),
        if rel.starts_with('/') {
            rel.to_string()
        } else {
            format!("/{rel}")
        }
    );
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        url.push('?');
        url.push_str(q);
    }
    reqwest::Url::parse(&url).map_err(|e| format!("invalid upstream url: {e}"))
}

pub async fn forward(
    app: &AppState,
    def: &ProxyDef,
    method: &str,
    path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Upstream, String> {
    let parsed = target_url(def, path, query)?;
    app.policy
        .check_url(&parsed)
        .map_err(|e| format!("blocked by egress policy: {e}"))?;
    let m =
        reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| "invalid method".to_string())?;
    let mut rb = app.http.request(m, parsed).timeout(Duration::from_secs(30));
    let removed: Vec<String> = def
        .remove_headers
        .iter()
        .map(|h| h.to_ascii_lowercase())
        .collect();
    for (k, v) in headers {
        let name = k.as_str();
        if blocked_outbound_header(name) || removed.iter().any(|r| r == name) {
            continue;
        }
        rb = rb.header(name, v.as_bytes());
    }
    for (k, v) in &def.additional_headers {
        if blocked_outbound_header(k) {
            continue;
        }
        rb = rb.header(k, v);
    }
    let resp = rb
        .body(body)
        .send()
        .await
        .map_err(|e| format!("upstream request failed: {e}"))?;
    let status = resp.status().as_u16();
    let hdrs: Vec<(String, String)> = resp
        .headers()
        .iter()
        .filter(|(k, _)| !HOP.contains(&k.as_str()))
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_UPSTREAM_BODY as u64)
    {
        return Err("upstream body exceeds 16 MiB".into());
    }
    let mut resp = resp;
    let mut buf = bytes::BytesMut::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("reading upstream body failed: {e}"))?
    {
        if buf.len() + chunk.len() > MAX_UPSTREAM_BODY {
            return Err("upstream body exceeds 16 MiB".into());
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Upstream {
        status,
        headers: hdrs,
        body: buf.freeze(),
    })
}

pub fn to_response(u: Upstream) -> Response {
    let mut r = Response::new(Body::from(u.body));
    *r.status_mut() = StatusCode::from_u16(u.status).unwrap_or(StatusCode::BAD_GATEWAY);
    for (k, v) in u.headers {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(&v),
        ) {
            r.headers_mut().append(n, v);
        }
    }
    r
}

pub fn bad_gateway(msg: &str) -> Response {
    metrics::counter!("fastmock_proxy_errors_total").increment(1);
    tracing::warn!(error = %msg, "proxying failed");
    let reason = if msg.starts_with("blocked by egress policy") {
        "blocked by egress policy"
    } else if msg.contains("exceeds") {
        "upstream body too large"
    } else {
        "upstream unavailable"
    };
    (
        StatusCode::BAD_GATEWAY,
        axum::Json(json!({"error": "proxy failed", "detail": reason})),
    )
        .into_response()
}

#[derive(Clone, Debug, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Recording {
    pub target_base_url: String,
    /// Upstream of gRPC calls when it differs from `targetBaseUrl`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc_target_base_url: Option<String>,
    pub tag: String,
    #[serde(default)]
    pub capture_headers: Vec<String>,
    #[serde(default)]
    pub request_headers: Vec<String>,
    #[serde(default)]
    pub started_by: String,
    #[serde(default)]
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Stores a stub recorded from a proxied exchange, unless a request with the same `identity` was recorded
/// already or the daily limit (`FASTMOCK_RECORDING_MAX_STUBS`) is reached; returns the stored stub.
pub async fn store_recorded(app: &AppState, identity: &str, stub: Stub) -> Option<Stub> {
    let key = format!(
        "recording:seen:{}",
        fastmock_core::template::hmac_bytes("sha256", b"rec", identity.as_bytes())
            .map(hex::encode)
            .unwrap_or_default()
    );
    let fresh = app.stores.hot.get(&key).await.ok().flatten().is_none();
    if !fresh {
        return None;
    }
    let within_limit = app
        .stores
        .hot
        .incr("recording:count", Some(Duration::from_secs(86_400)))
        .await
        .map(|n| n <= app.cfg.recording_max_stubs)
        .unwrap_or(false);
    if !within_limit {
        metrics::counter!("fastmock_recording_skipped_total", "reason" => "limit").increment(1);
        return None;
    }
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
            Some(stub)
        }
        Err(e) => {
            metrics::counter!("fastmock_recording_skipped_total", "reason" => "invalid")
                .increment(1);
            let path = stub.request.url_path.clone().unwrap_or_default();
            tracing::warn!(error = %e, path = %path, "recorded exchange could not be stored");
            None
        }
    }
}

pub async fn active_recording(app: &AppState) -> Option<Recording> {
    let raw = app.stores.hot.get(RECORDING_KEY).await.ok().flatten()?;
    serde_json::from_str(&raw).ok()
}

#[allow(clippy::too_many_arguments)]
pub fn recorded_stub(
    redactor: &crate::redact::Redactor,
    rec: &Recording,
    method: &str,
    path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    body: &Bytes,
    up: &Upstream,
) -> Stub {
    let mut req = RequestPattern {
        method: Some(method.to_string()),
        url_path: Some(path.to_string()),
        ..Default::default()
    };
    for (k, v) in fastmock_core::request::parse_query(query.unwrap_or("")) {
        req.query.insert(
            k,
            StringMatcher {
                equal_to: Some(v),
                ..Default::default()
            },
        );
    }
    for h in &rec.request_headers {
        if let Some(v) = headers.get(h.as_str()).and_then(|v| v.to_str().ok()) {
            let sensitive = redactor.sensitive_header(h);
            req.headers.insert(
                h.to_ascii_lowercase(),
                StringMatcher {
                    equal_to: (!sensitive).then(|| v.to_string()),
                    matches: sensitive.then(|| ".+".to_string()),
                    ..Default::default()
                },
            );
        }
    }
    if !body.is_empty() {
        match serde_json::from_slice::<Value>(body) {
            Ok(mut v) => {
                let had_secrets = redactor.strip_sensitive(&mut v);
                req.body.push(fastmock_core::BodyMatcher {
                    equal_to_json: Some(v),
                    ignore_extra_elements: had_secrets,
                    ..Default::default()
                })
            }
            Err(_) => {
                let text = String::from_utf8_lossy(body).into_owned();
                if redactor.mask_text(&text) == text {
                    req.body.push(fastmock_core::BodyMatcher {
                        equal_to: Some(text),
                        ..Default::default()
                    })
                }
            }
        }
    }
    let mut resp = ResponseDef {
        status: up.status,
        templated: false,
        ..Default::default()
    };
    let keep: Vec<String> = rec
        .capture_headers
        .iter()
        .map(|h| h.to_ascii_lowercase())
        .chain(["content-type".to_string()])
        .collect();
    let mut hm = BTreeMap::new();
    for (k, v) in &up.headers {
        if keep.contains(&k.to_ascii_lowercase()) {
            hm.insert(k.clone(), v.clone());
        }
    }
    resp.headers = hm;
    let is_json = up
        .headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("content-type") && v.contains("json"));
    match (is_json, std::str::from_utf8(&up.body)) {
        (true, Ok(t)) if serde_json::from_str::<Value>(t).is_ok() => {
            resp.json_body = serde_json::from_str(t).ok()
        }
        (_, Ok(t)) => resp.body = Some(t.to_string()),
        (_, Err(_)) => {
            use base64::Engine as _;
            resp.base64_body = Some(base64::engine::general_purpose::STANDARD.encode(&up.body));
        }
    }
    Stub {
        id: Uuid::now_v7(),
        name: Some(format!("recorded {method} {path}")),
        priority: 5,
        request: req,
        response: resp,
        scenario: None,
        callbacks: vec![],
        messages: vec![],
        tags: vec![rec.tag.clone(), "recorded".into()],
        metadata: None,
        enabled: true,
        updated_at: None,
        ..Default::default()
    }
}
