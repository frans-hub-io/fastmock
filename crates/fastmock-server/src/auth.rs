use std::path::Path;

use anyhow::{Context, bail};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::state::AppState;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Editor,
    Admin,
}

impl Role {
    fn parse(s: &str) -> anyhow::Result<Self> {
        Ok(match s {
            "viewer" => Role::Viewer,
            "editor" => Role::Editor,
            "admin" => Role::Admin,
            other => bail!("unknown role `{other}`"),
        })
    }
}

#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct Principal {
    pub name: String,
    pub role: Role,
}

struct TokenEntry {
    name: String,
    role: Role,
    hash: [u8; 32],
}

pub struct Authenticator {
    tokens: Vec<TokenEntry>,
    disabled: bool,
    allowed_origins: Vec<String>,
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn generate_token() -> String {
    let bytes: [u8; 32] = rand::random();
    format!("fm_{}", hex::encode(bytes))
}

fn parse_entry(line: &str) -> anyhow::Result<TokenEntry> {
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
    Ok(TokenEntry {
        name: name.to_string(),
        role: Role::parse(parts[1].trim())?,
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
        let tokens = lines
            .iter()
            .enumerate()
            .map(|(i, l)| parse_entry(l).with_context(|| format!("admin token entry #{}", i + 1)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        if tokens.is_empty() && !disabled {
            bail!(
                "no admin tokens configured: set FASTMOCK_ADMIN_TOKENS or FASTMOCK_ADMIN_TOKENS_FILE (or FASTMOCK_AUTH_DISABLED=true for local development only)"
            );
        }
        Ok(Self {
            tokens,
            disabled,
            allowed_origins,
        })
    }

    pub fn disabled(&self) -> bool {
        self.disabled
    }

    pub fn authenticate(&self, headers: &HeaderMap) -> Option<Principal> {
        if self.disabled {
            return Some(Principal {
                name: "anonymous".into(),
                role: Role::Admin,
            });
        }
        let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
        let token = value
            .strip_prefix("Bearer ")
            .or_else(|| value.strip_prefix("bearer "))?
            .trim();
        if token.is_empty() || token.len() > 512 {
            return None;
        }
        let presented: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let mut found: Option<&TokenEntry> = None;
        for t in &self.tokens {
            if bool::from(t.hash.ct_eq(&presented)) {
                found = Some(t);
            }
        }
        found.map(|t| Principal {
            name: t.name.clone(),
            role: t.role,
        })
    }

    fn origin_allowed(&self, headers: &HeaderMap) -> bool {
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

pub const VIEWER_POSTS: &[&str] = &[
    "/__admin/requests/count",
    "/__admin/requests/near-misses",
    "/__admin/requests/find",
    "/__admin/verify",
    "/__admin/verify/sequence",
    "/__admin/stubs/find-by-metadata",
    "/__admin/stubs/validate",
];

pub fn required_role(method: &axum::http::Method, path: &str) -> Role {
    let path = path.strip_suffix('/').unwrap_or(path);
    let read_only_post = method == axum::http::Method::POST && VIEWER_POSTS.contains(&path);
    if method == axum::http::Method::GET || method == axum::http::Method::HEAD || read_only_post {
        Role::Viewer
    } else {
        Role::Editor
    }
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
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"error": "origin not allowed", "details": []})),
        )
            .into_response();
    }
    let Some(principal) = app.auth.authenticate(req.headers()) else {
        metrics::counter!("fastmock_admin_auth_failures_total", "reason" => "token").increment(1);
        tracing::warn!(target: "audit", path = %full_path(&req), "admin request rejected: missing or invalid token");
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            axum::Json(serde_json::json!({"error": "unauthorized", "details": []})),
        )
            .into_response();
    };
    let need = required_role(req.method(), &full_path(&req));
    if principal.role < need {
        metrics::counter!("fastmock_admin_auth_failures_total", "reason" => "role").increment(1);
        tracing::warn!(target: "audit", principal = %principal.name, path = %full_path(&req), "admin request rejected: role too low");
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "error": "forbidden",
                "details": [format!("requires role {}", format!("{need:?}").to_lowercase())],
                "required": need
            })),
        )
            .into_response();
    }
    req.extensions_mut().insert(principal);
    next.run(req).await
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
    fn read_only_posts_need_viewer_only() {
        use axum::http::Method;
        for p in VIEWER_POSTS {
            assert_eq!(required_role(&Method::POST, p), Role::Viewer, "{p}");
        }
        assert_eq!(required_role(&Method::GET, "/__admin/stubs"), Role::Viewer);
        for p in [
            "/__admin/requests/remove",
            "/__admin/stubs/remove-by-metadata",
            "/__admin/recordings/snapshot",
            "/__admin/import",
            "/__admin/plugins/x/verify",
            "/__admin/plugins/x/requests/count",
        ] {
            assert_eq!(required_role(&Method::POST, p), Role::Editor, "{p}");
        }
        assert_eq!(
            required_role(&Method::PATCH, "/__admin/stubs/1"),
            Role::Editor
        );
        assert_eq!(
            required_role(&Method::PUT, "/__admin/files/a"),
            Role::Editor
        );
    }

    #[test]
    fn generated_tokens_are_long() {
        let t = generate_token();
        assert!(t.len() > 60);
        assert_ne!(t, generate_token());
    }
}
