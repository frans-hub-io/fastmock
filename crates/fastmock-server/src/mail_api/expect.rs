//! `POST /mail/messages/wait` and `POST /mail/verify`: which messages match a [`MailMatch`].

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use fastmock_core::SpaceFilter;
use fastmock_core::mail::{
    MailAddress, MailCursor, MailEntry, MailEventKind, MailFilter, MailQuery,
    MailSubscriptionError, MailTerm,
};
use fastmock_mail::{
    CompiledMailMatch, MailMatch, MailService, MatchView, ParseLimits, ParsedMail,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use uuid::Uuid;

use super::{ActiveGuard, blocking, service, shutting_down};
use crate::access::Permission;
use crate::admin::{ApiError, ApiResult, ErrorBody, bad};
use crate::auth::Principal;
use crate::spaces::{self, SpaceQuery};
use crate::state::AppState;

/// Longest `timeoutMs` of a wait.
pub const MAX_WAIT_MS: u64 = 60_000;
pub const DEFAULT_WAIT_MS: u64 = 10_000;
/// Largest `count` of a wait.
pub const MAX_WAIT_COUNT: usize = 500;
/// Stored messages one wait or verify looks at, at most.
pub const MAX_SCAN: usize = 10_000;
const PAGE: usize = 200;
const MAX_NEAR_MISSES: usize = 5;
/// Waits and verifications that read stored messages at the same time, per process.
static SCANS: Semaphore = Semaphore::const_new(4);
const SCAN_QUEUE_WAIT: Duration = Duration::from_secs(5);

/// Whether a match looks at the bodies or header fields, which only the raw message has.
fn needs_body(m: &MailMatch) -> bool {
    m.text.is_some() || m.html.is_some() || !m.headers.is_empty()
}

/// Matchers of a match, for near-miss scores.
fn matcher_count(m: &MailMatch) -> usize {
    [
        m.from.is_some(),
        m.to.is_some(),
        m.cc.is_some(),
        m.subject.is_some(),
        m.text.is_some(),
        m.html.is_some(),
        m.has_attachment.is_some(),
        m.attachment_name.is_some(),
        m.label.is_some(),
        m.source.is_some(),
        m.provider.is_some(),
    ]
    .into_iter()
    .filter(|x| *x)
    .count()
        + m.headers.len()
}

/// What a match sees of a message without its raw form.
fn view_of_entry(e: &MailEntry) -> MatchView {
    let mut to = e.summary.to.clone();
    for r in &e.envelope.rcpt_to {
        if !to.iter().any(|a| a.address.eq_ignore_ascii_case(r)) {
            to.push(MailAddress {
                name: None,
                address: r.clone(),
            });
        }
    }
    MatchView {
        from: e.summary.from.clone(),
        to,
        cc: e.summary.cc.clone(),
        subject: e.summary.subject.clone(),
        text: None,
        html: None,
        headers: vec![],
        has_attachments: e.summary.has_attachments,
        attachment_names: e.summary.attachment_names.clone(),
        labels: e.labels.clone(),
        source: e.source.as_str().to_string(),
        provider: e.provider.clone(),
    }
}

/// A compiled match and how to evaluate it.
struct Matcher {
    compiled: Arc<CompiledMailMatch>,
    body: bool,
    limits: ParseLimits,
}

impl Matcher {
    fn new(svc: &MailService, m: &MailMatch) -> ApiResult<Self> {
        let compiled =
            CompiledMailMatch::compile(m).map_err(|e| bad(format!("invalid match: {e}")))?;
        Ok(Self {
            compiled: Arc::new(compiled),
            body: needs_body(m),
            limits: svc.config.parse_limits(),
        })
    }

    /// The failing matchers of a message (empty when it matches), with its current entry; `None` when
    /// the message is gone.
    async fn failures(
        &self,
        svc: &MailService,
        spaces: &SpaceFilter,
        e: &MailEntry,
    ) -> ApiResult<Option<(MailEntry, Vec<String>)>> {
        if !self.body {
            return Ok(Some((e.clone(), self.compiled.failures(&view_of_entry(e)))));
        }
        let Some(msg) = svc.store.get(e.id, spaces).await? else {
            return Ok(None);
        };
        let compiled = self.compiled.clone();
        let limits = self.limits;
        blocking(move || {
            let parsed = ParsedMail::parse(&msg.raw, &limits);
            let f = compiled.failures(&MatchView::new(&msg, &parsed));
            Some((msg.entry(), f))
        })
        .await
    }
}

async fn scan_permit() -> ApiResult<tokio::sync::SemaphorePermit<'static>> {
    match tokio::time::timeout(SCAN_QUEUE_WAIT, SCANS.acquire()).await {
        Ok(Ok(p)) => Ok(p),
        _ => Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many mail waits and verifications are reading stored messages; retry in a moment"
                .into(),
        )),
    }
}

