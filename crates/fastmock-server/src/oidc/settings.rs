use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use reqwest::Url;

use crate::config::parse_duration;

pub const CALLBACK_PATH: &str = "/__admin/auth/callback";

/// The OIDC client settings (`FASTMOCK_OIDC_*`), checked at start-up.
#[derive(Clone)]
pub struct OidcSettings {
    /// Exactly as configured: it is compared with `iss` byte for byte.
    pub issuer: String,
    pub issuer_url: Url,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_url: Url,
    pub scopes: Vec<String>,
    pub username_claim: String,
    /// Name of the groups claim, or a dotted path to it (`realm_access.roles`).
    pub groups_claim: String,
    pub api_audience: Option<String>,
    pub ca_file: Option<PathBuf>,
    pub session_ttl: Duration,
    pub session_idle: Duration,
    /// Hosts besides the issuer's that discovery may point to.
    pub allowed_hosts: Vec<String>,
    /// Cookie without `Secure` and without the `__Host-` prefix (local http development only).
    pub insecure_cookie: bool,
}

impl std::fmt::Debug for OidcSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcSettings")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("client_secret", &"REDACTED")
            .field("redirect_url", &self.redirect_url.as_str())
            .field("scopes", &self.scopes)
            .field("username_claim", &self.username_claim)
            .field("groups_claim", &self.groups_claim)
            .field("api_audience", &self.api_audience)
            .field("ca_file", &self.ca_file)
            .field("session_ttl", &self.session_ttl)
            .field("session_idle", &self.session_idle)
            .field("allowed_hosts", &self.allowed_hosts)
            .field("insecure_cookie", &self.insecure_cookie)
            .finish()
    }
}

const VARS: &[&str] = &[
    "OIDC_CLIENT_ID",
    "OIDC_CLIENT_SECRET",
    "OIDC_CLIENT_SECRET_FILE",
    "OIDC_REDIRECT_URL",
    "OIDC_SCOPES",
    "OIDC_USERNAME_CLAIM",
    "OIDC_GROUPS_CLAIM",
    "OIDC_API_AUDIENCE",
    "OIDC_CA_FILE",
    "OIDC_SESSION_TTL",
    "OIDC_SESSION_IDLE",
    "OIDC_ALLOWED_HOSTS",
    "OIDC_INSECURE_COOKIE",
];

/// `localhost`, `127.0.0.1` or `::1`: the only hosts that may use plain http.
pub fn is_loopback(u: &Url) -> bool {
    match u.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip == std::net::Ipv4Addr::LOCALHOST,
        Some(url::Host::Ipv6(ip)) => ip == std::net::Ipv6Addr::LOCALHOST,
        None => false,
    }
}

fn http_url(raw: &str, var: &str) -> anyhow::Result<Url> {
    let u = Url::parse(raw.trim()).with_context(|| format!("FASTMOCK_{var} is not a URL"))?;
    if !u.username().is_empty() || u.password().is_some() || u.fragment().is_some() {
        bail!("FASTMOCK_{var} must not contain credentials or a fragment");
    }
    match u.scheme() {
        "https" => {}
        "http" if is_loopback(&u) => {}
        _ => bail!(
            "FASTMOCK_{var} must be an https URL (plain http is accepted only for localhost, 127.0.0.1 and ::1)"
        ),
    }
    if u.host_str().is_none_or(str::is_empty) {
        bail!("FASTMOCK_{var} has no host");
    }
    Ok(u)
}

fn bool_of(var: &str, v: Option<String>) -> anyhow::Result<bool> {
    match v
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        None => Ok(false),
        Some("1" | "true" | "yes" | "on") => Ok(true),
        Some("0" | "false" | "no" | "off") => Ok(false),
        Some(other) => bail!("FASTMOCK_{var}: not a boolean: {other}"),
    }
}

