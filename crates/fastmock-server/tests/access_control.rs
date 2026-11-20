use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use fastmock_server::state::AppState;
use reqwest::Method;
use serde_json::{Value, json};
use sqlx::Connection;
use tokio::sync::watch;

const ADMIN: &str = "fm_admin_token_for_access_tests_0123456789";
const EDITOR: &str = "fm_editor_token_for_access_tests_0123456789";
const VIEWER: &str = "fm_viewer_token_for_access_tests_0123456789";

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

fn legacy_tokens() -> Vec<String> {
    vec![
        format!("ops:admin:{}", hash_token(ADMIN)),
        format!("ci:editor:{}", hash_token(EDITOR)),
        format!("dash:viewer:{}", hash_token(VIEWER)),
    ]
}

#[allow(dead_code)]
async fn pg_storage() -> Option<StorageKind> {
    let admin_url = std::env::var("FASTMOCK_TEST_DATABASE_URL").ok()?;
    let valkey_url = std::env::var("FASTMOCK_TEST_VALKEY_URL").ok()?;
    let db = format!("fm_acl_{}", uuid::Uuid::new_v4().simple());
    let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut c)
        .await
        .unwrap();
    Some(StorageKind::Postgres {
        database_url: format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db),
        valkey_url,
        valkey_prefix: format!("acl:{}:", uuid::Uuid::new_v4().simple()),
        max_connections: 5,
        options: Default::default(),
    })
}

#[allow(dead_code)]
async fn storages() -> Vec<(&'static str, StorageKind)> {
    let mut v = vec![("memory", StorageKind::Memory)];
    match pg_storage().await {
        Some(p) => v.push(("postgres+valkey", p)),
        None => eprintln!(
            "FASTMOCK_TEST_DATABASE_URL/FASTMOCK_TEST_VALKEY_URL not set: PostgreSQL+Valkey part skipped"
        ),
    }
    v
}

#[allow(dead_code)]
struct H {
    data: String,
    admin: String,
    http: reqwest::Client,
    app: AppState,
    _dir: Option<std::path::PathBuf>,
    _shutdown: watch::Sender<bool>,
}

