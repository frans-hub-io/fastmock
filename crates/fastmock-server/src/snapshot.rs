use std::collections::BTreeMap;

use axum::extract::{Extension, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use axum::routing::post;
use axum::{Json, Router};
use fastmock_core::{JournalEntry, JournalFilter, RequestPattern, Stub};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::admin::{ApiResult, ErrorBody, audit_event, bad, refresh, valid_tag};
use crate::auth::Principal;
use crate::journal_api::{SCAN_WINDOW, compile_pattern, entry_matches};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/recordings/snapshot", post(snapshot))
}

#[derive(Deserialize, Default, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotFilters {
    /// Only entries matching this request pattern.
    #[serde(default)]
    pub request: Option<RequestPattern>,
    /// Only these journal entry ids.
    #[serde(default)]
    pub ids: Option<Vec<i64>>,
    #[serde(default)]
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub source: Option<String>,
    /// Only matched (true) or unmatched (false) entries.
    #[serde(default)]
    pub matched: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum OutputFormat {
    #[default]
    Full,
    Ids,
}

fn snapshot_tag() -> String {
    "snapshot".into()
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotBody {
    #[serde(default)]
    pub filters: SnapshotFilters,
    /// Request headers to add as `equalTo` matchers (sensitive ones become `matches: .+`).
    #[serde(default)]
    pub capture_headers: Vec<String>,
    /// Turn repeated identical requests with different responses into a scenario.
    #[serde(default)]
    pub repeats_as_scenarios: bool,
    /// Save the stubs (default true); false only returns them.
    #[serde(default = "crate::admin::yes")]
    pub persist: bool,
    #[serde(default)]
    pub output_format: OutputFormat,
    /// Tag added to every generated stub, next to `recorded`.
    #[serde(default = "snapshot_tag")]
    pub tag: String,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotResult {
    pub persisted: bool,
    /// Generated stubs (outputFormat FULL).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stubs: Option<Vec<Stub>>,
    /// Generated stub ids (outputFormat IDS).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ids: Option<Vec<Uuid>>,
    pub warnings: Vec<String>,
}

fn signature(e: &JournalEntry) -> String {
    format!("{}\n{}\n{}\n{}", e.method, e.path, e.query, e.body)
}

fn response_headers(e: &JournalEntry) -> Vec<(String, String)> {
    let looks_json = serde_json::from_str::<Value>(&e.response_body)
        .is_ok_and(|v| v.is_object() || v.is_array());
    if looks_json {
        vec![("content-type".into(), "application/json".into())]
    } else {
        vec![]
    }
}

fn to_stub(app: &AppState, e: &JournalEntry, rec: &crate::proxy::Recording) -> Stub {
    let mut headers = HeaderMap::new();
    for (k, v) in &e.headers {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            headers.insert(n, v);
        }
    }
    let up = crate::proxy::Upstream {
        status: e.status,
        headers: response_headers(e),
        body: bytes::Bytes::from(e.response_body.clone()),
    };
    let body = bytes::Bytes::from(e.body.clone());
    let query = (!e.query.is_empty()).then_some(e.query.as_str());
    let mut s = crate::proxy::recorded_stub(
        &app.redactor,
        rec,
        &e.method,
        &e.path,
        query,
        &headers,
        &body,
        &up,
    );
    s.name = Some(format!("snapshot {} {}", e.method, e.path));
    s
}

pub fn build_stubs(
    app: &AppState,
    entries: &[JournalEntry],
    b: &SnapshotBody,
) -> (Vec<Stub>, Vec<String>) {
    let rec = crate::proxy::Recording {
        target_base_url: String::new(),
        tag: b.tag.clone(),
        capture_headers: vec![],
        request_headers: b.capture_headers.clone(),
        started_by: String::new(),
        started_at: None,
    };
    let mut warnings = Vec::new();
    let mut groups: BTreeMap<String, Vec<&JournalEntry>> = BTreeMap::new();
    let mut order = Vec::new();
    for e in entries {
        if e.response_body.contains("…[truncated ") || e.body.contains("…[truncated ") {
            warnings.push(format!(
                "request {}: body was truncated in the journal; the stub holds the truncated text",
                e.id
            ));
        }
        if e.body.starts_with("[binary ") || e.response_body.starts_with("[binary ") {
            warnings.push(format!(
                "request {}: binary bodies are not kept in the journal; the stub has a placeholder body",
                e.id
            ));
        }
        let sig = signature(e);
        if !groups.contains_key(&sig) {
            order.push(sig.clone());
        }
        groups.entry(sig).or_default().push(e);
    }
    let mut stubs = Vec::new();
    for (gi, sig) in order.iter().enumerate() {
        let group = &groups[sig];
        let mut distinct: Vec<&JournalEntry> = Vec::new();
        for e in group {
            if !distinct
                .iter()
                .any(|d| d.status == e.status && d.response_body == e.response_body)
            {
                distinct.push(e);
            }
        }
        if distinct.len() > 1 && b.repeats_as_scenarios {
            let same = |a: &JournalEntry, b: &JournalEntry| {
                a.status == b.status && a.response_body == b.response_body
            };
            let mut calls: Vec<&JournalEntry> = group.clone();
            while calls.len() > 1 && same(calls[calls.len() - 1], calls[calls.len() - 2]) {
                calls.pop();
            }
            let first = calls[0];
            let mut name = format!("{} {} {} #{}", b.tag, first.method, first.path, gi + 1);
            if name.len() > 200 {
                let mut cut = 200;
                while !name.is_char_boundary(cut) {
                    cut -= 1;
                }
                name.truncate(cut);
            }
            let n = calls.len();
            for (i, e) in calls.iter().enumerate() {
                let mut s = to_stub(app, e, &rec);
                let required = if i == 0 {
                    fastmock_core::model::SCENARIO_STARTED.to_string()
                } else {
                    format!("call-{}", i + 1)
                };
                let next = (i + 1 < n).then(|| format!("call-{}", i + 2));
                s.scenario = serde_json::from_value(
                    json!({"name": name, "requiredState": required, "newState": next}),
                )
                .ok();
                stubs.push(s);
            }
        } else {
            if distinct.len() > 1 {
                warnings.push(format!(
                    "{} {}: {} different responses for the same request; kept the first (set repeatsAsScenarios to keep all)",
                    distinct[0].method,
                    distinct[0].path,
                    distinct.len()
                ));
            }
            stubs.push(to_stub(app, distinct[0], &rec));
        }
    }
    (stubs, warnings)
}

#[utoipa::path(
    post, path = "/__admin/recordings/snapshot", tag = "recordings", operation_id = "snapshotRecording",
    summary = "Turn journal entries into stubs (WireMock snapshot)",
    description = "Selects journal entries (newest 10000 passing `source`, `since`, `matched`; then `ids` and `request`), oldest first, and converts each distinct request into a stub with the recorded status and body. The journal keeps redacted, size-limited bodies, so secrets never reach the generated stubs; truncated bodies are reported as warnings.",
    request_body(content = SnapshotBody, example = json!({"filters": {"request": {"urlPathPattern": "/api/.*"}, "since": "2026-01-01T00:00:00Z"}, "captureHeaders": ["accept"], "repeatsAsScenarios": true, "persist": true, "outputFormat": "IDS", "tag": "snap-1"})),
    responses(
        (status = 200, description = "Generated stubs", body = SnapshotResult),
        (status = 400, description = "Invalid filters or a generated stub is invalid", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn snapshot(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Json(b): Json<SnapshotBody>,
) -> ApiResult<Json<SnapshotResult>> {
    valid_tag(&b.tag)?;
    if b.capture_headers.len() > 32 {
        return Err(bad("at most 32 captureHeaders"));
    }
    if b.filters.ids.as_ref().is_some_and(|i| i.len() > 10_000) {
        return Err(bad("at most 10000 ids"));
    }
    let compiled = b
        .filters
        .request
        .as_ref()
        .map(compile_pattern)
        .transpose()?;
    let f = JournalFilter {
        source: b.filters.source.clone(),
        since: b.filters.since,
        matched: b.filters.matched,
        limit: Some(SCAN_WINDOW),
        ..Default::default()
    };
    let mut entries = app.stores.journal.query(&f).await?;
    if let Some(ids) = &b.filters.ids {
        let set: std::collections::HashSet<i64> = ids.iter().copied().collect();
        entries.retain(|e| set.contains(&e.id));
    }
    if let Some(c) = &compiled {
        entries.retain(|e| entry_matches(c, e));
    }
    entries.sort_by_key(|e| e.id);
    let (stubs, warnings) = build_stubs(&app, &entries, &b);
    for (i, s) in stubs.iter().enumerate() {
        app.engine
            .validate(s)
            .map_err(|e| bad(format!("generated stub #{i} is invalid: {e}")))?;
    }
    if b.persist && !stubs.is_empty() {
        app.stores
            .stubs
            .replace_tag(None, &fastmock_core::SpaceFilter::Global, &stubs)
            .await?;
        refresh(&app).await?;
    }
    metrics::counter!("fastmock_snapshot_stubs_total").increment(stubs.len() as u64);
    audit_event(
        &app,
        &p,
        "recording.snapshot",
        json!({"entries": entries.len(), "stubs": stubs.len(), "persisted": b.persist, "tag": b.tag}),
    )
    .await;
    let (stubs, ids) = match b.output_format {
        OutputFormat::Full => (Some(stubs), None),
        OutputFormat::Ids => (None, Some(stubs.iter().map(|s| s.id).collect())),
    };
    Ok(Json(SnapshotResult {
        persisted: b.persist,
        stubs,
        ids,
        warnings,
    }))
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(snapshot))]
pub struct SnapshotApi;