/// Stored messages of `spaces` received at or after `since`, oldest first; at most [`MAX_SCAN`].
async fn stored(
    svc: &MailService,
    spaces: &SpaceFilter,
    since: Option<DateTime<Utc>>,
) -> ApiResult<Vec<MailEntry>> {
    let filter = MailFilter {
        terms: since.map(MailTerm::After).into_iter().collect(),
    };
    let mut out = Vec::new();
    let mut cursor: Option<MailCursor> = None;
    loop {
        let page = svc
            .store
            .list(&MailQuery {
                spaces: spaces.clone(),
                filter: filter.clone(),
                limit: PAGE,
                cursor,
                oldest_first: true,
            })
            .await?;
        out.extend(page.items);
        if out.len() > MAX_SCAN {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!(
                    "more than {MAX_SCAN} stored messages to check; narrow it with space or since"
                ),
            ));
        }
        match page.next {
            Some(c) => cursor = Some(c),
            None => return Ok(out),
        }
    }
}

/// A timer for re-reading the store when the event bus may lose events
/// ([`fastmock_core::mail::MailEvents::poll_interval`]).
pub fn ticker(every: Duration) -> tokio::time::Interval {
    let mut t = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    t
}

/// The next tick, or never without a ticker.
pub async fn poll_tick(t: &mut Option<tokio::time::Interval>) {
    match t {
        Some(t) => {
            t.tick().await;
        }
        None => std::future::pending().await,
    }
}

