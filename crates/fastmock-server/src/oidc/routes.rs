//! `/__admin/auth/*`: sign-in configuration, login, callback and logout. These routes are outside
//! the admin authentication middleware.

use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::session::{self, PendingLogin, Sessions, random_secret};
use super::{Identity, Oidc, user_principal};
use crate::access::PrincipalKind;
use crate::admin::{ErrorBody, audit_event};
use crate::auth::Principal;
use crate::state::AppState;

pub const CSRF_HEADER: &str = "x-fastmock-request";
const DEFAULT_RETURN: &str = "/ui/";

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuthConfig {
    /// SSO sign-in is configured (`FASTMOCK_OIDC_ISSUER`).
    pub oidc: bool,
    /// Sign-in with an admin token is always available.
    pub token_login: bool,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogoutResponse {
    /// A session was found and deleted.
    pub logged_out: bool,
    /// The provider's end-session URL; the UI navigates there to end the IdP session too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logout_url: Option<String>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LoginQuery {
    /// Where the browser goes after sign-in: a relative path under `/ui/` (default `/ui/`).
    #[serde(rename = "return")]
    pub return_to: Option<String>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

fn error(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({"error": msg, "details": []}))).into_response()
}

fn not_configured() -> Response {
    error(StatusCode::NOT_FOUND, "SSO is not configured")
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn plain_ui_path(s: &str, raw: bool) -> bool {
    s.len() <= 2048
        && s.starts_with("/ui/")
        && !s.contains("//")
        && !s.contains('\\')
        && !s
            .chars()
            .any(|c| c.is_control() || (raw && c.is_whitespace()))
        && !s
            .split(['/', '?', '#', ';'])
            .any(|seg| seg == ".." || seg == ".")
}

/// A return path is a relative path under `/ui/`: no scheme, no `//`, no backslash, no dot
/// segments, also after percent-decoding (repeatedly).
pub fn safe_return(raw: &str) -> bool {
    let mut cur = raw.to_string();
    for i in 0..4 {
        if !plain_ui_path(&cur, i == 0) {
            return false;
        }
        match percent_decode(&cur) {
            Some(d) if d == cur => return true,
            Some(d) => cur = d,
            None => return false,
        }
    }
    false
}

fn redirect(location: &str, cookies: &[String]) -> Response {
    let mut r = StatusCode::FOUND.into_response();
    if let Ok(v) = HeaderValue::from_str(location) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    for c in cookies {
        if let Ok(v) = HeaderValue::from_str(c) {
            r.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    r
}

fn login_result(result: &'static str) {
    metrics::counter!("fastmock_oidc_logins_total", "result" => result).increment(1);
}

#[utoipa::path(
    get, path = "/__admin/auth/config", tag = "auth", operation_id = "getAuthConfig",
    summary = "Sign-in methods for the login page",
    description = "No authentication. Tells the UI whether SSO sign-in is configured.",
    responses((status = 200, description = "Sign-in methods", body = AuthConfig)),
    security(())
)]
pub async fn auth_config(State(app): State<AppState>) -> Json<AuthConfig> {
    Json(AuthConfig {
        oidc: app.oidc.is_some(),
        token_login: true,
    })
}

#[utoipa::path(
    get, path = "/__admin/auth/login", tag = "auth", operation_id = "login",
    summary = "Start SSO sign-in",
    description = "No authentication. Redirects the browser to the identity provider (authorization code flow with `state`, `nonce` and PKCE S256) and sets a short-lived login cookie that ties the callback to this browser. The pending login is kept for 10 minutes in the shared hot store.",
    params(LoginQuery),
    responses(
        (status = 302, description = "Redirect to the identity provider"),
        (status = 400, description = "`return` is not a relative path under /ui/", body = ErrorBody),
        (status = 404, description = "SSO is not configured", body = ErrorBody),
        (status = 429, description = "Too many sign-in attempts on this replica", body = ErrorBody),
        (status = 503, description = "The identity provider has not been discovered yet, or the session store is unavailable", body = ErrorBody),
    ),
    security(())
)]
pub async fn login(State(app): State<AppState>, Query(q): Query<LoginQuery>) -> Response {
    let Some(oidc) = app.oidc.clone() else {
        return not_configured();
    };
    let return_to = q.return_to.unwrap_or_else(|| DEFAULT_RETURN.into());
    if !safe_return(&return_to) {
        return error(
            StatusCode::BAD_REQUEST,
            "return must be a relative path under /ui/",
        );
    }
    let Some(provider) = oidc.provider() else {
        login_result("unavailable");
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "SSO is not available yet: the identity provider has not been reached",
        );
    };
    if !oidc.admit_login() {
        login_result("rate_limited");
        return error(StatusCode::TOO_MANY_REQUESTS, "too many sign-in attempts");
    }
    let state = random_secret();
    let browser = random_secret();
    let pending = PendingLogin {
        nonce: random_secret(),
        verifier: random_secret(),
        return_to,
        browser: session::digest(&browser),
    };
    if let Err(e) = session::put_pending(app.stores.hot.as_ref(), &state, &pending).await {
        metrics::counter!("fastmock_store_errors_total", "store" => "sessions").increment(1);
        tracing::error!(error = %e, "storing a pending login failed");
        return error(StatusCode::SERVICE_UNAVAILABLE, "session store unavailable");
    }
    let url = oidc.authorization_url(&provider, &state, &pending);
    redirect(
        url.as_str(),
        &[oidc.cookie(oidc.login_cookie_name(), &browser, session::LOGIN_TTL)],
    )
}

