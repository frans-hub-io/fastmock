//! `GET /mail/stream`: server-sent events for live views.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Extension, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use fastmock_core::SpaceFilter;
use fastmock_core::mail::{
    MailEntry, MailEvent, MailEventKind, MailFilter, MailQuery, MailSubscription,
    MailSubscriptionError, MailTerm,
};
use fastmock_mail::MailService;
use futures::stream::{self, Stream};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::expect::{poll_tick, ticker};
use super::{ActiveGuard, service, shutting_down};
use crate::access::{Permission, PrincipalKind};
use crate::admin::{ApiError, ApiResult, ErrorBody};
use crate::auth::Principal;
use crate::spaces;
use crate::state::AppState;

/// Event streams one process serves at most.
pub const MAX_STREAMS: usize = 512;
/// Event streams one principal (token, user) may hold open at once.
pub const MAX_STREAMS_PER_PRINCIPAL: usize = 8;
/// Longest stream; clients reconnect.
pub const MAX_STREAM_SECONDS: u64 = 3600;
/// How often a stream checks that its caller still may read mail (token revoked or expired, binding
/// removed, session ended).
pub const RECHECK: Duration = Duration::from_secs(30);
/// Messages remembered as announced per stream.
const MAX_SENT: usize = 10_000;
pub const DEFAULT_HEARTBEAT_SECONDS: u64 = 15;
static STREAMS: AtomicUsize = AtomicUsize::new(0);
static PER_PRINCIPAL: Mutex<Option<HashMap<(usize, String), usize>>> = Mutex::new(None);

struct Slot {
    principal: Option<(usize, String)>,
}

impl Slot {
    /// A place among the [`MAX_STREAMS`] of the process and the [`MAX_STREAMS_PER_PRINCIPAL`] of the
    /// principal (`None` without authentication).
    fn take(principal: Option<(usize, String)>) -> Result<Self, &'static str> {
        STREAMS
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < MAX_STREAMS).then_some(n + 1)
            })
            .map_err(|_| "process")?;
        let mut slot = Slot { principal: None };
        if let Some(k) = principal {
            let mut m = PER_PRINCIPAL.lock().unwrap_or_else(|e| e.into_inner());
            let n = m
                .get_or_insert_with(HashMap::new)
                .entry(k.clone())
                .or_default();
            if *n >= MAX_STREAMS_PER_PRINCIPAL {
                return Err("principal");
            }
            *n += 1;
            slot.principal = Some(k);
        }
        Ok(slot)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        STREAMS.fetch_sub(1, Ordering::SeqCst);
        if let Some(k) = self.principal.take() {
            let mut m = PER_PRINCIPAL.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(map) = m.as_mut()
                && let Some(n) = map.get_mut(&k)
            {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    map.remove(&k);
                }
            }
        }
    }
}

#[derive(Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct StreamQuery {
    /// Space to follow. Without it, every area the caller may read.
    pub space: Option<String>,
    /// Seconds between heartbeat comments (`: heartbeat`), 1 to 60 (default 15); keeps proxies from
    /// closing an idle stream.
    pub heartbeat_seconds: Option<u64>,
    /// Seconds after which the server ends the stream, 1 to 3600 (default 3600); reconnect to go on.
    pub max_seconds: Option<u64>,
}

/// How far back a poll of the store looks for messages whose event was lost.
const POLL_WINDOW_SECONDS: i64 = 300;
/// Messages one poll reads at most.
const POLL_MAX: usize = 2000;

struct State_ {
    sub: MailSubscription,
    svc: Arc<MailService>,
    filter: SpaceFilter,
    ticker: Option<tokio::time::Interval>,
    /// Stream start: older messages are never announced.
    since: DateTime<Utc>,
    /// `message.received` already sent, by message, with the time it was received; at most
    /// [`MAX_SENT`], the oldest announcements forgotten first.
    sent: HashMap<Uuid, DateTime<Utc>>,
    sent_order: VecDeque<Uuid>,
    pending: VecDeque<Event>,
    ready: Arc<AtomicBool>,
    principal: Principal,
    /// What re-checks the caller: the app, the credentials of the request and its `space`.
    app: AppState,
    credentials: HeaderMap,
    space: Option<String>,
    recheck: tokio::time::Interval,
    deadline: tokio::time::Instant,
    _active: ActiveGuard,
    _slot: Slot,
    started: bool,
}

