//! The control-plane MCP endpoint `/mcp`: authentication, permissions per space, auditing, bounded
//! results and an end-to-end flow with a real MCP client. Runs on memory storage and, with
//! FASTMOCK_TEST_DATABASE_URL and FASTMOCK_TEST_VALKEY_URL, on PostgreSQL and Valkey.

use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ReadResourceRequestParams};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use sqlx::Connection;
use tokio::sync::watch;

const ADMIN: &str = "fm_admin_token_for_mcp_tests_0123456789";
const VIEWER: &str = "fm_viewer_token_for_mcp_tests_0123456789";
const PAY: &str = "fm_payments_editor_for_mcp_tests_0123456789";
const READER: &str = "fm_stub_reader_for_mcp_tests_0123456789";

const ACCESS: &str = r#"
roles:
  - name: stub-reader
    permissions: [stubs:read]
spaces:
  - name: payments
    pathPrefixes: [/payments/]
  - name: billing
    pathPrefixes: [/billing/]
bindings:
  - subject: token:pay
    role: editor
    spaces: [payments]
  - subject: token:reader
    role: stub-reader
    global: true
"#;

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

fn tokens() -> Vec<String> {
    vec![
        format!("ops:admin:{}", hash_token(ADMIN)),
        format!("dash:viewer:{}", hash_token(VIEWER)),
        format!("pay::{}", hash_token(PAY)),
        format!("reader::{}", hash_token(READER)),
    ]
}