fn failed_login(oidc: &Oidc, code: &'static str) -> Response {
    redirect(
        &format!("{DEFAULT_RETURN}?loginError={code}"),
        &[oidc.cookie(oidc.login_cookie_name(), "", Duration::ZERO)],
    )
}

async fn audit_failure(app: &AppState, name: Option<&str>, reason: &str) {
    let p = Principal::from_bindings(name.unwrap_or("unknown"), PrincipalKind::User, &[]);
    audit_event(app, &p, "auth.login_failed", json!({"reason": reason})).await;
}

#[utoipa::path(
    get, path = "/__admin/auth/callback", tag = "auth", operation_id = "loginCallback",
    summary = "SSO sign-in callback",
    description = "The identity provider redirects here. FastMock checks `state` (single use) and the login cookie, exchanges the code with the PKCE verifier, validates the ID token (signature, `iss`, `aud`, `azp`, `exp`, `iat`, `nonce`), reads the username and groups (falling back to userinfo for groups), and creates a session when the user has at least one binding. Success redirects to the `return` path with the session cookie; failure redirects to `/ui/?loginError=<code>` where code is `invalid_state`, `idp_error`, `failed`, `no_access` or `unavailable`.",
    params(CallbackQuery),
    responses(
        (status = 302, description = "Redirect to the UI"),
        (status = 404, description = "SSO is not configured", body = ErrorBody),
    ),
    security(())
)]
pub async fn callback(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let Some(oidc) = app.oidc.clone() else {
        return not_configured();
    };
    let hot = app.stores.hot.as_ref();
    let pending = match q.state.as_deref() {
        Some(s) => match session::take_pending(hot, s).await {
            Ok(p) => p,
            Err(e) => {
                metrics::counter!("fastmock_store_errors_total", "store" => "sessions")
                    .increment(1);
                tracing::error!(error = %e, "reading a pending login failed");
                login_result("unavailable");
                return failed_login(&oidc, "unavailable");
            }
        },
        None => None,
    };
    let Some(pending) = pending else {
        login_result("invalid_state");
        tracing::warn!(target: "audit", "sign-in callback rejected: unknown, used or expired state");
        return failed_login(&oidc, "invalid_state");
    };
    let browser_ok = super::cookie_value(&headers, oidc.login_cookie_name())
        .is_some_and(|b| session::digest(b) == pending.browser);
    if !browser_ok {
        login_result("invalid_state");
        tracing::warn!(target: "audit", "sign-in callback rejected: the login cookie does not match the state");
        return failed_login(&oidc, "invalid_state");
    }
    if let Some(e) = q.error.as_deref() {
        let code: String = e
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(64)
            .collect();
        login_result("idp_error");
        audit_failure(&app, None, &format!("identity provider error {code}")).await;
        return failed_login(&oidc, "idp_error");
    }
    let Some(provider) = oidc.provider() else {
        login_result("unavailable");
        return failed_login(&oidc, "unavailable");
    };
    let Some(code) = q
        .code
        .as_deref()
        .filter(|c| !c.is_empty() && c.len() <= 4096)
    else {
        login_result("invalid_state");
        return failed_login(&oidc, "invalid_state");
    };
    let (id_token, access_token) = match oidc.exchange(&provider, code, &pending.verifier).await {
        Ok(t) => t,
        Err(e) => {
            login_result("token_error");
            tracing::warn!(error = %e, "code exchange failed");
            audit_failure(&app, None, "code exchange failed").await;
            return failed_login(&oidc, "failed");
        }
    };
    let claims = match oidc
        .validate(
            &id_token,
            &oidc.settings.client_id,
            Some(&pending.nonce),
            true,
        )
        .await
    {
        Ok(c) => c,
        Err(e) => {
            login_result("token_error");
            metrics::counter!("fastmock_admin_auth_failures_total", "reason" => "jwt").increment(1);
            tracing::warn!(error = %e, "ID token rejected");
            audit_failure(&app, None, &format!("ID token rejected: {e}")).await;
            return failed_login(&oidc, "failed");
        }
    };
    let (mut name, mut groups) = oidc.identity(&claims);
    if (name.is_none() || groups.is_none())
        && provider.userinfo.is_some()
        && let Some(at) = access_token.as_deref()
    {
        let sub = claims
            .get("sub")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        match oidc.userinfo(&provider, at, sub).await {
            Ok(info) => {
                let (n, g) = oidc.identity(&info);
                name = name.or(n);
                groups = groups.or(g);
            }
            Err(e) => tracing::warn!(error = %e, "userinfo request failed"),
        }
    }
    let Some(name) = name else {
        login_result("token_error");
        audit_failure(
            &app,
            None,
            &format!("no {} claim", oidc.settings.username_claim),
        )
        .await;
        return failed_login(&oidc, "failed");
    };
    let identity = Identity {
        name,
        groups: groups.unwrap_or_default(),
    };
    let principal = match user_principal(&app.auth, identity) {
        Ok(p) => p,
        Err(id) => {
            login_result("no_binding");
            audit_failure(
                &app,
                Some(&id.name),
                "no binding for the user or its groups",
            )
            .await;
            return failed_login(&oidc, "no_access");
        }
    };
    let sessions = Sessions {
        hot,
        life: oidc.lifetimes(),
    };
    let groups = principal.groups.clone().unwrap_or_default();
    let id = match sessions
        .create(&principal.name, groups.clone(), Some(id_token))
        .await
    {
        Ok(id) => id,
        Err(e) => {
            metrics::counter!("fastmock_store_errors_total", "store" => "sessions").increment(1);
            tracing::error!(error = %e, "storing a session failed");
            login_result("unavailable");
            return failed_login(&oidc, "unavailable");
        }
    };
    login_result("success");
    audit_event(&app, &principal, "auth.login", json!({"groups": groups})).await;
    redirect(
        &pending.return_to,
        &[
            oidc.cookie(oidc.session_cookie_name(), &id, oidc.settings.session_ttl),
            oidc.cookie(oidc.login_cookie_name(), "", Duration::ZERO),
        ],
    )
}