impl State_ {
    /// Whether a `message.received` for this message still has to be sent; records it as sent.
    fn first_time(&mut self, e: &MailEntry) -> bool {
        if self.sent.insert(e.id, e.received_at).is_some() {
            return false;
        }
        self.sent_order.push_back(e.id);
        while self.sent_order.len() > MAX_SENT {
            if let Some(old) = self.sent_order.pop_front() {
                self.sent.remove(&old);
            }
        }
        true
    }

    /// Authenticates the request again; `false` when the caller may no longer read mail here.
    async fn still_allowed(&mut self) -> bool {
        let Some(p) = crate::auth::revalidate(&self.app, &self.credentials).await else {
            return false;
        };
        if spaces::select(&self.app, &p, Permission::MailRead, self.space.as_deref()).is_err() {
            return false;
        }
        self.principal = p;
        true
    }

    /// Reads the recent messages from the store and queues `message.received` for those whose event
    /// did not arrive.
    async fn poll(&mut self) {
        let from = self
            .since
            .max(Utc::now() - chrono::Duration::seconds(POLL_WINDOW_SECONDS));
        self.sent.retain(|_, at| *at >= from);
        let sent = &self.sent;
        self.sent_order.retain(|id| sent.contains_key(id));
        let mut cursor = None;
        let mut read = 0;
        loop {
            let page = match self
                .svc
                .store
                .list(&MailQuery {
                    spaces: self.filter.clone(),
                    filter: MailFilter {
                        terms: vec![MailTerm::After(from)],
                    },
                    limit: 500,
                    cursor,
                    oldest_first: true,
                })
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "mail stream: reading recent messages failed");
                    return;
                }
            };
            read += page.items.len();
            for entry in page.items {
                if self
                    .principal
                    .allowed(Permission::MailRead, entry.space.as_deref())
                    && self.first_time(&entry)
                {
                    let e = MailEvent {
                        kind: MailEventKind::Received,
                        space: entry.space.clone(),
                        ids: vec![entry.id],
                        mailboxes: vec![],
                        entry: Some(entry),
                    };
                    self.pending.push_back(sse_event(&e));
                }
            }
            match page.next {
                Some(c) if read < POLL_MAX => cursor = Some(c),
                _ => return,
            }
        }
    }
}

/// The SSE form of an event the principal may read.
fn sse_event(e: &MailEvent) -> Event {
    Event::default()
        .event(e.kind.as_str())
        .json_data(e)
        .unwrap_or_else(|_| Event::default().comment("unserializable event"))
}

fn events(state: State_) -> impl Stream<Item = Result<Event, Infallible>> {
    stream::unfold(Some(state), |state| async move {
        let mut st = state?;
        if !st.started {
            st.started = true;
            return Some((Ok(Event::default().comment("ready")), Some(st)));
        }
        loop {
            if let Some(ev) = st.pending.pop_front() {
                return Some((Ok(ev), Some(st)));
            }
            let next = tokio::select! {
                r = st.sub.recv() => r,
                _ = poll_tick(&mut st.ticker) => {
                    st.poll().await;
                    continue;
                }
                _ = st.recheck.tick() => {
                    if !st.still_allowed().await {
                        tracing::debug!(principal = %st.principal.name, "mail stream closed: the caller may no longer read mail");
                        return None;
                    }
                    continue;
                }
                _ = tokio::time::sleep_until(st.deadline) => return None,
                _ = shutting_down(st.ready.clone()) => return None,
            };
            match next {
                Ok(e) => {
                    if !st
                        .principal
                        .allowed(Permission::MailRead, e.space.as_deref())
                    {
                        continue;
                    }
                    if e.kind == MailEventKind::Received
                        && let Some(entry) = &e.entry
                        && !st.first_time(entry)
                    {
                        continue;
                    }
                    return Some((Ok(sse_event(&e)), Some(st)));
                }
                Err(MailSubscriptionError::Lagged(n)) => {
                    let ev = Event::default()
                        .event("resync")
                        .json_data(json!({"missed": n}))
                        .unwrap_or_else(|_| Event::default().event("resync"));
                    return Some((Ok(ev), Some(st)));
                }
                Err(MailSubscriptionError::Closed) => return None,
            }
        }
    })
}

