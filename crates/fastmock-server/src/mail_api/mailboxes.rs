//! IMAP/POP3 mailboxes: the list of addresses and their passwords.

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use fastmock_mail::address::{mailbox_of, split};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::password::{self, MAX_PASSWORD_BYTES};
use super::{blocking, service};
use crate::access::Permission;
use crate::admin::{ApiError, ApiResult, ErrorBody, audit_event_in, bad};
use crate::auth::Principal;
use crate::spaces::{self, SpaceQuery};
use crate::state::AppState;

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MailboxView {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "billing")]
    pub space: Option<String>,
    /// Lowercase mailbox address (plus tags removed); the IMAP/POP3 user name.
    #[schema(example = "alice@billing.example.test")]
    pub address: String,
    #[schema(example = json!(["INBOX"]))]
    pub folders: Vec<String>,
    /// A password was set with `PUT /mail/mailboxes/{address}/password` (it is never returned).
    pub has_password: bool,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct MailboxList {
    pub mailboxes: Vec<MailboxView>,
}

#[utoipa::path(
    get, path = "/__admin/mail/mailboxes", tag = "mail", operation_id = "listMailboxes",
    summary = "IMAP/POP3 mailboxes",
    description = "Addresses that have a mailbox (created by the first message filed to them, or by setting a password) in `space`, or in every area the caller may read, sorted by area and address.",
    params(SpaceQuery),
    responses(
        (status = 200, description = "Mailboxes", body = MailboxList),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, or an unknown space", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn list(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<Json<MailboxList>> {
    let svc = service(&app)?;
    let spaces = spaces::select(&app, &p, Permission::MailRead, q.space.as_deref())?;
    let mailboxes = svc
        .store
        .mailboxes(&spaces)
        .await?
        .into_iter()
        .map(|m| MailboxView {
            space: m.space,
            address: m.address,
            folders: m.folders,
            has_password: m.has_password,
        })
        .collect();
    Ok(Json(MailboxList { mailboxes }))
}

/// The mailbox address of a path segment: an address with a local part and a domain, at most 320
/// bytes, printable, without spaces; lowercase and without its plus tag.
fn mailbox_address(raw: &str) -> ApiResult<String> {
    let a = raw.trim();
    if a.is_empty()
        || a.len() > 320
        || a.chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '<' | '>' | ',' | '/'))
        || split(a).is_none()
    {
        return Err(bad(format!(
            "invalid mailbox address `{}`",
            a.chars().take(100).collect::<String>()
        )));
    }
    Ok(mailbox_of(a))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PasswordBody {
    /// 1 to 1024 bytes; stored as a salted PBKDF2-HMAC-SHA256 hash, never returned or logged.
    #[schema(example = "change-me")]
    pub password: String,
}

#[utoipa::path(
    put, path = "/__admin/mail/mailboxes/{address}/password", tag = "mail", operation_id = "setMailboxPassword",
    summary = "Set a mailbox password",
    description = "Sets the IMAP/POP3 password of a mailbox in `space` (the global area, or the caller's only space when it is bound to spaces), creating the mailbox. The address is stored lowercase without its plus tag. Only a salted hash is stored. Records the audit event `mail.mailbox.password.set` (without the password).",
    params(("address" = String, Path, description = "Mailbox address"), SpaceQuery),
    request_body(content = PasswordBody, example = json!({"password": "change-me"})),
    responses(
        (status = 204, description = "Set"),
        (status = 400, description = "Invalid address or password", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, or an unknown space", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn set_password(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(address): Path<String>,
    Query(q): Query<SpaceQuery>,
    Json(b): Json<PasswordBody>,
) -> ApiResult<StatusCode> {
    let svc = service(&app)?;
    let area = spaces::target(&app, &p, Permission::MailWrite, q.space.as_deref())?;
    let address = mailbox_address(&address)?;
    if b.password.is_empty() || b.password.len() > MAX_PASSWORD_BYTES {
        return Err(bad(format!(
            "password must be 1 to {MAX_PASSWORD_BYTES} bytes"
        )));
    }
    if b.password.chars().any(char::is_control) {
        return Err(bad("password must not contain control characters"));
    }
    let pw = b.password;
    let hash = blocking(move || password::hash(&pw)).await?;
    svc.store
        .set_mailbox_password(area.as_deref(), &address, Some(&hash))
        .await?;
    audit_event_in(
        &app,
        &p,
        area.as_deref(),
        "mail.mailbox.password.set",
        json!({"address": address}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete, path = "/__admin/mail/mailboxes/{address}/password", tag = "mail", operation_id = "deleteMailboxPassword",
    summary = "Remove a mailbox password",
    description = "Removes the password of a mailbox in `space`; logins then need a managed token with `mail:read`. Records the audit event `mail.mailbox.password.delete`.",
    params(("address" = String, Path, description = "Mailbox address"), SpaceQuery),
    responses(
        (status = 204, description = "Removed"),
        (status = 400, description = "Invalid address", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, an unknown space, or no password is set", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn delete_password(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(address): Path<String>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<StatusCode> {
    let svc = service(&app)?;
    let area = spaces::target(&app, &p, Permission::MailWrite, q.space.as_deref())?;
    let address = mailbox_address(&address)?;
    if !svc
        .store
        .set_mailbox_password(area.as_deref(), &address, None)
        .await?
    {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no password is set for {address}"),
        ));
    }
    audit_event_in(
        &app,
        &p,
        area.as_deref(),
        "mail.mailbox.password.delete",
        json!({"address": address}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}
