use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, bail};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::access::{
    AccessConfig, BUILTIN_ROLES, Binding, BindingScope, Grants, PermSet, Permission, PrincipalKind,
    ScopeKind, editor_permissions, legacy_fallback, legacy_gate, minimal_role, route_rule,
};
use crate::state::AppState;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Editor,
    Admin,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Editor => "editor",
            Role::Admin => "admin",
        }
    }

    fn effective(global: PermSet) -> Self {
        if global.includes(PermSet::all()) {
            Role::Admin
        } else if global.includes(editor_permissions()) {
            Role::Editor
        } else {
            Role::Viewer
        }
    }
}

#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Principal {
    pub name: String,
    /// The highest built-in role whose permissions the principal holds everywhere (`viewer` when it holds less); kept for older clients, decide with `permissions`.
    pub role: Role,
    pub kind: PrincipalKind,
    #[schema(value_type = crate::access::PermissionsView)]
    pub permissions: Grants,
    /// OIDC users: the group names from the identity provider (`kind: user` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<String>>,
    /// Role names of the bindings, for the audit log.
    #[serde(skip)]
    pub role_label: String,
    /// Every binding is a global binding of a built-in role: denials name the role, as before.
    #[serde(skip)]
    pub legacy: bool,
    /// Id of the managed token the request authenticated with.
    #[serde(skip)]
    pub token_id: Option<uuid::Uuid>,
}

impl Principal {
    pub fn from_bindings(name: &str, kind: PrincipalKind, bindings: &[Binding]) -> Self {
        let grants = Grants::from_bindings(bindings);
        let mut labels: Vec<&str> = Vec::new();
        for b in bindings {
            if !labels.contains(&b.role.as_str()) {
                labels.push(&b.role);
            }
        }
        Principal {
            name: name.to_string(),
            role: Role::effective(grants.global()),
            kind,
            legacy: !bindings.is_empty()
                && bindings.iter().all(|b| {
                    b.scope == BindingScope::Global && BUILTIN_ROLES.contains(&b.role.as_str())
                }),
            role_label: labels.join(","),
            permissions: grants,
            groups: None,
            token_id: None,
        }
    }

    pub fn anonymous() -> Self {
        let mut p = Self::from_bindings(
            "anonymous",
            PrincipalKind::Anonymous,
            &[Binding::global("admin", PermSet::all())],
        );
        p.permissions = Grants::admin();
        p
    }

    pub fn allowed(&self, perm: Permission, space: Option<&str>) -> bool {
        self.permissions.allowed(perm, space)
    }

    /// Text and `required` value of a denial: the lowest built-in role for principals that only
    /// hold global built-in roles (as before custom roles existed), the permission otherwise.
    pub fn denial(&self, perm: Permission, space: Option<&str>) -> (String, String) {
        if self.legacy {
            let r = minimal_role(perm);
            (format!("requires role {r}"), r.to_string())
        } else {
            let place = space.map(|s| format!(" in space {s}")).unwrap_or_default();
            (
                format!("requires permission {perm}{place}"),
                perm.name().to_string(),
            )
        }
    }

    pub fn count_denial(&self) {
        let reason = if self.legacy { "role" } else { "permission" };
        metrics::counter!("fastmock_admin_auth_failures_total", "reason" => reason).increment(1);
    }
}

struct TokenEntry {
    hash: [u8; 32],
    bindings: Vec<Binding>,
    principal: Principal,
}

pub struct Authenticator {
    tokens: Vec<TokenEntry>,
    sso: bool,
    disabled: bool,
    allowed_origins: Vec<String>,
    access: Arc<AccessConfig>,
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn generate_token() -> String {
    let bytes: [u8; 32] = rand::random();
    format!("fm_{}", hex::encode(bytes))
}

struct RawEntry {
    name: String,
    role: Option<String>,
    hash: [u8; 32],
}

fn parse_entry(line: &str) -> anyhow::Result<RawEntry> {
    let parts: Vec<&str> = line.trim().split(':').collect();
    if parts.len() != 3 {
        bail!("expected name:role:sha256hex");
    }
    let name = parts[0].trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
    {
        bail!("invalid token name `{name}`");
    }
    let raw = hex::decode(parts[2].trim()).context("hash is not hex")?;
    let hash: [u8; 32] = raw
        .try_into()
        .map_err(|_| anyhow::anyhow!("hash must be a SHA-256 (64 hex chars)"))?;
    let role = parts[1].trim();
    Ok(RawEntry {
        name: name.to_string(),
        role: (!role.is_empty()).then(|| role.to_string()),
        hash,
    })
}

impl Authenticator {
    pub fn new(
        entries: &[String],
        file: Option<&Path>,
        disabled: bool,
        allowed_origins: Vec<String>,
    ) -> anyhow::Result<Self> {
        Self::with_access(
            entries,
            file,
            disabled,
            allowed_origins,
            Arc::new(AccessConfig::default()),
            false,
        )
    }

