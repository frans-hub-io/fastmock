use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, SubsecRound, Utc};
use fastmock_core::{ApiToken, ApiTokenStatus, TokenStore};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::access::{Binding, BindingScope, PermSet, Permission, PrincipalKind};
use crate::admin::{ApiError, ApiResult, ErrorBody, audit_event_in, bad, not_found, require};
use crate::auth::{Authenticator, Principal, generate_token, hash_token};
use crate::state::AppState;

const CACHE_MAX: usize = 16_384;
const NEGATIVE_MAX: usize = 4_096;
const TOUCHED_MAX: usize = 16_384;
const TOUCH_EVERY: Duration = Duration::from_secs(60);
const MAX_NAME: usize = 128;
const MAX_DESCRIPTION: usize = 1024;
/// Revoked and expired tokens are kept this long for inspection, then deleted.
pub const KEEP_INACTIVE_DAYS: i64 = 90;

/// Lookups by hash: known tokens, and hashes the store does not know (bounded separately, since anyone
/// can present them).
#[derive(Default)]
struct Cache {
    known: HashMap<String, (Instant, Arc<ApiToken>)>,
    unknown: HashMap<String, Instant>,
}

/// Managed tokens: the store and a per-replica cache of lookups by hash. Cache entries live at most
/// `ttl`, so a revocation on another replica takes effect within it; expiry is checked on every use.
pub struct TokenService {
    store: Arc<dyn TokenStore>,
    ttl: Duration,
    cache: Mutex<Cache>,
    touched: Mutex<HashMap<Uuid, Instant>>,
    /// Whether the store holds any token, and when that was read: an empty store answers unknown
    /// values without a lookup.
    any: Mutex<Option<(Instant, bool)>>,
}

pub enum Lookup {
    Valid(Arc<ApiToken>),
    Expired(Arc<ApiToken>),
    Revoked(Arc<ApiToken>),
    Unknown,
}

/// The managed token format: `fm_` and 64 lowercase hex digits. Other values never reach the store.
pub fn looks_managed(value: &str) -> bool {
    value.len() == 67
        && value.starts_with("fm_")
        && value[3..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl TokenService {
    pub fn new(store: Arc<dyn TokenStore>, ttl: Duration) -> Self {
        Self {
            store,
            ttl,
            cache: Mutex::new(Cache::default()),
            touched: Mutex::new(HashMap::new()),
            any: Mutex::new(None),
        }
    }

    /// Whether the store may hold the presented token. A failed read keeps a previous "empty" answer,
    /// which only rejects; otherwise the error stands.
    async fn store_has_tokens(&self) -> anyhow::Result<bool> {
        let last = *self.any.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, any)) = last
            && at.elapsed() < self.ttl
        {
            return Ok(any);
        }
        match self.store.has_any().await {
            Ok(any) => {
                *self.any.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), any));
                Ok(any)
            }
            Err(e) => match last {
                Some((_, false)) => {
                    tracing::warn!(error = %e, "checking for managed tokens failed; the store held none");
                    Ok(false)
                }
                _ => Err(e),
            },
        }
    }

    /// Records that the store now holds a token.
    pub fn note_created(&self) {
        *self.any.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), true));
    }

    pub fn store(&self) -> &Arc<dyn TokenStore> {
        &self.store
    }

    fn cached(&self, hash: &str) -> Option<Option<Arc<ApiToken>>> {
        let c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, t)) = c.known.get(hash)
            && at.elapsed() < self.ttl
        {
            return Some(Some(t.clone()));
        }
        c.unknown
            .get(hash)
            .filter(|at| at.elapsed() < self.ttl)
            .map(|_| None)
    }

    fn remember(&self, hash: String, token: Option<Arc<ApiToken>>) {
        let ttl = self.ttl;
        let mut c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        match token {
            Some(t) => {
                c.unknown.remove(&hash);
                if c.known.len() >= CACHE_MAX {
                    c.known.retain(|_, (at, _)| at.elapsed() < ttl);
                    if c.known.len() >= CACHE_MAX {
                        c.known.clear();
                    }
                }
                c.known.insert(hash, (Instant::now(), t));
            }
            None => {
                c.known.remove(&hash);
                if c.unknown.len() >= NEGATIVE_MAX {
                    c.unknown.retain(|_, at| at.elapsed() < ttl);
                    if c.unknown.len() >= NEGATIVE_MAX {
                        c.unknown.clear();
                    }
                }
                c.unknown.insert(hash, Instant::now());
            }
        }
    }

    pub fn forget(&self, hash: &str) {
        let mut c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        c.known.remove(hash);
        c.unknown.remove(hash);
    }

    pub async fn lookup(&self, value: &str) -> anyhow::Result<Lookup> {
        let hash = hash_token(value);
        let token = match self.cached(&hash) {
            Some(t) => t,
            None if !self.store_has_tokens().await? => None,
            None => {
                let t = self.store.by_hash(&hash).await?.map(Arc::new);
                self.remember(hash, t.clone());
                t
            }
        };
        Ok(match token {
            None => Lookup::Unknown,
            Some(t) => match t.status(Utc::now()) {
                ApiTokenStatus::Active => Lookup::Valid(t),
                ApiTokenStatus::Expired => Lookup::Expired(t),
                ApiTokenStatus::Revoked => Lookup::Revoked(t),
            },
        })
    }

    /// Records the use of a token in the background, at most once per minute per token and replica.
    pub fn touch(&self, id: Uuid) {
        {
            let mut m = self.touched.lock().unwrap_or_else(|e| e.into_inner());
            if m.get(&id).is_some_and(|t| t.elapsed() < TOUCH_EVERY) {
                return;
            }
            if m.len() >= TOUCHED_MAX {
                m.retain(|_, t| t.elapsed() < TOUCH_EVERY);
                if m.len() >= TOUCHED_MAX {
                    m.clear();
                }
            }
            m.insert(id, Instant::now());
        }
        let store = self.store.clone();
        tokio::spawn(async move {
            if let Err(e) = store.touch(id, stored_precision(Utc::now())).await {
                metrics::counter!("fastmock_store_errors_total", "store" => "tokens").increment(1);
                tracing::warn!(error = %e, token_id = %id, "recording the last use of a token failed");
            }
        });
    }
}

