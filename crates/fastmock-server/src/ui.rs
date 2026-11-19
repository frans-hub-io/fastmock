use std::borrow::Cow;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::json;

use crate::config::Config;
use crate::state::AppState;

#[derive(rust_embed::RustEmbed)]
#[folder = "../../ui/dist"]
#[allow_missing = true]
struct Assets;

const NOT_BUILT: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>FastMock</title></head><body><h1>FastMock UI is not available</h1><p>This binary was built without the web UI. Build it with <code>npm ci &amp;&amp; npm run build</code> in <code>ui/</code> before <code>cargo build</code>, or use the admin API at <code>/__admin</code> and its description at <code>/openapi.json</code>.</p></body></html>";

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const NO_CACHE: &str = "no-cache";

const NONCE_PLACEHOLDER: &str = "__FASTMOCK_STYLE_NONCE__";

pub fn csp(connect_src: &str, style_nonce: Option<&str>) -> String {
    let style = match style_nonce {
        Some(n) => format!("'self' 'nonce-{n}'"),
        None => "'self'".to_string(),
    };
    format!(
        "default-src 'none'; script-src 'self'; style-src {style}; img-src 'self' data:; font-src 'self' data:; \
         connect-src {connect_src}; manifest-src 'self'; \
         base-uri 'none'; form-action 'self'; frame-ancestors 'none'; object-src 'none'"
    )
}

fn request_host(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::HOST)?.to_str().ok()?;
    let auth: axum::http::uri::Authority = raw.parse().ok()?;
    let host = auth.host().to_ascii_lowercase();
    (!host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']')))
    .then_some(host)
}

/// `connect-src` of the UI: the admin origin plus the exact data-plane origins from
/// `FASTMOCK_UI_DATA_PLANE_URL`; without that setting, the data-plane port on the host name the
/// browser used for the UI.
pub fn connect_sources(cfg: &Config, headers: &HeaderMap) -> String {
    let mut out = vec!["'self'".to_string()];
    if !cfg.ui_data_plane_origins.is_empty() {
        out.extend(cfg.ui_data_plane_origins.iter().cloned());
    } else if let Some(host) = request_host(headers) {
        let scheme = if cfg.tls.is_some() { "https" } else { "http" };
        out.push(format!("{scheme}://{host}:{}", cfg.listen.port()));
    }
    out.join(" ")
}

fn new_nonce() -> String {
    use base64::Engine as _;
    let bytes: [u8; 18] = rand::random();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        Some("txt") => "text/plain; charset=utf-8",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

pub fn cache_control(path: &str) -> &'static str {
    if path.starts_with("assets/") {
        IMMUTABLE
    } else {
        NO_CACHE
    }
}

fn looks_like_file(path: &str) -> bool {
    path.starts_with("assets/") || (!path.contains('/') && path.contains('.'))
}

fn respond(
    status: StatusCode,
    ctype: &'static str,
    cache: &'static str,
    connect: &str,
    body: Cow<'static, [u8]>,
) -> Response {
    respond_with_nonce(status, ctype, cache, connect, None, body)
}

fn respond_with_nonce(
    status: StatusCode,
    ctype: &'static str,
    cache: &'static str,
    connect: &str,
    style_nonce: Option<&str>,
    body: Cow<'static, [u8]>,
) -> Response {
    let mut r = (status, body).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    if let Ok(v) = HeaderValue::from_str(&csp(connect, style_nonce)) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    r
}

fn index(connect: &str) -> Response {
    metrics::counter!("fastmock_ui_requests_total", "kind" => "index").increment(1);
    match Assets::get("index.html") {
        Some(f) => {
            let nonce = new_nonce();
            let html = String::from_utf8_lossy(&f.data).replace(NONCE_PLACEHOLDER, &nonce);
            respond_with_nonce(
                StatusCode::OK,
                "text/html; charset=utf-8",
                NO_CACHE,
                connect,
                Some(&nonce),
                Cow::Owned(html.into_bytes()),
            )
        }
        None => respond(
            StatusCode::NOT_FOUND,
            "text/html; charset=utf-8",
            NO_CACHE,
            connect,
            Cow::Borrowed(NOT_BUILT.as_bytes()),
        ),
    }
}

async fn ui_index(State(app): State<AppState>, headers: HeaderMap) -> Response {
    index(&connect_sources(&app.cfg, &headers))
}

async fn ui_config(State(app): State<AppState>, headers: HeaderMap) -> Response {
    let mut body = json!({
        "dataPlanePort": app.cfg.listen.port(),
        "dataPlaneScheme": if app.cfg.tls.is_some() { "https" } else { "http" },
        "uiBuilt": Assets::get("index.html").is_some(),
    });
    if let Some(u) = app.cfg.ui_data_plane_origins.first() {
        body["dataPlaneUrl"] = json!(u);
    }
    respond(
        StatusCode::OK,
        "application/json",
        NO_CACHE,
        &connect_sources(&app.cfg, &headers),
        Cow::Owned(body.to_string().into_bytes()),
    )
}

