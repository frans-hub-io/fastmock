//! Admin API for provider webhooks (`/__admin/mail/webhooks`, `/__admin/mail/inbound`, M8).

use axum::body::Bytes;
use axum::extract::{Extension, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use chrono::Utc;
use fastmock_mail::compose::{ComposeRequest, compose};
use fastmock_mail::providers::Provider;
use fastmock_mail::{MailService, ParseLimits, ParsedMail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::access::Permission;
use crate::admin::{ApiError, ApiResult, ErrorBody, audit_event_in, bad};
use crate::auth::Principal;
use crate::mail_webhooks::{self, InboundMail, MailWebhooksBody, MailWebhooksView, WebhookKind};
use crate::spaces::{self, SpaceQuery};
use crate::state::AppState;

fn sealer(app: &AppState) -> ApiResult<&crate::mail_webhook_secrets::Sealer> {
    app.mail_sealer
        .as_deref()
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "mail is not enabled".into()))
}

fn mail(app: &AppState) -> ApiResult<&std::sync::Arc<MailService>> {
    app.mail.as_ref().ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            "mail is not enabled (FASTMOCK_MAIL_ENABLED)".into(),
        )
    })
}

#[utoipa::path(
    get, path = "/__admin/mail/webhooks", tag = "mail", operation_id = "getMailWebhooks",
    summary = "Provider webhooks of a space",
    description = "The webhooks, the expected provider API keys (SHA-256 digests) and the SendGrid verification key of one area. Signing keys are never returned and URL passwords are shown as `***`. Needs `mail:read`.",
    params(SpaceQuery),
    responses(
        (status = 200, description = "Configuration", body = MailWebhooksView),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires mail:read", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, or unknown space", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn get_webhooks(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<Json<MailWebhooksView>> {
    mail(&app)?;
    let area = spaces::target(&app, &p, Permission::MailRead, q.space.as_deref())?;
    let stored = mail_webhooks::load(&app.stores, area.as_deref()).await?;
    Ok(Json(mail_webhooks::view(area, &stored, sealer(&app)?)))
}

#[utoipa::path(
    put, path = "/__admin/mail/webhooks", tag = "mail", operation_id = "putMailWebhooks",
    summary = "Replace the provider webhooks of a space",
    description = "Stores the webhooks and expected API keys of one area. Events webhooks receive the provider's delivery events after each message sent through that provider's API in this area (`delivered`, or `bounce`, `complaint`, `deferred`, `dropped` for recipients with that local part); inbound webhooks receive the provider's inbound payload from `POST /__admin/mail/inbound`. Deliveries go through the callbacks outbox (retries, `/__admin/callbacks`). `signingKey` is write-only; a webhook `PUT` again with the same id without it (or with `***` as URL password) keeps the stored value. Signing keys and URL passwords are stored encrypted with the secret `mail-webhook-key` (required with PostgreSQL storage). The first `PUT` creates the space's SendGrid key pair. Needs `mail:write`.",
    params(SpaceQuery),
    request_body(content = MailWebhooksBody, example = json!({
        "webhooks": [
            {"id": "sg-events", "provider": "sendgrid", "kind": "events", "url": "https://app.example.com/hooks/sendgrid", "events": ["delivered", "bounce"]},
            {"id": "mg-inbound", "provider": "mailgun", "kind": "inbound", "url": "https://app.example.com/hooks/mailgun", "signingKey": "key-example"}
        ],
        "apiKeys": {"sendgrid": ["9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"]}
    })),
    responses(
        (status = 200, description = "Stored", body = MailWebhooksView),
        (status = 400, description = "Invalid configuration", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires mail:write", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, or unknown space", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn put_webhooks(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
    Json(body): Json<Value>,
) -> ApiResult<Json<MailWebhooksView>> {
    mail(&app)?;
    let area = spaces::target(&app, &p, Permission::MailWrite, q.space.as_deref())?;
    let body: MailWebhooksBody =
        serde_json::from_value(body).map_err(|e| bad(format!("invalid mail webhooks: {e}")))?;
    let old = mail_webhooks::load(&app.stores, area.as_deref()).await?;
    let stored = mail_webhooks::merge(&old, body, &app.policy, sealer(&app)?).map_err(bad)?;
    mail_webhooks::save(&app.stores, area.as_deref(), &stored).await?;
    audit_event_in(
        &app,
        &p,
        area.as_deref(),
        "mail.webhooks.set",
        json!({
            "webhooks": stored.webhooks.iter().map(|w| json!({"id": w.id, "provider": w.provider, "kind": w.kind})).collect::<Vec<_>>(),
            "apiKeys": stored.api_keys.keys().collect::<Vec<_>>(),
        }),
    )
    .await;
    Ok(Json(mail_webhooks::view(area, &stored, sealer(&app)?)))
}

#[utoipa::path(
    delete, path = "/__admin/mail/webhooks", tag = "mail", operation_id = "clearMailWebhooks",
    summary = "Remove the provider webhooks of a space",
    description = "Removes the webhooks, expected API keys and the SendGrid key pair of one area; queued deliveries stay in the outbox. Needs `mail:write`.",
    params(SpaceQuery),
    responses(
        (status = 204, description = "Removed"),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires mail:write", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, or unknown space", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn delete_webhooks(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<StatusCode> {
    mail(&app)?;
    let area = spaces::target(&app, &p, Permission::MailWrite, q.space.as_deref())?;
    mail_webhooks::remove(&app.stores, area.as_deref()).await?;
    audit_event_in(&app, &p, area.as_deref(), "mail.webhooks.clear", json!({})).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct InboundQuery {
    /// Space whose inbound webhooks receive the message.
    pub space: Option<String>,
    /// With `Content-Type: message/rfc822`: the provider (`ses`, `sendgrid`, `mailgun`, `postmark`).
    pub provider: Option<String>,
    /// With `Content-Type: message/rfc822`: only this webhook.
    pub webhook_id: Option<String>,
}

/// `POST /__admin/mail/inbound` with JSON: the provider and the message, built from fields
/// (`message`) or given raw (`raw` text or `rawBase64`).
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InboundBody {
    /// `ses`, `sendgrid`, `mailgun` or `postmark`.
    #[schema(example = "sendgrid")]
    pub provider: String,
    /// Only this inbound webhook; default all inbound webhooks of the provider in the space.
    #[serde(default)]
    pub webhook_id: Option<String>,
    #[serde(default)]
    pub message: Option<ComposeRequest>,
    /// The MIME message as text.
    #[serde(default)]
    pub raw: Option<String>,
    /// The MIME message, base64.
    #[serde(default)]
    pub raw_base64: Option<String>,
    /// Envelope sender; default the From address.
    #[serde(default)]
    pub mail_from: Option<String>,
    /// Envelope recipients; default the To, Cc and Bcc addresses.
    #[serde(default)]
    pub rcpt_to: Vec<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct InboundDelivery {
    pub webhook_id: String,
    /// The callback job (`/__admin/callbacks`).
    pub callback_id: uuid::Uuid,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct InboundResult {
    pub deliveries: Vec<InboundDelivery>,
}

#[utoipa::path(
    post, path = "/__admin/mail/inbound", tag = "mail", operation_id = "sendMailInbound",
    summary = "Post an inbound message to provider webhooks",
    description = "Builds the provider's inbound payload for a message (SendGrid Inbound Parse multipart, Mailgun route form/JSON/MIME, Postmark inbound JSON, SES receipt in an SNS envelope) and queues it for the inbound webhooks of that provider in the space. The message is not stored. Send JSON (`InboundBody`), or the raw message with `Content-Type: message/rfc822` and `provider` in the query. SNS envelopes are not signed. Needs `mail:write`.",
    params(InboundQuery),
    request_body(content = InboundBody, example = json!({
        "provider": "postmark",
        "message": {"from": "Customer <customer@example.com>", "to": ["support@inbound.example.test"], "subject": "Help", "text": "Hello"}
    })),
    responses(
        (status = 202, description = "Queued", body = InboundResult),
        (status = 400, description = "Invalid message or provider", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires mail:write", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, unknown space, or no inbound webhook of the provider", body = ErrorBody),
        (status = 413, description = "The message exceeds the mail message size limit", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn post_inbound(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<InboundQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<InboundResult>)> {
    let svc = mail(&app)?;
    let area = spaces::target(&app, &p, Permission::MailWrite, q.space.as_deref())?;
    let max = svc.config.max_message_bytes;
    let rfc822 = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.trim().to_ascii_lowercase().starts_with("message/rfc822"));
    let (provider, webhook_id, raw, mail_from, rcpt_to) = if rfc822 {
        let provider = q
            .provider
            .clone()
            .ok_or_else(|| bad("provider is required in the query with message/rfc822"))?;
        (provider, q.webhook_id.clone(), body.to_vec(), None, vec![])
    } else {
        let b: InboundBody = serde_json::from_slice(&body)
            .map_err(|e| bad(format!("invalid inbound request: {e}")))?;
        let given = [b.message.is_some(), b.raw.is_some(), b.raw_base64.is_some()]
            .iter()
            .filter(|x| **x)
            .count();
        if given != 1 {
            return Err(bad("give exactly one of message, raw and rawBase64"));
        }
        let raw = if let Some(m) = &b.message {
            compose(m, max).map_err(|e| match e {
                fastmock_mail::compose::ComposeError::TooLarge(_) => ApiError(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("the message exceeds {max} bytes"),
                ),
                other => bad(other.to_string()),
            })?
        } else if let Some(r) = &b.raw {
            r.clone().into_bytes()
        } else {
            base64::engine::general_purpose::STANDARD
                .decode(b.raw_base64.as_deref().unwrap_or("").trim())
                .map_err(|_| bad("rawBase64 is not valid base64"))?
        };
        (b.provider, b.webhook_id, raw, b.mail_from, b.rcpt_to)
    };
    let provider = Provider::parse(&provider)
        .filter(|p| mail_webhooks::WEBHOOK_PROVIDERS.contains(p))
        .ok_or_else(|| bad("provider must be ses, sendgrid, mailgun or postmark"))?;
    if raw.len() > max {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("the message exceeds {max} bytes"),
        ));
    }
    let parsed = ParsedMail::parse(
        &raw,
        &ParseLimits {
            max_bytes: max,
            ..Default::default()
        },
    );
    if !parsed.is_parsed() {
        return Err(bad("the message is not a MIME message"));
    }
    let rcpt_to = if rcpt_to.is_empty() {
        parsed.recipients()
    } else {
        rcpt_to
    };
    if rcpt_to.is_empty() {
        return Err(bad("the message has no recipients"));
    }
    let mail_from = mail_from
        .or_else(|| parsed.from().first().map(|a| a.address.clone()))
        .unwrap_or_default();
    let inbound = InboundMail {
        raw,
        mail_from,
        rcpt_to,
    };
    let sealer = sealer(&app)?;
    let stored = mail_webhooks::load(&app.stores, area.as_deref()).await?;
    let hooks = mail_webhooks::opened(&stored, sealer);
    let targets: Vec<_> = hooks
        .webhooks
        .iter()
        .filter(|w| w.kind == WebhookKind::Inbound && w.provider == provider.name())
        .filter(|w| webhook_id.as_deref().is_none_or(|id| id == w.id))
        .collect();
    if targets.is_empty() {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!(
                "no inbound {} webhook{} in this space",
                provider.name(),
                webhook_id
                    .as_deref()
                    .map(|i| format!(" with id {i}"))
                    .unwrap_or_default()
            ),
        ));
    }
    let now = Utc::now();
    let correlation = format!("inbound-{}", uuid::Uuid::now_v7());
    let mut deliveries = Vec::with_capacity(targets.len());
    for hook in targets {
        let Some(out) =
            mail_webhooks::inbound_request(&inbound, hook, hooks.sendgrid_key.as_deref(), max, now)
        else {
            continue;
        };
        let id = mail_webhooks::enqueue(
            &app.stores,
            sealer,
            area.as_deref(),
            hook,
            out,
            &correlation,
        )
        .await?;
        deliveries.push(InboundDelivery {
            webhook_id: hook.id.clone(),
            callback_id: id,
        });
    }
    audit_event_in(
        &app,
        &p,
        area.as_deref(),
        "mail.inbound",
        json!({"provider": provider.name(), "webhooks": deliveries.iter().map(|d| &d.webhook_id).collect::<Vec<_>>()}),
    )
    .await;
    Ok((StatusCode::ACCEPTED, Json(InboundResult { deliveries })))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/mail/webhooks",
            get(get_webhooks).put(put_webhooks).delete(delete_webhooks),
        )
        .route("/mail/inbound", post(post_inbound))
}

#[derive(utoipa::OpenApi)]
#[openapi(
    paths(get_webhooks, put_webhooks, delete_webhooks, post_inbound),
    components(schemas(
        MailWebhooksBody,
        MailWebhooksView,
        mail_webhooks::WebhookInput,
        mail_webhooks::WebhookView,
        mail_webhooks::WebhookKind,
        mail_webhooks::MailEventName,
        InboundBody,
        InboundResult,
        InboundDelivery
    ))
)]
pub struct MailWebhooksApi;
