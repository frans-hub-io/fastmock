//! Admin API for captured mail (`/__admin/mail/...`). Handlers reach the mail module through
//! [`crate::state::AppState::mail`] (absent while `FASTMOCK_MAIL_ENABLED` is off: every route answers
//! 404) and select areas with [`crate::spaces::select`] and [`crate::spaces::target`] using the `mail:*`
//! permissions, whose route rules are in [`crate::access::route_rule`].
//!
//! - [`messages`]: list and search, one message and its renditions (raw, HTML, text, parts, transcript,
//!   links, codes), labels, deletes.
//! - [`expect`]: `POST /mail/messages/wait` (long poll) and `POST /mail/verify`.
//! - [`inject`]: `POST /mail/messages` (JSON or `message/rfc822`).
//! - [`stream`]: `GET /mail/stream` (server-sent events).
//! - [`mailboxes`]: mailbox list and IMAP/POP3 passwords; [`password`] holds the hash format.

pub mod expect;
pub mod inject;
pub mod mailboxes;
pub mod messages;
pub mod password;
pub mod stream;

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use axum::Router;
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use fastmock_core::SpaceFilter;
use fastmock_mail::{MailMessage, MailService};
use uuid::Uuid;

use crate::access::Permission;
use crate::admin::{ApiError, ApiResult, not_found};
use crate::auth::Principal;
use crate::spaces;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/mail/messages",
            get(messages::list)
                .post(inject::inject)
                .delete(messages::delete_matching),
        )
        .route("/mail/messages/wait", post(expect::wait))
        .route("/mail/verify", post(expect::verify))
        .route(
            "/mail/messages/{id}",
            get(messages::get_message).delete(messages::delete_message),
        )
        .route("/mail/messages/{id}/raw", get(messages::raw))
        .route("/mail/messages/{id}/html", get(messages::html))
        .route("/mail/messages/{id}/text", get(messages::text))
        .route("/mail/messages/{id}/parts/{partId}", get(messages::part))
        .route("/mail/messages/{id}/transcript", get(messages::transcript))
        .route("/mail/messages/{id}/links", get(messages::links))
        .route("/mail/messages/{id}/codes", get(messages::codes))
        .route("/mail/messages/{id}/labels", put(messages::set_labels))
        .route("/mail/stream", get(stream::stream))
        .route("/mail/mailboxes", get(mailboxes::list))
        .route(
            "/mail/mailboxes/{address}/password",
            put(mailboxes::set_password).delete(mailboxes::delete_password),
        )
}

/// The mail module, or 404 while it is off.
pub fn service(app: &AppState) -> ApiResult<Arc<MailService>> {
    app.mail.clone().ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            "mail is not enabled; set FASTMOCK_MAIL_ENABLED=true".into(),
        )
    })
}

/// One stored message the caller may act on with `perm`: in the requested space when `space` is given.
/// Messages the caller cannot read answer 404 like missing ones; readable ones without `perm` answer
/// 403.
pub async fn find(
    app: &AppState,
    svc: &MailService,
    p: &Principal,
    id: Uuid,
    space: Option<&str>,
    perm: Permission,
) -> ApiResult<MailMessage> {
    let filter = match space {
        Some(s) => {
            spaces::known(app, p, s)?;
            SpaceFilter::In(vec![s.to_string()])
        }
        None => SpaceFilter::Any,
    };
    let msg = svc
        .store
        .get(id, &filter)
        .await?
        .ok_or_else(|| not_found("message"))?;
    spaces::item(
        p,
        Permission::MailRead,
        perm,
        msg.space.as_deref(),
        "message",
    )?;
    Ok(msg)
}

/// Runs CPU-bound work (MIME parsing, matching) on the blocking pool.
pub async fn blocking<T, F>(f: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| anyhow::anyhow!("mail task failed: {e}").into())
}

/// Resolves once the server withdrew readiness to shut down, so long polls and event streams end
/// before the connection drain.
pub async fn shutting_down(ready: Arc<std::sync::atomic::AtomicBool>) {
    while ready.load(std::sync::atomic::Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// Long polls and event streams in progress, per mail service (tests and the
/// `fastmock_mail_waits_active` / `fastmock_mail_streams_active` gauges read them).
static ACTIVE: LazyLock<Mutex<HashMap<(usize, &'static str), usize>>> =
    LazyLock::new(Default::default);

pub(crate) fn service_key(svc: &MailService) -> usize {
    svc as *const MailService as usize
}

/// Counts one long poll or stream while it lives.
pub struct ActiveGuard {
    key: (usize, &'static str),
}

impl ActiveGuard {
    pub fn new(svc: &MailService, kind: &'static str) -> Self {
        let key = (service_key(svc), kind);
        let mut m = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        *m.entry(key).or_default() += 1;
        metrics::gauge!(format!("fastmock_mail_{kind}_active")).increment(1.0);
        Self { key }
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        let mut m = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = m.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&self.key);
            }
        }
        metrics::gauge!(format!("fastmock_mail_{}_active", self.key.1)).decrement(1.0);
    }
}

/// `POST /mail/messages/wait` requests of this service in progress.
pub fn active_waits(svc: &MailService) -> usize {
    active(svc, "waits")
}

/// `GET /mail/stream` connections of this service in progress.
pub fn active_streams(svc: &MailService) -> usize {
    active(svc, "streams")
}

fn active(svc: &MailService, kind: &'static str) -> usize {
    ACTIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(service_key(svc), kind))
        .copied()
        .unwrap_or(0)
}

#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        messages::list,
        messages::get_message,
        messages::raw,
        messages::html,
        messages::text,
        messages::part,
        messages::transcript,
        messages::links,
        messages::codes,
        messages::delete_message,
        messages::delete_matching,
        messages::set_labels,
        expect::wait,
        expect::verify,
        inject::inject,
        stream::stream,
        mailboxes::list,
        mailboxes::set_password,
        mailboxes::delete_password,
    ),
    tags((name = "mail", description = "Captured mail: search and read messages, wait for and verify them, inject and delete them, live events, IMAP/POP3 mailbox passwords"))
)]
pub struct MailApi;