async fn pg_storage() -> Option<StorageKind> {
    let admin_url = std::env::var("FASTMOCK_TEST_DATABASE_URL").ok()?;
    let valkey_url = std::env::var("FASTMOCK_TEST_VALKEY_URL").ok()?;
    let db = format!("fm_mcp_{}", uuid::Uuid::new_v4().simple());
    let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut c)
        .await
        .unwrap();
    Some(StorageKind::Postgres {
        database_url: format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db),
        valkey_url,
        valkey_prefix: format!("mcp:{}:", uuid::Uuid::new_v4().simple()),
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

struct H {
    data: String,
    admin: String,
    http: reqwest::Client,
    _dir: std::path::PathBuf,
    _shutdown: watch::Sender<bool>,
}

impl H {
    async fn start(storage: StorageKind) -> H {
        Self::start_with(storage, |_| {}).await
    }

    async fn start_with(storage: StorageKind, tweak: impl FnOnce(&mut Config)) -> H {
        let dir = std::env::temp_dir().join(format!("fm-mcp-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("access.yaml");
        std::fs::write(&file, ACCESS).unwrap();
        let mut cfg = Config::for_tests(storage, tokens());
        cfg.plugins = Some(vec![]);
        cfg.access_file = Some(file);
        tweak(&mut cfg);
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
            _dir: dir,
            _shutdown: tx,
        }
    }

    async fn raw(&self, token: Option<&str>, origin: Option<&str>, body: Value) -> (u16, Value) {
        let mut rb = self
            .http
            .post(format!("{}/mcp", self.admin))
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-06-18")
            .json(&body);
        if let Some(t) = token {
            rb = rb.bearer_auth(t);
        }
        if let Some(o) = origin {
            rb = rb.header("origin", o);
        }
        let r = rb.send().await.unwrap();
        let st = r.status().as_u16();
        let text = r.text().await.unwrap();
        let json_text = text
            .lines()
            .find_map(|l| l.strip_prefix("data:"))
            .map(|d| d.trim().to_string())
            .unwrap_or(text);
        (
            st,
            serde_json::from_str(&json_text).unwrap_or(Value::String(json_text)),
        )
    }

    async fn tool(&self, token: &str, name: &str, args: Value) -> Value {
        let (st, b) = self
            .raw(
                Some(token),
                None,
                json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": name, "arguments": args}}),
            )
            .await;
        assert_eq!(st, 200, "{name}: {b}");
        b["result"].clone()
    }

    async fn ok(&self, token: &str, name: &str, args: Value) -> Value {
        let r = self.tool(token, name, args).await;
        assert_ne!(r["isError"], true, "{name}: {r}");
        r["structuredContent"].clone()
    }

    async fn err(&self, token: &str, name: &str, args: Value) -> String {
        let r = self.tool(token, name, args).await;
        assert_eq!(r["isError"], true, "{name} should fail: {r}");
        r["content"][0]["text"].as_str().unwrap().to_string()
    }

    async fn names(&self, token: &str) -> Vec<String> {
        let (st, b) = self
            .raw(
                Some(token),
                None,
                json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
            )
            .await;
        assert_eq!(st, 200, "{b}");
        let mut v: Vec<String> = b["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        v.sort();
        v
    }

    async fn admin_get(&self, path: &str) -> Value {
        self.http
            .get(format!("{}/__admin{path}", self.admin))
            .bearer_auth(ADMIN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn hit(&self, method: reqwest::Method, path: &str, body: Option<&str>) -> (u16, String) {
        let mut rb = self.http.request(method, format!("{}{path}", self.data));
        if let Some(b) = body {
            rb = rb
                .header("content-type", "application/json")
                .body(b.to_string());
        }
        let r = rb.send().await.unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }

    async fn journal_settles(&self, n: usize) {
        for _ in 0..100 {
            let v = self.admin_get("/requests?limit=100").await;
            if v["requests"].as_array().is_some_and(|l| l.len() >= n) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("journal never reached {n} entries");
    }
}

fn stub(path: &str, body: Value) -> Value {
    json!({"request": {"method": "GET", "urlPath": path}, "response": {"status": 200, "jsonBody": body}})
}

#[tokio::test]
async fn mcp_needs_the_admin_credentials_and_origin() {
    for (label, storage) in storages().await {
        let h = H::start(storage).await;
        let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18",
            "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}});
        let (st, _) = h.raw(None, None, init.clone()).await;
        assert_eq!(st, 401, "{label}");
        let (st, _) = h
            .raw(Some("fm_wrong_token_0123456789"), None, init.clone())
            .await;
        assert_eq!(st, 401, "{label}");
        let (st, b) = h
            .raw(Some(VIEWER), Some("https://evil.example.com"), init.clone())
            .await;
        assert_eq!(
            st, 403,
            "{label}: a foreign origin is refused like on the admin API: {b}"
        );
        let (st, b) = h.raw(Some(VIEWER), None, init).await;
        assert_eq!(st, 200, "{label}: {b}");
        assert_eq!(b["result"]["serverInfo"]["name"], "fastmock", "{b}");
        assert!(b["result"]["capabilities"]["tools"].is_object(), "{b}");
        let (st, b) = h
            .raw(
                Some(VIEWER),
                None,
                json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "rm_rf", "arguments": {}}}),
            )
            .await;
        assert_eq!(st, 200);
        assert!(
            b["error"].is_object(),
            "{label}: unknown tools are protocol errors: {b}"
        );
    }
}

#[tokio::test]
async fn built_in_roles_without_an_access_file() {
    let h = H::start_with(StorageKind::Memory, |c| {
        c.access_file = None;
        c.admin_tokens = vec![
            format!("ops:admin:{}", hash_token(ADMIN)),
            format!("dash:viewer:{}", hash_token(VIEWER)),
        ];
    })
    .await;
    let e = h
        .err(
            VIEWER,
            "create_stub",
            json!({"stub": stub("/x", json!({}))}),
        )
        .await;
    assert!(
        e.contains("403") && e.contains("requires role editor"),
        "{e}"
    );
    h.ok(ADMIN, "create_stub", json!({"stub": stub("/x", json!({}))}))
        .await;
    let l = h.ok(VIEWER, "list_stubs", json!({})).await;
    assert_eq!(l["total"], 1, "{l}");
    let e = h.err(VIEWER, "reset_journal", json!({})).await;
    assert!(e.contains("403"), "{e}");
}

#[tokio::test]
async fn mcp_can_be_switched_off() {
    let h = H::start_with(StorageKind::Memory, |c| c.mcp_enabled = false).await;
    let (st, _) = h
        .raw(
            Some(ADMIN),
            None,
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await;
    assert_eq!(st, 404);
}

#[tokio::test]
async fn tools_follow_permissions_scopes_and_spaces() {
    for (label, storage) in storages().await {
        let h = H::start(storage).await;
        let reader = h.names(READER).await;
        assert_eq!(
            reader,
            vec!["get_stub", "list_stubs", "validate_stub"],
            "{label}: a stubs:read token sees only the tools it can use"
        );
        let viewer = h.names(VIEWER).await;
        assert!(viewer.contains(&"find_requests".to_string()), "{viewer:?}");
        assert!(!viewer.contains(&"create_stub".to_string()), "{viewer:?}");
        assert!(
            !viewer.contains(&"wait_for_mail".to_string()),
            "mail is off: {viewer:?}"
        );
        let admin = h.names(ADMIN).await;
        assert!(admin.contains(&"reset_journal".to_string()), "{admin:?}");

        let e = h
            .err(
                VIEWER,
                "create_stub",
                json!({"stub": stub("/x", json!({}))}),
            )
            .await;
        assert!(e.contains("403") && e.contains("editor"), "{label}: {e}");
        let e = h.err(READER, "find_requests", json!({})).await;
        assert!(
            e.contains("403") && e.contains("journal:read"),
            "{label}: {e}"
        );

        let created = h
            .ok(
                PAY,
                "create_stub",
                json!({"stub": stub("/payments/a", json!({"ok": 1})), "space": "payments"}),
            )
            .await;
        assert_eq!(created["space"], "payments", "{label}: {created}");
        let pay_id = created["id"].as_str().unwrap().to_string();
        let e = h
            .err(
                PAY,
                "create_stub",
                json!({"stub": stub("/billing/a", json!({})), "space": "billing"}),
            )
            .await;
        assert!(e.contains("403") || e.contains("404"), "{label}: {e}");
        let billing = h
            .ok(
                ADMIN,
                "create_stub",
                json!({"stub": stub("/billing/a", json!({"b": 1})), "space": "billing"}),
            )
            .await;
        let billing_id = billing["id"].as_str().unwrap().to_string();
        let e = h.err(PAY, "delete_stub", json!({"id": billing_id})).await;
        assert!(e.contains("403") || e.contains("404"), "{label}: {e}");
        let e = h
            .err(
                PAY,
                "update_stub",
                json!({"id": billing_id, "patch": {"enabled": false}}),
            )
            .await;
        assert!(e.contains("403") || e.contains("404"), "{label}: {e}");
        let listed = h.ok(PAY, "list_stubs", json!({})).await;
        assert_eq!(
            listed["total"], 1,
            "{label}: only the payments stub: {listed}"
        );
        assert_eq!(listed["stubs"][0]["id"], pay_id.as_str());

        h.hit(reqwest::Method::GET, "/payments/a", None).await;
        h.hit(reqwest::Method::GET, "/billing/a", None).await;
        h.journal_settles(2).await;
        let found = h.ok(PAY, "find_requests", json!({})).await;
        let paths: Vec<&str> = found["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["/payments/a"], "{label}: {found}");
        let e = h
            .err(PAY, "find_requests", json!({"space": "billing"}))
            .await;
        assert!(e.contains("403") || e.contains("404"), "{label}: {e}");

        let e = h.err(VIEWER, "reset_journal", json!({})).await;
        assert!(e.contains("403"), "{label}: {e}");
        let cleared = h
            .ok(PAY, "reset_journal", json!({"space": "payments"}))
            .await;
        assert_eq!(cleared["deleted"], 1, "{label}: {cleared}");
        let all = h.admin_get("/requests").await;
        assert_eq!(
            all["requests"].as_array().unwrap().len(),
            1,
            "{label}: billing is kept"
        );

        let e = h
            .err(
                VIEWER,
                "set_scenario",
                json!({"name": "s", "state": "Paid"}),
            )
            .await;
        assert!(e.contains("403"), "{label}: {e}");
        h.ok(ADMIN, "set_scenario", json!({"name": "s", "state": "Paid"}))
            .await;
        let sc = h.ok(VIEWER, "get_scenarios", json!({})).await;
        assert_eq!(sc["scenarios"][0]["state"], "Paid", "{label}: {sc}");
        let chaos = h.ok(VIEWER, "get_chaos", json!({})).await;
        assert!(chaos["rules"].is_array(), "{chaos}");
        let e = h.err(PAY, "get_chaos", json!({})).await;
        assert!(e.contains("403"), "{label}: chaos:read is global only: {e}");
    }
}

#[tokio::test]
async fn mcp_writes_are_audited_like_rest_writes() {
    for (label, storage) in storages().await {
        let h = H::start(storage).await;
        let created = h
            .ok(
                PAY,
                "create_stub",
                json!({"stub": {"name": "audited", "request": {"urlPath": "/payments/z"}, "response": {"status": 201}}, "space": "payments"}),
            )
            .await;
        let id = created["id"].as_str().unwrap().to_string();
        h.ok(
            PAY,
            "update_stub",
            json!({"id": id, "patch": {"response": {"status": 202}}}),
        )
        .await;
        h.ok(PAY, "delete_stub", json!({"id": id})).await;
        let audit = h.admin_get("/audit?limit=50").await;
        let events = audit["events"].as_array().unwrap();
        for action in ["stub.create", "stub.update", "stub.delete"] {
            let e = events
                .iter()
                .find(|e| e["action"] == action)
                .unwrap_or_else(|| panic!("{label}: no {action} in {audit}"));
            assert_eq!(e["principal"], "pay", "{label}: {e}");
            assert_eq!(e["space"], "payments", "{label}: {e}");
            assert_eq!(e["detail"]["via"], "mcp", "{label}: {e}");
            assert!(e["detail"]["mcpTool"].is_string(), "{label}: {e}");
        }
        let upd = events
            .iter()
            .find(|e| e["action"] == "stub.update")
            .unwrap();
        assert!(
            upd["detail"]["changes"].to_string().contains("202"),
            "{upd}"
        );
    }
}

#[tokio::test]
async fn results_are_bounded_and_secrets_masked() {
    let h = H::start(StorageKind::Memory).await;
    let big = "x".repeat(40_000);
    h.ok(
        ADMIN,
        "create_stub",
        json!({"stub": {"request": {"method": "POST", "urlPath": "/big"},
            "response": {"status": 200, "headers": {"authorization": "Bearer secret-value-123"}, "body": big}}}),
    )
    .await;
    let (st, _) = h
        .hit(
            reqwest::Method::POST,
            "/big",
            Some(&json!({"password": "hunter2", "data": "y".repeat(10_000)}).to_string()),
        )
        .await;
    assert_eq!(st, 200);
    h.journal_settles(1).await;
    let found = h
        .ok(VIEWER, "find_requests", json!({"maxBodyBytes": 100}))
        .await;
    let e = &found["requests"][0];
    assert!(
        e["body"].as_str().unwrap().contains("truncated")
            && e["body"].as_str().unwrap().len() < 200,
        "{e}"
    );
    assert!(
        !e.to_string().contains("hunter2"),
        "journal secrets stay redacted: {e}"
    );
    assert!(
        e["responseBody"].as_str().unwrap().contains("truncated"),
        "{e}"
    );
    let one = h.ok(VIEWER, "get_request", json!({"id": e["id"]})).await;
    assert!(
        one["responseBody"].as_str().unwrap().len() < 17_000,
        "{one}"
    );
    let stub_id = h.ok(VIEWER, "list_stubs", json!({})).await["stubs"][0]["id"].clone();
    let viewed = h.ok(VIEWER, "get_stub", json!({"id": stub_id})).await;
    assert!(
        !viewed.to_string().contains("secret-value-123"),
        "viewers see credentials masked: {viewed}"
    );
    assert!(
        viewed["response"]["body"]
            .as_str()
            .unwrap()
            .contains("truncated")
    );
    let e = h.err(VIEWER, "list_stubs", json!({"limit": 1000})).await;
    assert!(e.contains("limit"), "{e}");
    let e = h
        .err(VIEWER, "find_requests", json!({"maxBodyBytes": 10_000_000}))
        .await;
    assert!(e.contains("maxBodyBytes"), "{e}");
    let e = h
        .err(VIEWER, "list_stubs", json!({"unknownField": 1}))
        .await;
    assert!(e.contains("invalid arguments"), "{e}");
}

#[tokio::test]
async fn end_to_end_with_an_mcp_client() {
    for (label, storage) in storages().await {
        let h = H::start(storage).await;
        let cfg = StreamableHttpClientTransportConfig::with_uri(format!("{}/mcp", h.admin))
            .auth_header(ADMIN);
        let client = ().serve(StreamableHttpClientTransport::from_config(cfg)).await.unwrap();
        let tools = client.list_all_tools().await.unwrap();
        let create = tools.iter().find(|t| t.name == "create_stub").unwrap();
        assert!(create.output_schema.is_some() && create.description.is_some());
        assert!(
            create.input_schema["properties"]["stub"].is_object(),
            "{:?}",
            create.input_schema
        );
        let args = |v: Value| v.as_object().unwrap().clone();
        let r = client
            .call_tool(
                CallToolRequestParams::new("create_stub").with_arguments(args(json!({"stub": {
                    "name": "order",
                    "request": {"method": "POST", "urlPath": "/orders", "body": [{"jsonPath": "$.sku", "value": {"equalTo": "A-1"}}]},
                    "response": {"status": 201, "jsonBody": {"id": "{{randomValue type='UUID'}}", "sku": "{{jsonPath request.body '$.sku'}}"}}
                }}))),
            )
            .await
            .unwrap();
        assert_ne!(r.is_error, Some(true), "{label}: {r:?}");
        let (st, body) = h
            .hit(reqwest::Method::POST, "/orders", Some(r#"{"sku": "A-1"}"#))
            .await;
        assert_eq!(st, 201, "{label}: {body}");
        assert!(body.contains("A-1"), "{body}");
        let (st, _) = h
            .hit(reqwest::Method::POST, "/orders", Some(r#"{"sku": "B-2"}"#))
            .await;
        assert_eq!(st, 404);
        h.journal_settles(2).await;
        let found = client
            .call_tool(
                CallToolRequestParams::new("find_requests").with_arguments(args(
                    json!({"pattern": {"method": "POST", "urlPath": "/orders"}}),
                )),
            )
            .await
            .unwrap();
        let found = found.structured_content.unwrap();
        assert_eq!(found["total"], 2, "{label}: {found}");
        let unmatched = client
            .call_tool(
                CallToolRequestParams::new("find_requests")
                    .with_arguments(args(json!({"matched": false}))),
            )
            .await
            .unwrap()
            .structured_content
            .unwrap();
        let miss_id = unmatched["requests"][0]["id"].clone();
        let near = client
            .call_tool(
                CallToolRequestParams::new("near_misses")
                    .with_arguments(args(json!({"requestId": miss_id}))),
            )
            .await
            .unwrap()
            .structured_content
            .unwrap();
        assert!(
            near["nearMisses"][0]["mismatches"]
                .to_string()
                .contains("body matcher"),
            "{label}: {near}"
        );
        let verified = client
            .call_tool(CallToolRequestParams::new("verify").with_arguments(args(json!({
                "request": {"method": "POST", "urlPath": "/orders", "body": [{"jsonPath": "$.sku", "value": {"equalTo": "A-1"}}]},
                "count": 1
            }))))
            .await
            .unwrap()
            .structured_content
            .unwrap();
        assert_eq!(verified["verified"], true, "{label}: {verified}");
        let failed = client
            .call_tool(
                CallToolRequestParams::new("verify").with_arguments(args(json!({
                    "request": {"method": "POST", "urlPath": "/orders"}, "count": 5
                }))),
            )
            .await
            .unwrap()
            .structured_content
            .unwrap();
        assert_eq!(failed["verified"], false, "{failed}");
        assert_eq!(failed["count"], 2, "{failed}");
        let res = client.list_all_resources().await.unwrap();
        assert_eq!(res.len(), 2);
        let guide = client
            .read_resource(ReadResourceRequestParams::new("fastmock://guide/stubs"))
            .await
            .unwrap();
        assert!(format!("{:?}", guide.contents).contains("urlPathTemplate"));
        let schema = client
            .read_resource(ReadResourceRequestParams::new("fastmock://schema/stub"))
            .await
            .unwrap();
        assert!(format!("{:?}", schema.contents).contains("RequestPattern"));
        client.cancel().await.unwrap();
    }
}

#[tokio::test]
async fn import_validate_and_callbacks_through_mcp() {
    let h = H::start(StorageKind::Memory).await;
    let v = h
        .ok(
            VIEWER,
            "validate_stub",
            json!({"stub": {"request": {"urlPathPattern": "(["}, "response": {}}}),
        )
        .await;
    assert_eq!(v["valid"], false, "{v}");
    let openapi = json!({
        "openapi": "3.0.3", "info": {"title": "pets", "version": "1"},
        "paths": {"/pets": {"get": {"responses": {"200": {"description": "ok",
            "content": {"application/json": {"example": [{"id": 1}]}}}}}}}
    })
    .to_string();
    let e = h.err(VIEWER, "import", json!({"document": openapi})).await;
    assert!(e.contains("403"), "{e}");
    let r = h
        .ok(ADMIN, "import", json!({"document": openapi, "tag": "pets"}))
        .await;
    assert!(r.to_string().contains("imported"), "{r}");
    let (st, body) = h.hit(reqwest::Method::GET, "/pets", None).await;
    assert_eq!(st, 200, "{body}");
    let cb = h.ok(VIEWER, "list_callbacks", json!({})).await;
    assert!(cb["callbacks"].is_array(), "{cb}");
}