#[utoipa::path(
    get, path = "/__admin/mail/stream", tag = "mail", operation_id = "streamMailEvents",
    summary = "Live mail events",
    description = "Server-sent events of `space` (or of every area the caller may read): `message.received` (data: `MailEvent` with the message's `entry`) and `message.deleted` (data: `MailEvent` with the deleted `ids`). Starts with the comment `: ready`, sends `: heartbeat` comments while idle, ends when the server shuts down, after `maxSeconds` (at most one hour; reconnect to go on) or when a check every 30 seconds finds that the caller may no longer read mail there (token revoked or expired, binding removed, session ended), and sends `resync` (data `{\"missed\": n}`) when this stream fell behind: list the messages again. A caller may hold 8 streams at once. Each event is checked against the caller's `mail:read` permission for its area. With PostgreSQL storage the stream also re-reads the last 5 minutes of messages every few seconds and sends `message.received` for those whose event was lost (`mailboxes` is empty then); each message is announced once.",
    params(StreamQuery),
    responses(
        (status = 200, description = "Event stream", content_type = "text/event-stream", body = fastmock_core::mail::MailEvent),
        (status = 400, description = "Invalid heartbeatSeconds or maxSeconds", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, or an unknown space", body = ErrorBody),
        (status = 429, description = "The caller already holds 8 event streams", body = ErrorBody),
        (status = 503, description = "Too many event streams are open", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn stream(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    headers: HeaderMap,
    Query(q): Query<StreamQuery>,
) -> ApiResult<Response> {
    let svc = service(&app)?;
    let filter = spaces::select(&app, &p, Permission::MailRead, q.space.as_deref())?;
    let heartbeat = q.heartbeat_seconds.unwrap_or(DEFAULT_HEARTBEAT_SECONDS);
    if !(1..=60).contains(&heartbeat) {
        return Err(crate::admin::bad(
            "heartbeatSeconds must be between 1 and 60",
        ));
    }
    let max_seconds = q.max_seconds.unwrap_or(MAX_STREAM_SECONDS);
    if !(1..=MAX_STREAM_SECONDS).contains(&max_seconds) {
        return Err(crate::admin::bad(format!(
            "maxSeconds must be between 1 and {MAX_STREAM_SECONDS}"
        )));
    }
    let who = (p.kind != PrincipalKind::Anonymous)
        .then(|| (super::service_key(&svc), format!("{:?}:{}", p.kind, p.name)));
    let slot = Slot::take(who).map_err(|which| match which {
        "principal" => ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            format!("{MAX_STREAMS_PER_PRINCIPAL} mail event streams of this caller are open; close one first"),
        ),
        _ => ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("{MAX_STREAMS} mail event streams are open; retry later"),
        ),
    })?;
    let mut credentials = HeaderMap::new();
    for name in [
        axum::http::header::AUTHORIZATION,
        axum::http::header::COOKIE,
    ] {
        for v in headers.get_all(&name) {
            credentials.append(name.clone(), v.clone());
        }
    }
    let mut recheck = tokio::time::interval_at(tokio::time::Instant::now() + RECHECK, RECHECK);
    recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let state = State_ {
        sub: svc.events.subscribe(filter.clone()),
        ticker: svc.events.poll_interval().map(ticker),
        svc: svc.clone(),
        filter,
        since: fastmock_core::mail::micros(Utc::now()),
        sent: HashMap::new(),
        sent_order: VecDeque::new(),
        pending: VecDeque::new(),
        ready: app.ready.clone(),
        principal: p,
        app: app.clone(),
        credentials,
        space: q.space.clone(),
        recheck,
        deadline: tokio::time::Instant::now() + Duration::from_secs(max_seconds),
        _active: ActiveGuard::new(&svc, "streams"),
        _slot: slot,
        started: false,
    };
    let mut r = Sse::new(events(state))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(heartbeat))
                .text("heartbeat"),
        )
        .into_response();
    r.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    r.headers_mut().insert(
        "x-accel-buffering",
        axum::http::HeaderValue::from_static("no"),
    );
    Ok(r)
}