    pub fn with_access(
        entries: &[String],
        file: Option<&Path>,
        disabled: bool,
        allowed_origins: Vec<String>,
        access: Arc<AccessConfig>,
        sso: bool,
    ) -> anyhow::Result<Self> {
        let mut lines: Vec<String> = entries.to_vec();
        if let Some(f) = file {
            let content =
                std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?;
            lines.extend(
                content
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .map(String::from),
            );
        }
        let raw = lines
            .iter()
            .enumerate()
            .map(|(i, l)| parse_entry(l).with_context(|| format!("admin token entry #{}", i + 1)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        if raw.is_empty() && !disabled && !sso {
            bail!(
                "no admin tokens configured: set FASTMOCK_ADMIN_TOKENS or FASTMOCK_ADMIN_TOKENS_FILE, configure OIDC sign-in (FASTMOCK_OIDC_ISSUER), or set FASTMOCK_AUTH_DISABLED=true for local development only"
            );
        }
        if disabled && sso {
            bail!(
                "FASTMOCK_AUTH_DISABLED=true cannot be combined with OIDC sign-in (FASTMOCK_OIDC_ISSUER)"
            );
        }
        let mut tokens = Vec::with_capacity(raw.len());
        for (i, e) in raw.into_iter().enumerate() {
            let at = format!("admin token entry #{} ({})", i + 1, e.name);
            let mut bindings = Vec::new();
            if let Some(role) = &e.role {
                let perms = access
                    .role(role)
                    .ok_or_else(|| anyhow::anyhow!("{at}: unknown role `{role}`"))?;
                bindings.push(Binding::global(role, perms));
            }
            bindings.extend(access.bindings_for(PrincipalKind::Token, &e.name, &[]));
            if bindings.is_empty() {
                bail!(
                    "{at}: the role is empty and the access file has no binding for token:{}",
                    e.name
                );
            }
            tokens.push(TokenEntry {
                hash: e.hash,
                principal: Principal::from_bindings(&e.name, PrincipalKind::Token, &bindings),
                bindings,
            });
        }
        if !disabled {
            for (i, name) in access.token_subjects() {
                if !tokens.iter().any(|t| t.principal.name == name) {
                    bail!(
                        "{}: bindings[{i}] (token:{name}): no token named {name} in FASTMOCK_ADMIN_TOKENS or FASTMOCK_ADMIN_TOKENS_FILE",
                        access.origin()
                    );
                }
            }
        }
        Ok(Self {
            tokens,
            sso,
            disabled,
            allowed_origins,
            access,
        })
    }

    pub fn disabled(&self) -> bool {
        self.disabled
    }

    pub fn access(&self) -> &AccessConfig {
        &self.access
    }

    /// Bindings of a principal: for static tokens the global binding of their entry plus `token:`
    /// bindings; for users `user:` and `group:` bindings of the access file.
    pub fn bindings_for(&self, kind: PrincipalKind, name: &str, groups: &[String]) -> Vec<Binding> {
        match kind {
            PrincipalKind::Token => self
                .tokens
                .iter()
                .find(|t| t.principal.name == name)
                .map(|t| t.bindings.clone())
                .unwrap_or_default(),
            _ => self.access.bindings_for(kind, name, groups),
        }
    }

    /// The principal for a kind, name and groups, or `None` when no binding applies.
    pub fn principal_for(
        &self,
        kind: PrincipalKind,
        name: &str,
        groups: &[String],
    ) -> Option<Principal> {
        let b = self.bindings_for(kind, name, groups);
        (!b.is_empty()).then(|| Principal::from_bindings(name, kind, &b))
    }

    /// True when a static token entry has this name; managed tokens must not reuse it.
    pub fn is_static_name(&self, name: &str) -> bool {
        self.tokens.iter().any(|t| t.principal.name == name)
    }

    pub fn authenticate(&self, headers: &HeaderMap) -> Option<Principal> {
        if self.disabled {
            return Some(Principal::anonymous());
        }
        let token = bearer(headers)?;
        let presented: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let mut found: Option<&TokenEntry> = None;
        for t in &self.tokens {
            if bool::from(t.hash.ct_eq(&presented)) {
                found = Some(t);
            }
        }
        found.map(|t| t.principal.clone())
    }

    pub fn sso(&self) -> bool {
        self.sso
    }

    pub fn origin_allowed(&self, headers: &HeaderMap) -> bool {
        match headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()) {
            None => true,
            Some(o) => {
                if self.allowed_origins.iter().any(|a| a == o) {
                    return true;
                }
                !self.disabled
                    && headers
                        .get(header::HOST)
                        .and_then(|h| h.to_str().ok())
                        .is_some_and(|h| {
                            o.strip_prefix("http://")
                                .or_else(|| o.strip_prefix("https://"))
                                == Some(h)
                        })
            }
        }
    }
}

/// Longest static or managed token accepted, in bytes; longer values are rejected without hashing.
const MAX_STATIC_TOKEN_BYTES: usize = 512;

/// The bearer value of the `Authorization` header (at most 512 bytes).
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    bearer_up_to(headers, MAX_STATIC_TOKEN_BYTES)
}

fn bearer_up_to(headers: &HeaderMap, max: usize) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?
        .trim();
    (!token.is_empty() && token.len() <= max).then_some(token)
}

