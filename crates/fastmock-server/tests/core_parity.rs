use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use serde_json::{Value, json};
use sqlx::Connection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

const EDITOR: &str = "fm_core_editor_token_0123456789abcdef";
const VIEWER: &str = "fm_core_viewer_token_0123456789abcdef";

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

struct Harness {
    data: String,
    data_addr: std::net::SocketAddr,
    admin: String,
    http: reqwest::Client,
    app: fastmock_server::state::AppState,
    _shutdown: watch::Sender<bool>,
}

impl Harness {
    async fn start(storage: StorageKind) -> Harness {
        let tokens = vec![
            format!("ci:editor:{}", hash_token(EDITOR)),
            format!("dash:viewer:{}", hash_token(VIEWER)),
        ];
        let cfg = Config::for_tests(storage, tokens);
        let (app, data, admin, _policy) = fastmock_server::build(cfg, prom()).await.unwrap();
        let (tx, rx) = watch::channel(false);
        let dl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (da, aa) = (dl.local_addr().unwrap(), al.local_addr().unwrap());
        tokio::spawn(fastmock_server::tls::serve(
            dl,
            data,
            None,
            rx.clone(),
            Duration::from_secs(5),
            Duration::from_secs(1),
        ));
        tokio::spawn(async move { axum::serve(al, admin).await.unwrap() });
        app.ready.store(true, Ordering::SeqCst);
        Harness {
            data: format!("http://{da}"),
            data_addr: da,
            admin: format!("http://{aa}/__admin"),
            http: reqwest::Client::new(),
            app,
            _shutdown: tx,
        }
    }

    async fn stub(&self, stub: Value) -> Value {
        let r = self
            .http
            .post(format!("{}/stubs", self.admin))
            .bearer_auth(EDITOR)
            .json(&stub)
            .send()
            .await
            .unwrap();
        let st = r.status();
        let body: Value = r.json().await.unwrap();
        assert_eq!(st, 201, "{body}");
        body
    }

    async fn get(&self, path: &str) -> (u16, String) {
        let r = self
            .http
            .get(format!("{}{path}", self.data))
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }

    async fn raw(&self, request: &str) -> Vec<u8> {
        let mut s = tokio::net::TcpStream::connect(self.data_addr)
            .await
            .unwrap();
        s.write_all(request.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
        out
    }
}

async fn pg_storage() -> Option<StorageKind> {
    let admin_url = std::env::var("FASTMOCK_TEST_DATABASE_URL").ok()?;
    let valkey_url = std::env::var("FASTMOCK_TEST_VALKEY_URL").ok()?;
    let db = format!("fm_core_{}", uuid::Uuid::new_v4().simple());
    let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut c)
        .await
        .unwrap();
    Some(StorageKind::Postgres {
        database_url: format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db),
        valkey_url,
        valkey_prefix: format!("core:{}:", uuid::Uuid::new_v4().simple()),
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

#[tokio::test]
async fn sequences_times_and_counters() {
    for storage in storages().await {
        let h = Harness::start(storage).await;
        h.stub(json!({"priority": 9, "request": {"urlPath": "/pay"}, "response": {"status": 402, "body": "fallback"}}))
            .await;
        h.stub(
            json!({"priority": 1, "times": 3, "request": {"urlPath": "/pay"},
            "responses": [{"status": 200, "body": "first"}, {"status": 202, "body": "second"}],
            "responseMode": "sequential"}),
        )
        .await;
        h.stub(
            json!({"request": {"urlPath": "/cycle"}, "responseMode": "cycle",
            "responses": [{"body": "a"}, {"body": "b"}]}),
        )
        .await;
        let mut got = vec![];
        for _ in 0..5 {
            got.push(h.get("/pay").await);
        }
        assert_eq!(
            got,
            vec![
                (200, "first".to_string()),
                (202, "second".to_string()),
                (202, "second".to_string()),
                (402, "fallback".to_string()),
                (402, "fallback".to_string()),
            ]
        );
        let cyc: Vec<String> = futures_join(&h, "/cycle", 3).await;
        assert_eq!(cyc, ["a", "b", "a"]);
        let counters: Value = h
            .http
            .get(format!("{}/stubs/counters", h.admin))
            .bearer_auth(VIEWER)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(counters["total"].as_u64().unwrap() >= 3, "{counters}");
        let r = h
            .http
            .delete(format!("{}/stubs/counters", h.admin))
            .bearer_auth(VIEWER)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        let r = h
            .http
            .delete(format!("{}/stubs/counters", h.admin))
            .bearer_auth(EDITOR)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(h.get("/pay").await, (200, "first".to_string()));
        let bad = h
            .http
            .post(format!("{}/stubs", h.admin))
            .bearer_auth(EDITOR)
            .json(&json!({"request": {}, "response": {"body": "x"}, "responses": [{"body": "y"}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 400);
    }
}

async fn futures_join(h: &Harness, path: &str, n: usize) -> Vec<String> {
    let mut v = vec![];
    for _ in 0..n {
        v.push(h.get(path).await.1);
    }
    v
}

#[tokio::test]
async fn global_settings_delay_and_proxy_pass_through() {
    let h = Harness::start(StorageKind::Memory).await;
    h.stub(json!({"request": {"urlPath": "/slow"}, "response": {"body": "ok"}}))
        .await;
    h.stub(json!({"request": {"urlPath": "/px"}, "response": {"proxy": {"baseUrl": "http://upstream.example.com"}}}))
        .await;
    let url = format!("{}/settings", h.admin);
    let initial: Value = h
        .http
        .get(&url)
        .bearer_auth(VIEWER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(initial, json!({"proxyPassThrough": true}));
    let r = h
        .http
        .put(&url)
        .bearer_auth(VIEWER)
        .json(&json!({"fixedDelayMs": 300}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = h
        .http
        .put(&url)
        .bearer_auth(EDITOR)
        .json(&json!({"delay": {"lognormal": {"medianMs": 10, "sigma": 50.0}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let r = h
        .http
        .put(&url)
        .bearer_auth(EDITOR)
        .json(&json!({"fixedDelayMs": 300, "proxyPassThrough": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let started = Instant::now();
    assert_eq!(h.get("/slow").await, (200, "ok".to_string()));
    assert!(
        started.elapsed() >= Duration::from_millis(290),
        "{:?}",
        started.elapsed()
    );
    let (status, body) = h.get("/px").await;
    assert_eq!(status, 404);
    assert!(body.contains("pass-through"), "{body}");
    let stored: Value = h
        .http
        .get(&url)
        .bearer_auth(VIEWER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stored["fixedDelayMs"], 300);
    let events = h.app.stores.audit.list(10).await.unwrap();
    assert!(events.iter().any(|e| e.action == "settings.update"));
    h.http
        .put(&url)
        .bearer_auth(EDITOR)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let started = Instant::now();
    h.get("/slow").await;
    assert!(started.elapsed() < Duration::from_millis(250));
}

#[tokio::test]
async fn body_files_dribble_status_message_and_faults() {
    let h = Harness::start(StorageKind::Memory).await;
    let r = h
        .http
        .put(format!("{}/files/greetings/hello.json", h.admin))
        .bearer_auth(EDITOR)
        .header("content-type", "application/json")
        .body(r#"{"hello": "{{request.pathParams.name}}"}"#)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    h.stub(json!({"request": {"urlPathTemplate": "/hello/{name}"}, "response": {"bodyFileName": "greetings/hello.json"}}))
        .await;
    h.stub(json!({"request": {"urlPath": "/missing"}, "response": {"bodyFileName": "nope/{{request.path}}"}}))
        .await;
    let r = h
        .http
        .get(format!("{}/hello/ann", h.data))
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers()["content-type"], "application/json");
    assert_eq!(r.json::<Value>().await.unwrap(), json!({"hello": "ann"}));
    let (status, body) = h.get("/missing").await;
    assert_eq!(status, 500);
    assert!(
        !body.contains("nope"),
        "the file name is not disclosed: {body}"
    );

    h.stub(json!({"request": {"urlPath": "/drip"}, "response": {"body": "0123456789", "templated": false,
        "chunkedDribbleDelay": {"numberOfChunks": 5, "totalDurationMs": 500}}}))
        .await;
    let started = Instant::now();
    assert_eq!(h.get("/drip").await, (200, "0123456789".to_string()));
    assert!(
        started.elapsed() >= Duration::from_millis(400),
        "{:?}",
        started.elapsed()
    );

    h.stub(json!({"request": {"urlPath": "/reason"}, "response": {"status": 299, "statusMessage": "Mostly Fine", "body": "x"}}))
        .await;
    let raw = h
        .raw("GET /reason HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
        .await;
    let text = String::from_utf8_lossy(&raw);
    assert!(text.starts_with("HTTP/1.1 299 Mostly Fine\r\n"), "{text}");

    h.stub(json!({"request": {"urlPath": "/chunk"}, "response": {"fault": "MALFORMED_RESPONSE_CHUNK"}}))
        .await;
    h.stub(json!({"request": {"urlPath": "/garbage"}, "response": {"fault": "RANDOM_DATA_THEN_CLOSE"}}))
        .await;
    let raw = h.raw("GET /chunk HTTP/1.1\r\nhost: x\r\n\r\n").await;
    assert_eq!(raw, fastmock_server::dataplane::MALFORMED_CHUNK);
    let client_err = async {
        let r = h.http.get(format!("{}/chunk", h.data)).send().await?;
        r.bytes().await
    }
    .await;
    assert!(
        client_err.is_err(),
        "a malformed chunk must fail the client"
    );
    let raw = h.raw("GET /garbage HTTP/1.1\r\nhost: x\r\n\r\n").await;
    assert_eq!(raw.len(), 128);
    assert!(!raw.starts_with(b"HTTP/"));
    let still_alive = h.get("/drip").await;
    assert_eq!(still_alive.0, 200);
}

#[tokio::test]
async fn cookies_forms_multipart_and_origin_over_http() {
    let h = Harness::start(StorageKind::Memory).await;
    h.stub(json!({"request": {"method": "POST", "urlPath": "/login",
        "cookies": {"session": {"matches": "s-[0-9]+"}},
        "formParameters": {"user": {"equalTo": "ann lee"}, "role": {"hasExactly": [{"equalTo": "a"}, {"equalTo": "b"}]}},
        "scheme": "http", "host": {"equalTo": "api.example.com"}},
        "response": {"body": "{{request.formParams.user}}/{{request.cookies.session}}/{{request.host}}"}}))
        .await;
    h.stub(
        json!({"request": {"method": "POST", "urlPath": "/upload", "multipart": [
        {"name": "meta", "body": [{"jsonPath": "$.kind", "value": {"equalTo": "invoice"}}]},
        {"filename": {"matches": ".*\\.pdf"}, "body": [{"contains": "%PDF"}]}]},
        "response": {"status": 201}}),
    )
    .await;
    let r = h
        .http
        .post(format!("{}/login", h.data))
        .header("host", "api.example.com")
        .header("cookie", "theme=dark; session=s-42")
        .header("content-type", "application/x-www-form-urlencoded")
        .body("user=ann+lee&role=b&role=a")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "ann lee/s-42/api.example.com");
    let r = h
        .http
        .post(format!("{}/login", h.data))
        .header("host", "api.example.com")
        .header("cookie", "session=s-42")
        .header("content-type", "application/x-www-form-urlencoded")
        .body("user=ann+lee&role=b")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let multipart = |parts: &[(&str, Option<&str>, &str, &str)]| {
        let mut body = String::new();
        for (name, file, ct, content) in parts {
            body.push_str("--BnD\r\n");
            match file {
                Some(f) => body.push_str(&format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\n"
                )),
                None => body.push_str(&format!(
                    "Content-Disposition: form-data; name=\"{name}\"\r\n"
                )),
            }
            body.push_str(&format!("Content-Type: {ct}\r\n\r\n{content}\r\n"));
        }
        body.push_str("--BnD--\r\n");
        body
    };
    let r = h
        .http
        .post(format!("{}/upload", h.data))
        .header("content-type", "multipart/form-data; boundary=BnD")
        .body(multipart(&[
            ("meta", None, "application/json", r#"{"kind":"invoice"}"#),
            ("file", Some("a.pdf"), "application/pdf", "%PDF-1.7 ..."),
        ]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let r = h
        .http
        .post(format!("{}/upload", h.data))
        .header("content-type", "multipart/form-data; boundary=BnD")
        .body(multipart(&[("file", Some("a.txt"), "text/plain", "%PDF")]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let nm: Value = h
        .http
        .post(format!("{}/requests/near-misses", h.admin))
        .bearer_auth(VIEWER)
        .json(&json!({"method": "POST", "path": "/login", "headers": {"cookie": "session=x"}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let text = nm.to_string();
    assert!(text.contains("cookie `session` does not match"), "{text}");
}

#[tokio::test]
async fn mockserver_import_endpoint() {
    let h = Harness::start(StorageKind::Memory).await;
    let expectations = json!([
        {"id": "ping-once", "httpRequest": {"method": "GET", "path": "/ping"},
         "httpResponse": {"statusCode": 200, "body": "pong"}, "times": {"remainingTimes": 1, "unlimited": false}},
        {"httpRequest": {"method": "POST", "path": "/orders", "body": {"type": "JSON", "json": {"kind": "a"}}},
         "httpResponse": {"statusCode": 201, "reasonPhrase": "Made", "body": {"type": "JSON", "json": {"ok": true}}}},
        {"httpRequest": {"path": "/js"}, "httpResponseTemplate": {"templateType": "JAVASCRIPT", "template": "return {}"}}
    ]);
    let dry: Value = h
        .http
        .post(format!("{}/import/mockserver?tag=ms&dryRun=true", h.admin))
        .bearer_auth(EDITOR)
        .json(&expectations)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(dry["imported"], 0, "{dry}");
    assert_eq!(dry["stubs"].as_array().unwrap().len(), 2, "{dry}");
    assert!(
        dry["warnings"][0].as_str().unwrap().contains("JAVASCRIPT"),
        "{dry}"
    );
    assert_eq!(h.get("/ping").await.0, 404);
    let r = h
        .http
        .post(format!("{}/import/mockserver", h.admin))
        .bearer_auth(VIEWER)
        .json(&expectations)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let done: Value = h
        .http
        .post(format!("{}/import/mockserver", h.admin))
        .bearer_auth(EDITOR)
        .json(&json!({"expectations": expectations, "tag": "ms"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(done["imported"], 2, "{done}");
    assert_eq!(h.get("/ping").await, (200, "pong".to_string()));
    assert_eq!(h.get("/ping").await.0, 404, "times: 1 is exhausted");
    let r = h
        .http
        .post(format!("{}/orders", h.data))
        .json(&json!({"kind": "a", "extra": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let raw = h
        .raw("POST /orders HTTP/1.1\r\nhost: x\r\ncontent-type: application/json\r\ncontent-length: 12\r\nconnection: close\r\n\r\n{\"kind\":\"a\"}")
        .await;
    assert!(String::from_utf8_lossy(&raw).starts_with("HTTP/1.1 201 Made"));
    let listed: Value = h
        .http
        .get(format!("{}/stubs", h.admin))
        .bearer_auth(VIEWER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        listed["stubs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["tags"][0] == "ms")
    );
    let bad = h
        .http
        .post(format!("{}/import/mockserver?tag=bad%20tag", h.admin))
        .bearer_auth(EDITOR)
        .json(&expectations)
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
}