/// The principal of a managed token, or `None` when its role no longer exists.
pub fn principal_of(auth: &Authenticator, t: &ApiToken) -> Option<Principal> {
    let perms = auth.access().role(&t.role)?;
    let scope = if t.global {
        BindingScope::Global
    } else {
        BindingScope::Spaces(t.spaces.clone())
    };
    let binding = Binding {
        role: t.role.clone(),
        permissions: perms,
        scope,
    };
    let mut p = Principal::from_bindings(&t.name, PrincipalKind::ApiToken, &[binding]);
    p.token_id = Some(t.id);
    Some(p)
}

pub async fn refresh_gauge(app: &AppState) {
    match app.tokens.store().count_active(Utc::now()).await {
        Ok(n) => metrics::gauge!("fastmock_api_tokens_active").set(n as f64),
        Err(e) => {
            metrics::counter!("fastmock_store_errors_total", "store" => "tokens").increment(1);
            tracing::warn!(error = %e, "counting active tokens failed");
        }
    }
}

pub async fn maintain(app: &AppState) {
    let cutoff = Utc::now() - chrono::Duration::days(KEEP_INACTIVE_DAYS);
    match app.tokens.store().purge_inactive_before(cutoff).await {
        Ok(n) if n > 0 => tracing::info!(deleted = n, "old revoked and expired tokens deleted"),
        Ok(_) => {}
        Err(e) => tracing::error!(error = %e, "token retention failed"),
    }
    refresh_gauge(app).await;
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateToken {
    /// `[A-Za-z0-9._@-]`, at most 128 characters; unique among active tokens and distinct from the static token names.
    #[schema(example = "ci-billing")]
    pub name: String,
    /// Built-in role (`viewer`, `editor`, `admin`) or a custom role of the access file.
    #[schema(example = "editor")]
    pub role: String,
    /// Spaces the role is granted in. Set either `spaces` or `global: true`.
    #[serde(default)]
    #[schema(example = json!(["billing"]))]
    pub spaces: Option<Vec<String>>,
    /// Grant the role everywhere.
    #[serde(default)]
    pub global: Option<bool>,
    /// Lifetime in days: 1 to `FASTMOCK_TOKENS_MAX_DAYS` (default 365); 90 when omitted.
    #[serde(default)]
    #[schema(minimum = 1, example = 30)]
    pub expires_in_days: Option<u32>,
    #[serde(default)]
    #[schema(example = "CI pipeline of the billing team")]
    pub description: Option<String>,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TokenStatus {
    Active,
    Expired,
    Revoked,
}

impl From<ApiTokenStatus> for TokenStatus {
    fn from(s: ApiTokenStatus) -> Self {
        match s {
            ApiTokenStatus::Active => TokenStatus::Active,
            ApiTokenStatus::Expired => TokenStatus::Expired,
            ApiTokenStatus::Revoked => TokenStatus::Revoked,
        }
    }
}

/// Metadata of a managed token; the value is never shown again after creation.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TokenInfo {
    pub id: Uuid,
    #[schema(example = "ci-billing")]
    pub name: String,
    #[schema(example = "editor")]
    pub role: String,
    /// Spaces of the token; empty for a global token.
    pub spaces: Vec<String>,
    pub global: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Name of the principal that created the token.
    #[schema(example = "ops")]
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// Last use, recorded at most once per minute.
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub status: TokenStatus,
    /// The managed token that created this one; it expires no later than that token and is revoked
    /// with it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<Uuid>,
}

/// Token times as PostgreSQL stores them (microseconds). Times are taken at this precision and
/// serialised at it, so a create response, later reads and every backend show the same values.
fn stored_precision(t: DateTime<Utc>) -> DateTime<Utc> {
    t.trunc_subsecs(6)
}

impl TokenInfo {
    fn of(t: &ApiToken) -> Self {
        TokenInfo {
            id: t.id,
            name: t.name.clone(),
            role: t.role.clone(),
            spaces: t.spaces.clone(),
            global: t.global,
            description: t.description.clone(),
            created_by: t.created_by.clone(),
            created_at: stored_precision(t.created_at),
            expires_at: stored_precision(t.expires_at),
            last_used_at: t.last_used_at.map(stored_precision),
            revoked_at: t
                .revoked_at
                .filter(|_| t.status(Utc::now()) == ApiTokenStatus::Revoked)
                .map(stored_precision),
            status: t.status(Utc::now()).into(),
            parent_id: t.parent_id,
        }
    }
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct CreatedToken {
    /// The token value (`fm_` and 64 hex digits). Returned only in this response: store it now.
    #[schema(example = "fm_3f9c0d5e8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f4a3b2c1d0")]
    pub token: String,
    #[serde(flatten)]
    pub info: TokenInfo,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct TokenList {
    pub tokens: Vec<TokenInfo>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TokensQuery {
    /// Only tokens granted in this space.
    pub space: Option<String>,
    /// `active`, `expired` or `revoked`.
    pub status: Option<String>,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
}

fn scopes_of(t: &ApiToken) -> Vec<Option<&str>> {
    if t.global {
        vec![None]
    } else {
        t.spaces.iter().map(|s| Some(s.as_str())).collect()
    }
}

/// True when the caller holds `perm` in every scope of the token.
fn holds_everywhere(p: &Principal, perm: Permission, t: &ApiToken) -> bool {
    let scopes = scopes_of(t);
    !scopes.is_empty() && scopes.into_iter().all(|s| p.allowed(perm, s))
}

fn place(space: Option<&str>) -> String {
    match space {
        Some(s) => format!(" in space {s}"),
        None => " globally".into(),
    }
}

fn deny(p: &Principal, msg: String) -> ApiError {
    p.count_denial();
    tracing::warn!(target: "audit", principal = %p.name, kind = p.kind.as_str(), reason = %msg, "admin request rejected: token escalation");
    ApiError(StatusCode::FORBIDDEN, msg)
}

/// The first permission of a role (with `perms`) that the caller does not hold in `scope`.
fn missing_for_role(p: &Principal, perms: PermSet, scope: Option<&str>) -> Option<Permission> {
    let (needed, held) = match scope {
        None => (perms, p.permissions.global()),
        Some(space) => (perms.space_scoped(), p.permissions.in_space(space)),
    };
    needed.iter().find(|x| !held.contains(*x))
}

/// The escalation rule: in each requested scope the caller needs `tokens:write` and every permission the
/// role grants there.
fn check_grant(
    p: &Principal,
    role: &str,
    perms: PermSet,
    scopes: &[Option<&str>],
) -> ApiResult<()> {
    for s in scopes {
        require(p, Permission::TokensWrite, *s)?;
        if let Some(missing) = missing_for_role(p, perms, *s) {
            return Err(deny(
                p,
                format!(
                    "requires permission {missing}{} to grant role {role} there",
                    place(*s)
                ),
            ));
        }
    }
    Ok(())
}

fn audit_detail(t: &ApiToken) -> serde_json::Value {
    json!({
        "id": t.id,
        "name": t.name,
        "kind": PrincipalKind::ApiToken.as_str(),
        "role": t.role,
        "global": t.global,
        "spaces": t.spaces,
        "expiresAt": stored_precision(t.expires_at),
    })
}

fn audit_space(t: &ApiToken) -> Option<&str> {
    match t.spaces.as_slice() {
        [one] if !t.global => Some(one.as_str()),
        _ => None,
    }
}

#[utoipa::path(
    post, path = "/__admin/tokens", tag = "tokens", operation_id = "createToken",
    summary = "Create a managed token",
    description = "Returns the token value once. The caller needs `tokens:write` and every permission of the role in each requested scope (globally for `global: true`), so a token never holds more than its creator; a caller bound only to spaces cannot create global tokens. Expiry is mandatory; a token created by a managed token expires no later than that token and is revoked with it.",
    request_body = CreateToken,
    responses(
        (status = 201, description = "Created; `token` is shown only here", body = CreatedToken),
        (status = 400, description = "Invalid name, role, scope or lifetime", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "The caller cannot grant this role in these scopes", body = ErrorBody),
        (status = 404, description = "Unknown space", body = ErrorBody),
        (status = 409, description = "An active token or a static token has this name", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn create_token(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Json(body): Json<CreateToken>,
) -> ApiResult<(StatusCode, Json<CreatedToken>)> {
    let name = body.name.trim().to_string();
    if !valid_name(&name) {
        return Err(bad(format!(
            "name must be 1..{MAX_NAME} characters of [A-Za-z0-9._@-]"
        )));
    }
    let perms = app
        .auth
        .access()
        .role(&body.role)
        .ok_or_else(|| bad(format!("unknown role `{}`", body.role)))?;
    let spaces = match (body.global.unwrap_or(false), body.spaces) {
        (true, None) => vec![],
        (true, Some(l)) if l.is_empty() => vec![],
        (false, Some(list)) if !list.is_empty() => {
            let mut seen = BTreeSet::new();
            for s in &list {
                crate::spaces::known(&app, &p, s)?;
                if !seen.insert(s.clone()) {
                    return Err(bad(format!("space `{s}` listed twice")));
                }
            }
            list
        }
        (true, Some(_)) => return Err(bad("set either spaces or global: true, not both")),
        _ => return Err(bad("set spaces (a non-empty list) or global: true")),
    };
    let global = spaces.is_empty();
    let scopes: Vec<Option<&str>> = if global {
        vec![None]
    } else {
        spaces.iter().map(|s| Some(s.as_str())).collect()
    };
    check_grant(&p, &body.role, perms, &scopes)?;
    let max = app.cfg.tokens_max_days;
    let days = body
        .expires_in_days
        .unwrap_or(crate::config::DEFAULT_TOKEN_DAYS.min(max));
    if days == 0 || days > max {
        return Err(bad(format!(
            "expiresInDays must be between 1 and {max} (FASTMOCK_TOKENS_MAX_DAYS)"
        )));
    }
    let description = body
        .description
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty());
    if description
        .as_ref()
        .is_some_and(|d| d.len() > MAX_DESCRIPTION || d.chars().any(char::is_control))
    {
        return Err(bad(format!(
            "description is limited to {MAX_DESCRIPTION} bytes without control characters"
        )));
    }
    if app.auth.is_static_name(&name) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("name {name} belongs to a static token"),
        ));
    }
    let value = generate_token();
    let now = stored_precision(Utc::now());
    let mut expires_at = now + chrono::Duration::days(i64::from(days));
    if let Some(parent) = p.token_id {
        let pt = app
            .tokens
            .store()
            .get(parent)
            .await?
            .filter(|t| t.status(now) == ApiTokenStatus::Active)
            .ok_or_else(|| {
                ApiError(
                    StatusCode::UNAUTHORIZED,
                    "the calling token is no longer active".into(),
                )
            })?;
        expires_at = expires_at.min(pt.expires_at);
    }
    let t = ApiToken {
        id: Uuid::now_v7(),
        name,
        sha256: hash_token(&value),
        role: body.role,
        spaces,
        global,
        description,
        created_by: p.name.clone(),
        created_at: now,
        expires_at,
        last_used_at: None,
        revoked_at: None,
        revoked: false,
        parent_id: p.token_id,
    };
    if !app.tokens.store().create(&t, now).await? {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("an active token named {} exists", t.name),
        ));
    }
    app.tokens.forget(&t.sha256);
    app.tokens.note_created();
    audit_event_in(&app, &p, audit_space(&t), "token.create", audit_detail(&t)).await;
    refresh_gauge(&app).await;
    Ok((
        StatusCode::CREATED,
        Json(CreatedToken {
            token: value,
            info: TokenInfo::of(&t),
        }),
    ))
}