enum Rejection {
    Token,
    Expired,
    Unavailable(&'static str),
    Jwt,
    Session,
    Csrf,
    /// An OIDC user without any binding.
    NoBinding(String),
}

/// An `Authorization` header decides alone: static tokens first (constant time over the list), then
/// managed `fm_` tokens by hash through the cache, then OIDC JWTs when `FASTMOCK_OIDC_API_AUDIENCE`
/// is set. Without the header, the OIDC session cookie; requests other than GET and HEAD then need
/// the `X-FastMock-Request: 1` header.
async fn principal_of(
    app: &AppState,
    headers: &HeaderMap,
    method: &axum::http::Method,
    path: &str,
) -> Result<Principal, Rejection> {
    if let Some(p) = app.auth.authenticate(headers) {
        return Ok(p);
    }
    if !headers.contains_key(header::AUTHORIZATION) {
        return match &app.oidc {
            Some(o) => session_principal(app, o, headers, method).await,
            None => Err(Rejection::Token),
        };
    }
    let Some(value) = bearer_up_to(headers, usize::MAX) else {
        return Err(Rejection::Token);
    };
    if let Some(o) = app.oidc.as_ref().filter(|o| o.api_enabled())
        && crate::oidc::jwt::jwt_shaped(value)
    {
        return match o.bearer_principal(&app.auth, value).await {
            Ok(Ok(p)) => Ok(p),
            Ok(Err(id)) => Err(Rejection::NoBinding(id.name)),
            Err(e) => {
                tracing::warn!(target: "audit", path = %path, reason = %e, "admin request rejected: invalid bearer JWT");
                Err(Rejection::Jwt)
            }
        };
    }
    if value.len() > MAX_STATIC_TOKEN_BYTES || !crate::tokens::looks_managed(value) {
        return Err(Rejection::Token);
    }
    match app.tokens.lookup(value).await {
        Ok(crate::tokens::Lookup::Valid(t)) => match crate::tokens::principal_of(&app.auth, &t) {
            Some(p) => {
                app.tokens.touch(t.id);
                Ok(p)
            }
            None => {
                tracing::warn!(target: "audit", token = %t.name, role = %t.role, path = %path, "admin request rejected: the role of the managed token is not defined");
                Err(Rejection::Token)
            }
        },
        Ok(crate::tokens::Lookup::Expired(t)) => {
            tracing::warn!(target: "audit", token = %t.name, path = %path, "admin request rejected: token expired");
            Err(Rejection::Expired)
        }
        Ok(crate::tokens::Lookup::Revoked(t)) => {
            tracing::warn!(target: "audit", token = %t.name, path = %path, "admin request rejected: token revoked");
            Err(Rejection::Token)
        }
        Ok(crate::tokens::Lookup::Unknown) => Err(Rejection::Token),
        Err(e) => {
            metrics::counter!("fastmock_store_errors_total", "store" => "tokens").increment(1);
            tracing::error!(error = %e, "looking up a managed token failed");
            Err(Rejection::Unavailable("token store unavailable"))
        }
    }
}

async fn session_principal(
    app: &AppState,
    oidc: &crate::oidc::Oidc,
    headers: &HeaderMap,
    method: &axum::http::Method,
) -> Result<Principal, Rejection> {
    let Some(id) = oidc.session_id(headers) else {
        return Err(Rejection::Token);
    };
    let sessions = crate::oidc::session::Sessions {
        hot: app.stores.hot.as_ref(),
        life: oidc.lifetimes(),
    };
    let record = match sessions.get(id).await {
        Ok(crate::oidc::session::Found::Live(r)) => r,
        Ok(crate::oidc::session::Found::Missing) => return Err(Rejection::Session),
        Err(e) => {
            metrics::counter!("fastmock_store_errors_total", "store" => "sessions").increment(1);
            tracing::error!(error = %e, "looking up a session failed");
            return Err(Rejection::Unavailable("session store unavailable"));
        }
    };
    if method != axum::http::Method::GET
        && method != axum::http::Method::HEAD
        && !crate::oidc::routes::csrf_header_ok(headers)
    {
        return Err(Rejection::Csrf);
    }
    let identity = crate::oidc::Identity {
        name: record.name,
        groups: record.groups,
    };
    crate::oidc::user_principal(&app.auth, identity).map_err(|id| Rejection::NoBinding(id.name))
}

fn full_path(req: &Request) -> String {
    req.extensions()
        .get::<axum::extract::OriginalUri>()
        .map(|u| u.0.path().to_string())
        .unwrap_or_else(|| req.uri().path().to_string())
}

pub async fn admin_auth(State(app): State<AppState>, mut req: Request, next: Next) -> Response {
    if !app.auth.origin_allowed(req.headers()) {
        metrics::counter!("fastmock_admin_auth_failures_total", "reason" => "origin").increment(1);
        let body = if app.legacy_setup() {
            serde_json::json!({"error": "origin not allowed", "details": []})
        } else {
            serde_json::json!({"error": "forbidden", "details": ["origin not allowed"]})
        };
        return (StatusCode::FORBIDDEN, axum::Json(body)).into_response();
    }
    let path = full_path(&req);
    let principal = match principal_of(&app, req.headers(), req.method(), &path).await {
        Ok(p) => p,
        Err(Rejection::Unavailable(what)) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({"error": what, "details": []})),
            )
                .into_response();
        }
        Err(Rejection::Jwt) => {
            metrics::counter!("fastmock_admin_auth_failures_total", "reason" => "jwt").increment(1);
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer error=\"invalid_token\"")],
                axum::Json(serde_json::json!({"error": "unauthorized", "details": ["invalid bearer token"]})),
            )
                .into_response();
        }
        Err(Rejection::Session) => {
            metrics::counter!("fastmock_admin_auth_failures_total", "reason" => "session")
                .increment(1);
            let mut r = (
                StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({"error": "unauthorized", "details": ["session expired or unknown: sign in again"]})),
            )
                .into_response();
            if let Some(o) = &app.oidc
                && let Ok(v) = header::HeaderValue::from_str(&o.cookie(
                    o.session_cookie_name(),
                    "",
                    std::time::Duration::ZERO,
                ))
            {
                r.headers_mut().insert(header::SET_COOKIE, v);
            }
            return r;
        }
        Err(Rejection::Csrf) => {
            metrics::counter!("fastmock_admin_auth_failures_total", "reason" => "csrf")
                .increment(1);
            tracing::warn!(target: "audit", path = %path, "admin request rejected: cookie-authenticated change without the X-FastMock-Request header");
            return (
                StatusCode::FORBIDDEN,
                axum::Json(serde_json::json!({"error": "forbidden", "details": ["missing X-FastMock-Request: 1 header"]})),
            )
                .into_response();
        }
        Err(Rejection::NoBinding(name)) => {
            metrics::counter!("fastmock_admin_auth_failures_total", "reason" => "permission")
                .increment(1);
            tracing::warn!(target: "audit", principal = %name, kind = "user", path = %path, "admin request rejected: no binding for the user or its groups");
            let required = route_rule(req.method(), &path).map_or("admin", |(p, _)| p.name());
            return (
                StatusCode::FORBIDDEN,
                axum::Json(serde_json::json!({
                    "error": "forbidden",
                    "details": [format!("user {name} has no access: the access file binds neither user:{name} nor any of the user's groups")],
                    "required": required
                })),
            )
                .into_response();
        }
        Err(Rejection::Expired) => {
            metrics::counter!("fastmock_admin_auth_failures_total", "reason" => "expired")
                .increment(1);
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer error=\"invalid_token\"")],
                axum::Json(
                    serde_json::json!({"error": "unauthorized", "details": ["token expired"]}),
                ),
            )
                .into_response();
        }
        Err(Rejection::Token) => {
            metrics::counter!("fastmock_admin_auth_failures_total", "reason" => "token")
                .increment(1);
            tracing::warn!(target: "audit", path = %path, "admin request rejected: missing or invalid token");
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                axum::Json(serde_json::json!({"error": "unauthorized", "details": []})),
            )
                .into_response();
        }
    };
    let legacy_setup = app.legacy_setup();
    let rule = if legacy_setup {
        route_rule(req.method(), &path)
            .or_else(|| Some(legacy_fallback(req.method())))
            .map(legacy_gate)
    } else if purges_stubs(&req, &path) {
        Some((Permission::StubsPurge, ScopeKind::Global))
    } else {
        route_rule(req.method(), &path)
    };
    let ok = match rule {
        Some((perm, ScopeKind::Space)) => principal.permissions.anywhere(perm),
        Some((perm, ScopeKind::Global)) => principal.permissions.global_has(perm),
        None => principal.permissions.global().includes(PermSet::all()),
    };
    if !ok {
        principal.count_denial();
        let needed = rule.map_or("admin", |(p, _)| p.name());
        tracing::warn!(target: "audit", principal = %principal.name, kind = principal.kind.as_str(), path = %path, permission = %needed, "admin request rejected: permission missing");
        let (detail, required) = match rule {
            Some((perm, _)) => principal.denial(perm, None),
            None => ("requires role admin".to_string(), "admin".to_string()),
        };
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "error": "forbidden",
                "details": [detail],
                "required": required
            })),
        )
            .into_response();
    }
    req.extensions_mut().insert(principal);
    let mut res = next.run(req).await;
    if !legacy_setup
        && res.status() == StatusCode::FORBIDDEN
        && let Some(d) = res.extensions_mut().remove::<crate::admin::Denial>()
    {
        return crate::admin::forbidden(d.message, d.required);
    }
    res
}

