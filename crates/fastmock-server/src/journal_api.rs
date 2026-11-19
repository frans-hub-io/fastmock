use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use fastmock_core::matcher::CompiledStub;
use fastmock_core::{CapturedRequest, JournalEntry, JournalFilter, RequestPattern, Stub};
use serde::{Deserialize, Serialize};
use serde_json::json;

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::admin::{
    ApiError, ApiResult, ErrorBody, RequestList, audit_event, bad, journal_page, not_found,
};
use crate::auth::Principal;
use crate::state::AppState;

pub const SCAN_WINDOW: i64 = 10_000;
const MAX_SEQUENCE: usize = 50;
const MAX_NEAR_MISSES: usize = 5;
const SCAN_QUEUE_WAIT: Duration = Duration::from_secs(5);
const UNBOUNDED: Duration = Duration::from_secs(365 * 86_400);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanAbort {
    Deadline,
    Budget,
    Cancelled,
}

impl ScanAbort {
    fn label(self) -> &'static str {
        match self {
            ScanAbort::Deadline => "deadline",
            ScanAbort::Budget => "budget",
            ScanAbort::Cancelled => "cancelled",
        }
    }
}

/// Work budget of one journal scan: a deadline, a work counter (entry bytes × pattern weight) and
/// a cancellation flag set when the client goes away.
pub struct ScanBudget {
    deadline: std::time::Instant,
    left: Cell<u64>,
    cancel: Arc<AtomicBool>,
}

impl ScanBudget {
    pub fn new(timeout: Duration, work: u64) -> Self {
        Self {
            deadline: std::time::Instant::now() + timeout,
            left: Cell::new(work),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn charge(&self, units: u64) -> Result<(), ScanAbort> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(ScanAbort::Cancelled);
        }
        if std::time::Instant::now() >= self.deadline {
            return Err(ScanAbort::Deadline);
        }
        let left = self.left.get();
        if units > left {
            return Err(ScanAbort::Budget);
        }
        self.left.set(left - units);
        Ok(())
    }
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub fn entry_cost(e: &JournalEntry) -> u64 {
    let headers: usize = e.headers.iter().map(|(k, v)| k.len() + v.len()).sum();
    (e.body.len() + e.path.len() + e.query.len() + headers + 64) as u64
}

pub fn pattern_weight(p: &RequestPattern) -> u64 {
    let matchers = 1
        + p.path_params.len()
        + p.query.len()
        + p.headers.len()
        + p.cookies.len()
        + p.form_parameters.len()
        + p.body.len()
        + 2 * p.multipart.len()
        + usize::from(p.graphql.is_some());
    let size = serde_json::to_vec(p).map(|v| v.len()).unwrap_or(0) / 1024;
    (matchers + size) as u64
}

/// Runs CPU-bound journal work on the blocking pool under the per-replica scan semaphore, a
/// deadline (`FASTMOCK_JOURNAL_SCAN_TIMEOUT`) and a work budget (`FASTMOCK_JOURNAL_SCAN_MAX_WORK`).
/// Dropping the returned future (client disconnect) stops the work at the next charge.
pub async fn run_bounded<T, F>(app: &AppState, kind: &'static str, work: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(&ScanBudget) -> Result<T, ScanAbort> + Send + 'static,
{
    let started = std::time::Instant::now();
    let permit = match tokio::time::timeout(SCAN_QUEUE_WAIT, app.scans.clone().acquire_owned())
        .await
    {
        Ok(Ok(p)) => p,
        _ => {
            metrics::counter!("fastmock_journal_scans_total", "kind" => kind, "result" => "busy")
                .increment(1);
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "too many journal scans are running; retry in a moment".into(),
            ));
        }
    };
    let timeout = app.cfg.journal_scan_timeout;
    let budget = ScanBudget::new(timeout, app.cfg.journal_scan_max_work);
    let _cancel = CancelOnDrop(budget.cancel.clone());
    let handle = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work(&budget)
    });
    let outcome = match tokio::time::timeout(timeout + Duration::from_secs(1), handle).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return Err(anyhow::anyhow!("journal scan task failed: {e}").into());
        }
        Err(_) => Err(ScanAbort::Deadline),
    };
    let result = match &outcome {
        Ok(_) => "ok",
        Err(a) => a.label(),
    };
    metrics::counter!("fastmock_journal_scans_total", "kind" => kind, "result" => result)
        .increment(1);
    metrics::histogram!("fastmock_journal_scan_duration_seconds", "kind" => kind)
        .record(started.elapsed().as_secs_f64());
    outcome.map_err(|a| match a {
        ScanAbort::Budget => ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "the scan exceeds the journal scan work budget (FASTMOCK_JOURNAL_SCAN_MAX_WORK); narrow it with since, source, method or fewer matchers".into(),
        ),
        ScanAbort::Deadline | ScanAbort::Cancelled => ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "the scan exceeded {} ms (FASTMOCK_JOURNAL_SCAN_TIMEOUT); narrow it with since, source, method or simpler matchers",
                timeout.as_millis()
            ),
        ),
    })
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/requests/unmatched", get(unmatched))
        .route("/requests/find", post(find))
        .route("/requests/remove", post(remove))
        .route("/requests/{id}", get(get_request))
        .route("/verify", post(verify))
        .route("/verify/sequence", post(verify_sequence))
}