#[utoipa::path(
    post, path = "/__admin/auth/logout", tag = "auth", operation_id = "logout",
    summary = "End the SSO session",
    description = "Deletes the session of the cookie and clears the cookie. Needs the `X-FastMock-Request: 1` header. When the provider has an `end_session_endpoint`, `logoutUrl` tells the UI where to send the browser to end the provider session too; it carries `client_id`, `post_logout_redirect_uri` and, when the session holds one, the sign-in's ID token as `id_token_hint`.",
    params(("X-FastMock-Request" = String, Header, description = "Must be `1`")),
    responses(
        (status = 200, description = "Logged out", body = LogoutResponse),
        (status = 403, description = "Missing X-FastMock-Request header or origin not allowed", body = ErrorBody),
        (status = 404, description = "SSO is not configured", body = ErrorBody),
        (status = 503, description = "Session store unavailable", body = ErrorBody),
    ),
    security(("session" = []))
)]
pub async fn logout(State(app): State<AppState>, headers: HeaderMap) -> Response {
    let Some(oidc) = app.oidc.clone() else {
        return not_configured();
    };
    if !app.auth.origin_allowed(&headers) {
        metrics::counter!("fastmock_admin_auth_failures_total", "reason" => "origin").increment(1);
        return error(StatusCode::FORBIDDEN, "origin not allowed");
    }
    if !csrf_header_ok(&headers) {
        metrics::counter!("fastmock_admin_auth_failures_total", "reason" => "csrf").increment(1);
        return (
            StatusCode::FORBIDDEN,
            Json(
                json!({"error": "forbidden", "details": ["missing X-FastMock-Request: 1 header"]}),
            ),
        )
            .into_response();
    }
    let mut logged_out = false;
    let mut id_token_hint = None;
    if let Some(id) = oidc.session_id(&headers) {
        let sessions = Sessions {
            hot: app.stores.hot.as_ref(),
            life: oidc.lifetimes(),
        };
        match sessions.delete(id).await {
            Ok(Some(r)) => {
                logged_out = true;
                id_token_hint = r.id_token.clone();
                let p = Principal::from_bindings(&r.name, PrincipalKind::User, &[]);
                audit_event(&app, &p, "auth.logout", json!({})).await;
            }
            Ok(None) => {}
            Err(e) => {
                metrics::counter!("fastmock_store_errors_total", "store" => "sessions")
                    .increment(1);
                tracing::error!(error = %e, "deleting a session failed");
                return error(StatusCode::SERVICE_UNAVAILABLE, "session store unavailable");
            }
        }
    }
    let mut r = Json(LogoutResponse {
        logged_out,
        logout_url: oidc.logout_url(id_token_hint.as_deref()),
    })
    .into_response();
    if let Ok(v) =
        HeaderValue::from_str(&oidc.cookie(oidc.session_cookie_name(), "", Duration::ZERO))
    {
        r.headers_mut().insert(header::SET_COOKIE, v);
    }
    r
}

