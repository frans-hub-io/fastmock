use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::Duration;

use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use reqwest::Method;
use serde_json::{Value, json};
use sqlx::Connection;
use tokio::sync::watch;

const ADMIN: &str = "fm_admin_token_for_api_tests_0123456789";
const EDITOR: &str = "fm_editor_token_for_api_tests_0123456789";
const VIEWER: &str = "fm_viewer_token_for_api_tests_0123456789";

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

fn tokens() -> Vec<String> {
    vec![
        format!("ops:admin:{}", hash_token(ADMIN)),
        format!("ci:editor:{}", hash_token(EDITOR)),
        format!("dash:viewer:{}", hash_token(VIEWER)),
    ]
}

async fn pg_storage() -> Option<StorageKind> {
    let admin_url = std::env::var("FASTMOCK_TEST_DATABASE_URL").ok()?;
    let valkey_url = std::env::var("FASTMOCK_TEST_VALKEY_URL").ok()?;
    let db = format!("fm_api_{}", uuid::Uuid::new_v4().simple());
    let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut c)
        .await
        .unwrap();
    Some(StorageKind::Postgres {
        database_url: format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db),
        valkey_url,
        valkey_prefix: format!("api:{}:", uuid::Uuid::new_v4().simple()),
        max_connections: 5,
        options: Default::default(),
    })
}

async fn storages() -> Vec<StorageKind> {
    let mut v = vec![StorageKind::Memory];
    if let Some(p) = pg_storage().await {
        v.push(p);
    }
    v
}

struct H {
    data: String,
    admin: String,
    http: reqwest::Client,
    _shutdown: watch::Sender<bool>,
}

impl H {
    async fn start(storage: StorageKind) -> H {
        let mut cfg = Config::for_tests(storage, tokens());
        cfg.plugins = Some(vec![]);
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
            _shutdown: tx,
        }
    }

    fn req(&self, m: Method, path: &str, token: &str) -> reqwest::RequestBuilder {
        self.http
            .request(m, format!("{}/__admin{path}", self.admin))
            .bearer_auth(token)
    }

    async fn call(&self, m: Method, path: &str, token: &str, body: Option<Value>) -> (u16, Value) {
        let mut rb = self.req(m, path, token);
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

    async fn hit(&self, m: Method, path: &str, body: Option<&str>) -> u16 {
        let mut rb = self.http.request(m, format!("{}{path}", self.data));
        if let Some(b) = body {
            rb = rb
                .header("content-type", "application/json")
                .body(b.to_string());
        }
        rb.send().await.unwrap().status().as_u16()
    }

    async fn wait_journal(&self, n: i64) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let v = self.ok(Method::GET, "/requests/count", VIEWER, None).await;
            if v["count"].as_i64().unwrap() >= n {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "journal has {v}, want {n}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn assert_error_shape(v: &Value) {
    assert!(v["error"].is_string(), "{v}");
    assert!(v["details"].is_array(), "{v}");
}

#[tokio::test]
async fn openapi_spec_is_public_and_matches_the_file() {
    let h = H::start(StorageKind::Memory).await;
    let r = h
        .http
        .get(format!("{}/openapi.json", h.admin))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["cache-control"], "no-cache");
    assert_eq!(r.headers()["x-content-type-options"], "nosniff");
    assert!(
        r.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    let spec: Value = r.json().await.unwrap();
    assert_eq!(spec["info"]["title"], "FastMock Admin API");
    for p in [
        "/__admin/verify",
        "/__admin/files/{name}",
        "/__admin/import/har",
        "/__admin/stubs/{id}",
    ] {
        assert!(spec["paths"].get(p).is_some(), "{p} missing");
    }
    let on_disk: Value = serde_json::from_str(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/api/openapi.json"
        ))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(spec, on_disk);
    let (st, _) = h.call(Method::GET, "/version", "wrong", None).await;
    assert_eq!(st, 401);
    let v = h.ok(Method::GET, "/version", VIEWER, None).await;
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        v["features"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "files")
    );
}