#[utoipa::path(
    get, path = "/__admin/tokens", tag = "tokens", operation_id = "listTokens",
    summary = "List managed tokens",
    description = "Metadata only, newest first, including revoked and expired tokens (kept for 90 days). Lists the tokens for which the caller holds `tokens:read` in every scope; global tokens need it globally.",
    params(TokensQuery),
    responses(
        (status = 200, description = "Tokens", body = TokenList),
        (status = 400, description = "Invalid status", body = ErrorBody),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires tokens:read", body = ErrorBody),
        (status = 404, description = "Unknown space", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn list_tokens(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<TokensQuery>,
) -> ApiResult<Json<TokenList>> {
    let status = match q.status.as_deref() {
        None => None,
        Some("active") => Some(ApiTokenStatus::Active),
        Some("expired") => Some(ApiTokenStatus::Expired),
        Some("revoked") => Some(ApiTokenStatus::Revoked),
        Some(_) => return Err(bad("status must be active, expired or revoked")),
    };
    if let Some(s) = &q.space {
        crate::spaces::known(&app, &p, s)?;
        require(&p, Permission::TokensRead, Some(s))?;
    }
    let now = Utc::now();
    let tokens = app
        .tokens
        .store()
        .list()
        .await?
        .iter()
        .filter(|t| holds_everywhere(&p, Permission::TokensRead, t))
        .filter(|t| status.is_none_or(|s| t.status(now) == s))
        .filter(|t| {
            q.space
                .as_ref()
                .is_none_or(|s| !t.global && t.spaces.contains(s))
        })
        .map(TokenInfo::of)
        .collect();
    Ok(Json(TokenList { tokens }))
}

#[utoipa::path(
    get, path = "/__admin/tokens/self", tag = "tokens", operation_id = "getOwnToken",
    summary = "The caller's own managed token",
    description = "Any managed token may read its own metadata. Other principals get 404.",
    responses(
        (status = 200, description = "Token", body = TokenInfo),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 404, description = "The caller is not a managed token", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn own_token(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
) -> ApiResult<Json<TokenInfo>> {
    let id = p.token_id.ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            "the caller is not a managed token".into(),
        )
    })?;
    let t = app
        .tokens
        .store()
        .get(id)
        .await?
        .ok_or_else(|| not_found("token"))?;
    Ok(Json(TokenInfo::of(&t)))
}

