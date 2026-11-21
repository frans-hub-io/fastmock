//! Mail chaos rules per space (`/__admin/mail/chaos`, M6) and their storage: rules in the data store
//! (PostgreSQL in postgres mode), counters in the hot store (Valkey), so every replica evaluates and
//! counts alike.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use fastmock_core::Stores;
use fastmock_mail::chaos::{
    ChaosBackend, ChaosCounters, ChaosError, MailChaos, MailChaosRule, StoredChaos, area_key,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::access::Permission;
use crate::admin::{ApiError, ApiResult, ErrorBody, audit_event_in, bad};
use crate::auth::Principal;
use crate::spaces::{self, SpaceQuery};
use crate::state::AppState;

/// Data-store namespace of the rules; the key is the space name or `_global`.
pub const NAMESPACE: &str = "mail-chaos";

/// [`ChaosBackend`] over the server's shared stores.
pub struct StoreChaosBackend {
    stores: Stores,
}

impl StoreChaosBackend {
    pub fn new(stores: Stores) -> Self {
        Self { stores }
    }
}

#[async_trait]
impl ChaosBackend for StoreChaosBackend {
    async fn load(&self, area: Option<&str>) -> anyhow::Result<Option<StoredChaos>> {
        match self.stores.data.get(NAMESPACE, area_key(area)).await? {
            Some(v) => Ok(Some(serde_json::from_value(v)?)),
            None => Ok(None),
        }
    }

    async fn save(&self, area: Option<&str>, chaos: &StoredChaos) -> anyhow::Result<()> {
        self.stores
            .data
            .put(NAMESPACE, area_key(area), &serde_json::to_value(chaos)?)
            .await
    }

    async fn remove(&self, area: Option<&str>) -> anyhow::Result<bool> {
        self.stores.data.delete(NAMESPACE, area_key(area)).await
    }

    async fn incr(&self, key: &str, ttl: Duration) -> anyhow::Result<i64> {
        self.stores.hot.incr(key, Some(ttl)).await
    }

    async fn counter(&self, key: &str) -> anyhow::Result<i64> {
        Ok(self
            .stores
            .hot
            .get(key)
            .await?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }

    async fn clear_counters(&self, prefix: &str) -> anyhow::Result<()> {
        self.stores.hot.delete_prefix(prefix).await.map(|_| ())
    }
}

/// The mail chaos of a server built on its stores.
pub fn build(stores: &Stores) -> Arc<MailChaos> {
    Arc::new(MailChaos::new(Arc::new(StoreChaosBackend::new(
        stores.clone(),
    ))))
}

#[derive(Deserialize, Serialize, utoipa::ToSchema)]
pub struct MailChaosBody {
    /// Evaluated in order; the first rule that fires wins. At most 100.
    pub rules: Vec<MailChaosRule>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MailChaosView {
    /// The space; absent for the global area.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "billing")]
    pub space: Option<String>,
    /// The rules, each with its id.
    pub rules: Vec<MailChaosRule>,
    /// How often each rule matched and fired since the rules were stored, over all replicas.
    pub counters: Vec<ChaosCounters>,
}

fn chaos(app: &AppState) -> ApiResult<&Arc<MailChaos>> {
    app.mail_chaos.as_ref().ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            "mail is not enabled (FASTMOCK_MAIL_ENABLED)".into(),
        )
    })
}

async fn view(
    c: &MailChaos,
    area: Option<String>,
    stored: StoredChaos,
) -> ApiResult<MailChaosView> {
    let counters = c.counters(area.as_deref(), &stored).await?;
    Ok(MailChaosView {
        space: area,
        rules: stored.rules,
        counters,
    })
}

#[utoipa::path(
    get, path = "/__admin/mail/chaos", tag = "mail", operation_id = "getMailChaos",
    summary = "Mail chaos rules of a space",
    description = "The rules of one area (the space, or the global area) with their counters. Needs `mail:read` there.",
    params(SpaceQuery),
    responses(
        (status = 200, description = "Rules and counters", body = MailChaosView),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires mail:read", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, or unknown space", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn get_chaos(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<Json<MailChaosView>> {
    let c = chaos(&app)?;
    let area = spaces::target(&app, &p, Permission::MailRead, q.space.as_deref())?;
    let stored = c.get(area.as_deref()).await?;
    Ok(Json(view(c, area, stored).await?))
}

#[utoipa::path(
    put, path = "/__admin/mail/chaos", tag = "mail", operation_id = "putMailChaos",
    summary = "Replace the mail chaos rules of a space",
    description = "Stores the rules of one area and starts their counters from zero. Rules are evaluated in order at their stage; the first rule whose match holds and that fires (after `every`, `probability` and `times`) decides. Stages before routing (`connect`, `tls`, `ehlo`) use the global area's rules; `auth` and `mail` the space named like the AUTH user (else global); `rcpt` the space of the recipient; `data` the spaces the message is routed to. Rules with stage `provider` apply to the provider HTTP APIs. Needs `mail:write`.",
    params(SpaceQuery),
    request_body(content = MailChaosBody, example = json!({"rules": [
        {"id": "third-fails", "stage": "data", "every": 3, "times": 1, "action": {"reply": {"code": 451, "enhanced": "4.3.0", "text": "Try again later"}}},
        {"stage": "rcpt", "match": {"rcptTo": {"contains": "slow@"}}, "action": {"delay": "2s"}}
    ]})),
    responses(
        (status = 200, description = "Rules stored", body = MailChaosView),
        (status = 400, description = "Invalid rule", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires mail:write", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, or unknown space", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn put_chaos(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
    Json(body): Json<Value>,
) -> ApiResult<Json<MailChaosView>> {
    let c = chaos(&app)?;
    let area = spaces::target(&app, &p, Permission::MailWrite, q.space.as_deref())?;
    let body: MailChaosBody =
        serde_json::from_value(body).map_err(|e| bad(format!("invalid mail chaos rules: {e}")))?;
    let stored = match c.replace(area.as_deref(), body.rules).await {
        Ok(s) => s,
        Err(ChaosError::Invalid(e)) => return Err(bad(e)),
        Err(ChaosError::Store(e)) => return Err(e.into()),
    };
    audit_event_in(
        &app,
        &p,
        area.as_deref(),
        "mail.chaos.set",
        json!({"rules": stored.rules.iter().map(|r| r.id()).collect::<Vec<_>>()}),
    )
    .await;
    Ok(Json(view(c, area, stored).await?))
}

#[utoipa::path(
    delete, path = "/__admin/mail/chaos", tag = "mail", operation_id = "clearMailChaos",
    summary = "Remove the mail chaos rules of a space",
    description = "Removes the rules of one area and their counters. Needs `mail:write`.",
    params(SpaceQuery),
    responses(
        (status = 204, description = "Removed"),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires mail:write", body = ErrorBody),
        (status = 404, description = "Mail is not enabled, or unknown space", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn delete_chaos(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<SpaceQuery>,
) -> ApiResult<StatusCode> {
    let c = chaos(&app)?;
    let area = spaces::target(&app, &p, Permission::MailWrite, q.space.as_deref())?;
    c.clear(area.as_deref()).await?;
    audit_event_in(&app, &p, area.as_deref(), "mail.chaos.clear", json!({})).await;
    Ok(StatusCode::NO_CONTENT)
}

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/mail/chaos",
        get(get_chaos).put(put_chaos).delete(delete_chaos),
    )
}

#[derive(utoipa::OpenApi)]
#[openapi(
    paths(get_chaos, put_chaos, delete_chaos),
    components(schemas(MailChaosBody, MailChaosView))
)]
pub struct MailChaosApi;