/// `DELETE /stubs` without `tag` deletes every stub and needs `stubs:purge`.
fn purges_stubs(req: &Request, path: &str) -> bool {
    req.method() == axum::http::Method::DELETE
        && matches!(
            path.strip_prefix("/__admin")
                .unwrap_or(path)
                .trim_end_matches('/'),
            "/stubs"
        )
        && !req
            .uri()
            .query()
            .unwrap_or_default()
            .split('&')
            .any(|kv| kv.split('=').next() == Some("tag"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn auth() -> Authenticator {
        let viewer = format!("ci:viewer:{}", hash_token("viewer-token"));
        let admin = format!("ops:admin:{}", hash_token("admin-token"));
        Authenticator::new(
            &[viewer, admin],
            None,
            false,
            vec!["https://ui.example.com".into()],
        )
        .unwrap()
    }

    fn headers(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_str(v).unwrap());
        h
    }

    #[test]
    fn authenticates_by_hash() {
        let a = auth();
        assert_eq!(
            a.authenticate(&headers("Bearer viewer-token"))
                .unwrap()
                .role,
            Role::Viewer
        );
        assert_eq!(
            a.authenticate(&headers("Bearer admin-token")).unwrap().name,
            "ops"
        );
        assert!(a.authenticate(&headers("Bearer wrong")).is_none());
        assert!(a.authenticate(&headers("Basic dmlld2VyLXRva2Vu")).is_none());
        assert!(a.authenticate(&HeaderMap::new()).is_none());
    }

    #[test]
    fn rejects_bad_config() {
        assert!(Authenticator::new(&[], None, false, vec![]).is_err());
        assert!(Authenticator::new(&["a:root:00".into()], None, false, vec![]).is_err());
        assert!(Authenticator::new(&["a:admin:zz".into()], None, false, vec![]).is_err());
        assert!(
            Authenticator::new(
                &["bad name:admin:".to_string() + &hash_token("x")],
                None,
                false,
                vec![]
            )
            .is_err()
        );
        assert!(Authenticator::new(&[], None, true, vec![]).is_ok());
        let access = || Arc::new(AccessConfig::default());
        assert!(Authenticator::with_access(&[], None, false, vec![], access(), true).is_ok());
        let both = Authenticator::with_access(&[], None, true, vec![], access(), true);
        assert!(
            both.err()
                .is_some_and(|e| e.to_string().contains("cannot be combined"))
        );
    }

    #[test]
    fn origin_check() {
        let a = auth();
        let mut h = HeaderMap::new();
        assert!(a.origin_allowed(&h));
        h.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example.com"),
        );
        assert!(!a.origin_allowed(&h));
        h.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://ui.example.com"),
        );
        assert!(a.origin_allowed(&h));
    }

    #[test]
    fn static_tokens_get_global_bindings_and_file_bindings() {
        let access = AccessConfig::parse(
            "roles: [{name: reader, permissions: [stubs:read]}]\nspaces: [{name: billing}]\nbindings: [{subject: 'token:ci', role: editor, spaces: [billing]}]",
        )
        .unwrap();
        let a = Authenticator::with_access(
            &[
                format!("ci::{}", hash_token("ci-token")),
                format!("dash:reader:{}", hash_token("dash-token")),
                format!("ops:admin:{}", hash_token("ops-token")),
            ],
            None,
            false,
            vec![],
            Arc::new(access.clone()),
            false,
        )
        .unwrap();
        let ci = a.authenticate(&headers("Bearer ci-token")).unwrap();
        assert!(ci.allowed(Permission::StubsWrite, Some("billing")));
        assert!(!ci.allowed(Permission::StubsRead, None));
        assert!(!ci.legacy);
        assert_eq!(ci.kind, PrincipalKind::Token);
        assert_eq!(
            ci.denial(Permission::StubsWrite, Some("x")).1,
            "stubs:write"
        );
        let dash = a.authenticate(&headers("Bearer dash-token")).unwrap();
        assert!(dash.allowed(Permission::StubsRead, Some("billing")));
        assert!(!dash.allowed(Permission::JournalRead, None));
        assert_eq!(dash.role, Role::Viewer);
        let ops = a.authenticate(&headers("Bearer ops-token")).unwrap();
        assert!(ops.legacy);
        assert_eq!(ops.role, Role::Admin);
        assert_eq!(ops.role_label, "admin");
        assert_eq!(a.bindings_for(PrincipalKind::Token, "ci", &[]).len(), 1);
        assert_eq!(a.bindings_for(PrincipalKind::Token, "ops", &[]).len(), 1);
        assert!(
            a.principal_for(PrincipalKind::User, "nobody", &[])
                .is_none()
        );
        let err = |entries: &[String]| {
            Authenticator::with_access(
                entries,
                None,
                false,
                vec![],
                Arc::new(access.clone()),
                false,
            )
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default()
        };
        assert!(err(&[format!("x::{}", hash_token("x"))]).contains("admin token entry #1 (x)"));
        assert!(err(&[format!("x:nobody:{}", hash_token("x"))]).contains("unknown role `nobody`"));
        assert_eq!(
            err(&[format!("y:viewer:{}", hash_token("y"))]),
            "access file: bindings[0] (token:ci): no token named ci in FASTMOCK_ADMIN_TOKENS or FASTMOCK_ADMIN_TOKENS_FILE"
        );
    }

    #[test]
    fn legacy_denials_name_the_lowest_role() {
        let a = auth();
        let v = a.authenticate(&headers("Bearer viewer-token")).unwrap();
        assert!(v.legacy);
        assert_eq!(
            v.denial(Permission::StubsWrite, None),
            ("requires role editor".to_string(), "editor".to_string())
        );
        assert_eq!(v.denial(Permission::AuditRead, None).1, "admin");
        let anon = Principal::anonymous();
        assert!(anon.allowed(Permission::SystemReset, None));
        assert_eq!(anon.kind, PrincipalKind::Anonymous);
    }

    #[test]
    fn generated_tokens_are_long() {
        let t = generate_token();
        assert!(t.len() > 60);
        assert_ne!(t, generate_token());
    }
}