pub fn compile_pattern(p: &RequestPattern) -> ApiResult<CompiledStub> {
    let stub: Stub = serde_json::from_value(json!({"request": p, "response": {}}))
        .map_err(|e| bad(format!("invalid request pattern: {e}")))?;
    CompiledStub::compile(&stub).map_err(|e| bad(format!("invalid request pattern: {e}")))
}

pub fn captured(e: &JournalEntry) -> CapturedRequest {
    CapturedRequest::new(
        &e.method,
        &e.path,
        Some(&e.query),
        e.headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        bytes::Bytes::from(e.body.clone()),
    )
}

pub fn entry_matches(c: &CompiledStub, e: &JournalEntry) -> bool {
    let req = captured(e);
    c.match_request(&req, req.json()).is_some()
}

#[derive(Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ScanQuery {
    /// Only entries of this source (`stubs` or a plugin name).
    source: Option<String>,
    /// Only entries at or after this instant (RFC 3339).
    since: Option<chrono::DateTime<chrono::Utc>>,
    /// Maximum number of entries to return (default 100).
    #[param(minimum = 1, maximum = 10000)]
    limit: Option<i64>,
}

#[derive(Deserialize, Default, Clone, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScanScope {
    /// Only entries of this source (`stubs` or a plugin name).
    #[serde(default)]
    pub source: Option<String>,
    /// Only entries at or after this instant.
    #[serde(default)]
    pub since: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn scan(
    app: &AppState,
    pattern: &RequestPattern,
    scope: &ScanScope,
) -> ApiResult<Vec<JournalEntry>> {
    let f = JournalFilter {
        source: scope.source.clone(),
        method: pattern
            .method
            .clone()
            .filter(|m| !m.eq_ignore_ascii_case("ANY")),
        since: scope.since,
        limit: Some(SCAN_WINDOW),
        ..Default::default()
    };
    Ok(app.stores.journal.query(&f).await?)
}

type Split = (Vec<JournalEntry>, Vec<JournalEntry>);

fn partition_bounded(
    c: &CompiledStub,
    weight: u64,
    all: Vec<JournalEntry>,
    b: &ScanBudget,
) -> Result<Split, ScanAbort> {
    let mut hit = Vec::new();
    let mut miss = Vec::new();
    for e in all {
        b.charge(entry_cost(&e).saturating_mul(weight))?;
        if entry_matches(c, &e) {
            hit.push(e);
        } else {
            miss.push(e);
        }
    }
    Ok((hit, miss))
}