#[utoipa::path(
    get, path = "/__admin/tokens/{id}", tag = "tokens", operation_id = "getToken",
    summary = "One managed token",
    description = "Needs `tokens:read` in every scope of the token; tokens outside the caller's scopes answer 404.",
    params(("id" = Uuid, Path, description = "Token id")),
    responses(
        (status = 200, description = "Token", body = TokenInfo),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires tokens:read", body = ErrorBody),
        (status = 404, description = "Not found", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn get_token(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<TokenInfo>> {
    let t = app
        .tokens
        .store()
        .get(id)
        .await?
        .filter(|t| p.token_id == Some(t.id) || holds_everywhere(&p, Permission::TokensRead, t))
        .ok_or_else(|| not_found("token"))?;
    Ok(Json(TokenInfo::of(&t)))
}

#[utoipa::path(
    delete, path = "/__admin/tokens/{id}", tag = "tokens", operation_id = "revokeToken",
    summary = "Revoke a managed token",
    description = "Needs `tokens:write` in every scope of the token; a managed token may always revoke itself. Every replica rejects the token within 10 seconds. Tokens created by this token, directly or not, are revoked with it. Revoking a token that is no longer active (revoked or expired) changes nothing and records no audit event.",
    params(("id" = Uuid, Path, description = "Token id")),
    responses(
        (status = 200, description = "The token after revocation", body = TokenInfo),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires tokens:write in every scope of the token", body = ErrorBody),
        (status = 404, description = "Not found", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn revoke_token(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<TokenInfo>> {
    let own = p.token_id == Some(id);
    if !own && !p.permissions.anywhere(Permission::TokensWrite) {
        require(&p, Permission::TokensWrite, None)?;
    }
    let store = app.tokens.store();
    let t = store.get(id).await?.ok_or_else(|| not_found("token"))?;
    if !own {
        if !holds_everywhere(&p, Permission::TokensRead, &t)
            && !holds_everywhere(&p, Permission::TokensWrite, &t)
        {
            return Err(not_found("token"));
        }
        for s in scopes_of(&t) {
            require(&p, Permission::TokensWrite, s)?;
        }
    }
    let now = stored_precision(Utc::now());
    let revoked = store.revoke(id, now).await?;
    app.tokens.forget(&t.sha256);
    let t = store.get(id).await?.ok_or_else(|| not_found("token"))?;
    if revoked {
        let children = revoke_descendants(&app, id, now).await?;
        let mut detail = audit_detail(&t);
        detail["self"] = json!(own);
        if !children.is_empty() {
            detail["revokedChildren"] = json!(children);
        }
        audit_event_in(&app, &p, audit_space(&t), "token.revoke", detail).await;
        refresh_gauge(&app).await;
    }
    Ok(Json(TokenInfo::of(&t)))
}

/// Revokes every active token created, directly or not, by the token `root`.
async fn revoke_descendants(app: &AppState, root: Uuid, at: DateTime<Utc>) -> ApiResult<Vec<Uuid>> {
    let store = app.tokens.store();
    let all = store.list().await?;
    let mut frontier = vec![root];
    let mut seen: Vec<Uuid> = vec![root];
    let mut revoked: Vec<Uuid> = Vec::new();
    while let Some(parent) = frontier.pop() {
        for t in all.iter().filter(|t| t.parent_id == Some(parent)) {
            if seen.contains(&t.id) {
                continue;
            }
            seen.push(t.id);
            if store.revoke(t.id, at).await? {
                app.tokens.forget(&t.sha256);
                revoked.push(t.id);
            }
            frontier.push(t.id);
        }
    }
    Ok(revoked)
}

/// A role and the scopes in which the caller may grant it.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GrantableRole {
    #[schema(example = "editor")]
    pub name: String,
    /// `viewer`, `editor` or `admin`; false for custom roles of the access file.
    pub builtin: bool,
    #[schema(example = json!(["stubs:read", "stubs:write"]))]
    pub permissions: Vec<String>,
    /// The caller may create global tokens with this role.
    pub global: bool,
    /// Spaces in which the caller may create tokens with this role.
    #[schema(example = json!(["payments"]))]
    pub spaces: Vec<String>,
}

/// What the caller may choose when creating a token.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TokenOptions {
    /// Longest lifetime in days (`FASTMOCK_TOKENS_MAX_DAYS`).
    #[schema(example = 365)]
    pub max_days: u32,
    /// Lifetime used when `expiresInDays` is omitted.
    #[schema(example = 90)]
    pub default_days: u32,
    /// Every role, built-in first, with the scopes where the caller may grant it (the escalation rule
    /// applied to the caller).
    pub roles: Vec<GrantableRole>,
}

#[utoipa::path(
    get, path = "/__admin/tokens/options", tag = "tokens", operation_id = "getTokenOptions",
    summary = "Choices for creating a token",
    description = "The roles the caller may grant and where, and the lifetime limits. Needs `tokens:write` somewhere. A role is grantable in a scope when the caller holds `tokens:write` and every permission of the role there.",
    responses(
        (status = 200, description = "Options", body = TokenOptions),
        (status = 401, description = "Missing or invalid token", body = ErrorBody),
        (status = 403, description = "Requires tokens:write", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn token_options(
    State(app): State<AppState>,
    Extension(p): Extension<Principal>,
) -> Json<TokenOptions> {
    let access = app.auth.access();
    let space_names: Vec<&str> = access.spaces().map(|s| s.name.as_str()).collect();
    let roles = access
        .roles()
        .into_iter()
        .map(|(name, perms, builtin)| {
            let grantable = |s: Option<&str>| {
                p.allowed(Permission::TokensWrite, s) && missing_for_role(&p, perms, s).is_none()
            };
            GrantableRole {
                name: name.to_string(),
                builtin,
                permissions: perms.names().into_iter().map(String::from).collect(),
                global: grantable(None),
                spaces: space_names
                    .iter()
                    .filter(|s| grantable(Some(s)))
                    .map(|s| s.to_string())
                    .collect(),
            }
        })
        .collect();
    let max_days = app.cfg.tokens_max_days;
    Json(TokenOptions {
        max_days,
        default_days: crate::config::DEFAULT_TOKEN_DAYS.min(max_days),
        roles,
    })
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/tokens", get(list_tokens).post(create_token))
        .route("/tokens/options", get(token_options))
        .route("/tokens/self", get(own_token))
        .route("/tokens/{id}", get(get_token).delete(revoke_token))
}

#[derive(utoipa::OpenApi)]
#[openapi(
    tags((name = "tokens", description = "Managed admin tokens with expiry and revocation")),
    paths(create_token, list_tokens, token_options, own_token, get_token, revoke_token),
    components(schemas(TokenStatus))
)]
pub struct TokensApi;

#[cfg(test)]
mod tests {
    use super::*;
    use fastmock_core::memory::MemoryTokens;

    fn token(name: &str, value: &str, expires_in: chrono::Duration) -> ApiToken {
        let now = Utc::now();
        ApiToken {
            id: Uuid::now_v7(),
            name: name.into(),
            sha256: hash_token(value),
            role: "viewer".into(),
            spaces: vec![],
            global: true,
            description: None,
            created_by: "ops".into(),
            created_at: now,
            expires_at: now + expires_in,
            last_used_at: None,
            revoked_at: None,
            revoked: false,
            parent_id: None,
        }
    }

    #[test]
    fn managed_format() {
        assert!(looks_managed(&generate_token()));
        assert!(!looks_managed("fm_admin_token_for_access_tests_0123456789"));
        assert!(!looks_managed(&generate_token().to_uppercase()));
        assert!(!looks_managed(&format!("{}0", generate_token())));
    }

    #[tokio::test]
    async fn cache_honours_ttl_and_bounds_negatives() {
        let store = Arc::new(MemoryTokens::default());
        let svc = TokenService::new(store.clone(), Duration::from_millis(200));
        let v = generate_token();
        let t = token("a", &v, chrono::Duration::days(1));
        store.create(&t, Utc::now()).await.unwrap();
        assert!(matches!(svc.lookup(&v).await.unwrap(), Lookup::Valid(_)));
        store.revoke(t.id, Utc::now()).await.unwrap();
        assert!(matches!(svc.lookup(&v).await.unwrap(), Lookup::Valid(_)));
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(matches!(svc.lookup(&v).await.unwrap(), Lookup::Revoked(_)));
        for _ in 0..(NEGATIVE_MAX + 10) {
            assert!(matches!(
                svc.lookup(&generate_token()).await.unwrap(),
                Lookup::Unknown
            ));
        }
        let c = svc.cache.lock().unwrap();
        assert!(c.unknown.len() <= NEGATIVE_MAX);
        assert_eq!(c.known.len(), 1);
    }

    #[derive(Default)]
    struct Probe {
        inner: MemoryTokens,
        lookups: std::sync::atomic::AtomicUsize,
        down: std::sync::atomic::AtomicBool,
    }

    impl Probe {
        fn check(&self) -> anyhow::Result<()> {
            if self.down.load(std::sync::atomic::Ordering::SeqCst) {
                anyhow::bail!("store down");
            }
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl TokenStore for Probe {
        async fn create(&self, t: &ApiToken, now: DateTime<Utc>) -> anyhow::Result<bool> {
            self.inner.create(t, now).await
        }
        async fn get(&self, id: Uuid) -> anyhow::Result<Option<ApiToken>> {
            self.inner.get(id).await
        }
        async fn by_hash(&self, h: &str) -> anyhow::Result<Option<ApiToken>> {
            self.lookups
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.check()?;
            self.inner.by_hash(h).await
        }
        async fn list(&self) -> anyhow::Result<Vec<ApiToken>> {
            self.inner.list().await
        }
        async fn revoke(&self, id: Uuid, at: DateTime<Utc>) -> anyhow::Result<bool> {
            self.inner.revoke(id, at).await
        }
        async fn touch(&self, id: Uuid, at: DateTime<Utc>) -> anyhow::Result<()> {
            self.inner.touch(id, at).await
        }
        async fn count_active(&self, now: DateTime<Utc>) -> anyhow::Result<i64> {
            self.inner.count_active(now).await
        }
        async fn purge_inactive_before(&self, at: DateTime<Utc>) -> anyhow::Result<u64> {
            self.inner.purge_inactive_before(at).await
        }
        async fn has_any(&self) -> anyhow::Result<bool> {
            self.check()?;
            self.inner.has_any().await
        }
    }

    #[tokio::test]
    async fn unknown_values_skip_the_store_while_it_holds_no_tokens() {
        use std::sync::atomic::Ordering::SeqCst;
        let probe = Arc::new(Probe::default());
        let svc = TokenService::new(probe.clone(), Duration::from_millis(100));
        for _ in 0..5 {
            assert!(matches!(
                svc.lookup(&generate_token()).await.unwrap(),
                Lookup::Unknown
            ));
        }
        assert_eq!(probe.lookups.load(SeqCst), 0);
        probe.down.store(true, SeqCst);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            matches!(
                svc.lookup(&generate_token()).await.unwrap(),
                Lookup::Unknown
            ),
            "the store held no tokens, so a failed check still rejects"
        );
        probe.down.store(false, SeqCst);
        let v = generate_token();
        probe
            .create(&token("c", &v, chrono::Duration::days(1)), Utc::now())
            .await
            .unwrap();
        svc.note_created();
        assert!(matches!(svc.lookup(&v).await.unwrap(), Lookup::Valid(_)));
        assert_eq!(probe.lookups.load(SeqCst), 1);
        probe.down.store(true, SeqCst);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            svc.lookup(&generate_token()).await.is_err(),
            "with tokens in the store, an outage is reported"
        );
    }

    #[tokio::test]
    async fn expiry_is_checked_on_every_use() {
        let store = Arc::new(MemoryTokens::default());
        let svc = TokenService::new(store.clone(), Duration::from_secs(10));
        let v = generate_token();
        let t = token("b", &v, chrono::Duration::milliseconds(300));
        store.create(&t, Utc::now()).await.unwrap();
        assert!(matches!(svc.lookup(&v).await.unwrap(), Lookup::Valid(_)));
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert!(matches!(svc.lookup(&v).await.unwrap(), Lookup::Expired(_)));
        let again = token("b", &generate_token(), chrono::Duration::days(1));
        assert!(store.create(&again, Utc::now()).await.unwrap());
        assert_eq!(
            store.get(t.id).await.unwrap().unwrap().status(Utc::now()),
            ApiTokenStatus::Expired
        );
    }
}
