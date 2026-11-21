//! `GET /mail/stream`: server-sent events for live views.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
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
use crate::access::Permission;
use crate::admin::{ApiError, ApiResult, ErrorBody};
use crate::auth::Principal;
use crate::spaces;
use crate::state::AppState;

/// Event streams one process serves at most.
pub const MAX_STREAMS: usize = 512;
pub const DEFAULT_HEARTBEAT_SECONDS: u64 = 15;
static STREAMS: AtomicUsize = AtomicUsize::new(0);

struct Slot;

impl Slot {
    fn take() -> Option<Self> {
        STREAMS
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < MAX_STREAMS).then_some(n + 1)
            })
            .ok()
            .map(|_| Slot)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        STREAMS.fetch_sub(1, Ordering::SeqCst);
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
    /// `message.received` already sent, by message, with the time it was received.
    sent: HashMap<Uuid, DateTime<Utc>>,
    pending: VecDeque<Event>,
    ready: Arc<AtomicBool>,
    principal: Principal,
    _active: ActiveGuard,
    _slot: Slot,
    started: bool,
}

impl State_ {
    /// Whether a `message.received` for this message still has to be sent; records it as sent.
    fn first_time(&mut self, e: &MailEntry) -> bool {
        self.sent.insert(e.id, e.received_at).is_none()
    }

    /// Reads the recent messages from the store and queues `message.received` for those whose event
    /// did not arrive.
    async fn poll(&mut self) {
        let from = self
            .since
            .max(Utc::now() - chrono::Duration::seconds(POLL_WINDOW_SECONDS));
        self.sent.retain(|_, at| *at >= from);
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
    description = "Server-sent events of `space` (or of every area the caller may read): `message.received` (data: `MailEvent` with the message's `entry`) and `message.deleted` (data: `MailEvent` with the deleted `ids`). Starts with the comment `: ready`, sends `: heartbeat` comments while idle, ends when the server shuts down, and `resync` (data `{\"missed\": n}`) when this stream fell behind: list the messages again. Each event is checked against the caller's `mail:read` permission for its area. With PostgreSQL storage the stream also re-reads the last 5 minutes of messages every few seconds and sends `message.received` for those whose event was lost (`mailboxes` is empty then); each message is announced once.",
    params(StreamQuery),
    responses(
        (status = 200, description = "Event stream", content_type = "text/event-stream", body = fastmock_core::mail::MailEvent),
        (status = 400, description = "Invalid heartbeatSeconds", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, or an unknown space", body = ErrorBody),
        (status = 503, description = "Too many event streams are open", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn stream(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
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
    let slot = Slot::take().ok_or_else(|| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("{MAX_STREAMS} mail event streams are open; retry later"),
        )
    })?;
    let state = State_ {
        sub: svc.events.subscribe(filter.clone()),
        ticker: svc.events.poll_interval().map(ticker),
        svc: svc.clone(),
        filter,
        since: fastmock_core::mail::micros(Utc::now()),
        sent: HashMap::new(),
        pending: VecDeque::new(),
        ready: app.ready.clone(),
        principal: p,
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