#[tokio::test]
async fn stub_search_patch_metadata_unused_and_audit() {
    for storage in storages().await {
        let h = H::start(storage).await;
        for (name, path, tag, service, enabled) in [
            (
                "billing list",
                "/billing/invoices",
                "billing",
                "billing",
                true,
            ),
            (
                "billing one",
                "/billing/invoices/1",
                "billing",
                "billing",
                true,
            ),
            ("users", "/users", "users", "accounts", false),
        ] {
            h.ok(
                Method::POST,
                "/stubs",
                EDITOR,
                Some(json!({"name": name, "request": {"method": "GET", "urlPath": path}, "response": {"body": name, "headers": {"X-Api-Key": "k"}}, "tags": [tag], "metadata": {"service": service, "owner": {"team": "t1"}}, "enabled": enabled})),
            )
            .await;
        }
        let all = h.ok(Method::GET, "/stubs", VIEWER, None).await;
        assert_eq!(all["total"], 3);
        assert_eq!(all["stubs"][0]["response"]["headers"]["X-Api-Key"], "***");
        let page = h
            .ok(Method::GET, "/stubs?limit=1&offset=1", VIEWER, None)
            .await;
        assert_eq!(page["total"], 3);
        assert_eq!(page["stubs"].as_array().unwrap().len(), 1);
        assert_eq!(page["offset"], 1);
        let f = h
            .ok(Method::GET, "/stubs?tag=billing&q=INVOICES/1", VIEWER, None)
            .await;
        assert_eq!(f["total"], 1);
        assert_eq!(f["stubs"][0]["name"], "billing one");
        let f = h
            .ok(Method::GET, "/stubs?enabled=false", VIEWER, None)
            .await;
        assert_eq!(f["stubs"][0]["name"], "users");
        let (st, v) = h.call(Method::GET, "/stubs?limit=0", VIEWER, None).await;
        assert_eq!(st, 400);
        assert_error_shape(&v);

        let id = f["stubs"][0]["id"].as_str().unwrap().to_string();
        let (st, _) = h
            .call(
                Method::PATCH,
                &format!("/stubs/{id}"),
                VIEWER,
                Some(json!({"enabled": true})),
            )
            .await;
        assert_eq!(st, 403);
        let patched = h
            .ok(
                Method::PATCH,
                &format!("/stubs/{id}"),
                EDITOR,
                Some(json!({"enabled": true, "response": {"status": 202, "headers": null}, "id": "00000000-0000-0000-0000-000000000000"})),
            )
            .await;
        assert_eq!(patched["id"], id);
        assert_eq!(patched["enabled"], true);
        assert_eq!(patched["response"]["status"], 202);
        assert_eq!(patched["response"]["body"], "users");
        assert!(patched["response"].get("headers").is_none());
        let (st, v) = h
            .call(
                Method::PATCH,
                &format!("/stubs/{id}"),
                EDITOR,
                Some(json!({"priority": "high"})),
            )
            .await;
        assert_eq!(st, 400, "{v}");
        let (st, _) = h
            .call(
                Method::PATCH,
                &format!("/stubs/{}", uuid::Uuid::new_v4()),
                EDITOR,
                Some(json!({})),
            )
            .await;
        assert_eq!(st, 404);
        assert_eq!(h.hit(Method::GET, "/users", None).await, 202);

        let found = h
            .ok(
                Method::POST,
                "/stubs/find-by-metadata",
                VIEWER,
                Some(json!({"matchesJsonPath": "$.service", "equalTo": "billing"})),
            )
            .await;
        assert_eq!(found["total"], 2);
        let found = h
            .ok(
                Method::POST,
                "/stubs/find-by-metadata",
                VIEWER,
                Some(json!({"matchesJsonPath": {"expression": "$.owner.team", "equalTo": "t1"}})),
            )
            .await;
        assert_eq!(found["total"], 3);
        let found = h
            .ok(
                Method::POST,
                "/stubs/find-by-metadata",
                VIEWER,
                Some(json!({"equalToJson": {"service": "accounts"}, "ignoreExtraElements": true})),
            )
            .await;
        assert_eq!(found["total"], 1);
        let (st, _) = h
            .call(
                Method::POST,
                "/stubs/find-by-metadata",
                VIEWER,
                Some(json!({"matchesJsonPath": "$[?("})),
            )
            .await;
        assert_eq!(st, 400);

        h.wait_journal(1).await;
        let unused = h.ok(Method::GET, "/stubs/unused", VIEWER, None).await;
        assert_eq!(unused["total"], 2);
        assert!(
            unused["stubs"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["id"] != id.as_str())
        );
        let future = h
            .ok(
                Method::GET,
                "/stubs/unused?since=2999-01-01T00:00:00Z",
                VIEWER,
                None,
            )
            .await;
        assert_eq!(future["total"], 3);

        let (st, _) = h
            .call(
                Method::POST,
                "/stubs/remove-by-metadata",
                VIEWER,
                Some(json!({"matchesJsonPath": "$.service", "equalTo": "billing"})),
            )
            .await;
        assert_eq!(st, 403);
        let removed = h
            .ok(
                Method::POST,
                "/stubs/remove-by-metadata",
                EDITOR,
                Some(json!({"matchesJsonPath": "$.service", "equalTo": "billing"})),
            )
            .await;
        assert_eq!(removed["deleted"], 2);
        h.ok(Method::DELETE, &format!("/stubs/{id}"), EDITOR, None)
            .await;

        let (st, _) = h.call(Method::GET, "/audit", EDITOR, None).await;
        assert_eq!(st, 403);
        let upd = h
            .ok(Method::GET, "/audit?action=stub.update", ADMIN, None)
            .await;
        let ev = &upd["events"][0];
        assert_eq!(ev["principal"], "ci");
        let changes = ev["detail"]["changes"].as_array().unwrap();
        assert!(
            changes.iter().any(|c| c["path"] == "/response/status"
                && c["before"] == 200
                && c["after"] == 202),
            "{changes:?}"
        );
        assert!(
            changes
                .iter()
                .any(|c| c["path"] == "/response/headers" && c["before"]["X-Api-Key"] == "***"),
            "{changes:?}"
        );
        let del = h
            .ok(Method::GET, "/audit?action=stub.delete", ADMIN, None)
            .await;
        assert_eq!(del["events"][0]["detail"]["before"]["name"], "users");
        let stub_events = h
            .ok(
                Method::GET,
                "/audit?action=stub.*&principal=ci&limit=100",
                ADMIN,
                None,
            )
            .await;
        let actions: Vec<&str> = stub_events["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["action"].as_str().unwrap())
            .collect();
        assert!(
            actions.contains(&"stub.create") && actions.contains(&"stub.delete_by_metadata"),
            "{actions:?}"
        );
        let none = h
            .ok(Method::GET, "/audit?principal=nobody", ADMIN, None)
            .await;
        assert!(none["events"].as_array().unwrap().is_empty());
        let since = h
            .ok(
                Method::GET,
                "/audit?since=2999-01-01T00:00:00Z",
                ADMIN,
                None,
            )
            .await;
        assert!(since["events"].as_array().unwrap().is_empty());
    }
}

#[tokio::test]
async fn files_api() {
    for storage in storages().await {
        let h = H::start(storage).await;
        let put = |name: &str, token: &str, ct: &str, body: Vec<u8>| {
            h.req(Method::PUT, &format!("/files/{name}"), token)
                .header("content-type", ct)
                .body(body)
                .send()
        };
        let r = put("users/42.json", VIEWER, "application/json", b"{}".to_vec())
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        let r = put(
            "users/42.json",
            EDITOR,
            "application/json",
            br#"{"id":42}"#.to_vec(),
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 201);
        let info: Value = r.json().await.unwrap();
        assert_eq!(info["size"], 9);
        assert_eq!(info["contentType"], "application/json");
        let r = put(
            "users/42.json",
            EDITOR,
            "application/json",
            br#"{"id":43}"#.to_vec(),
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 200);
        let r = put("logo.png", EDITOR, "image/png", vec![0, 159, 146, 150])
            .await
            .unwrap();
        assert_eq!(r.status(), 201);
        for bad in ["a..b/c", "x..y", "sp%20ace", "%2Fabs", "dir/"] {
            let r = put(bad, EDITOR, "text/plain", b"x".to_vec()).await.unwrap();
            assert_eq!(r.status(), 400, "{bad}");
            assert_error_shape(&r.json().await.unwrap());
        }
        let r = put(
            "big.bin",
            EDITOR,
            "application/octet-stream",
            vec![1u8; 10 * 1024 * 1024 + 1],
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 413);

        let list = h.ok(Method::GET, "/files", VIEWER, None).await;
        assert_eq!(list["total"], 2);
        assert_eq!(list["files"][0]["name"], "logo.png");
        assert!(list["files"][0].get("base64").is_none());
        let r = h
            .req(Method::GET, "/files/users/42.json", VIEWER)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["content-type"], "application/json");
        assert!(
            r.headers()["content-disposition"]
                .to_str()
                .unwrap()
                .starts_with("attachment")
        );
        assert!(
            r.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("sandbox")
        );
        assert_eq!(r.text().await.unwrap(), r#"{"id":43}"#);
        let r = h
            .req(Method::GET, "/files/logo.png", VIEWER)
            .send()
            .await
            .unwrap();
        assert_eq!(r.bytes().await.unwrap().as_ref(), &[0, 159, 146, 150]);
        let (st, _) = h
            .call(Method::DELETE, "/files/logo.png", VIEWER, None)
            .await;
        assert_eq!(st, 403);
        let (st, _) = h
            .call(Method::DELETE, "/files/logo.png", EDITOR, None)
            .await;
        assert_eq!(st, 204);
        let (st, v) = h.call(Method::GET, "/files/logo.png", VIEWER, None).await;
        assert_eq!(st, 404);
        assert_error_shape(&v);
        let audit = h.ok(Method::GET, "/audit?action=file.*", ADMIN, None).await;
        assert_eq!(audit["events"].as_array().unwrap().len(), 4);
    }
}

#[tokio::test]
async fn journal_find_verify_sequence_and_remove() {
    for storage in storages().await {
        let h = H::start(storage).await;
        h.ok(
            Method::POST,
            "/stubs",
            EDITOR,
            Some(json!({"request": {"urlPathPattern": "/api/.*"}, "response": {"jsonBody": {"ok": true}}})),
        )
        .await;
        assert_eq!(
            h.hit(Method::POST, "/api/login", Some(r#"{"user":"a"}"#))
                .await,
            200
        );
        assert_eq!(h.hit(Method::GET, "/api/profile", None).await, 200);
        assert_eq!(
            h.hit(Method::POST, "/api/orders", Some(r#"{"amount":5}"#))
                .await,
            200
        );
        assert_eq!(
            h.hit(Method::POST, "/api/orders", Some(r#"{"amount":7}"#))
                .await,
            200
        );
        assert_eq!(h.hit(Method::GET, "/nowhere", None).await, 404);
        assert_eq!(h.hit(Method::POST, "/api/logout", None).await, 200);
        h.wait_journal(6).await;

        let page = h
            .ok(Method::GET, "/requests?limit=2&offset=1", VIEWER, None)
            .await;
        assert_eq!(page["total"], 6);
        assert_eq!(page["requests"].as_array().unwrap().len(), 2);
        assert_eq!(page["requests"][0]["path"], "/nowhere");
        let id = page["requests"][0]["id"].as_i64().unwrap();
        let one = h
            .ok(Method::GET, &format!("/requests/{id}"), VIEWER, None)
            .await;
        assert_eq!(one["path"], "/nowhere");
        let (st, _) = h.call(Method::GET, "/requests/999999", VIEWER, None).await;
        assert_eq!(st, 404);
        let (st, _) = h.call(Method::GET, "/requests/abc", VIEWER, None).await;
        assert_eq!(st, 400);
        let un = h.ok(Method::GET, "/requests/unmatched", VIEWER, None).await;
        assert_eq!(un["total"], 1);
        assert_eq!(un["requests"][0]["path"], "/nowhere");

        let found = h
            .ok(
                Method::POST,
                "/requests/find",
                VIEWER,
                Some(json!({"method": "POST", "urlPath": "/api/orders"})),
            )
            .await;
        assert_eq!(found["total"], 2);
        let found = h
            .ok(Method::POST, "/requests/find?limit=1", VIEWER, Some(json!({"urlPath": "/api/orders", "body": [{"jsonPath": "$.amount", "value": {"equalTo": "7"}}]})))
            .await;
        assert_eq!(found["total"], 1);

        let ok = h
            .ok(
                Method::POST,
                "/verify",
                VIEWER,
                Some(
                    json!({"request": {"method": "POST", "urlPath": "/api/orders"}, "exactly": 2}),
                ),
            )
            .await;
        assert_eq!(ok["verified"], true);
        assert_eq!(ok["count"], 2);
        let (st, v) = h
            .call(Method::POST, "/verify", VIEWER, Some(json!({"request": {"method": "POST", "urlPath": "/api/orders", "body": [{"jsonPath": "$.amount", "value": {"equalTo": "9"}}]}, "atLeast": 1})))
            .await;
        assert_eq!(st, 422, "{v}");
        assert_eq!(v["verified"], false);
        assert_eq!(v["count"], 0);
        assert_eq!(v["expected"], "at least 1");
        assert_error_shape(&v);
        let nm = v["nearMisses"].as_array().unwrap();
        assert!(!nm.is_empty() && nm[0]["path"] == "/api/orders", "{v}");
        let (st, v) = h
            .call(
                Method::POST,
                "/verify",
                VIEWER,
                Some(json!({"request": {"urlPath": "/api/profile"}, "atMost": 0})),
            )
            .await;
        assert_eq!(st, 422, "{v}");
        let (st, _) = h
            .call(
                Method::POST,
                "/verify",
                VIEWER,
                Some(json!({"request": {"urlPath": "/x"}, "count": 1, "atMost": 2})),
            )
            .await;
        assert_eq!(st, 400);
        let (st, _) = h
            .call(
                Method::POST,
                "/verify",
                VIEWER,
                Some(json!({"request": {"urlPathPattern": "("}})),
            )
            .await;
        assert_eq!(st, 400);

        let seq = h
            .ok(Method::POST, "/verify/sequence", VIEWER, Some(json!({"requests": [{"urlPath": "/api/login"}, {"urlPath": "/api/orders"}, {"urlPath": "/api/logout"}]})))
            .await;
        assert_eq!(seq["verified"], true);
        assert_eq!(seq["requestIds"].as_array().unwrap().len(), 3);
        let (st, v) = h
            .call(
                Method::POST,
                "/verify/sequence",
                VIEWER,
                Some(json!({"requests": [{"urlPath": "/api/logout"}, {"urlPath": "/api/login"}]})),
            )
            .await;
        assert_eq!(st, 422);
        assert_eq!(v["matched"], 1);
        assert_error_shape(&v);
        let (st, _) = h
            .call(
                Method::POST,
                "/verify/sequence",
                VIEWER,
                Some(json!({"requests": []})),
            )
            .await;
        assert_eq!(st, 400);

        let (st, _) = h
            .call(
                Method::POST,
                "/requests/remove",
                VIEWER,
                Some(json!({"urlPath": "/api/orders"})),
            )
            .await;
        assert_eq!(st, 403);
        let rm = h
            .ok(
                Method::POST,
                "/requests/remove",
                EDITOR,
                Some(json!({"urlPath": "/api/orders"})),
            )
            .await;
        assert_eq!(rm["deleted"], 2);
        let c = h.ok(Method::GET, "/requests/count", VIEWER, None).await;
        assert_eq!(c["count"], 4);
    }
}

#[tokio::test]
async fn recording_snapshot() {
    for storage in storages().await {
        let h = H::start(storage).await;
        for stub in [
            json!({"request": {"urlPathPattern": "/svc/.*"}, "priority": 9, "response": {"jsonBody": {"generic": true}}, "tags": ["source"]}),
            json!({"request": {"urlPath": "/svc/counter"}, "scenario": {"name": "c", "requiredState": "Started", "newState": "two"}, "response": {"jsonBody": {"n": 1}}, "tags": ["source"]}),
            json!({"request": {"urlPath": "/svc/counter"}, "scenario": {"name": "c", "requiredState": "two"}, "response": {"jsonBody": {"n": 2}}, "tags": ["source"]}),
        ] {
            h.ok(Method::POST, "/stubs", EDITOR, Some(stub)).await;
        }
        for _ in 0..2 {
            h.hit(Method::GET, "/svc/counter", None).await;
        }
        h.hit(
            Method::POST,
            "/svc/login",
            Some(r#"{"user":"a","password":"p"}"#),
        )
        .await;
        h.hit(Method::GET, "/other", None).await;
        h.wait_journal(4).await;

        let (st, _) = h
            .call(
                Method::POST,
                "/recordings/snapshot",
                VIEWER,
                Some(json!({})),
            )
            .await;
        assert_eq!(st, 403);
        let dry = h
            .ok(
                Method::POST,
                "/recordings/snapshot",
                EDITOR,
                Some(json!({"filters": {"request": {"urlPathPattern": "/svc/.*"}}, "persist": false, "repeatsAsScenarios": true, "tag": "snap"})),
            )
            .await;
        assert_eq!(dry["persisted"], false);
        let stubs = dry["stubs"].as_array().unwrap();
        assert_eq!(stubs.len(), 3, "{dry}");
        let counter: Vec<&Value> = stubs
            .iter()
            .filter(|s| s["request"]["urlPath"] == "/svc/counter")
            .collect();
        assert_eq!(counter.len(), 2);
        assert_eq!(counter[0]["scenario"]["requiredState"], "Started");
        assert_eq!(counter[0]["scenario"]["newState"], "call-2");
        assert_eq!(counter[1]["scenario"]["requiredState"], "call-2");
        let login = stubs
            .iter()
            .find(|s| s["request"]["urlPath"] == "/svc/login")
            .unwrap();
        assert_eq!(
            login["request"]["body"][0]["equalToJson"],
            json!({"user": "a"})
        );
        assert!(login["tags"].as_array().unwrap().contains(&json!("snap")));
        assert_eq!(counter[0]["response"]["jsonBody"], json!({"n": 1}));
        assert_eq!(counter[1]["response"]["jsonBody"], json!({"n": 2}));
        let total = h.ok(Method::GET, "/stubs", VIEWER, None).await;
        assert_eq!(total["total"], 3);

        let saved = h
            .ok(
                Method::POST,
                "/recordings/snapshot",
                EDITOR,
                Some(json!({"filters": {"request": {"urlPath": "/svc/counter"}}, "outputFormat": "IDS", "tag": "snap"})),
            )
            .await;
        assert_eq!(saved["persisted"], true);
        assert_eq!(saved["ids"].as_array().unwrap().len(), 1);
        assert!(saved.get("stubs").is_none());
        assert!(
            saved["warnings"][0]
                .as_str()
                .unwrap()
                .contains("different responses"),
            "{saved}"
        );
        let (st, v) = h
            .call(
                Method::POST,
                "/recordings/snapshot",
                EDITOR,
                Some(json!({"outputFormat": "XML"})),
            )
            .await;
        assert!(st == 400 || st == 422, "{st} {v}");
        h.ok(Method::DELETE, "/stubs?tag=source", EDITOR, None)
            .await;
        let r = h
            .http
            .get(format!("{}/svc/counter", h.data))
            .send()
            .await
            .unwrap();
        assert_eq!(r.json::<Value>().await.unwrap(), json!({"n": 1}));
    }
}

#[tokio::test]
async fn postman_har_and_auto_import() {
    let h = H::start(StorageKind::Memory).await;
    let collection = json!({
        "info": {"name": "Shop", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
        "variable": [{"key": "baseUrl", "value": "https://api.example.com"}],
        "item": [{"name": "Users", "item": [
            {"name": "Get user", "request": {"method": "GET", "url": "{{baseUrl}}/users/:id?api_key=abc"},
             "response": [{"name": "ok", "code": 200, "header": [{"key": "Content-Type", "value": "application/json"}], "body": "{\"id\": 1}"}]}
        ]}]
    });
    let (st, _) = h
        .call(
            Method::POST,
            "/import/postman",
            VIEWER,
            Some(json!({"collection": collection})),
        )
        .await;
    assert_eq!(st, 403);
    let dry = h
        .ok(
            Method::POST,
            "/import/postman",
            EDITOR,
            Some(json!({"collection": collection, "tag": "shop", "dryRun": true})),
        )
        .await;
    assert_eq!(dry["dryRun"], true);
    assert_eq!(dry["imported"], 0);
    assert_eq!(dry["stubs"][0]["request"]["urlPathTemplate"], "/users/{id}");
    assert_eq!(
        dry["stubs"][0]["request"]["query"]["api_key"]["matches"],
        ".*"
    );
    assert!(
        dry["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("secrets"))
    );
    let r = h
        .ok(
            Method::POST,
            "/import/postman",
            EDITOR,
            Some(json!({"collection": collection.to_string(), "tag": "shop"})),
        )
        .await;
    assert_eq!(r["imported"], 1);
    assert_eq!(h.hit(Method::GET, "/users/9?api_key=zzz", None).await, 200);
    let (st, v) = h
        .call(
            Method::POST,
            "/import/postman",
            EDITOR,
            Some(json!({"collection": {"requests": []}})),
        )
        .await;
    assert_eq!(st, 400);
    assert_error_shape(&v);

    let har = json!({"log": {"version": "1.2", "entries": [
        {"request": {"method": "GET", "url": "https://api.example.com/catalog?page=1"},
         "response": {"status": 200, "headers": [{"name": "Set-Cookie", "value": "sid=1"}], "content": {"mimeType": "application/json", "text": "[1,2]"}}},
        {"request": {"method": "GET", "url": "https://api.example.com/logo.png"},
         "response": {"status": 200, "headers": [], "content": {"mimeType": "image/png", "text": "AA==", "encoding": "base64"}}}
    ]}});
    let r = h
        .ok(
            Method::POST,
            "/import/har",
            EDITOR,
            Some(json!({"har": har, "tag": "browser", "mimeTypes": ["application/json"]})),
        )
        .await;
    assert_eq!(r["imported"], 1, "{r}");
    let r = h
        .http
        .get(format!("{}/catalog?page=1", h.data))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().get("set-cookie").is_none());
    assert_eq!(r.json::<Value>().await.unwrap(), json!([1, 2]));

    let auto = |body: String, query: &str| {
        h.req(Method::POST, &format!("/import{query}"), EDITOR)
            .header("content-type", "application/octet-stream")
            .body(body)
            .send()
    };
    let r = auto(
        "openapi: 3.0.3\ninfo: {title: Pets, version: '1'}\npaths:\n  /pets:\n    get:\n      responses:\n        '200':\n          description: ok\n          content:\n            application/json:\n              example: [{\"name\": \"rex\"}]\n".into(),
        "?tag=pets",
    )
    .await
    .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert!(v["imported"].as_u64().unwrap() >= 1, "{v}");
    assert_eq!(h.hit(Method::GET, "/pets", None).await, 200);
    let r = auto(json!({"mappings": [{"request": {"method": "GET", "url": "/wm"}, "response": {"status": 204}}]}).to_string(), "")
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(h.hit(Method::GET, "/wm", None).await, 204);
    let wm = h.ok(Method::GET, "/stubs?tag=wiremock", VIEWER, None).await;
    assert_eq!(wm["total"], 1);
    let r = auto(collection.to_string(), "?dryRun=true").await.unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["dryRun"], true, "{v}");
    let r = auto(har.to_string(), "?tag=har2").await.unwrap();
    assert_eq!(r.status(), 200);
    let r = auto(
        json!([{"httpRequest": {"method": "GET", "path": "/ms"}, "httpResponse": {"statusCode": 202, "body": "ms"}}]).to_string(),
        "",
    )
    .await
    .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    assert_eq!(h.hit(Method::GET, "/ms", None).await, 202);
    let r = auto("type Query { hello: String }\n".into(), "?dryRun=true")
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let r = auto(r#"{"hello": "world"}"#.into(), "").await.unwrap();
    assert_eq!(r.status(), 415);
    assert_error_shape(&r.json().await.unwrap());
    let r = auto(r#"{"swagger": "2.0"}"#.into(), "").await.unwrap();
    assert_eq!(r.status(), 415);
    let r = auto("x".into(), "?format=nope").await.unwrap();
    assert_eq!(r.status(), 400);
    let r = h
        .req(Method::POST, "/import", VIEWER)
        .body(collection.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}