async fn find_matching(
    app: &AppState,
    kind: &'static str,
    pattern: &RequestPattern,
    scope: &ScanScope,
) -> ApiResult<Split> {
    let c = compile_pattern(pattern)?;
    let all = scan(app, pattern, scope).await?;
    let weight = pattern_weight(pattern);
    run_bounded(app, kind, move |b| partition_bounded(&c, weight, all, b)).await
}

#[utoipa::path(
    get, path = "/__admin/requests/{id}", tag = "requests", operation_id = "getRequest",
    summary = "One journal entry",
    params(("id" = i64, Path, description = "Journal entry id")),
    responses(
        (status = 200, description = "The entry", body = JournalEntry),
        (status = 400, description = "Invalid id", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "No such entry", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn get_request(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<JournalEntry>> {
    let id: i64 = id
        .parse()
        .map_err(|_| bad("request id must be an integer"))?;
    app.stores
        .journal
        .get(id)
        .await?
        .map(Json)
        .ok_or_else(|| not_found("request"))
}

#[derive(Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UnmatchedQuery {
    source: Option<String>,
    since: Option<chrono::DateTime<chrono::Utc>>,
    #[param(minimum = 1, maximum = 10000)]
    limit: Option<i64>,
    #[param(minimum = 0)]
    offset: Option<i64>,
}

#[utoipa::path(
    get, path = "/__admin/requests/unmatched", tag = "requests", operation_id = "listUnmatchedRequests",
    summary = "Requests that no stub matched",
    description = "Same as `GET /requests?matched=false`; use `POST /requests/near-misses` to see why one did not match.",
    params(UnmatchedQuery),
    responses(
        (status = 200, description = "Unmatched entries, newest first", body = RequestList),
        (status = 400, description = "Invalid parameters", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn unmatched(
    State(app): State<AppState>,
    Query(q): Query<UnmatchedQuery>,
) -> ApiResult<Json<RequestList>> {
    let f = JournalFilter {
        source: q.source,
        since: q.since,
        matched: Some(false),
        limit: q.limit,
        ..Default::default()
    };
    journal_page(&app, f, q.offset).await
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct FoundRequests {
    pub requests: Vec<JournalEntry>,
    /// All matching entries in the scanned window (may exceed `requests.len()`).
    pub total: usize,
}

#[utoipa::path(
    post, path = "/__admin/requests/find", tag = "verification", operation_id = "findRequests",
    summary = "Find journal entries matching a request pattern",
    description = "The body is a stub `request` pattern (WireMock `POST /__admin/requests/find`). The newest 10000 entries passing `source`, `since` and the pattern's method are scanned.",
    params(ScanQuery),
    request_body(content = RequestPattern, example = json!({"method": "POST", "urlPath": "/orders", "body": [{"jsonPath": "$.amount", "value": {"matches": "[0-9]+"}}]})),
    responses(
        (status = 200, description = "Matching entries, newest first", body = FoundRequests),
        (status = 400, description = "Invalid pattern", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 422, description = "The scan exceeds FASTMOCK_JOURNAL_SCAN_MAX_WORK", body = ErrorBody),
        (status = 503, description = "Too many journal scans are running, or the scan exceeded FASTMOCK_JOURNAL_SCAN_TIMEOUT", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn find(
    State(app): State<AppState>,
    Query(q): Query<ScanQuery>,
    Json(pattern): Json<RequestPattern>,
) -> ApiResult<Json<FoundRequests>> {
    let scope = ScanScope {
        source: q.source,
        since: q.since,
    };
    let (hit, _) = find_matching(&app, "find", &pattern, &scope).await?;
    let total = hit.len();
    let limit = q.limit.unwrap_or(100).clamp(1, SCAN_WINDOW) as usize;
    Ok(Json(FoundRequests {
        requests: hit.into_iter().take(limit).collect(),
        total,
    }))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct RemovedRequests {
    pub deleted: u64,
    pub ids: Vec<i64>,
}

#[utoipa::path(
    post, path = "/__admin/requests/remove", tag = "requests", operation_id = "removeRequests",
    summary = "Delete journal entries matching a request pattern",
    params(ScanQuery),
    request_body(content = RequestPattern, example = json!({"urlPathPattern": "/health.*"})),
    responses(
        (status = 200, description = "Deleted entries", body = RemovedRequests),
        (status = 400, description = "Invalid pattern", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires the editor role", body = ErrorBody),
        (status = 422, description = "The scan exceeds FASTMOCK_JOURNAL_SCAN_MAX_WORK", body = ErrorBody),
        (status = 503, description = "Too many journal scans are running, or the scan exceeded FASTMOCK_JOURNAL_SCAN_TIMEOUT", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn remove(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<ScanQuery>,
    Json(pattern): Json<RequestPattern>,
) -> ApiResult<Json<RemovedRequests>> {
    let scope = ScanScope {
        source: q.source,
        since: q.since,
    };
    let (hit, _) = find_matching(&app, "remove", &pattern, &scope).await?;
    let ids: Vec<i64> = hit.iter().map(|e| e.id).collect();
    let deleted = if ids.is_empty() {
        0
    } else {
        app.stores.journal.delete_ids(&ids).await?
    };
    audit_event(
        &app,
        &p,
        "journal.remove",
        json!({"deleted": deleted, "pattern": pattern}),
    )
    .await;
    Ok(Json(RemovedRequests { deleted, ids }))
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct JournalNearMiss {
    pub request_id: i64,
    pub at: chrono::DateTime<chrono::Utc>,
    pub method: String,
    pub path: String,
    /// Share of the pattern's matchers that matched, 0..1.
    pub score: f64,
    pub mismatches: Vec<String>,
}

#[cfg(test)]
fn near_misses(c: &CompiledStub, entries: &[JournalEntry]) -> Vec<JournalNearMiss> {
    near_misses_bounded(c, 1, entries, &ScanBudget::new(UNBOUNDED, u64::MAX)).unwrap_or_default()
}

fn near_misses_bounded(
    c: &CompiledStub,
    weight: u64,
    entries: &[JournalEntry],
    b: &ScanBudget,
) -> Result<Vec<JournalNearMiss>, ScanAbort> {
    let mut v: Vec<JournalNearMiss> = Vec::new();
    for e in entries {
        b.charge(entry_cost(e).saturating_mul(weight))?;
        let req = captured(e);
        let nm = c.near_miss(&req, req.json());
        if nm.mismatches.is_empty() {
            continue;
        }
        v.push(JournalNearMiss {
            request_id: e.id,
            at: e.at,
            method: e.method.clone(),
            path: e.path.clone(),
            score: nm.score,
            mismatches: nm.mismatches,
        });
    }
    v.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(b.request_id.cmp(&a.request_id))
    });
    v.truncate(MAX_NEAR_MISSES);
    Ok(v)
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VerifyBody {
    pub request: RequestPattern,
    /// Exactly this many matching requests.
    #[serde(default)]
    pub count: Option<u64>,
    /// Same as `count` (MockServer and WireMock wording); give only one of them.
    #[serde(default)]
    pub exactly: Option<u64>,
    #[serde(default)]
    pub at_least: Option<u64>,
    #[serde(default)]
    pub at_most: Option<u64>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub since: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct VerifyResult {
    pub verified: bool,
    /// Matching requests found.
    pub count: u64,
    /// The expectation, e.g. `exactly 2`, `at least 1`, `between 1 and 3`.
    pub expected: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub details: Vec<String>,
    /// Closest non-matching requests (only on failure).
    pub near_misses: Vec<JournalNearMiss>,
}

fn verified(r: VerifyResult) -> Response {
    let status = if r.verified {
        StatusCode::OK
    } else {
        StatusCode::UNPROCESSABLE_ENTITY
    };
    metrics::counter!("fastmock_admin_verifications_total", "kind" => "count", "result" => if r.verified { "pass" } else { "fail" }).increment(1);
    (status, Json(r)).into_response()
}

pub fn expectation(b: &VerifyBody) -> Result<(u64, u64, String), String> {
    if b.count.is_some() && b.exactly.is_some() {
        return Err("use either count or exactly".into());
    }
    match (b.count.or(b.exactly), b.at_least, b.at_most) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            Err("use either count/exactly or atLeast/atMost".into())
        }
        (Some(n), None, None) => Ok((n, n, format!("exactly {n}"))),
        (None, lo, hi) => {
            let lo_v = lo.unwrap_or(if hi.is_some() { 0 } else { 1 });
            let hi_v = hi.unwrap_or(u64::MAX);
            if lo_v > hi_v {
                return Err("atLeast must not exceed atMost".into());
            }
            let text = match (lo, hi) {
                (Some(l), Some(h)) => format!("between {l} and {h}"),
                (None, Some(h)) => format!("at most {h}"),
                _ => format!("at least {lo_v}"),
            };
            Ok((lo_v, hi_v, text))
        }
    }
}

#[utoipa::path(
    post, path = "/__admin/verify", tag = "verification", operation_id = "verify",
    summary = "Verify how often a request was received",
    description = "Counts journal entries matching `request` (a stub request pattern). Without `count`/`exactly`/`atLeast`/`atMost` the expectation is `atLeast: 1`. Answers 200 when the expectation holds and 422 with the actual count and the closest non-matching requests when it does not.",
    request_body(content = VerifyBody, example = json!({"request": {"method": "POST", "urlPath": "/orders"}, "exactly": 1})),
    responses(
        (status = 200, description = "Expectation met", body = VerifyResult),
        (status = 400, description = "Invalid pattern or expectation", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 422, description = "Expectation not met (`VerifyResult`), or the scan exceeds FASTMOCK_JOURNAL_SCAN_MAX_WORK (`ErrorBody`)", body = VerifyResult),
        (status = 503, description = "Too many journal scans are running, or the scan exceeded FASTMOCK_JOURNAL_SCAN_TIMEOUT", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn verify(State(app): State<AppState>, Json(b): Json<VerifyBody>) -> ApiResult<Response> {
    let (lo, hi, expected) = expectation(&b).map_err(bad)?;
    let scope = ScanScope {
        source: b.source.clone(),
        since: b.since,
    };
    let c = compile_pattern(&b.request)?;
    let all = scan(&app, &b.request, &scope).await?;
    let weight = pattern_weight(&b.request);
    let (count, near) = run_bounded(&app, "verify", move |budget| {
        let (hit, miss) = partition_bounded(&c, weight, all, budget)?;
        let count = hit.len() as u64;
        if (lo..=hi).contains(&count) {
            return Ok((count, None));
        }
        Ok((count, Some(near_misses_bounded(&c, weight, &miss, budget)?)))
    })
    .await?;
    let Some(near) = near else {
        return Ok(verified(VerifyResult {
            verified: true,
            count,
            expected,
            error: None,
            details: vec![],
            near_misses: vec![],
        }));
    };
    let details = near
        .iter()
        .map(|n| {
            format!(
                "request {} {} {}: {}",
                n.request_id,
                n.method,
                n.path,
                n.mismatches.join("; ")
            )
        })
        .collect();
    Ok(verified(VerifyResult {
        verified: false,
        count,
        error: Some(format!(
            "expected {expected} matching requests, received {count}"
        )),
        expected,
        details,
        near_misses: near,
    }))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VerifySequenceBody {
    /// Patterns that must have been received in this order (other requests may come in between). 1..50.
    pub requests: Vec<RequestPattern>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub since: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SequenceResult {
    pub verified: bool,
    /// Number of patterns found in order.
    pub matched: usize,
    /// Journal ids of the requests that satisfied the patterns, in order.
    pub request_ids: Vec<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub details: Vec<String>,
}

pub fn match_sequence(compiled: &[CompiledStub], chronological: &[JournalEntry]) -> Vec<i64> {
    match_sequence_bounded(
        compiled,
        &[],
        chronological,
        &ScanBudget::new(UNBOUNDED, u64::MAX),
    )
    .unwrap_or_default()
}

fn match_sequence_bounded(
    compiled: &[CompiledStub],
    weights: &[u64],
    chronological: &[JournalEntry],
    b: &ScanBudget,
) -> Result<Vec<i64>, ScanAbort> {
    let mut ids = Vec::new();
    let mut i = 0;
    for e in chronological {
        if i == compiled.len() {
            break;
        }
        b.charge(entry_cost(e).saturating_mul(weights.get(i).copied().unwrap_or(1)))?;
        if entry_matches(&compiled[i], e) {
            ids.push(e.id);
            i += 1;
        }
    }
    Ok(ids)
}

#[utoipa::path(
    post, path = "/__admin/verify/sequence", tag = "verification", operation_id = "verifySequence",
    summary = "Verify that requests arrived in a given order",
    description = "MockServer-style sequence verification over the newest 10000 journal entries passing `source` and `since`. Answers 200 when every pattern was matched by a later request than the previous one, otherwise 422.",
    request_body(content = VerifySequenceBody, example = json!({"requests": [{"method": "POST", "urlPath": "/login"}, {"method": "GET", "urlPath": "/profile"}, {"method": "POST", "urlPath": "/logout"}]})),
    responses(
        (status = 200, description = "Sequence found", body = SequenceResult),
        (status = 400, description = "Invalid pattern", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 422, description = "Sequence not found (`SequenceResult`), or the scan exceeds FASTMOCK_JOURNAL_SCAN_MAX_WORK (`ErrorBody`)", body = SequenceResult),
        (status = 503, description = "Too many journal scans are running, or the scan exceeded FASTMOCK_JOURNAL_SCAN_TIMEOUT", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn verify_sequence(
    State(app): State<AppState>,
    Json(b): Json<VerifySequenceBody>,
) -> ApiResult<Response> {
    if b.requests.is_empty() || b.requests.len() > MAX_SEQUENCE {
        return Err(bad(format!(
            "requests must hold 1..{MAX_SEQUENCE} patterns"
        )));
    }
    let compiled = b
        .requests
        .iter()
        .enumerate()
        .map(|(i, p)| compile_pattern(p).map_err(|e| bad(format!("requests[{i}]: {}", e.1))))
        .collect::<ApiResult<Vec<_>>>()?;
    let f = JournalFilter {
        source: b.source.clone(),
        since: b.since,
        limit: Some(SCAN_WINDOW),
        ..Default::default()
    };
    let mut entries = app.stores.journal.query(&f).await?;
    entries.sort_by_key(|e| e.id);
    let weights: Vec<u64> = b.requests.iter().map(pattern_weight).collect();
    let patterns = compiled.len();
    let ids = run_bounded(&app, "sequence", move |budget| {
        match_sequence_bounded(&compiled, &weights, &entries, budget)
    })
    .await?;
    let ok = ids.len() == patterns;
    metrics::counter!("fastmock_admin_verifications_total", "kind" => "sequence", "result" => if ok { "pass" } else { "fail" }).increment(1);
    let (error, details) = if ok {
        (None, vec![])
    } else {
        let k = ids.len();
        let p = &b.requests[k];
        let what = format!(
            "{} {}",
            p.method.as_deref().unwrap_or("ANY"),
            p.url_path
                .as_deref()
                .or(p.url_path_template.as_deref())
                .or(p.url_path_pattern.as_deref())
                .unwrap_or("*")
        );
        let after = ids
            .last()
            .map(|id| format!(" after request {id}"))
            .unwrap_or_default();
        (
            Some(format!(
                "sequence broken at requests[{k}]: {} of {} patterns matched in order",
                k, patterns
            )),
            vec![format!("no request matching requests[{k}] ({what}){after}")],
        )
    };
    let r = SequenceResult {
        verified: ok,
        matched: ids.len(),
        request_ids: ids,
        error,
        details,
    };
    Ok((
        if ok {
            StatusCode::OK
        } else {
            StatusCode::UNPROCESSABLE_ENTITY
        },
        Json(r),
    )
        .into_response())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(get_request, unmatched, find, remove, verify, verify_sequence))]
pub struct JournalApi;

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64, method: &str, path: &str, body: &str) -> JournalEntry {
        serde_json::from_value(json!({
            "id": id, "at": chrono::Utc::now(), "source": "stubs", "method": method, "path": path,
            "query": "", "headers": {}, "body": body, "status": 200, "responseBody": "",
            "matched": true, "durationMs": 1
        }))
        .unwrap()
    }

    fn pat(v: serde_json::Value) -> RequestPattern {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn patterns_match_journal_entries() {
        let c = compile_pattern(&pat(
            json!({"method": "POST", "urlPath": "/orders", "body": [{"jsonPath": "$.amount", "value": {"equalTo": "5"}}]}),
        ))
        .unwrap();
        assert!(entry_matches(
            &c,
            &entry(1, "POST", "/orders", r#"{"amount":5}"#)
        ));
        assert!(!entry_matches(
            &c,
            &entry(2, "POST", "/orders", r#"{"amount":6}"#)
        ));
        assert!(!entry_matches(
            &c,
            &entry(3, "GET", "/orders", r#"{"amount":5}"#)
        ));
        let nm = near_misses(&c, &[entry(2, "POST", "/orders", r#"{"amount":6}"#)]);
        assert_eq!(nm.len(), 1);
        assert!(nm[0].score > 0.5 && nm[0].score < 1.0);
        assert!(compile_pattern(&pat(json!({"urlPathPattern": "("}))).is_err());
    }

    #[test]
    fn expectations() {
        let b = |v: serde_json::Value| -> VerifyBody {
            let mut v = v;
            v["request"] = json!({});
            serde_json::from_value(v).unwrap()
        };
        assert_eq!(
            expectation(&b(json!({}))).unwrap(),
            (1, u64::MAX, "at least 1".into())
        );
        assert_eq!(
            expectation(&b(json!({"exactly": 0}))).unwrap(),
            (0, 0, "exactly 0".into())
        );
        assert_eq!(expectation(&b(json!({"count": 2}))).unwrap().2, "exactly 2");
        assert_eq!(
            expectation(&b(json!({"atMost": 3}))).unwrap(),
            (0, 3, "at most 3".into())
        );
        assert_eq!(
            expectation(&b(json!({"atLeast": 1, "atMost": 3})))
                .unwrap()
                .2,
            "between 1 and 3"
        );
        assert!(expectation(&b(json!({"atLeast": 4, "atMost": 3}))).is_err());
        assert!(expectation(&b(json!({"count": 1, "atLeast": 1}))).is_err());
    }

    #[test]
    fn sequences_allow_gaps_but_not_reordering() {
        let c: Vec<_> = ["/a", "/b", "/c"]
            .iter()
            .map(|p| compile_pattern(&pat(json!({"urlPath": p}))).unwrap())
            .collect();
        let e = |id, p| entry(id, "GET", p, "");
        let ok = vec![e(1, "/a"), e(2, "/x"), e(3, "/b"), e(4, "/a"), e(5, "/c")];
        assert_eq!(match_sequence(&c, &ok), vec![1, 3, 5]);
        let bad_order = vec![e(1, "/b"), e(2, "/a"), e(3, "/c")];
        assert_eq!(match_sequence(&c, &bad_order), vec![2]);
    }
}