fn default_count() -> usize {
    1
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WaitBody {
    /// Conditions on the message; all given ones must hold. Empty: any message.
    #[serde(default, rename = "match")]
    pub matcher: MailMatch,
    /// Matching messages to wait for, 1 to 500 (default 1).
    #[serde(default = "default_count")]
    pub count: usize,
    /// How long to wait, 0 to 60000 ms (default 10000).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Count matching messages already stored, whenever they arrived.
    #[serde(default)]
    pub include_existing: bool,
    /// Count stored messages received at or after this time too (ignored with `includeExisting`).
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    /// Without `since`: count stored messages received up to this many milliseconds before the request
    /// (default 0: only messages arriving from now on).
    #[serde(default)]
    pub lookback_ms: Option<u64>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct WaitResult {
    /// The first `count` matching messages, oldest first.
    pub messages: Vec<MailEntry>,
    pub count: usize,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct WaitTimeout {
    #[schema(example = "timed out after 10000 ms: 0 of 1 matching messages received")]
    pub error: String,
    pub details: Vec<String>,
    /// Matching messages seen before the time ran out.
    pub count: usize,
    pub expected: usize,
    pub messages: Vec<MailEntry>,
}

#[utoipa::path(
    post, path = "/__admin/mail/messages/wait", tag = "mail", operation_id = "waitForMail",
    summary = "Wait for messages",
    description = "Long-polls until `count` messages matching `match` have arrived in `space` (or the areas the caller may read) and answers 200 with them, or 408 with what was seen when `timeoutMs` (at most 60 s) runs out. Messages stored before the request count only with `includeExisting`, `since` or `lookbackMs`. Works on any replica (events travel through Valkey with PostgreSQL storage; the store is also re-read every few seconds there, so a lost event delays a match but does not miss it). The wait stops when the client disconnects, and answers 503 when the server shuts down. Matchers use the stub string matcher syntax (`equalTo`, `contains`, `matches`, `caseInsensitive`, `and`/`or`/`not`, `absent`, ...).",
    params(SpaceQuery),
    request_body(content = WaitBody, example = json!({"match": {"to": {"contains": "alice@"}, "subject": {"contains": "Verify"}}, "count": 1, "timeoutMs": 10000})),
    responses(
        (status = 200, description = "The matching messages arrived", body = WaitResult),
        (status = 400, description = "Invalid match, count or timeout", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, or an unknown space", body = ErrorBody),
        (status = 408, description = "Timed out; `count` matching messages were seen", body = WaitTimeout),
        (status = 422, description = "More than 10000 stored messages to check", body = ErrorBody),
        (status = 503, description = "Too many waits are reading stored messages, or the server is shutting down", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn wait(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
    Json(b): Json<WaitBody>,
) -> ApiResult<Response> {
    let svc = service(&app)?;
    let spaces = spaces::select(&app, &p, Permission::MailRead, q.space.as_deref())?;
    if !(1..=MAX_WAIT_COUNT).contains(&b.count) {
        return Err(bad(format!("count must be between 1 and {MAX_WAIT_COUNT}")));
    }
    let timeout_ms = b.timeout_ms.unwrap_or(DEFAULT_WAIT_MS);
    if timeout_ms > MAX_WAIT_MS {
        return Err(bad(format!(
            "timeoutMs must be between 0 and {MAX_WAIT_MS}"
        )));
    }
    let lookback = b.lookback_ms.unwrap_or(0);
    if lookback > 7 * 86_400_000 {
        return Err(bad("lookbackMs must be at most 7 days"));
    }
    let matcher = Matcher::new(&svc, &b.matcher)?;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    let started = Utc::now();
    let since = if b.include_existing {
        None
    } else {
        Some(
            b.since
                .unwrap_or(started - chrono::Duration::milliseconds(lookback as i64)),
        )
    };
    let _active = ActiveGuard::new(&svc, "waits");
    let mut sub = svc.events.subscribe(spaces.clone());
    let poll = svc.events.poll_interval();
    let mut ticker = poll.map(ticker);
    let mut found: Vec<MailEntry> = Vec::new();
    let mut checked: HashSet<Uuid> = HashSet::new();
    let mut rescan = true;
    loop {
        if rescan {
            rescan = false;
            let entries = {
                let _permit = scan_permit().await?;
                stored(&svc, &spaces, since).await?
            };
            for e in entries {
                if !checked.insert(e.id) {
                    continue;
                }
                if let Some((e, f)) = matcher.failures(&svc, &spaces, &e).await?
                    && f.is_empty()
                {
                    found.push(e);
                }
            }
        }
        if found.len() >= b.count {
            found.sort_by_key(|e| (e.received_at, e.id));
            found.truncate(b.count);
            metrics::counter!("fastmock_mail_waits_total", "result" => "found").increment(1);
            return Ok(Json(WaitResult {
                count: found.len(),
                messages: found,
            })
            .into_response());
        }
        let next = tokio::select! {
            r = tokio::time::timeout_at(deadline, sub.recv()) => r,
            _ = poll_tick(&mut ticker) => {
                rescan = true;
                continue;
            }
            _ = shutting_down(app.ready.clone()) => {
                return Err(ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "the server is shutting down; retry on another replica".into(),
                ));
            }
        };
        let event = match next {
            Err(_) => {
                metrics::counter!("fastmock_mail_waits_total", "result" => "timeout").increment(1);
                found.sort_by_key(|e| (e.received_at, e.id));
                return Ok((
                    StatusCode::REQUEST_TIMEOUT,
                    Json(WaitTimeout {
                        error: format!(
                            "timed out after {timeout_ms} ms: {} of {} matching messages received",
                            found.len(),
                            b.count
                        ),
                        details: vec![],
                        count: found.len(),
                        expected: b.count,
                        messages: found,
                    }),
                )
                    .into_response());
            }
            Ok(Err(MailSubscriptionError::Lagged(_))) => {
                rescan = true;
                continue;
            }
            Ok(Err(MailSubscriptionError::Closed)) => {
                return Err(ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "mail events are not available; retry".into(),
                ));
            }
            Ok(Ok(e)) => e,
        };
        if poll.is_some() {
            rescan = true;
        }
        if event.kind != MailEventKind::Received {
            continue;
        }
        for id in &event.ids {
            if !checked.insert(*id) {
                continue;
            }
            let entry = match event.entry.as_ref().filter(|e| e.id == *id) {
                Some(e) => e.clone(),
                None => match svc.store.get(*id, &spaces).await? {
                    Some(m) => m.entry(),
                    None => continue,
                },
            };
            if since.is_some_and(|s| entry.received_at < s) {
                continue;
            }
            if let Some((e, f)) = matcher.failures(&svc, &spaces, &entry).await?
                && f.is_empty()
            {
                found.push(e);
            }
        }
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MailVerifyBody {
    /// Conditions on the message; all given ones must hold. Empty: any message.
    #[serde(default, rename = "match")]
    pub matcher: MailMatch,
    /// Exactly this many matching messages.
    #[serde(default)]
    pub count: Option<u64>,
    #[serde(default)]
    pub at_least: Option<u64>,
    #[serde(default)]
    pub at_most: Option<u64>,
    /// Only messages received at or after this time.
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
}

/// `(lowest, highest, text)` of the expectation; without any of the three, at least 1.
pub fn expectation(b: &MailVerifyBody) -> Result<(u64, u64, String), String> {
    match (b.count, b.at_least, b.at_most) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            Err("use either count or atLeast/atMost".into())
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

/// A stored message that fails some matchers.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MailNearMiss {
    pub id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    pub received_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub from: Vec<MailAddress>,
    pub to: Vec<String>,
    /// Share of the matchers that matched, 0..1.
    pub score: f64,
    /// The matchers that failed: `from`, `subject`, `headers.x-mailer`, `hasAttachment`, ...
    #[schema(example = json!(["subject"]))]
    pub mismatches: Vec<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MailVerifyResult {
    pub verified: bool,
    /// Matching messages found.
    pub count: u64,
    /// The expectation, e.g. `exactly 2`, `at least 1`, `between 1 and 3`.
    pub expected: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub details: Vec<String>,
    /// The closest non-matching messages, best first (only on failure, at most 5).
    pub near_misses: Vec<MailNearMiss>,
}

#[utoipa::path(
    post, path = "/__admin/mail/verify", tag = "mail", operation_id = "verifyMail",
    summary = "Verify how many messages were received",
    description = "Counts the stored messages of `space` (or of the areas the caller may read) matching `match`. Without `count`/`atLeast`/`atMost` the expectation is `atLeast: 1`. Answers 200 when it holds and 422 with the count and the closest non-matching messages (which matchers failed) when it does not.",
    params(SpaceQuery),
    request_body(content = MailVerifyBody, example = json!({"match": {"to": {"equalTo": "alice@example.test"}, "subject": {"contains": "Welcome"}}, "count": 1})),
    responses(
        (status = 200, description = "Expectation met", body = MailVerifyResult),
        (status = 400, description = "Invalid match or expectation", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, or an unknown space", body = ErrorBody),
        (status = 422, description = "Expectation not met (`MailVerifyResult`), or more than 10000 stored messages to check (`ErrorBody`)", body = MailVerifyResult),
        (status = 503, description = "Too many verifications are reading stored messages", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn verify(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
    Json(b): Json<MailVerifyBody>,
) -> ApiResult<Response> {
    let (lo, hi, expected) = expectation(&b).map_err(bad)?;
    let svc = service(&app)?;
    let spaces = spaces::select(&app, &p, Permission::MailRead, q.space.as_deref())?;
    let matcher = Matcher::new(&svc, &b.matcher)?;
    let total = matcher_count(&b.matcher).max(1) as f64;
    let mut count = 0u64;
    let mut misses: Vec<MailNearMiss> = Vec::new();
    {
        let _permit = scan_permit().await?;
        for e in stored(&svc, &spaces, b.since).await? {
            let Some((e, failures)) = matcher.failures(&svc, &spaces, &e).await? else {
                continue;
            };
            if failures.is_empty() {
                count += 1;
                continue;
            }
            misses.push(MailNearMiss {
                id: e.id,
                space: e.space,
                received_at: e.received_at,
                subject: e.summary.subject,
                from: e.summary.from,
                to: e.envelope.rcpt_to,
                score: (total - failures.len() as f64).max(0.0) / total,
                mismatches: failures,
            });
        }
    }
    let verified = (lo..=hi).contains(&count);
    metrics::counter!("fastmock_admin_verifications_total", "kind" => "mail", "result" => if verified { "pass" } else { "fail" }).increment(1);
    if verified {
        return Ok(Json(MailVerifyResult {
            verified,
            count,
            expected,
            error: None,
            details: vec![],
            near_misses: vec![],
        })
        .into_response());
    }
    misses.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(b.received_at.cmp(&a.received_at))
            .then(b.id.cmp(&a.id))
    });
    misses.truncate(MAX_NEAR_MISSES);
    let details = misses
        .iter()
        .map(|m| {
            format!(
                "message {} ({}): {}",
                m.id,
                m.subject.as_deref().unwrap_or("no subject"),
                m.mismatches.join(", ")
            )
        })
        .collect();
    Ok((
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(MailVerifyResult {
            verified,
            count,
            error: Some(format!(
                "expected {expected} matching messages, received {count}"
            )),
            expected,
            details,
            near_misses: misses,
        }),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(v: serde_json::Value) -> MailVerifyBody {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn expectations() {
        assert_eq!(
            expectation(&body(json!({}))).unwrap(),
            (1, u64::MAX, "at least 1".into())
        );
        assert_eq!(
            expectation(&body(json!({"count": 0}))).unwrap(),
            (0, 0, "exactly 0".into())
        );
        assert_eq!(
            expectation(&body(json!({"atMost": 3}))).unwrap(),
            (0, 3, "at most 3".into())
        );
        assert_eq!(
            expectation(&body(json!({"atLeast": 1, "atMost": 3})))
                .unwrap()
                .2,
            "between 1 and 3"
        );
        assert!(expectation(&body(json!({"atLeast": 4, "atMost": 3}))).is_err());
        assert!(expectation(&body(json!({"count": 1, "atLeast": 1}))).is_err());
        assert!(serde_json::from_value::<MailVerifyBody>(json!({"exactly": 1})).is_err());
    }

    #[test]
    fn matcher_counts_and_body_needs() {
        let m: MailMatch = serde_json::from_value(json!({
            "from": {"contains": "a"}, "subject": {"equalTo": "b"},
            "headers": {"X-A": {"equalTo": "1"}, "X-B": {"absent": true}}, "hasAttachment": true
        }))
        .unwrap();
        assert_eq!(matcher_count(&m), 5);
        assert!(needs_body(&m));
        let m: MailMatch = serde_json::from_value(json!({"label": {"equalTo": "otp"}})).unwrap();
        assert!(!needs_body(&m));
        assert_eq!(matcher_count(&MailMatch::default()), 0);
    }
}