pub fn csrf_header_ok(headers: &HeaderMap) -> bool {
    headers
        .get(CSRF_HEADER)
        .is_some_and(|v| v.as_bytes() == b"1")
}

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/__admin/auth/config", get(auth_config))
        .route("/__admin/auth/login", get(login))
        .route("/__admin/auth/callback", get(callback))
        .route("/__admin/auth/logout", post(logout))
}

#[derive(utoipa::OpenApi)]
#[openapi(
    tags((name = "auth", description = "SSO sign-in for the UI (OIDC, backend for frontend)")),
    paths(auth_config, login, callback, logout),
    components(schemas(AuthConfig, LogoutResponse))
)]
pub struct AuthApi;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_paths() {
        for ok in [
            "/ui/",
            "/ui/stubs",
            "/ui/stubs/0190e0d0-0000?space=payments",
            "/ui/requests#top",
            "/ui/a%20b",
        ] {
            assert!(safe_return(ok), "{ok}");
        }
        for bad in [
            "",
            "/ui",
            "/",
            "//evil.example.com",
            "//evil.example.com/ui/",
            "https://evil.example.com/ui/",
            "/ui//evil.example.com",
            "/ui/\\evil.example.com",
            "/\\evil.example.com",
            "/ui/%2F%2Fevil.example.com",
            "/ui/%252F%252Fevil.example.com",
            "/ui/%25252F%25252Fevil",
            "%2Fui%2F",
            "/ui/%5Cevil",
            "/ui/../__admin/stubs",
            "/ui/%2e%2e/__admin",
            "/ui/a\nb",
            "/ui/a b",
            "/ui/%0d%0aSet-Cookie:x",
            "javascript:alert(1)",
            "/ui/%zz",
            "\\\\evil.example.com",
            "/ui/%E0%A4%A",
        ] {
            assert!(!safe_return(bad), "{bad}");
        }
        assert!(!safe_return(&format!("/ui/{}", "a".repeat(3000))));
    }
}
