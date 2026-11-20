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
        Self::start_cfg(cfg).await
    }

    async fn start_cfg(cfg: Config) -> H {
        let file = cfg.access_file.clone();
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

    async fn hit_host(&self, path: &str, host: &str) -> (u16, String) {
        let r = self
            .http
            .get(format!("{}{path}", self.data))
            .header("host", host)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }

    async fn journal_total(&self, token: &str, query: &str) -> i64 {
        let v = self
            .ok(
                Method::GET,
                &format!("/requests?limit=1{query}"),
                token,
                None,
            )
            .await;
        v["total"].as_i64().unwrap()
    }

    async fn wait_journal(&self, want: i64) {
        for _ in 0..100 {
            if self.journal_total(ADMIN, "").await >= want {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("journal did not reach {want} entries");
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
/// read-only POSTs need viewer, everything else editor) and the handler-level admin checks. The token
/// routes and the access view came later: they need admin, `/tokens/self` any role.
fn old_required(method: &str, path: &str) -> (u8, u8) {
    let path = path.strip_suffix('/').unwrap_or(path);
    if path == "/__admin/tokens/self" {
        return (0, 0);
    }
    if path.starts_with("/__admin/tokens") || path == "/__admin/access" {
        return (2, 2);
    }
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
        if !path.starts_with("/__admin/") || path.starts_with("/__admin/auth/") {
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
                if st == 403 && level < need && v.get("required").is_none() {
                    assert_eq!(
                        v,
                        json!({"error": "requires role admin", "details": []}),
                        "{m} {path}: the handler denial of earlier versions"
                    );
                } else if st == 403 && level < need {
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
        json!({"error": "requires role admin", "details": []}),
        "the handler denial of earlier versions"
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

const SPACES: &str = r#"
spaces:
  - name: pay
    pathPrefixes: [/pay/]
  - name: bill
    pathPrefixes: [/bill/]
    hosts: [bill.example.com]
bindings:
  - subject: token:pay
    role: editor
    spaces: [pay]
  - subject: token:bill
    role: editor
    spaces: [bill]
  - subject: token:multi
    role: viewer
    spaces: [pay, bill]
"#;

const PAY_T: &str = "fm_pay_space_token_0123456789abcdef";
const BILL_T: &str = "fm_bill_space_token_0123456789abcdef";
const MULTI_T: &str = "fm_multi_space_token_0123456789abcdef";

fn space_tokens() -> Vec<String> {
    let mut t = legacy_tokens();
    t.push(format!("pay::{}", hash_token(PAY_T)));
    t.push(format!("bill::{}", hash_token(BILL_T)));
    t.push(format!("multi::{}", hash_token(MULTI_T)));
    t
}

fn ids(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = v["stubs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_string())
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn ownership_rules_apply_on_create_update_and_import() {
    for (name, storage) in storages().await {
        let h = H::start(storage, space_tokens(), Some(SPACES)).await;
        let (st, v) = h
            .call(
                Method::POST,
                "/stubs",
                PAY_T,
                Some(json!({"request": {"urlPath": "/bill/x"}, "response": {"status": 200}})),
            )
            .await;
        assert_eq!(st, 400, "{name}: {v}");
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .contains("space pay owns /pay/"),
            "{name}: {v}"
        );
        let (st, v) = h
            .call(
                Method::POST,
                "/stubs",
                PAY_T,
                Some(
                    json!({"request": {"urlPathPattern": "/pay/.*"}, "response": {"status": 200}}),
                ),
            )
            .await;
        assert_eq!(st, 400, "{name}: {v}");
        for request in [
            json!({"url": "/pay/../bill/x"}),
            json!({"urlPath": "/pay/%2e%2e/bill/y"}),
            json!({"urlPath": "/pay/%2E%2e/bill/y"}),
            json!({"urlPath": "/pay/./x"}),
            json!({"urlPathTemplate": "/pay/../bill/{id}"}),
        ] {
            let (st, v) = h
                .call(
                    Method::POST,
                    "/stubs",
                    PAY_T,
                    Some(json!({"request": request, "response": {"status": 200}})),
                )
                .await;
            assert_eq!(st, 400, "{name}: dot segments in {request}: {v}");
            assert!(
                v["error"].as_str().unwrap().contains("segment"),
                "{name}: {v}"
            );
        }
        let (st, v) = h
            .call(
                Method::POST,
                "/stubs/import",
                PAY_T,
                Some(json!({"stubs": [
                    {"request": {"urlPath": "/pay/%2E%2E/bill/z"}, "response": {"status": 200}}
                ]})),
            )
            .await;
        assert_eq!(st, 400, "{name}: dot segments in an import: {v}");
        let (st, v) = h
            .call(
                Method::POST,
                "/import/wiremock",
                PAY_T,
                Some(json!({"mappings": {"mappings": [{"request": {"method": "GET", "url": "/pay/../bill/w"}, "response": {"status": 200}}]}, "tag": "dots"})),
            )
            .await;
        assert_eq!(st, 400, "{name}: dot segments in a WireMock import: {v}");
        for request in [
            json!({"url": "/pay/..%2fbill/x"}),
            json!({"urlPath": "/pay/..%2Fbill/y"}),
            json!({"urlPath": "/pay/a%5cb"}),
            json!({"urlPath": "/pay/a%5Cb"}),
            json!({"urlPath": "/pay\\..\\bill/z"}),
            json!({"urlPathTemplate": "/pay/%2F/{id}"}),
        ] {
            let (st, v) = h
                .call(
                    Method::POST,
                    "/stubs",
                    PAY_T,
                    Some(json!({"request": request, "response": {"status": 200}})),
                )
                .await;
            assert_eq!(st, 400, "{name}: encoded separator in {request}: {v}");
            assert!(
                v["error"].as_str().unwrap().contains("encoded slash"),
                "{name}: {v}"
            );
        }
        let (st, v) = h
            .call(
                Method::POST,
                "/stubs/import",
                PAY_T,
                Some(json!({"stubs": [
                    {"request": {"urlPath": "/pay/..%2Fbill/z"}, "response": {"status": 200}}
                ]})),
            )
            .await;
        assert_eq!(st, 400, "{name}: encoded slash in an import: {v}");
        let (st, v) = h
            .call(
                Method::POST,
                "/import/wiremock",
                PAY_T,
                Some(json!({"mappings": {"mappings": [{"request": {"method": "GET", "url": "/pay/..%5Cbill/w"}, "response": {"status": 200}}]}, "tag": "slashes"})),
            )
            .await;
        assert_eq!(
            st, 400,
            "{name}: encoded backslash in a WireMock import: {v}"
        );
        let created = h
            .ok(
                Method::POST,
                "/stubs",
                PAY_T,
                Some(json!({"request": {"urlPath": "/pay/x"}, "response": {"status": 200}})),
            )
            .await;
        assert_eq!(
            created["space"], "pay",
            "{name}: the only space is the default"
        );
        let id = created["id"].as_str().unwrap().to_string();
        let (st, v) = h
            .call(
                Method::PUT,
                &format!("/stubs/{id}"),
                PAY_T,
                Some(json!({"request": {"urlPath": "/bill/x"}, "response": {"status": 200}})),
            )
            .await;
        assert_eq!(st, 400, "{name}: update outside the prefixes: {v}");
        let (st, v) = h
            .call(
                Method::PUT,
                &format!("/stubs/{id}"),
                PAY_T,
                Some(json!({"request": {"urlPath": "/pay/..%2fbill/x"}, "response": {"status": 200}})),
            )
            .await;
        assert_eq!(st, 400, "{name}: update with an encoded slash: {v}");
        let (st, v) = h
            .call(
                Method::PATCH,
                &format!("/stubs/{id}"),
                PAY_T,
                Some(json!({"request": {"urlPath": "/other"}})),
            )
            .await;
        assert_eq!(st, 400, "{name}: patch outside the prefixes: {v}");
        let (st, v) = h
            .call(
                Method::PATCH,
                &format!("/stubs/{id}"),
                ADMIN,
                Some(json!({"space": "bill"})),
            )
            .await;
        assert_eq!(st, 400, "{name}: the space cannot change: {v}");
        assert!(
            v["error"].as_str().unwrap().contains("cannot change"),
            "{name}"
        );
        let (st, v) = h
            .call(
                Method::POST,
                "/stubs/import",
                PAY_T,
                Some(json!({"stubs": [
                    {"request": {"urlPath": "/pay/a"}, "response": {"status": 200}},
                    {"request": {"urlPath": "/bill/a"}, "response": {"status": 200}}
                ]})),
            )
            .await;
        assert_eq!(st, 400, "{name}: {v}");
        assert!(
            v["error"].as_str().unwrap().starts_with("stubs[1]"),
            "{name}: {v}"
        );
        let (st, v) = h
            .call(
                Method::POST,
                "/import/wiremock",
                PAY_T,
                Some(json!({"mappings": {"mappings": [{"request": {"method": "GET", "url": "/bill/w"}, "response": {"status": 200}}]}, "tag": "wm"})),
            )
            .await;
        assert_eq!(st, 400, "{name}: {v}");
        assert!(
            v["error"].as_str().unwrap().contains("space pay owns"),
            "{name}: {v}"
        );
        let v = h
            .ok(
                Method::POST,
                "/import/wiremock",
                PAY_T,
                Some(json!({"mappings": {"mappings": [{"request": {"method": "GET", "url": "/pay/w"}, "response": {"status": 200}}]}, "tag": "wm"})),
            )
            .await;
        assert_eq!(v["imported"], 1, "{name}");
        let (st, v) = h
            .call(
                Method::POST,
                "/stubs",
                BILL_T,
                Some(json!({"request": {"urlPath": "/bill/x"}, "response": {"status": 200}})),
            )
            .await;
        assert_eq!(st, 400, "{name}: bill needs a host matcher: {v}");
        let v = h
            .ok(
                Method::POST,
                "/stubs",
                BILL_T,
                Some(json!({"request": {"urlPath": "/bill/x", "host": {"equalTo": "bill.example.com"}}, "response": {"status": 200}})),
            )
            .await;
        assert_eq!(v["space"], "bill", "{name}");
        let foreign = v["id"].as_str().unwrap().to_string();
        let (st, v) = h
            .call(
                Method::POST,
                "/stubs/import",
                PAY_T,
                Some(json!({"stubs": [{"id": foreign, "request": {"urlPath": "/pay/steal"}, "response": {"status": 200}}]})),
            )
            .await;
        assert_eq!(
            st, 409,
            "{name}: an import cannot take over another space's stub: {v}"
        );
        let msg = v["error"].as_str().unwrap();
        assert!(
            msg.contains("already in use") && !msg.contains("bill"),
            "{name}: the conflict names no other space: {v}"
        );
        let (st, _) = h
            .call(
                Method::PUT,
                &format!("/stubs/{foreign}"),
                PAY_T,
                Some(json!({"request": {"urlPath": "/pay/steal"}, "response": {"status": 200}})),
            )
            .await;
        assert_eq!(st, 404, "{name}: PUT cannot take over another space's stub");
        let (st, _) = h
            .call(
                Method::POST,
                "/stubs?space=bill",
                PAY_T,
                Some(json!({"request": {"urlPath": "/bill/y"}, "response": {"status": 200}})),
            )
            .await;
        assert_eq!(st, 404, "{name}");
        let (st, _) = h
            .call(
                Method::POST,
                "/stubs?space=nope",
                ADMIN,
                Some(json!({"request": {"urlPath": "/x"}, "response": {"status": 200}})),
            )
            .await;
        assert_eq!(st, 404, "{name}: unknown space");
        let v = h
            .ok(
                Method::POST,
                "/stubs",
                ADMIN,
                Some(json!({"request": {"urlPath": "/anything"}, "response": {"status": 200}})),
            )
            .await;
        assert!(
            v.get("space").is_none(),
            "{name}: global principals write globally"
        );
    }
}

#[tokio::test]
async fn space_principals_see_only_their_spaces() {
    for (name, storage) in storages().await {
        let h = H::start(storage, space_tokens(), Some(SPACES)).await;
        assert_eq!(
            h.ok(Method::GET, "/info", PAY_T, None).await["storage"],
            name
        );
        let pay = h
            .ok(
                Method::POST,
                "/stubs",
                PAY_T,
                Some(
                    json!({"request": {"method": "GET", "urlPath": "/pay/order"},
                    "response": {"status": 200, "body": "pay"},
                    "callbacks": [{"url": "http://127.0.0.1:9/hook", "body": "x"}]}),
                ),
            )
            .await;
        let bill = h
            .ok(
                Method::POST,
                "/stubs",
                BILL_T,
                Some(json!({"request": {"method": "GET", "urlPath": "/bill/order", "host": {"equalTo": "bill.example.com"}},
                    "response": {"status": 200, "body": "bill"}})),
            )
            .await;
        let global = h
            .ok(
                Method::POST,
                "/stubs",
                ADMIN,
                Some(json!({"request": {"method": "GET", "urlPath": "/global"}, "response": {"status": 200}})),
            )
            .await;
        let (pay_id, bill_id) = (pay["id"].as_str().unwrap(), bill["id"].as_str().unwrap());
        let v = h.ok(Method::GET, "/stubs", PAY_T, None).await;
        assert_eq!(ids(&v), vec![pay_id.to_string()], "{name}");
        let v = h.ok(Method::GET, "/stubs", MULTI_T, None).await;
        let mut both = vec![pay_id.to_string(), bill_id.to_string()];
        both.sort();
        assert_eq!(ids(&v), both, "{name}: union of the bound spaces");
        assert_eq!(
            v["stubs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["id"] == pay_id)
                .unwrap()["callbacks"][0]["body"],
            "x",
            "{name}"
        );
        let v = h.ok(Method::GET, "/stubs", ADMIN, None).await;
        assert_eq!(ids(&v).len(), 3, "{name}");
        let v = h.ok(Method::GET, "/stubs?space=bill", ADMIN, None).await;
        assert_eq!(ids(&v), vec![bill_id.to_string()], "{name}");
        let (st, _) = h.call(Method::GET, "/stubs?space=bill", PAY_T, None).await;
        assert_eq!(
            st, 404,
            "{name}: spaces the caller is not bound to look unknown"
        );
        let (st, _) = h.call(Method::GET, "/stubs?space=nope", ADMIN, None).await;
        assert_eq!(st, 404, "{name}");
        for m in [Method::GET, Method::DELETE] {
            let (st, _) = h
                .call(m.clone(), &format!("/stubs/{bill_id}"), PAY_T, None)
                .await;
            assert_eq!(st, 404, "{name}: {m} of another space's stub");
            let (st, _) = h
                .call(
                    m.clone(),
                    &format!("/stubs/{}", global["id"].as_str().unwrap()),
                    PAY_T,
                    None,
                )
                .await;
            assert_eq!(st, 404, "{name}: {m} of a global stub");
        }
        let (st, _) = h
            .call(Method::DELETE, &format!("/stubs/{pay_id}"), MULTI_T, None)
            .await;
        assert_eq!(st, 403, "{name}: readable but not writable");
        let (st, _) = h.call(Method::DELETE, "/stubs", PAY_T, None).await;
        assert_eq!(st, 403, "{name}: purging needs a global permission");
        let (st, _) = h
            .call(
                Method::POST,
                "/stubs",
                MULTI_T,
                Some(json!({"request": {"urlPath": "/pay/z"}})),
            )
            .await;
        assert_eq!(st, 403, "{name}");

        assert_eq!(h.hit(Method::GET, "/pay/order").await.0, 200, "{name}");
        assert_eq!(
            h.hit_host("/bill/order", "bill.example.com").await.0,
            200,
            "{name}"
        );
        assert_eq!(h.hit(Method::GET, "/global").await.0, 200, "{name}");
        assert_eq!(h.hit(Method::GET, "/nothing").await.0, 404, "{name}");
        h.wait_journal(4).await;
        assert_eq!(h.journal_total(PAY_T, "").await, 1, "{name}");
        assert_eq!(h.journal_total(BILL_T, "").await, 1, "{name}");
        assert_eq!(h.journal_total(MULTI_T, "").await, 2, "{name}");
        assert_eq!(h.journal_total(ADMIN, "&space=pay").await, 1, "{name}");
        let v = h.ok(Method::GET, "/requests", PAY_T, None).await;
        assert_eq!(v["requests"][0]["space"], "pay", "{name}");
        assert_eq!(v["requests"][0]["path"], "/pay/order", "{name}");
        let (st, _) = h
            .call(Method::GET, "/requests/unmatched", PAY_T, None)
            .await;
        assert_eq!(
            st, 403,
            "{name}: unmatched requests need global journal:read"
        );
        let all = h.ok(Method::GET, "/requests", ADMIN, None).await;
        for e in all["requests"].as_array().unwrap() {
            let (st, _) = h
                .call(Method::GET, &format!("/requests/{}", e["id"]), PAY_T, None)
                .await;
            let mine = e["space"] == "pay";
            assert_eq!(st == 200, mine, "{name}: {e}");
            if !mine {
                assert_eq!(st, 404, "{name}");
            }
        }
        let v = h
            .ok(
                Method::POST,
                "/requests/find",
                PAY_T,
                Some(json!({"method": "GET"})),
            )
            .await;
        assert_eq!(v["total"], 1, "{name}");
        let (st, v) = h
            .call(
                Method::POST,
                "/verify",
                PAY_T,
                Some(json!({"request": {"urlPath": "/bill/order"}, "count": 0})),
            )
            .await;
        assert_eq!(st, 200, "{name}: another space's traffic is invisible: {v}");
        let v = h
            .ok(Method::POST, "/requests/count", MULTI_T, Some(json!({})))
            .await;
        assert_eq!(v["count"], 2, "{name}");
        let v = h
            .ok(
                Method::POST,
                "/requests/near-misses",
                PAY_T,
                Some(json!({"method": "GET", "path": "/bill/order"})),
            )
            .await;
        assert!(
            v["nearMisses"]
                .as_array()
                .unwrap()
                .iter()
                .all(|n| n["stubId"] == pay_id),
            "{name}: {v}"
        );

        let v = h.ok(Method::GET, "/callbacks", PAY_T, None).await;
        assert_eq!(v["callbacks"].as_array().unwrap().len(), 1, "{name}");
        assert_eq!(v["callbacks"][0]["space"], "pay", "{name}");
        let v = h.ok(Method::GET, "/callbacks", BILL_T, None).await;
        assert!(v["callbacks"].as_array().unwrap().is_empty(), "{name}");
        let v = h.ok(Method::DELETE, "/callbacks", BILL_T, None).await;
        assert_eq!(v["deleted"], 0, "{name}");

        let spec = json!({"openapi": "3.0.3", "info": {"title": "t", "version": "1"},
            "paths": {"/pay/a": {"get": {"responses": {"200": {"description": "ok"}}}}}});
        let (st, run) = h
            .call(
                Method::POST,
                "/contract-tests",
                PAY_T,
                Some(json!({"spec": spec, "targetUrl": "http://127.0.0.1:9/"})),
            )
            .await;
        assert_eq!(st, 202, "{name}: {run}");
        let run_id = run["id"].as_str().unwrap();
        let v = h.ok(Method::GET, "/contract-tests", PAY_T, None).await;
        assert_eq!(v["runs"][0]["space"], "pay", "{name}");
        let v = h.ok(Method::GET, "/contract-tests", BILL_T, None).await;
        assert!(v["runs"].as_array().unwrap().is_empty(), "{name}");
        let (st, _) = h
            .call(
                Method::GET,
                &format!("/contract-tests/{run_id}"),
                BILL_T,
                None,
            )
            .await;
        assert_eq!(st, 404, "{name}");
        let (st, _) = h
            .call(
                Method::DELETE,
                &format!("/contract-tests/{run_id}"),
                MULTI_T,
                None,
            )
            .await;
        assert_eq!(st, 403, "{name}");

        let v = h.ok(Method::DELETE, "/requests", PAY_T, None).await;
        assert_eq!(v["deleted"], 1, "{name}: only the space's entries");
        assert_eq!(h.journal_total(ADMIN, "").await, 3, "{name}");
        let v = h.ok(Method::DELETE, "/stubs?tag=none", PAY_T, None).await;
        assert_eq!(v["deleted"], 0, "{name}");
        let v = h
            .ok(Method::GET, "/audit?action=stub.create", ADMIN, None)
            .await;
        let ev = v["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["principal"] == "bill")
            .unwrap();
        assert_eq!(ev["space"], "bill", "{name}");
        assert_eq!(ev["principalKind"], "token", "{name}");
    }
}

#[tokio::test]
async fn state_and_files_are_namespaced_per_space() {
    for (name, storage) in storages().await {
        let h = H::start(storage, space_tokens(), Some(SPACES)).await;
        let flow = |path: &str, label: &str| {
            json!({"request": {"method": "GET", "urlPath": path},
                "response": {"status": 200, "body": format!("{label} {{{{state.current}}}}"), "templated": true},
                "scenario": {"name": "flow", "newState": "Paid"}})
        };
        h.ok(
            Method::POST,
            "/stubs",
            PAY_T,
            Some(flow("/pay/order", "pay")),
        )
        .await;
        let mut bill = flow("/bill/order", "bill");
        bill["request"]["host"] = json!({"equalTo": "bill.example.com"});
        h.ok(Method::POST, "/stubs", BILL_T, Some(bill)).await;
        h.ok(
            Method::POST,
            "/stubs",
            ADMIN,
            Some(flow("/global/order", "global")),
        )
        .await;
        assert_eq!(
            h.hit(Method::GET, "/pay/order").await.1,
            "pay Started",
            "{name}"
        );
        assert_eq!(
            h.hit(Method::GET, "/pay/order").await.1,
            "pay Paid",
            "{name}"
        );
        assert_eq!(
            h.hit_host("/bill/order", "bill.example.com").await.1,
            "bill Started",
            "{name}: the same scenario name is independent per space"
        );
        assert_eq!(
            h.hit(Method::GET, "/global/order").await.1,
            "global Started",
            "{name}"
        );
        let v = h.ok(Method::GET, "/scenarios", PAY_T, None).await;
        assert_eq!(
            v["scenarios"],
            json!([{"name": "flow", "state": "Paid", "space": "pay"}]),
            "{name}"
        );
        let v = h.ok(Method::GET, "/scenarios", ADMIN, None).await;
        assert_eq!(v["scenarios"].as_array().unwrap().len(), 3, "{name}");
        h.ok(
            Method::PUT,
            "/scenarios/flow",
            BILL_T,
            Some(json!({"state": "Refunded"})),
        )
        .await;
        assert_eq!(
            h.hit_host("/bill/order", "bill.example.com").await.1,
            "bill Refunded",
            "{name}"
        );
        assert_eq!(
            h.hit(Method::GET, "/pay/order").await.1,
            "pay Paid",
            "{name}"
        );
        let v = h.ok(Method::DELETE, "/scenarios", PAY_T, None).await;
        assert_eq!(v["reset"], 1, "{name}: only the space's scenarios");
        let v = h.ok(Method::GET, "/scenarios", ADMIN, None).await;
        assert_eq!(v["scenarios"].as_array().unwrap().len(), 2, "{name}");

        for (t, value) in [(PAY_T, "p"), (BILL_T, "b")] {
            let (st, v) = h
                .call(Method::PUT, "/kv/k", t, Some(json!({"value": value})))
                .await;
            assert_eq!(st, 204, "{name}: {v}");
        }
        h.call(Method::PUT, "/kv/k", ADMIN, Some(json!({"value": "g"})))
            .await;
        assert_eq!(
            h.ok(Method::GET, "/kv/k", PAY_T, None).await["value"],
            "p",
            "{name}"
        );
        assert_eq!(
            h.ok(Method::GET, "/kv/k", BILL_T, None).await["value"],
            "b",
            "{name}"
        );
        assert_eq!(
            h.ok(Method::GET, "/kv/k", ADMIN, None).await["value"],
            "g",
            "{name}"
        );
        assert_eq!(
            h.ok(Method::GET, "/kv/k?space=bill", ADMIN, None).await["value"],
            "b",
            "{name}"
        );
        let (st, v) = h.call(Method::GET, "/kv/k", MULTI_T, None).await;
        assert_eq!(
            st, 400,
            "{name}: several spaces need an explicit space: {v}"
        );
        assert_eq!(
            h.ok(Method::GET, "/kv/k?space=pay", MULTI_T, None).await["value"],
            "p",
            "{name}"
        );
        let (st, _) = h.call(Method::DELETE, "/kv/k", PAY_T, None).await;
        assert_eq!(st, 204, "{name}");
        assert_eq!(
            h.ok(Method::GET, "/kv/k", BILL_T, None).await["value"],
            "b",
            "{name}"
        );

        let put = |token: &'static str, path: &'static str, body: &'static str| {
            h.http
                .put(format!("{}/__admin{path}", h.admin))
                .bearer_auth(token)
                .header("content-type", "application/json")
                .body(body)
                .send()
        };
        assert_eq!(
            put(PAY_T, "/files/a.json", "{\"who\":\"pay\"}")
                .await
                .unwrap()
                .status(),
            201
        );
        assert_eq!(
            put(ADMIN, "/files/a.json", "{\"who\":\"global\"}")
                .await
                .unwrap()
                .status(),
            201
        );
        assert_eq!(
            put(ADMIN, "/files/g.json", "{\"who\":\"g\"}")
                .await
                .unwrap()
                .status(),
            201
        );
        let (st, _) = h.call(Method::GET, "/files/a.json", BILL_T, None).await;
        assert_eq!(st, 404, "{name}");
        let v = h.ok(Method::GET, "/files", PAY_T, None).await;
        assert_eq!(v["files"].as_array().unwrap().len(), 1, "{name}");
        assert_eq!(v["files"][0]["name"], "a.json", "{name}");
        assert_eq!(v["files"][0]["space"], "pay", "{name}");
        let v = h.ok(Method::GET, "/files", ADMIN, None).await;
        assert_eq!(v["total"], 3, "{name}");
        h.ok(
            Method::POST,
            "/stubs",
            PAY_T,
            Some(json!({"request": {"urlPath": "/pay/file"}, "response": {"bodyFileName": "a.json"}})),
        )
        .await;
        h.ok(
            Method::POST,
            "/stubs",
            PAY_T,
            Some(json!({"request": {"urlPath": "/pay/global-file"}, "response": {"bodyFileName": "g.json"}})),
        )
        .await;
        assert_eq!(
            h.hit(Method::GET, "/pay/file").await.1,
            "{\"who\":\"pay\"}",
            "{name}"
        );
        assert_eq!(
            h.hit(Method::GET, "/pay/global-file").await.1,
            "{\"who\":\"g\"}",
            "{name}: global files are the fallback"
        );
    }
}

#[tokio::test]
async fn journal_pagination_for_a_scoped_principal() {
    for (name, storage) in storages().await {
        let h = H::start(storage, space_tokens(), Some(SPACES)).await;
        h.ok(
            Method::POST,
            "/stubs",
            PAY_T,
            Some(json!({"request": {"urlPathTemplate": "/pay/{n}"}, "response": {"status": 200}})),
        )
        .await;
        h.ok(
            Method::POST,
            "/stubs",
            BILL_T,
            Some(json!({"request": {"urlPathTemplate": "/bill/{n}", "host": {"equalTo": "bill.example.com"}}, "response": {"status": 200}})),
        )
        .await;
        for i in 0..12 {
            h.hit(Method::GET, &format!("/pay/{i}")).await;
            h.hit_host(&format!("/bill/{i}"), "bill.example.com").await;
            h.hit(Method::GET, &format!("/none/{i}")).await;
            if i % 4 == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
        h.wait_journal(36).await;
        let mut seen = Vec::new();
        for offset in [0, 5, 10] {
            let v = h
                .ok(
                    Method::GET,
                    &format!("/requests?limit=5&offset={offset}"),
                    PAY_T,
                    None,
                )
                .await;
            assert_eq!(v["total"], 12, "{name}");
            let page = v["requests"].as_array().unwrap();
            assert_eq!(
                page.len(),
                if offset == 10 { 2 } else { 5 },
                "{name}: offset {offset}"
            );
            for e in page {
                assert_eq!(e["space"], "pay", "{name}");
                seen.push(e["path"].as_str().unwrap().to_string());
            }
        }
        let want: Vec<String> = (0..12).rev().map(|i| format!("/pay/{i}")).collect();
        assert_eq!(seen, want, "{name}: pages are contiguous and newest first");
        let v = h
            .ok(Method::GET, "/requests/count?space=pay", MULTI_T, None)
            .await;
        assert_eq!(v["count"], 12, "{name}");
        let v = h.ok(Method::GET, "/requests?limit=5", MULTI_T, None).await;
        assert_eq!(v["total"], 24, "{name}");
        let v = h.ok(Method::GET, "/requests?limit=1", ADMIN, None).await;
        assert_eq!(v["total"], 36, "{name}");
    }
}

#[tokio::test]
async fn space_data_never_resolves_across_spaces_or_from_global_names() {
    for (name, storage) in storages().await {
        let h = H::start(storage, space_tokens(), Some(SPACES)).await;
        let put = |token: &'static str, path: &'static str, body: &'static str| {
            h.http
                .put(format!("{}/__admin{path}", h.admin))
                .bearer_auth(token)
                .header("content-type", "application/json")
                .body(body)
                .send()
        };
        assert_eq!(
            put(BILL_T, "/files/secret.json", "{\"who\":\"bill\"}")
                .await
                .unwrap()
                .status(),
            201
        );
        for stub in [
            json!({"request": {"urlPath": "/pay/steal"}, "response": {"bodyFileName": "bill/secret.json"}}),
            json!({"request": {"urlPathTemplate": "/pay/t/{a}/{b}"}, "response": {"bodyFileName": "{{request.path.a}}/{{request.path.b}}", "templated": true}}),
        ] {
            h.ok(Method::POST, "/stubs", PAY_T, Some(stub)).await;
        }
        for path in ["/pay/steal", "/pay/t/bill/secret.json"] {
            let (_, body) = h.hit(Method::GET, path).await;
            assert!(
                !body.contains("bill"),
                "{name}: {path} served another space's file: {body}"
            );
        }

        let (st, v) = h
            .call(
                Method::PUT,
                "/kv/pay%2Fx",
                ADMIN,
                Some(json!({"value": "global"})),
            )
            .await;
        assert_eq!(st, 204, "{name}: {v}");
        let (st, v) = h.call(Method::GET, "/kv/x", PAY_T, None).await;
        assert_eq!(st, 404, "{name}: a global key is not space data: {v}");
        assert_eq!(
            h.ok(Method::GET, "/kv/pay%2Fx", ADMIN, None).await["value"],
            "global",
            "{name}"
        );
        h.call(Method::PUT, "/kv/x", PAY_T, Some(json!({"value": "pay"})))
            .await;
        assert_eq!(
            h.ok(Method::GET, "/kv/pay%2Fx", ADMIN, None).await["value"],
            "global",
            "{name}: space keys do not overwrite global ones"
        );

        h.call(
            Method::PUT,
            "/scenarios/pay%2Fflow",
            ADMIN,
            Some(json!({"state": "Global"})),
        )
        .await;
        let v = h.ok(Method::GET, "/scenarios", PAY_T, None).await;
        assert_eq!(v["scenarios"], json!([]), "{name}");
        let v = h.ok(Method::GET, "/scenarios", ADMIN, None).await;
        assert_eq!(
            v["scenarios"],
            json!([{"name": "pay/flow", "state": "Global"}]),
            "{name}"
        );
        let v = h.ok(Method::DELETE, "/scenarios", PAY_T, None).await;
        assert_eq!(v["reset"], 0, "{name}");

        assert_eq!(
            put(ADMIN, "/files/pay/g.json", "{\"who\":\"global\"}")
                .await
                .unwrap()
                .status(),
            201
        );
        let v = h.ok(Method::GET, "/files", PAY_T, None).await;
        assert_eq!(v["files"], json!([]), "{name}");
        let (st, _) = h.call(Method::GET, "/files/g.json", PAY_T, None).await;
        assert_eq!(st, 404, "{name}");
        let v = h.ok(Method::GET, "/files", ADMIN, None).await;
        let names: Vec<(String, Value)> = v["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| (f["name"].as_str().unwrap().to_string(), f["space"].clone()))
            .collect();
        assert!(
            names.contains(&("pay/g.json".to_string(), Value::Null)),
            "{name}: {names:?}"
        );
        assert!(
            names.contains(&("secret.json".to_string(), json!("bill"))),
            "{name}: {names:?}"
        );
    }
}

const OWNED: &str = r#"
spaces:
  - name: pay
    pathPrefixes: [/pay/]
  - name: bill
    hosts: [bill.example.com]
  - name: free
bindings:
  - subject: token:pay
    role: editor
    spaces: [pay]
  - subject: token:bill
    role: editor
    spaces: [bill]
  - subject: token:free
    role: editor
    spaces: [free]
"#;

const FREE_T: &str = "fm_free_space_token_0123456789abcdef";

#[tokio::test]
async fn owned_paths_and_hosts_are_answered_only_by_their_space_or_global_stubs() {
    for (bad, both) in [
        (
            "spaces:\n  - name: a\n    pathPrefixes: [/pay/]\n  - name: b\n    pathPrefixes: [/pay/x]\n",
            ("a", "b"),
        ),
        (
            "spaces:\n  - name: c\n    pathPrefixes: [/]\n  - name: d\n    pathPrefixes: [/d/]\n",
            ("c", "d"),
        ),
        (
            "spaces:\n  - name: e\n    hosts: [psp.example.com]\n  - name: f\n    hosts: [PSP.example.com]\n",
            ("e", "f"),
        ),
    ] {
        let mut cfg = Config::for_tests(StorageKind::Memory, legacy_tokens());
        cfg.plugins = Some(vec![]);
        cfg.access_file = Some(access_file(bad));
        let e = match fastmock_server::build(cfg, prom()).await {
            Ok(_) => panic!("overlapping ownership accepted: {bad}"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            e.contains(&format!("space {}", both.0)) && e.contains(&format!("space {}", both.1)),
            "{e}"
        );
    }

    for (name, storage) in storages().await {
        let mut tokens = legacy_tokens();
        tokens.push(format!("pay::{}", hash_token(PAY_T)));
        tokens.push(format!("bill::{}", hash_token(BILL_T)));
        tokens.push(format!("free::{}", hash_token(FREE_T)));
        let h = H::start(storage, tokens, Some(OWNED)).await;
        h.ok(
            Method::POST,
            "/stubs",
            FREE_T,
            Some(json!({"request": {"urlPathPattern": ".*"}, "response": {"status": 200, "body": "free"}, "priority": 1})),
        )
        .await;
        h.ok(
            Method::POST,
            "/stubs",
            PAY_T,
            Some(json!({"request": {"urlPath": "/pay/a"}, "response": {"status": 200, "body": "pay"}, "priority": 9})),
        )
        .await;
        h.ok(
            Method::POST,
            "/stubs",
            BILL_T,
            Some(json!({"request": {"urlPath": "/b", "host": {"equalTo": "bill.example.com"}}, "response": {"status": 200, "body": "bill"}, "priority": 9})),
        )
        .await;
        h.ok(
            Method::POST,
            "/stubs",
            ADMIN,
            Some(json!({"request": {"urlPath": "/pay/g"}, "response": {"status": 200, "body": "global"}, "priority": 9})),
        )
        .await;
        assert_eq!(h.hit(Method::GET, "/pay/a").await.1, "pay", "{name}");
        assert_eq!(h.hit(Method::GET, "/pay/g").await.1, "global", "{name}");
        let (_, body) = h.hit(Method::GET, "/pay/other").await;
        assert_ne!(body, "free", "{name}: another space answered an owned path");
        assert_eq!(h.hit(Method::GET, "/other").await.1, "free", "{name}");
        assert_eq!(
            h.hit_host("/b", "bill.example.com").await.1,
            "bill",
            "{name}"
        );
        let (_, body) = h.hit_host("/x", "bill.example.com").await;
        assert_ne!(body, "free", "{name}: another space answered an owned host");
        assert_eq!(
            h.hit_host("/x", "other.example.com").await.1,
            "free",
            "{name}"
        );
    }
}

#[tokio::test]
async fn admin_only_denials_keep_the_previous_body_for_legacy_tokens() {
    let h = H::start(StorageKind::Memory, legacy_tokens(), None).await;
    let raw = |m: Method, path: &'static str, token: &'static str| {
        let rb = h
            .http
            .request(m, format!("{}/__admin{path}", h.admin))
            .bearer_auth(token);
        async move {
            let r = rb.send().await.unwrap();
            (r.status().as_u16(), r.text().await.unwrap())
        }
    };
    let handler = r#"{"error":"requires role admin","details":[]}"#;
    assert_eq!(
        raw(Method::GET, "/audit", VIEWER).await,
        (403, handler.to_string())
    );
    assert_eq!(
        raw(Method::GET, "/audit", EDITOR).await,
        (403, handler.to_string())
    );
    assert_eq!(
        raw(Method::POST, "/reset", EDITOR).await,
        (403, handler.to_string())
    );
    assert_eq!(
        raw(Method::POST, "/reset", VIEWER).await,
        (
            403,
            r#"{"error":"forbidden","details":["requires role editor"],"required":"editor"}"#
                .to_string()
        )
    );
    assert_eq!(raw(Method::GET, "/audit", ADMIN).await.0, 200);
}

#[tokio::test]
async fn info_counts_only_the_stubs_the_caller_can_read() {
    let h = H::start(StorageKind::Memory, space_tokens(), Some(SPACES)).await;
    h.ok(
        Method::POST,
        "/stubs",
        PAY_T,
        Some(json!({"request": {"urlPath": "/pay/a"}, "response": {"status": 200}})),
    )
    .await;
    h.ok(
        Method::POST,
        "/stubs",
        BILL_T,
        Some(json!({"request": {"urlPath": "/bill/a", "host": {"equalTo": "bill.example.com"}}, "response": {"status": 200}})),
    )
    .await;
    h.ok(
        Method::POST,
        "/stubs",
        ADMIN,
        Some(json!({"request": {"urlPath": "/g"}, "response": {"status": 200}})),
    )
    .await;
    h.app.engine.reload().await.unwrap();
    assert_eq!(
        h.ok(Method::GET, "/info", ADMIN, None).await["stubsLoaded"],
        3
    );
    assert_eq!(
        h.ok(Method::GET, "/info", PAY_T, None).await["stubsLoaded"],
        1
    );
    assert_eq!(
        h.ok(Method::GET, "/info", MULTI_T, None).await["stubsLoaded"],
        2
    );
}

/// Unknown admin paths answer as in earlier versions when neither an access file nor OIDC is set
/// (GET needs viewer, other methods editor, then 404); with an access file only admins pass.
#[tokio::test]
async fn unknown_admin_paths_keep_the_previous_answers_without_access_control() {
    let h = H::start(StorageKind::Memory, legacy_tokens(), None).await;
    let raw = |m: Method, path: &'static str, token: &'static str| {
        let rb = h
            .http
            .request(m, format!("{}/__admin{path}", h.admin))
            .bearer_auth(token);
        async move {
            let r = rb.send().await.unwrap();
            (r.status().as_u16(), r.text().await.unwrap())
        }
    };
    let not_found = r#"{"error":"not found","details":[]}"#.to_string();
    let editor_needed =
        r#"{"error":"forbidden","details":["requires role editor"],"required":"editor"}"#
            .to_string();
    assert_eq!(
        raw(Method::GET, "/nope", VIEWER).await,
        (404, not_found.clone())
    );
    assert_eq!(
        raw(Method::GET, "/stubs/a/b/c", VIEWER).await,
        (404, not_found.clone())
    );
    assert_eq!(
        raw(Method::POST, "/nope", VIEWER).await,
        (403, editor_needed.clone())
    );
    assert_eq!(
        raw(Method::DELETE, "/nope/x", VIEWER).await,
        (403, editor_needed)
    );
    assert_eq!(
        raw(Method::POST, "/nope", EDITOR).await,
        (404, not_found.clone())
    );
    assert_eq!(raw(Method::GET, "/nope", ADMIN).await, (404, not_found));

    let h = H::start(StorageKind::Memory, access_tokens(), Some(ACCESS)).await;
    let (st, v) = h.call(Method::GET, "/nope", VIEWER, None).await;
    assert_eq!((st, v["required"].clone()), (403, json!("admin")), "{v}");
    let (st, _) = h.call(Method::GET, "/nope", ADMIN, None).await;
    assert_eq!(st, 404);
}

/// Stubs stored before the access file gained prefixes (or lost the space) are reported in `/info` to the
/// principals that can read them, and every process start has its own instance id.
#[tokio::test]
async fn info_reports_stored_stubs_that_break_their_space_rules() {
    for (name, storage) in storages().await {
        let h = H::start(storage, space_tokens(), Some(SPACES)).await;
        let outside: fastmock_core::Stub = serde_json::from_value(json!({
            "request": {"urlPath": "/elsewhere/x"}, "response": {"status": 200}, "space": "pay"
        }))
        .unwrap();
        let orphan: fastmock_core::Stub = serde_json::from_value(json!({
            "request": {"urlPath": "/gone/x"}, "response": {"status": 200}, "space": "gone"
        }))
        .unwrap();
        let fine: fastmock_core::Stub = serde_json::from_value(json!({
            "request": {"urlPath": "/pay/x"}, "response": {"status": 200}, "space": "pay"
        }))
        .unwrap();
        for s in [&outside, &orphan, &fine] {
            h.app.stores.stubs.upsert(s).await.unwrap();
        }
        h.app.engine.reload().await.unwrap();

        let v = h.ok(Method::GET, "/info", ADMIN, None).await;
        let warnings: Vec<String> = serde_json::from_value(v["warnings"].clone()).unwrap();
        assert_eq!(warnings.len(), 2, "{name}: {warnings:?}");
        assert!(
            warnings
                .iter()
                .any(|w| w.contains(&outside.id.to_string())
                    && w.contains("/elsewhere/x lies outside")),
            "{name}: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains(&orphan.id.to_string())
                    && w.contains("space gone is not defined")),
            "{name}: {warnings:?}"
        );
        let v = h.ok(Method::GET, "/info", PAY_T, None).await;
        let warnings: Vec<String> = serde_json::from_value(v["warnings"].clone()).unwrap();
        assert_eq!(warnings.len(), 1, "{name}: {warnings:?}");
        let v = h.ok(Method::GET, "/info", BILL_T, None).await;
        assert!(v.get("warnings").is_none(), "{name}: {v}");

        let id = v["instanceId"].as_str().unwrap().to_string();
        assert_eq!(id.len(), 32, "{name}: {id}");
        assert_eq!(
            h.ok(Method::GET, "/info", ADMIN, None).await["instanceId"],
            json!(id),
            "{name}: stable while the process runs"
        );
        let other = H::start(StorageKind::Memory, space_tokens(), Some(SPACES)).await;
        assert_ne!(
            other.ok(Method::GET, "/info", ADMIN, None).await["instanceId"],
            json!(id),
            "{name}: new on every start"
        );
    }
}

/// With an access file every 403 has the `forbidden` shape, and `required` names what the operation
/// really needs (admin-only operations name admin, not the editor role of the route).
#[tokio::test]
async fn every_forbidden_answer_has_one_shape_with_an_access_file() {
    let h = H::start(StorageKind::Memory, access_tokens(), Some(ACCESS)).await;
    let forbidden = |detail: &str, required: &str| json!({"error": "forbidden", "details": [detail], "required": required});
    let admin_needed = forbidden("requires role admin", "admin");
    let secret = json!({"spec": {"openapi": "3.0.3", "info": {"title": "t", "version": "1"}, "paths": {}},
        "targetUrl": "http://127.0.0.1:9/", "secretHeaders": {"x-api-key": "k"}});
    let token_id = uuid::Uuid::now_v7();
    for (m, path, token, body, want) in [
        (
            Method::GET,
            "/audit".to_string(),
            EDITOR,
            None,
            admin_needed.clone(),
        ),
        (
            Method::POST,
            "/reset".to_string(),
            EDITOR,
            None,
            admin_needed.clone(),
        ),
        (
            Method::POST,
            "/reset".to_string(),
            VIEWER,
            None,
            admin_needed.clone(),
        ),
        (
            Method::DELETE,
            "/stubs".to_string(),
            VIEWER,
            None,
            admin_needed.clone(),
        ),
        (
            Method::DELETE,
            "/stubs".to_string(),
            EDITOR,
            None,
            admin_needed.clone(),
        ),
        (
            Method::DELETE,
            "/stubs?tag=x".to_string(),
            VIEWER,
            None,
            forbidden("requires role editor", "editor"),
        ),
        (
            Method::DELETE,
            "/stubs".to_string(),
            AUTHOR,
            None,
            forbidden("requires permission stubs:purge", "stubs:purge"),
        ),
        (
            Method::DELETE,
            format!("/tokens/{token_id}"),
            PAY,
            None,
            forbidden("requires permission tokens:write", "tokens:write"),
        ),
        (
            Method::DELETE,
            format!("/tokens/{token_id}"),
            VIEWER,
            None,
            forbidden("requires role admin", "admin"),
        ),
        (
            Method::POST,
            "/contract-tests".to_string(),
            EDITOR,
            Some(secret.clone()),
            admin_needed.clone(),
        ),
        (
            Method::POST,
            "/contract-tests?space=payments".to_string(),
            PAY,
            Some(secret.clone()),
            forbidden("requires permission contracts:secrets", "contracts:secrets"),
        ),
    ] {
        let (st, v) = h.call(m.clone(), &path, token, body).await;
        assert_eq!((st, &v), (403, &want), "{m} {path}");
    }
    let r = h
        .http
        .get(format!("{}/__admin/stubs", h.admin))
        .bearer_auth(ADMIN)
        .header("origin", "https://evil.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    assert_eq!(
        r.json::<Value>().await.unwrap(),
        json!({"error": "forbidden", "details": ["origin not allowed"]})
    );

    let legacy = H::start(StorageKind::Memory, legacy_tokens(), None).await;
    let r = legacy
        .http
        .get(format!("{}/__admin/stubs", legacy.admin))
        .bearer_auth(ADMIN)
        .header("origin", "https://evil.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.json::<Value>().await.unwrap(),
        json!({"error": "origin not allowed", "details": []}),
        "the body of earlier versions without an access file"
    );
}

/// `contracts:secrets` is global: `admin` bound in a space lacks it, a custom role holding it must be
/// bound globally, and the access file refuses it in a space binding.
#[tokio::test]
async fn contract_secrets_need_the_permission_globally() {
    const DOC: &str = r#"
roles:
  - name: secret-runner
    permissions: [contracts:read, contracts:write, contracts:secrets]
spaces:
  - name: payments
bindings:
  - subject: token:spadmin
    role: admin
    spaces: [payments]
  - subject: token:runner
    role: secret-runner
    global: true
"#;
    const SPADMIN: &str = "fm_space_admin_token_for_access_tests_0123";
    const RUNNER: &str = "fm_secret_runner_token_for_access_tests_012";
    let mut tokens = legacy_tokens();
    tokens.push(format!("spadmin::{}", hash_token(SPADMIN)));
    tokens.push(format!("runner::{}", hash_token(RUNNER)));
    let h = H::start(StorageKind::Memory, tokens.clone(), Some(DOC)).await;
    let body = json!({"spec": {"openapi": "3.0.3", "info": {"title": "t", "version": "1"}, "paths": {}},
        "targetUrl": "http://127.0.0.1:9/", "secretHeaders": {"x-api-key": "k"}});
    let (st, v) = h
        .call(
            Method::POST,
            "/contract-tests?space=payments",
            SPADMIN,
            Some(body.clone()),
        )
        .await;
    assert_eq!(
        (st, v),
        (
            403,
            json!({"error": "forbidden", "details": ["requires permission contracts:secrets"], "required": "contracts:secrets"})
        )
    );
    let info = h.ok(Method::GET, "/info", SPADMIN, None).await;
    let perms = info["principal"]["permissions"]["spaces"]["payments"]
        .as_array()
        .cloned()
        .unwrap();
    assert!(perms.contains(&json!("contracts:write")), "{info}");
    assert!(!perms.contains(&json!("contracts:secrets")), "{info}");
    for token in [RUNNER, ADMIN] {
        let (st, v) = h
            .call(Method::POST, "/contract-tests", token, Some(body.clone()))
            .await;
        assert_eq!(st, 400, "past the permission check: {v}");
    }

    let mut cfg = Config::for_tests(StorageKind::Memory, tokens);
    cfg.plugins = Some(vec![]);
    cfg.access_file = Some(access_file(&DOC.replace(
        "role: secret-runner\n    global: true",
        "role: secret-runner\n    spaces: [payments]",
    )));
    let e = match fastmock_server::build(cfg, prom()).await {
        Ok(_) => panic!("a space binding of contracts:secrets was accepted"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        e.contains("bindings[1] (token:runner): role `secret-runner` lists contracts:secrets, a global permission"),
        "{e}"
    );
    assert_eq!(e.lines().count(), 1, "{e}");
}

/// An id taken by a stub the caller cannot see (or in another area) is "already in use"; only an id of a
/// visible stub in the same area suggests PUT.
#[tokio::test]
async fn cross_space_id_conflicts_do_not_suggest_put() {
    for (name, storage) in storages().await {
        let h = H::start(storage, space_tokens(), Some(SPACES)).await;
        let id = uuid::Uuid::now_v7();
        h.ok(
            Method::POST,
            "/stubs",
            PAY_T,
            Some(json!({"id": id, "request": {"urlPath": "/pay/a"}, "response": {"status": 200}})),
        )
        .await;
        for (token, stub) in [
            (
                BILL_T,
                json!({"id": id, "request": {"urlPath": "/bill/a", "host": {"equalTo": "bill.example.com"}}, "response": {"status": 200}}),
            ),
            (
                ADMIN,
                json!({"id": id, "request": {"urlPath": "/g"}, "response": {"status": 200}}),
            ),
        ] {
            let (st, v) = h.call(Method::POST, "/stubs", token, Some(stub)).await;
            assert_eq!(
                (st, v),
                (409, json!({"error": "id is already in use", "details": []})),
                "{name}"
            );
        }
        let (st, v) = h
            .call(
                Method::POST,
                "/stubs",
                PAY_T,
                Some(json!({"id": id, "request": {"urlPath": "/pay/b"}, "response": {"status": 200}})),
            )
            .await;
        assert_eq!(
            (st, v["error"].clone()),
            (409, json!("stub with this id exists; use PUT")),
            "{name}"
        );
    }
}

const HOSTS: &str = r#"
spaces:
  - name: one
    pathPrefixes: [/one/]
    hosts: [one.example.com]
  - name: many
    hosts: [a.example.com, b.example.com]
  - name: plain
    pathPrefixes: [/plain/]
"#;

/// Every importer takes `host`; imports into a space with one host default to it, a space with several
/// hosts needs it, and the stubs get `request.host`.
#[tokio::test]
async fn imports_into_spaces_that_own_hosts_set_request_host() {
    let h = H::start(StorageKind::Memory, legacy_tokens(), Some(HOSTS)).await;
    let openapi = |path: &str| {
        json!({"spec": {"openapi": "3.0.3", "info": {"title": "t", "version": "1"},
            "paths": {path: {"get": {"responses": {"200": {"description": "ok"}}}}}}, "tag": "oa"})
    };
    h.ok(
        Method::POST,
        "/import/openapi?space=one",
        ADMIN,
        Some(openapi("/one/pets")),
    )
    .await;
    h.ok(
        Method::POST,
        "/import/wiremock?space=one",
        ADMIN,
        Some(json!({"mappings": {"mappings": [{"request": {"method": "GET", "url": "/one/wm"}, "response": {"status": 200, "body": "wm"}}]}})),
    )
    .await;
    h.ok(
        Method::POST,
        "/import/mockserver?space=one",
        ADMIN,
        Some(json!([{"httpRequest": {"method": "GET", "path": "/one/ms"}, "httpResponse": {"statusCode": 200}}])),
    )
    .await;
    h.ok(
        Method::POST,
        "/import/postman?space=one",
        ADMIN,
        Some(json!({"collection": {"info": {"name": "Shop", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
            "item": [{"name": "Get", "request": {"method": "GET", "url": "https://api.example.com/one/pm"},
                "response": [{"name": "ok", "code": 200, "body": "pm"}]}]}})),
    )
    .await;
    h.ok(
        Method::POST,
        "/import/har?space=one",
        ADMIN,
        Some(json!({"har": {"log": {"version": "1.2", "entries": [{"request": {"method": "GET", "url": "https://api.example.com/one/har", "headers": []},
            "response": {"status": 200, "headers": [], "content": {"mimeType": "text/plain", "text": "har"}}}]}}})),
    )
    .await;
    let r = h
        .http
        .post(format!("{}/__admin/import?space=one&tag=auto", h.admin))
        .bearer_auth(ADMIN)
        .body(r#"{"mappings": [{"request": {"method": "GET", "url": "/one/auto"}, "response": {"status": 200}}]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    h.ok(
        Method::POST,
        "/stubs/import?space=one",
        ADMIN,
        Some(
            json!({"stubs": [{"request": {"urlPath": "/one/bulk"}, "response": {"status": 200}}]}),
        ),
    )
    .await;
    let v = h.ok(Method::GET, "/stubs?space=one", ADMIN, None).await;
    let stubs = v["stubs"].as_array().unwrap();
    assert_eq!(stubs.len(), 7, "{v}");
    for s in stubs {
        assert_eq!(
            s["request"]["host"],
            json!({"equalTo": "one.example.com"}),
            "{s}"
        );
    }
    assert_eq!(h.hit_host("/one/wm", "one.example.com").await.0, 200);

    let (st, v) = h
        .call(
            Method::POST,
            "/import/openapi?space=many",
            ADMIN,
            Some(openapi("/m")),
        )
        .await;
    assert_eq!(st, 400, "{v}");
    assert_eq!(
        v["error"],
        "space many owns several hosts (a.example.com, b.example.com): set the host parameter to the one the imported stubs answer"
    );
    let (st, v) = h
        .call(
            Method::POST,
            "/import/openapi?space=many&host=c.example.com",
            ADMIN,
            Some(openapi("/m")),
        )
        .await;
    assert_eq!(st, 400, "{v}");
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("is not a host of space many"),
        "{v}"
    );
    h.ok(
        Method::POST,
        "/import/openapi?space=many&host=B.example.com",
        ADMIN,
        Some(openapi("/m")),
    )
    .await;
    let v = h.ok(Method::GET, "/stubs?space=many", ADMIN, None).await;
    assert_eq!(
        v["stubs"][0]["request"]["host"],
        json!({"equalTo": "b.example.com"})
    );

    h.ok(
        Method::POST,
        "/import/openapi?space=plain",
        ADMIN,
        Some(openapi("/plain/x")),
    )
    .await;
    let v = h.ok(Method::GET, "/stubs?space=plain", ADMIN, None).await;
    assert!(v["stubs"][0]["request"].get("host").is_none(), "{v}");
    let r = h
        .http
        .post(format!(
            "{}/__admin/import?format=graphql&host=a.example.com",
            h.admin
        ))
        .bearer_auth(ADMIN)
        .body("type Query { a: Int }")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

/// With authentication disabled every caller is an anonymous admin: managed tokens minted then would
/// outlive the mode, so they cannot be created or revoked.
#[tokio::test]
async fn managed_tokens_need_authentication() {
    let mut cfg = Config::for_tests(StorageKind::Memory, vec![]);
    cfg.plugins = Some(vec![]);
    cfg.auth_disabled = true;
    let h = H::start_cfg(cfg).await;
    let (st, v) = h
        .call(
            Method::POST,
            "/tokens",
            "anything",
            Some(json!({"name": "ci", "role": "admin", "global": true})),
        )
        .await;
    assert_eq!(
        (st, v),
        (
            409,
            json!({"error": "managed tokens cannot be created while authentication is disabled (FASTMOCK_AUTH_DISABLED=true)", "details": []})
        )
    );
    let (st, v) = h
        .call(
            Method::DELETE,
            &format!("/tokens/{}", uuid::Uuid::now_v7()),
            "anything",
            None,
        )
        .await;
    assert_eq!(st, 409, "{v}");
    assert!(v["error"].as_str().unwrap().contains("cannot be revoked"));
    assert_eq!(
        h.ok(Method::GET, "/tokens", "anything", None).await["tokens"],
        json!([])
    );
}

/// Runs `fastmock` with only the given `FASTMOCK_*` variables; a server that does not exit within 30
/// seconds is killed. Returns whether it succeeded and its output.
fn run_fastmock(args: &[&str], env: &[(&str, String)], stdin: Option<&str>) -> (bool, String) {
    use std::io::Write;
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_fastmock"));
    for (k, _) in std::env::vars() {
        if k.starts_with("FASTMOCK_") {
            cmd.env_remove(k);
        }
    }
    cmd.args(args)
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    drop(child.stdin.take());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let o = child.wait_with_output().unwrap();
    (
        o.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
    )
}

fn serve_env(extra: &[(&'static str, String)]) -> Vec<(&'static str, String)> {
    let mut env = vec![
        ("FASTMOCK_STORAGE", "memory".to_string()),
        ("FASTMOCK_LISTEN", "127.0.0.1:0".to_string()),
        ("FASTMOCK_ADMIN_LISTEN", "127.0.0.1:1".to_string()),
        ("FASTMOCK_LOG_FORMAT", "text".to_string()),
        (
            "FASTMOCK_ADMIN_TOKENS",
            format!("ci:viewer:{}", hash_token("x")),
        ),
    ];
    env.extend_from_slice(extra);
    env
}

#[test]
fn serve_refuses_a_blank_api_audience() {
    for blank in ["", "  "] {
        let (ok, text) = run_fastmock(
            &["serve"],
            &serve_env(&[
                ("FASTMOCK_OIDC_ISSUER", "http://127.0.0.1:9/realms/x".into()),
                ("FASTMOCK_OIDC_CLIENT_ID", "fastmock".into()),
                ("FASTMOCK_OIDC_CLIENT_SECRET", "s3cret".into()),
                (
                    "FASTMOCK_OIDC_REDIRECT_URL",
                    "http://127.0.0.1:9/__admin/auth/callback".into(),
                ),
                ("FASTMOCK_OIDC_API_AUDIENCE", blank.into()),
            ]),
            None,
        );
        assert!(!ok, "{text}");
        assert!(
            text.contains("FASTMOCK_OIDC_API_AUDIENCE is set but empty"),
            "{text}"
        );
    }
}

#[test]
fn serve_names_the_binding_of_an_unknown_token_on_one_line() {
    let f = access_file(
        "bindings:\n  - {subject: 'user:a', role: viewer, global: true}\n  - {subject: 'token:nosuch', role: viewer, global: true}\n",
    );
    let (ok, text) = run_fastmock(
        &["serve"],
        &serve_env(&[("FASTMOCK_ACCESS_FILE", f.display().to_string())]),
        None,
    );
    assert!(!ok, "{text}");
    let want = format!(
        "access file {}: bindings[1] (token:nosuch): no token named nosuch in FASTMOCK_ADMIN_TOKENS or FASTMOCK_ADMIN_TOKENS_FILE",
        f.display()
    );
    let line = text
        .lines()
        .find(|l| l.contains("bindings[1]"))
        .unwrap_or_default();
    assert_eq!(line, format!("Error: {want}"), "{text}");
    assert!(!text.contains("Caused by"), "{text}");

    let f = access_file("roles: [{name: stub-author, permissions: [stubs:wrte]}]\n");
    let (ok, text) = run_fastmock(
        &["serve"],
        &serve_env(&[("FASTMOCK_ACCESS_FILE", f.display().to_string())]),
        None,
    );
    assert!(!ok, "{text}");
    assert!(
        text.contains(&format!(
            "Error: access file {}: roles[0] (stub-author): unknown permission `stubs:wrte`",
            f.display()
        )),
        "{text}"
    );
    assert!(!text.contains("Caused by"), "{text}");
}

#[test]
fn hash_token_prints_only_the_hex_hash() {
    let want = format!("{}\n", hash_token("my-dev-token"));
    let (ok, out) = run_fastmock(&["hash-token", "my-dev-token"], &[], None);
    assert!(ok);
    assert_eq!(out, want);
    let (ok, out) = run_fastmock(&["hash-token"], &[], Some("my-dev-token\n"));
    assert!(ok);
    assert_eq!(out, want);
    let (ok, out) = run_fastmock(&["hash-token", "--generate"], &[], None);
    assert!(ok);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2, "{out}");
    let token = lines[0].strip_prefix("token: fm_").unwrap();
    assert_eq!(token.len(), 64);
    assert_eq!(
        lines[1],
        format!("hash:  {}", hash_token(&format!("fm_{token}")))
    );
}