fn access_file(content: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("fm-access-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("access.yaml");
    std::fs::write(&f, content).unwrap();
    f
}

#[allow(dead_code)]
impl H {
    async fn start(storage: StorageKind, tokens: Vec<String>, access: Option<&str>) -> H {
        let mut cfg = Config::for_tests(storage, tokens);
        cfg.plugins = Some(vec![]);
        let file = access.map(access_file);
        cfg.access_file = file.clone();
        let (app, data, admin, _policy) = fastmock_server::build(cfg, prom()).await.unwrap();
        let (tx, rx) = watch::channel(false);
        let dl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (da, aa) = (dl.local_addr().unwrap(), al.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(dl, data).await.unwrap() });
        tokio::spawn(async move { axum::serve(al, admin).await.unwrap() });
        tokio::spawn(fastmock_server::listen_notifications(app.clone(), rx));
        app.ready.store(true, Ordering::SeqCst);
        H {
            data: format!("http://{da}"),
            admin: format!("http://{aa}"),
            http: reqwest::Client::new(),
            app,
            _dir: file,
            _shutdown: tx,
        }
    }

    async fn call(&self, m: Method, path: &str, token: &str, body: Option<Value>) -> (u16, Value) {
        let mut rb = self
            .http
            .request(m, format!("{}/__admin{path}", self.admin))
            .bearer_auth(token);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let r = rb.send().await.unwrap();
        let st = r.status().as_u16();
        let text = r.text().await.unwrap();
        (
            st,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn ok(&self, m: Method, path: &str, token: &str, body: Option<Value>) -> Value {
        let (st, v) = self.call(m.clone(), path, token, body).await;
        assert!((200..300).contains(&st), "{m} {path}: {st} {v}");
        v
    }

    async fn hit(&self, m: Method, path: &str) -> (u16, String) {
        let r = self
            .http
            .request(m, format!("{}{path}", self.data))
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }
}

const OLD_VIEWER_POSTS: &[&str] = &[
    "/__admin/requests/count",
    "/__admin/requests/near-misses",
    "/__admin/requests/find",
    "/__admin/verify",
    "/__admin/verify/sequence",
    "/__admin/stubs/find-by-metadata",
    "/__admin/stubs/validate",
];

/// The role table before permissions existed: `required_role` of the middleware (GET/HEAD and the
/// read-only POSTs need viewer, everything else editor) and the handler-level admin checks.
fn old_required(method: &str, path: &str) -> (u8, u8) {
    let path = path.strip_suffix('/').unwrap_or(path);
    let middleware = if method == "GET"
        || method == "HEAD"
        || (method == "POST" && OLD_VIEWER_POSTS.contains(&path))
    {
        0
    } else {
        1
    };
    let handler_admin = matches!(
        (method, path),
        ("DELETE", "/__admin/stubs") | ("POST", "/__admin/reset") | ("GET", "/__admin/audit")
    );
    (middleware, if handler_admin { 2 } else { middleware })
}

fn concrete(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            if seg == "{id}" {
                "0190e0d0-0000-7000-8000-000000000001".to_string()
            } else if seg.starts_with('{') {
                "x".to_string()
            } else {
                seg.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[tokio::test]
async fn without_access_file_every_openapi_route_keeps_its_role() {
    let h = H::start(StorageKind::Memory, legacy_tokens(), None).await;
    let doc = serde_json::to_value(fastmock_server::openapi_doc::spec()).unwrap();
    let mut checked = 0;
    for (path, item) in doc["paths"].as_object().unwrap() {
        if !path.starts_with("/__admin/") {
            continue;
        }
        for method in ["get", "put", "post", "delete", "patch", "head"] {
            if item.get(method).is_none() {
                continue;
            }
            let m = method.to_ascii_uppercase();
            let (first, need) = old_required(&m, path);
            let url = concrete(path).replacen("/__admin", "", 1);
            for (level, token) in [(0u8, VIEWER), (1, EDITOR), (2, ADMIN)] {
                let body = matches!(m.as_str(), "POST" | "PUT" | "PATCH").then(|| json!({}));
                let (st, v) = h
                    .call(Method::from_bytes(m.as_bytes()).unwrap(), &url, token, body)
                    .await;
                assert_eq!(
                    st == 403,
                    level < need,
                    "{m} {path} with role level {level}: {st} {v}"
                );
                if st == 403 && level < need {
                    let shown = if level < first { first } else { need };
                    let names = ["viewer", "editor", "admin"];
                    assert!(
                        v["required"] == names[shown as usize]
                            || v["required"] == names[need as usize],
                        "{m} {path}: {v}"
                    );
                }
                checked += 1;
            }
        }
    }
    assert!(checked >= 240, "only {checked} route checks");
    let (st, _) = h.call(Method::DELETE, "/stubs?tag=x", EDITOR, None).await;
    assert_eq!(st, 200);
    let (st, v) = h
        .call(Method::POST, "/stubs", VIEWER, Some(json!({})))
        .await;
    assert_eq!(st, 403);
    assert_eq!(
        v,
        json!({"error": "forbidden", "details": ["requires role editor"], "required": "editor"})
    );
    let (st, v) = h.call(Method::DELETE, "/stubs", EDITOR, None).await;
    assert_eq!(st, 403);
    assert_eq!(
        v,
        json!({"error": "requires role admin", "details": [], "required": "admin"})
    );
}

#[tokio::test]
async fn contract_details_and_secret_headers_keep_their_roles() {
    let h = H::start(StorageKind::Memory, legacy_tokens(), None).await;
    let spec = json!({"openapi": "3.0.3", "info": {"title": "t", "version": "1"}, "paths": {"/a": {"get": {"responses": {"200": {"description": "ok"}}}}}});
    let body = json!({"spec": spec, "targetUrl": "http://127.0.0.1:9/", "secretHeaders": {"x-api-key": "k"}});
    let (st, _) = h
        .call(Method::POST, "/contract-tests", VIEWER, Some(body.clone()))
        .await;
    assert_eq!(st, 403);
    let (st, v) = h
        .call(Method::POST, "/contract-tests", EDITOR, Some(body.clone()))
        .await;
    assert_eq!(st, 403, "{v}");
    assert_eq!(v["error"], "secretHeaders require the admin role");
    let (st, v) = h
        .call(Method::POST, "/contract-tests", ADMIN, Some(body))
        .await;
    assert_ne!(st, 403, "{v}");
    let id = uuid::Uuid::now_v7();
    let run = json!({
        "id": id, "kind": "openapi", "status": "failed", "startedAt": "2026-01-01T00:00:00Z",
        "startedBy": "ops", "summary": {"total": 1, "passed": 0, "failed": 1, "errors": 0},
        "results": [{"operation": "GET /a", "verdict": "failed", "errors": ["status 500 is not declared"],
            "request": {"method": "GET", "url": "http://127.0.0.1:9/a", "headers": {}},
            "durationMs": 1}]
    });
    h.app
        .stores
        .data
        .put(fastmock_server::contract::NAMESPACE, &id.to_string(), &run)
        .await
        .unwrap();
    let v = h
        .ok(Method::GET, &format!("/contract-tests/{id}"), VIEWER, None)
        .await;
    assert_eq!(v["detailsHidden"], true);
    assert!(v["results"][0]["errors"].as_array().unwrap().is_empty());
    let v = h
        .ok(Method::GET, &format!("/contract-tests/{id}"), EDITOR, None)
        .await;
    assert!(v.get("detailsHidden").is_none());
    assert_eq!(v["results"][0]["errors"][0], "status 500 is not declared");
}

const ACCESS: &str = r#"
roles:
  - name: stub-author
    permissions: [stubs:read, stubs:write, imports:run, journal:read, state:read]
  - name: auditor
    permissions: [audit:read, stubs:read]
spaces:
  - name: payments
    description: Payment provider mocks
    pathPrefixes: [/payments/, /psp/]
    hosts: [psp.example.com]
  - name: billing
bindings:
  - subject: token:pay
    role: editor
    spaces: [payments]
  - subject: token:auditor
    role: viewer
    spaces: [billing]
  - subject: group:platform
    role: admin
    global: true
"#;

const PAY: &str = "fm_pay_token_for_access_tests_0123456789";
const AUDITOR: &str = "fm_auditor_token_for_access_tests_0123456789";
const AUTHOR: &str = "fm_author_token_for_access_tests_0123456789";

fn access_tokens() -> Vec<String> {
    let mut t = legacy_tokens();
    t.push(format!("pay::{}", hash_token(PAY)));
    t.push(format!("auditor:auditor:{}", hash_token(AUDITOR)));
    t.push(format!("author:stub-author:{}", hash_token(AUTHOR)));
    t
}

#[tokio::test]
async fn custom_roles_bindings_info_and_spaces() {
    let h = H::start(StorageKind::Memory, access_tokens(), Some(ACCESS)).await;
    let v = h.ok(Method::GET, "/info", AUTHOR, None).await;
    assert_eq!(v["principal"]["name"], "author");
    assert_eq!(v["principal"]["kind"], "token");
    assert_eq!(v["principal"]["role"], "viewer");
    assert_eq!(
        v["principal"]["permissions"]["global"],
        json!([
            "stubs:read",
            "stubs:write",
            "imports:run",
            "journal:read",
            "state:read",
            "info:read"
        ])
    );
    assert_eq!(v["principal"]["permissions"]["spaces"], json!({}));
    let created = h
        .ok(
            Method::POST,
            "/stubs",
            AUTHOR,
            Some(json!({"request": {"urlPath": "/author"}, "response": {"status": 200}})),
        )
        .await;
    assert!(created["id"].is_string());
    let (st, v) = h.call(Method::GET, "/files", AUTHOR, None).await;
    assert_eq!(st, 403);
    assert_eq!(
        v,
        json!({"error": "forbidden", "details": ["requires permission files:read"], "required": "files:read"})
    );
    let (st, v) = h.call(Method::DELETE, "/stubs", AUTHOR, None).await;
    assert_eq!(st, 403);
    assert_eq!(v["required"], "stubs:purge");

    let v = h.ok(Method::GET, "/audit", AUDITOR, None).await;
    assert!(
        v["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["principal"] == "author"
                && e["principalKind"] == "token"
                && e["role"] == "stub-author")
    );
    let v = h.ok(Method::GET, "/info", AUDITOR, None).await;
    assert_eq!(
        v["principal"]["permissions"]["spaces"]["billing"],
        json!([
            "stubs:read",
            "journal:read",
            "files:read",
            "callbacks:read",
            "state:read",
            "contracts:read"
        ])
    );
    let v = h.ok(Method::GET, "/spaces", AUDITOR, None).await;
    let names: Vec<&str> = v["spaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["billing", "payments"],
        "global stubs:read sees every space"
    );

    let v = h.ok(Method::GET, "/info", PAY, None).await;
    assert_eq!(
        v["principal"]["permissions"]["global"],
        json!(["info:read"])
    );
    assert!(
        v["principal"]["permissions"]["spaces"]["payments"]
            .as_array()
            .unwrap()
            .contains(&json!("stubs:write"))
    );
    let v = h.ok(Method::GET, "/spaces", PAY, None).await;
    assert_eq!(v["spaces"].as_array().unwrap().len(), 1);
    assert_eq!(v["spaces"][0]["name"], "payments");
    assert_eq!(
        v["spaces"][0]["pathPrefixes"],
        json!(["/payments/", "/psp/"])
    );
    assert_eq!(v["spaces"][0]["hosts"], json!(["psp.example.com"]));
    assert!(
        v["spaces"][0]["permissions"]
            .as_array()
            .unwrap()
            .contains(&json!("journal:delete"))
    );
    let (st, v) = h.call(Method::POST, "/reset", PAY, None).await;
    assert_eq!(st, 403);
    assert_eq!(v["required"], "system:reset");

    let v = h.ok(Method::GET, "/spaces", ADMIN, None).await;
    assert_eq!(v["spaces"].as_array().unwrap().len(), 2);
    let auth = &h.app.auth;
    let p = auth
        .principal_for(
            fastmock_server::access::PrincipalKind::User,
            "someone",
            &["platform".to_string()],
        )
        .unwrap();
    assert!(p.allowed(fastmock_server::access::Permission::SystemReset, None));
    assert!(
        auth.principal_for(fastmock_server::access::PrincipalKind::User, "someone", &[])
            .is_none()
    );
}

#[tokio::test]
async fn bad_access_files_fail_at_start_up() {
    for (doc, want) in [
        (
            "roles: [{name: x, permissions: [stubs:wrte]}]",
            "roles[0] (x): unknown permission `stubs:wrte`",
        ),
        (
            "spaces: [{name: a}]\nbindings: [{subject: 'token:ghost', role: viewer, spaces: [a]}]",
            "token:ghost",
        ),
    ] {
        let mut cfg = Config::for_tests(StorageKind::Memory, legacy_tokens());
        cfg.plugins = Some(vec![]);
        cfg.access_file = Some(access_file(doc));
        let e = match fastmock_server::build(cfg, prom()).await {
            Ok(_) => panic!("{doc} was accepted"),
            Err(e) => format!("{e:#}"),
        };
        assert!(e.contains(want), "{e}");
    }
    let mut cfg = Config::for_tests(
        StorageKind::Memory,
        vec![format!("ci:stub-author:{}", hash_token("x"))],
    );
    cfg.plugins = Some(vec![]);
    let e = match fastmock_server::build(cfg, prom()).await {
        Ok(_) => panic!("unknown role accepted"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        e.contains("admin token entry #1 (ci): unknown role `stub-author`"),
        "{e}"
    );
}

#[test]
fn serve_refuses_a_bad_access_file() {
    let f = access_file("bindings: [{subject: 'user:a', role: nobody, global: true}]");
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_fastmock"));
    for (k, _) in std::env::vars() {
        if k.starts_with("FASTMOCK_") {
            cmd.env_remove(k);
        }
    }
    let o = cmd
        .env("FASTMOCK_STORAGE", "memory")
        .env("FASTMOCK_LISTEN", "127.0.0.1:0")
        .env("FASTMOCK_ADMIN_LISTEN", "127.0.0.1:1")
        .env(
            "FASTMOCK_ADMIN_TOKENS",
            format!("ci:viewer:{}", hash_token("x")),
        )
        .env("FASTMOCK_ACCESS_FILE", &f)
        .env("FASTMOCK_LOG_FORMAT", "text")
        .output()
        .unwrap();
    assert!(!o.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(
        text.contains("bindings[0] (user:a): unknown role `nobody`"),
        "{text}"
    );
}