async fn ui_path(
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(path): Path<String>,
) -> Response {
    let connect = connect_sources(&app.cfg, &headers);
    if path.is_empty() || path == "index.html" {
        return index(&connect);
    }
    if !path.split('/').any(|seg| seg == ".." || seg.is_empty())
        && let Some(f) = Assets::get(&path)
    {
        metrics::counter!("fastmock_ui_requests_total", "kind" => "asset").increment(1);
        return respond(
            StatusCode::OK,
            content_type(&path),
            cache_control(&path),
            &connect,
            f.data,
        );
    }
    if looks_like_file(&path) {
        metrics::counter!("fastmock_ui_requests_total", "kind" => "missing").increment(1);
        return respond(
            StatusCode::NOT_FOUND,
            "text/plain; charset=utf-8",
            NO_CACHE,
            &connect,
            Cow::Borrowed(b"not found"),
        );
    }
    index(&connect)
}

pub fn built() -> bool {
    Assets::get("index.html").is_some()
}

pub fn enabled_from(value: Option<&str>) -> bool {
    !matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("false" | "0" | "no" | "off")
    )
}

pub fn router() -> Router<AppState> {
    if !enabled_from(std::env::var("FASTMOCK_UI_ENABLED").ok().as_deref()) {
        tracing::info!("web UI disabled by FASTMOCK_UI_ENABLED");
        return Router::new();
    }
    Router::new()
        .route("/ui", get(ui_index))
        .route("/ui/", get(ui_index))
        .route("/ui/config.json", get(ui_config))
        .route("/ui/{*path}", get(ui_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashed_assets_are_immutable_and_the_rest_revalidates() {
        assert_eq!(cache_control("assets/index-abc123.js"), IMMUTABLE);
        assert_eq!(cache_control("favicon.svg"), NO_CACHE);
        assert_eq!(cache_control("index.html"), NO_CACHE);
    }

    #[test]
    fn content_types_follow_the_extension() {
        assert_eq!(
            content_type("assets/a.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type("assets/a.CSS"), "text/css; charset=utf-8");
        assert_eq!(content_type("favicon.svg"), "image/svg+xml");
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("blob"), "application/octet-stream");
    }

    #[test]
    fn csp_never_relaxes_scripts() {
        let n = new_nonce();
        assert_eq!(n.len(), 24);
        assert_ne!(n, new_nonce());
        let with = csp("'self'", Some(&n));
        assert!(with.contains(&format!("style-src 'self' 'nonce-{n}';")));
        assert!(with.contains("script-src 'self';"));
        let p = csp("'self' http://mock.example.com:8080", None);
        assert!(p.contains("script-src 'self';"));
        assert!(!p.contains("unsafe-inline"));
        assert!(!p.contains("unsafe-eval"));
        assert!(p.contains("connect-src 'self' http://mock.example.com:8080;"));
        assert!(!p.contains('*'));
        assert!(p.contains("frame-ancestors 'none'"));
    }

    #[test]
    fn connect_src_names_exact_origins() {
        let mut cfg = Config::for_tests(crate::config::StorageKind::Memory, vec![]);
        cfg.listen = "0.0.0.0:8080".parse().unwrap();
        let mut h = HeaderMap::new();
        h.insert(
            header::HOST,
            HeaderValue::from_static("ui.example.com:9090"),
        );
        assert_eq!(
            connect_sources(&cfg, &h),
            "'self' http://ui.example.com:8080"
        );
        h.insert(
            header::HOST,
            HeaderValue::from_static("evil.example.com; script-src *"),
        );
        assert_eq!(connect_sources(&cfg, &h), "'self'");
        assert_eq!(connect_sources(&cfg, &HeaderMap::new()), "'self'");
        cfg.ui_data_plane_origins = vec![
            "https://mock.example.com".into(),
            "https://mock2.example.com:8443".into(),
        ];
        h.insert(header::HOST, HeaderValue::from_static("ui.example.com"));
        assert_eq!(
            connect_sources(&cfg, &h),
            "'self' https://mock.example.com https://mock2.example.com:8443"
        );
    }

    #[test]
    fn client_routes_fall_back_but_missing_files_do_not() {
        assert!(looks_like_file("assets/x.js"));
        assert!(looks_like_file("favicon.ico"));
        assert!(looks_like_file("assets/deep"));
        assert!(!looks_like_file("stubs/0b1c"));
        assert!(!looks_like_file("requests"));
        assert!(!looks_like_file("services/petstore:1.0.2"));
    }

    #[test]
    fn the_ui_can_be_switched_off() {
        assert!(enabled_from(None));
        assert!(enabled_from(Some("true")));
        assert!(!enabled_from(Some("false")));
        assert!(!enabled_from(Some(" OFF ")));
        assert!(!enabled_from(Some("0")));
    }

    #[test]
    fn traversal_is_never_served() {
        assert!(Assets::get("../Cargo.toml").is_none());
    }
}