fn split(v: &str) -> Vec<String> {
    v.split([',', ' ', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn duration(var: &str, v: Option<String>, default: Duration) -> anyhow::Result<Duration> {
    let d = match v {
        None => default,
        Some(v) => parse_duration(&v).with_context(|| format!("FASTMOCK_{var}"))?,
    };
    if d.is_zero() {
        bail!("FASTMOCK_{var} must be greater than zero");
    }
    Ok(d)
}

/// Reads the OIDC settings through `env` (names without the `FASTMOCK_` prefix). `None` when
/// `FASTMOCK_OIDC_ISSUER` is not set; then no other `FASTMOCK_OIDC_*` variable may be set either.
pub fn from_env(env: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Option<OidcSettings>> {
    let Some(issuer) = env("OIDC_ISSUER").map(|s| s.trim().to_string()) else {
        if let Some(v) = VARS.iter().find(|v| env(v).is_some()) {
            bail!("FASTMOCK_{v} is set but FASTMOCK_OIDC_ISSUER is not: OIDC needs an issuer");
        }
        return Ok(None);
    };
    let issuer_url = http_url(&issuer, "OIDC_ISSUER")?;
    if issuer_url.query().is_some() {
        bail!("FASTMOCK_OIDC_ISSUER must not contain a query");
    }
    let client_id = env("OIDC_CLIENT_ID")
        .map(|s| s.trim().to_string())
        .context("FASTMOCK_OIDC_CLIENT_ID is required with FASTMOCK_OIDC_ISSUER")?;
    let client_secret = match (env("OIDC_CLIENT_SECRET"), env("OIDC_CLIENT_SECRET_FILE")) {
        (Some(_), Some(_)) => bail!(
            "set either FASTMOCK_OIDC_CLIENT_SECRET or FASTMOCK_OIDC_CLIENT_SECRET_FILE, not both"
        ),
        (Some(s), None) => s.trim().to_string(),
        (None, Some(f)) => std::fs::read_to_string(f.trim())
            .map_err(|e| anyhow::anyhow!("reading FASTMOCK_OIDC_CLIENT_SECRET_FILE {f}: {e}"))?
            .trim()
            .to_string(),
        (None, None) => bail!(
            "FASTMOCK_OIDC_CLIENT_SECRET or FASTMOCK_OIDC_CLIENT_SECRET_FILE is required: FastMock is a confidential client"
        ),
    };
    if client_secret.is_empty() {
        bail!("the OIDC client secret is empty");
    }
    let redirect_url = http_url(
        &env("OIDC_REDIRECT_URL")
            .context("FASTMOCK_OIDC_REDIRECT_URL is required with FASTMOCK_OIDC_ISSUER")?,
        "OIDC_REDIRECT_URL",
    )?;
    if redirect_url.path() != CALLBACK_PATH || redirect_url.query().is_some() {
        bail!(
            "FASTMOCK_OIDC_REDIRECT_URL must be the public admin URL followed by {CALLBACK_PATH}, without a query (for example https://mock-admin.example.com{CALLBACK_PATH})"
        );
    }
    let scopes = split(&env("OIDC_SCOPES").unwrap_or_else(|| "openid profile email".into()));
    if !scopes.iter().any(|s| s == "openid") {
        bail!("FASTMOCK_OIDC_SCOPES must include openid");
    }
    if let Some(bad) = scopes.iter().find(|s| {
        !s.bytes()
            .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\')
    }) {
        bail!("FASTMOCK_OIDC_SCOPES: invalid scope `{bad}`");
    }
    let username_claim = env("OIDC_USERNAME_CLAIM")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "preferred_username".into());
    let groups_claim = env("OIDC_GROUPS_CLAIM")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "groups".into());
    if groups_claim.split('.').any(str::is_empty) {
        bail!("FASTMOCK_OIDC_GROUPS_CLAIM `{groups_claim}` has an empty path segment");
    }
    if username_claim.split('.').any(str::is_empty) {
        bail!("FASTMOCK_OIDC_USERNAME_CLAIM `{username_claim}` has an empty path segment");
    }
    let api_audience = env("OIDC_API_AUDIENCE").map(|s| s.trim().to_string());
    let session_ttl = duration(
        "OIDC_SESSION_TTL",
        env("OIDC_SESSION_TTL"),
        Duration::from_secs(8 * 3600),
    )?;
    let session_idle = duration(
        "OIDC_SESSION_IDLE",
        env("OIDC_SESSION_IDLE"),
        Duration::from_secs(3600),
    )?;
    let mut allowed_hosts = Vec::new();
    for h in split(&env("OIDC_ALLOWED_HOSTS").unwrap_or_default()) {
        let h = h.to_ascii_lowercase();
        if !h
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
        {
            bail!("FASTMOCK_OIDC_ALLOWED_HOSTS: `{h}` is not a host name");
        }
        allowed_hosts.push(h);
    }
    let insecure_cookie = bool_of("OIDC_INSECURE_COOKIE", env("OIDC_INSECURE_COOKIE"))?;
    if insecure_cookie && !is_loopback(&redirect_url) {
        bail!(
            "FASTMOCK_OIDC_INSECURE_COOKIE=true is allowed only when FASTMOCK_OIDC_REDIRECT_URL points to localhost, 127.0.0.1 or ::1"
        );
    }
    Ok(Some(OidcSettings {
        issuer,
        issuer_url,
        client_id,
        client_secret,
        redirect_url,
        scopes,
        username_claim,
        groups_claim,
        api_audience,
        ca_file: env("OIDC_CA_FILE").map(|s| PathBuf::from(s.trim())),
        session_ttl,
        session_idle,
        allowed_hosts,
        insecure_cookie,
    }))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn parse(vars: &[(&str, &str)]) -> anyhow::Result<Option<OidcSettings>> {
        let m: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        from_env(&|k| m.get(k).cloned())
    }

    fn base() -> Vec<(&'static str, &'static str)> {
        vec![
            ("OIDC_ISSUER", "https://idp.example.com/realms/test"),
            ("OIDC_CLIENT_ID", "fastmock"),
            ("OIDC_CLIENT_SECRET", "s3cret"),
            (
                "OIDC_REDIRECT_URL",
                "https://mock-admin.example.com/__admin/auth/callback",
            ),
        ]
    }

    fn err(vars: &[(&str, &str)]) -> String {
        format!("{:#}", parse(vars).unwrap_err())
    }

    fn with(extra: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
        let mut v: Vec<_> = base()
            .into_iter()
            .filter(|(k, _)| !extra.iter().any(|(e, _)| e == k))
            .collect();
        v.extend_from_slice(extra);
        v
    }

    #[test]
    fn off_without_issuer() {
        assert!(parse(&[]).unwrap().is_none());
        assert!(err(&[("OIDC_CLIENT_ID", "x")]).contains("FASTMOCK_OIDC_ISSUER is not"));
    }

    #[test]
    fn defaults() {
        let s = parse(&base()).unwrap().unwrap();
        assert_eq!(s.scopes, ["openid", "profile", "email"]);
        assert_eq!(s.username_claim, "preferred_username");
        assert_eq!(s.groups_claim, "groups");
        assert_eq!(s.session_ttl, Duration::from_secs(8 * 3600));
        assert_eq!(s.session_idle, Duration::from_secs(3600));
        assert!(s.api_audience.is_none());
        assert!(!s.insecure_cookie);
        assert!(!format!("{s:?}").contains("s3cret"));
    }

    #[test]
    fn validation() {
        assert!(err(&with(&[("OIDC_ISSUER", "http://idp.example.com")])).contains("https"));
        assert!(parse(&with(&[("OIDC_ISSUER", "http://127.0.0.1:8081/realms/x")])).is_ok());
        assert!(parse(&with(&[("OIDC_ISSUER", "http://localhost:8081")])).is_ok());
        assert!(err(&with(&[("OIDC_ISSUER", "ftp://idp.example.com")])).contains("https"));
        assert!(
            err(&with(&[(
                "OIDC_REDIRECT_URL",
                "http://mock-admin.example.com/__admin/auth/callback"
            )]))
            .contains("https")
        );
        assert!(
            err(&with(&[(
                "OIDC_REDIRECT_URL",
                "https://mock-admin.example.com/callback"
            )]))
            .contains("/__admin/auth/callback")
        );
        assert!(err(&with(&[("OIDC_SCOPES", "profile email")])).contains("openid"));
        assert!(err(&with(&[("OIDC_GROUPS_CLAIM", "realm_access..roles")])).contains("empty"));
        let s = parse(&with(&[("OIDC_GROUPS_CLAIM", "realm_access.roles")]))
            .unwrap()
            .unwrap();
        assert_eq!(s.groups_claim, "realm_access.roles");
        assert!(err(&with(&[("OIDC_SESSION_IDLE", "0s")])).contains("greater than zero"));
        assert!(err(&with(&[("OIDC_SESSION_TTL", "8x")])).contains("FASTMOCK_OIDC_SESSION_TTL"));
        assert!(err(&with(&[("OIDC_INSECURE_COOKIE", "true")])).contains("localhost"));
        let local = parse(&with(&[
            ("OIDC_INSECURE_COOKIE", "true"),
            (
                "OIDC_REDIRECT_URL",
                "http://localhost:9090/__admin/auth/callback",
            ),
        ]))
        .unwrap()
        .unwrap();
        assert!(local.insecure_cookie);
        let no_secret: Vec<_> = base()
            .into_iter()
            .filter(|(k, _)| *k != "OIDC_CLIENT_SECRET")
            .collect();
        assert!(err(&no_secret).contains("CLIENT_SECRET"));
        assert!(err(&with(&[("OIDC_CLIENT_SECRET_FILE", "/nonexistent")])).contains("not both"));
        let no_id: Vec<_> = base()
            .into_iter()
            .filter(|(k, _)| *k != "OIDC_CLIENT_ID")
            .collect();
        assert!(err(&no_id).contains("CLIENT_ID"));
    }

    #[test]
    fn secret_from_file() {
        let f = std::env::temp_dir().join(format!("fm-oidc-{}", uuid::Uuid::new_v4().simple()));
        std::fs::write(&f, "from-file\n").unwrap();
        let path = f.to_string_lossy().to_string();
        let vars: Vec<(&str, &str)> = base()
            .into_iter()
            .filter(|(k, _)| *k != "OIDC_CLIENT_SECRET")
            .chain([("OIDC_CLIENT_SECRET_FILE", path.as_str())])
            .collect();
        assert_eq!(parse(&vars).unwrap().unwrap().client_secret, "from-file");
        std::fs::remove_file(f).unwrap();
    }
}
