use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use serde_json::{Value, json};
use sqlx::Connection;
use tokio::sync::watch;

const ADMIN: &str = "fm_admin_token_for_tests_0123456789";
const EDITOR: &str = "fm_editor_token_for_tests_0123456789";
const VIEWER: &str = "fm_viewer_token_for_tests_0123456789";

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

type Call = (String, BTreeMap<String, String>, String);

#[derive(Clone, Default)]
struct Received(Arc<Mutex<Vec<Call>>>);

impl Received {
    fn all(&self) -> Vec<Call> {
        self.0.lock().unwrap().clone()
    }
    async fn wait_for(&self, n: usize, timeout: Duration) -> Vec<Call> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let v = self.all();
            if v.len() >= n || tokio::time::Instant::now() > deadline {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

async fn receive(
    State(r): State<Received>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: String,
) -> StatusCode {
    let h = headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let path = uri.path().to_string();
    let flaky = path.contains("flaky");
    let mut v = r.0.lock().unwrap();
    v.push((path, h, body));
    if flaky && v.iter().filter(|x| x.0.contains("flaky")).count() < 2 {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::NO_CONTENT
    }
}

async fn receiver() -> (String, Received) {
    let r = Received::default();
    let app = Router::new()
        .route("/{*rest}", post(receive))
        .with_state(r.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}"), r)
}

struct Harness {
    data: String,
    admin: String,
    http: reqwest::Client,
    _shutdown: watch::Sender<bool>,
}

impl Harness {
    async fn start(mut cfg: Config, receiver: &str) -> Harness {
        let _ = receiver;
        fastmock_server::register_plugins(test_plugins);
        cfg.plugin_overrides.insert(
            "echo".into(),
            BTreeMap::from([("greeting".into(), "hello from settings".into())]),
        );
        let (app, data, admin, policy) = fastmock_server::build(cfg, prom()).await.unwrap();
        let (tx, rx) = watch::channel(false);
        let dl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (da, aa) = (dl.local_addr().unwrap(), al.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(dl, data).await.unwrap() });
        tokio::spawn(async move { axum::serve(al, admin).await.unwrap() });
        tokio::spawn(fastmock_server::outbox::run(
            app.clone(),
            policy,
            rx.clone(),
        ));
        tokio::spawn(fastmock_server::maintenance(app.clone(), rx.clone()));
        tokio::spawn(fastmock_server::listen_notifications(
            app.clone(),
            rx.clone(),
        ));
        app.ready.store(true, Ordering::SeqCst);
        Harness {
            data: format!("http://{da}"),
            admin: format!("http://{aa}"),
            http: reqwest::Client::new(),
            _shutdown: tx,
        }
    }

    fn admin(&self, method: reqwest::Method, path: &str, token: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}/__admin{path}", self.admin))
            .bearer_auth(token)
    }

    async fn create_stub(&self, stub: Value) -> Value {
        let r = self
            .admin(reqwest::Method::POST, "/stubs", EDITOR)
            .json(&stub)
            .send()
            .await
            .unwrap();
        let st = r.status();
        let body: Value = r.json().await.unwrap();
        assert_eq!(st, 201, "{body}");
        body
    }

    async fn requests(&self, query: &str) -> Vec<Value> {
        let v: Value = self
            .admin(reqwest::Method::GET, &format!("/requests?{query}"), VIEWER)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        v["requests"].as_array().unwrap().clone()
    }
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
    let db = format!("fm_srv_{}", uuid::Uuid::new_v4().simple());
    let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut c)
        .await
        .unwrap();
    Some(StorageKind::Postgres {
        database_url: format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db),
        valkey_url,
        valkey_prefix: format!("srv:{}:", uuid::Uuid::new_v4().simple()),
        max_connections: 5,
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
async fn admin_api_requires_tokens_and_roles() {
    let (rx_url, _) = receiver().await;
    let h = Harness::start(Config::for_tests(StorageKind::Memory, tokens()), &rx_url).await;
    let stub = json!({"request": {"urlPath": "/x"}, "response": {"body": "x"}});
    let r = h
        .http
        .get(format!("{}/__admin/stubs", h.admin))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = h
        .admin(reqwest::Method::GET, "/stubs", "wrong-token")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = h
        .admin(reqwest::Method::POST, "/stubs", VIEWER)
        .json(&stub)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = h
        .admin(reqwest::Method::GET, "/stubs", VIEWER)
        .header("origin", "https://evil.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = h
        .admin(reqwest::Method::GET, "/stubs", VIEWER)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["cache-control"], "no-store");
    assert_eq!(r.headers()["x-content-type-options"], "nosniff");
    h.create_stub(stub).await;
    let r = h
        .admin(reqwest::Method::DELETE, "/stubs", EDITOR)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "deleting all stubs needs admin");
    let r = h
        .admin(reqwest::Method::POST, "/reset", EDITOR)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = h
        .admin(reqwest::Method::GET, "/audit", EDITOR)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r: Value = h
        .admin(reqwest::Method::GET, "/audit", ADMIN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["events"][0]["action"], "stub.create");
    assert_eq!(r["events"][0]["principal"], "ci");
    let r = h
        .admin(reqwest::Method::POST, "/stubs", EDITOR)
        .json(&json!({"request": {"urlPathPattern": "("}, "response": {}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let r = h
        .admin(reqwest::Method::POST, "/stubs", EDITOR)
        .json(&json!({"request": {"bogus": 1}, "response": {}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let r = h
        .http
        .get(format!("{}/__admin/stubs", h.data))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        404,
        "control plane is not served on the mock port"
    );
    let m = h
        .http
        .get(format!("{}/metrics", h.admin))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(m.contains("fastmock_admin_auth_failures_total"));
}

#[tokio::test]
async fn stub_flow_with_callbacks_journal_and_faults() {
    for storage in storages().await {
        let label = format!("{storage:?}").chars().take(8).collect::<String>();
        let (rx_url, rx) = receiver().await;
        let mut cfg = Config::for_tests(storage, tokens());
        cfg.secrets_dir = None;
        let h = Harness::start(cfg, &rx_url).await;
        h.create_stub(json!({
            "name": "send",
            "request": {"method": "POST", "urlPathTemplate": "/api/{ver}/send", "body": [{"jsonPath": "$.phone", "value": {"matches": "380\\d{9}"}}]},
            "response": {"status": 202, "jsonBody": {"phone": "{{request.json.phone}}", "ver": "{{request.pathParams.ver}}"}},
            "callbacks": [
                {"url": format!("{rx_url}/cb"), "jsonBody": {"phone": "{{request.json.phone}}", "password": "p"}, "correlation": "{{request.json.phone}}"},
                {"url": format!("{rx_url}/flaky"), "body": "x", "maxAttempts": 3},
                {"url": "http://169.254.169.254/latest"}
            ]
        }))
        .await;
        h.create_stub(
            json!({"request": {"urlPath": "/reset"}, "response": {"fault": "CONNECTION_RESET"}}),
        )
        .await;
        h.create_stub(
            json!({"request": {"urlPath": "/malformed"}, "response": {"fault": "MALFORMED_JSON"}}),
        )
        .await;
        h.create_stub(json!({"request": {"urlPath": "/slow"}, "response": {"delay": {"fixedMs": 300}, "body": "late"}})).await;
        let r = h
            .http
            .post(format!("{}/api/v2/send", h.data))
            .header(
                "traceparent",
                "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
            )
            .header("authorization", "Bearer caller-secret")
            .json(&json!({"phone": "380501234567", "password": "hunter2"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 202, "{label}");
        assert_eq!(
            r.headers()["x-fastmock-trace-id"],
            "0af7651916cd43dd8448eb211c80319c"
        );
        assert_eq!(
            r.json::<Value>().await.unwrap(),
            json!({"phone": "380501234567", "ver": "v2"})
        );
        let got = rx.wait_for(3, Duration::from_secs(10)).await;
        let cb = got
            .iter()
            .find(|x| x.0 == "/cb")
            .expect("callback delivered");
        assert!(
            cb.1["traceparent"].starts_with("00-0af7651916cd43dd8448eb211c80319c-"),
            "{label}: trace continues into callback"
        );
        let flaky_ids: Vec<&String> = got
            .iter()
            .filter(|x| x.0 == "/flaky")
            .map(|x| &x.1["x-fastmock-delivery-id"])
            .collect();
        assert_eq!(flaky_ids.len(), 2);
        assert_eq!(
            flaky_ids[0], flaky_ids[1],
            "{label}: retries carry the same idempotency key"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&cb.2).unwrap()["phone"],
            "380501234567"
        );
        assert_eq!(
            got.iter().filter(|x| x.0 == "/flaky").count(),
            2,
            "{label}: retried once after 503"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let cbs: Value = h
            .admin(reqwest::Method::GET, "/callbacks", VIEWER)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let cbs = cbs["callbacks"].as_array().unwrap();
        let blocked = cbs
            .iter()
            .find(|c| c["url"].as_str().unwrap().contains("169.254"))
            .unwrap();
        assert_eq!(blocked["status"], "failed");
        assert!(
            blocked["lastError"]
                .as_str()
                .unwrap()
                .starts_with("blocked by egress policy"),
            "{label}"
        );
        let sent = cbs
            .iter()
            .find(|c| c["url"].as_str().unwrap().ends_with("/cb"))
            .unwrap();
        assert!(
            sent["body"]
                .as_str()
                .unwrap()
                .contains("\"password\":\"***\""),
            "{label}: callback bodies are redacted in the admin API"
        );
        let reqs = h.requests("correlation=380501234567").await;
        assert_eq!(reqs.len(), 1, "{label}");
        assert_eq!(reqs[0]["headers"]["authorization"], "Bearer ***");
        assert!(
            reqs[0]["body"]
                .as_str()
                .unwrap()
                .contains("\"password\":\"***\"")
        );
        assert_eq!(reqs[0]["traceId"], "0af7651916cd43dd8448eb211c80319c");
        let r = h
            .http
            .post(format!("{}/api/v2/send", h.data))
            .json(&json!({"phone": "1"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
        let body: Value = r.json().await.unwrap();
        assert_eq!(
            body["nearMisses"][0]["mismatches"][0],
            "body matcher #0 does not match"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        let c: Value = h
            .admin(
                reqwest::Method::GET,
                "/requests/count?matched=false",
                VIEWER,
            )
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(c["count"], 1, "{label}");
        let err = h
            .http
            .get(format!("{}/reset", h.data))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map(|_| ());
        let body = match err {
            Ok(()) => "ok".to_string(),
            Err(e) => e.to_string(),
        };
        assert_ne!(
            body, "ok",
            "{label}: connection reset fault breaks the response"
        );
        let r = h
            .http
            .get(format!("{}/malformed", h.data))
            .send()
            .await
            .unwrap();
        assert!(r.json::<Value>().await.is_err(), "{label}");
        let start = std::time::Instant::now();
        let r = h.http.get(format!("{}/slow", h.data)).send().await.unwrap();
        assert_eq!(r.text().await.unwrap(), "late");
        assert!(start.elapsed() >= Duration::from_millis(290));
        let big = "x".repeat(2 * 1024 * 1024);
        let r = h
            .http
            .post(format!("{}/api/v2/send", h.data))
            .body(big)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 413, "{label}");
    }
}

struct EchoPlugin;

impl fastmock_core::Plugin for EchoPlugin {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn description(&self) -> &'static str {
        "Test plugin: GET /hello answers with the greeting setting"
    }

    fn router(&self, ctx: fastmock_core::PluginContext) -> anyhow::Result<Router> {
        let greeting = ctx.settings.get_or("greeting", "hello");
        Ok(Router::new().route(
            "/hello",
            axum::routing::get(move || async move { greeting }),
        ))
    }

    fn admin_router(&self, ctx: fastmock_core::PluginContext) -> Option<Router> {
        let name = ctx.name.clone();
        Some(Router::new().route(
            "/info",
            axum::routing::get(move || async move { axum::Json(json!({"plugin": name})) }),
        ))
    }
}

fn test_plugins() -> Vec<Box<dyn fastmock_core::Plugin>> {
    vec![Box::new(EchoPlugin)]
}

#[tokio::test]
async fn registered_plugins_are_mounted() {
    for storage in storages().await {
        let (rx_url, _rx) = receiver().await;
        let h = Harness::start(Config::for_tests(storage, tokens()), &rx_url).await;
        let r = h
            .http
            .get(format!("{}/echo/hello", h.data))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.text().await.unwrap(), "hello from settings");
        let info: Value = h
            .admin(reqwest::Method::GET, "/plugins/echo/info", VIEWER)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(info["plugin"], "echo");
        let list: Value = h
            .admin(reqwest::Method::GET, "/plugins", VIEWER)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let plugins = list["plugins"].as_array().unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0]["name"], "echo");
        assert_eq!(plugins[0]["mount"], "/echo");
    }
}

#[tokio::test]
async fn replicas_share_state_and_deliver_callbacks_once() {
    let Some(storage) = pg_storage().await else {
        eprintln!("postgres/valkey not configured: skipping");
        return;
    };
    let (rx_url, rx) = receiver().await;
    let a = Harness::start(Config::for_tests(storage.clone(), tokens()), &rx_url).await;
    let b = Harness::start(Config::for_tests(storage, tokens()), &rx_url).await;
    a.create_stub(json!({
        "request": {"urlPath": "/order"},
        "scenario": {"name": "order-{{request.query.id}}", "requiredState": "Started", "newState": "paid"},
        "response": {"body": "created"},
        "callbacks": [{"url": format!("{rx_url}/hook"), "body": "{{request.query.id}}"}]
    }))
    .await;
    a.create_stub(json!({"request": {"urlPath": "/order"}, "scenario": {"name": "order-{{request.query.id}}", "requiredState": "paid"}, "response": {"body": "already paid"}})).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let r1 = a
        .http
        .get(format!("{}/order?id=1", a.data))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let r2 = b
        .http
        .get(format!("{}/order?id=1", b.data))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        (r1.as_str(), r2.as_str()),
        ("created", "already paid"),
        "scenario state is shared through Valkey"
    );
    for i in 2..40 {
        let target = if i % 2 == 0 { &a } else { &b };
        target
            .http
            .get(format!("{}/order?id={i}", target.data))
            .send()
            .await
            .unwrap();
    }
    let got = rx.wait_for(39, Duration::from_secs(15)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let got_after = rx.all();
    assert_eq!(got.len(), 39);
    assert_eq!(
        got_after.len(),
        39,
        "every callback is delivered exactly once by the two replicas"
    );
    let journal = b.requests("path_prefix=/order&limit=1000").await;
    assert!(journal.len() >= 40, "journal is shared: {}", journal.len());
}

#[tokio::test]
async fn phase2_openapi_wiremock_proxy_and_recording() {
    let (rx_url, _) = receiver().await;
    let upstream = Harness::start(Config::for_tests(StorageKind::Memory, tokens()), &rx_url).await;
    upstream.create_stub(json!({"request": {"method": "GET", "urlPath": "/real/customers/42"}, "response": {"status": 200, "headers": {"x-up": "1"}, "jsonBody": {"id": 42, "name": "Ann"}}})).await;
    upstream.create_stub(json!({"request": {"method": "POST", "urlPath": "/real/orders"}, "response": {"status": 201, "body": "created"}})).await;
    let h = Harness::start(Config::for_tests(StorageKind::Memory, tokens()), &rx_url).await;
    let spec = r##"
openapi: 3.0.3
info: {title: T, version: "1"}
paths:
  /pets/{id}:
    get:
      operationId: getPet
      parameters: [{name: id, in: path, required: true, schema: {type: integer}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Pet"}}}}
  /pets:
    post:
      operationId: addPet
      requestBody: {content: {application/json: {schema: {type: object, required: [name], properties: {name: {type: string, minLength: 2}}}}}}
      responses: {"201": {description: created, content: {application/json: {example: {id: 1}}}}}
components:
  schemas:
    Pet: {type: object, properties: {id: {type: integer, example: 5}, name: {type: string, example: rex}}}
"##;
    let r: Value = h
        .admin(reqwest::Method::POST, "/import/openapi", EDITOR)
        .json(&json!({"spec": spec, "tag": "pets", "pathPrefix": "/v1"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["imported"], 4, "{r}");
    let v: Value = h
        .http
        .get(format!("{}/v1/pets/9", h.data))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v, json!({"id": 5, "name": "rex"}));
    assert_eq!(
        h.http
            .get(format!("{}/v1/pets/abc", h.data))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        h.http
            .post(format!("{}/v1/pets", h.data))
            .json(&json!({"name": "ok"}))
            .send()
            .await
            .unwrap()
            .status(),
        201
    );
    let bad = h
        .http
        .post(format!("{}/v1/pets", h.data))
        .json(&json!({"name": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    assert_eq!(bad.json::<Value>().await.unwrap()["operation"], "addPet");
    let r: Value = h
        .admin(reqwest::Method::POST, "/import/openapi", EDITOR)
        .json(&json!({"spec": spec, "tag": "pets", "pathPrefix": "/v1"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (r["imported"].as_i64(), r["removed"].as_i64()),
        (Some(4), Some(4)),
        "re-import replaces by tag"
    );
    let wm = json!({"mappings": [{"request": {"method": "GET", "urlPathPattern": "/wm/[0-9]+"}, "response": {"status": 200, "body": "seg={{request.pathSegments.[1]}}", "transformers": ["response-template"]}}]});
    let r: Value = h
        .admin(reqwest::Method::POST, "/import/wiremock", EDITOR)
        .json(&json!({"mappings": wm}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["imported"], 1, "{r}");
    assert_eq!(
        h.http
            .get(format!("{}/wm/77", h.data))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "seg=77"
    );
    h.create_stub(json!({"request": {"urlPathPattern": "/api/customers/.*"}, "response": {"proxy": {"baseUrl": format!("{}/real", upstream.data), "stripPrefix": "/api"}}})).await;
    let r = h
        .http
        .get(format!("{}/api/customers/42", h.data))
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers()["x-up"], "1");
    assert_eq!(r.json::<Value>().await.unwrap()["name"], "Ann");
    let bad_target = h
        .admin(reqwest::Method::POST, "/recordings/start", EDITOR)
        .json(&json!({"targetBaseUrl": "http://169.254.169.254", "tag": "rec"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        bad_target.status(),
        400,
        "recording target obeys the egress policy"
    );
    let r = h
        .admin(reqwest::Method::POST, "/recordings/start", EDITOR)
        .json(&json!({"targetBaseUrl": upstream.data, "tag": "rec", "captureHeaders": ["x-up"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let a = h
        .http
        .get(format!("{}/real/customers/42", h.data))
        .send()
        .await
        .unwrap();
    assert_eq!(a.status(), 200);
    let b = h
        .http
        .post(format!("{}/real/orders", h.data))
        .json(&json!({"sku": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(b.text().await.unwrap(), "created");
    let st: Value = h
        .admin(reqwest::Method::POST, "/recordings/stop", EDITOR)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(st["recordedStubs"], 2, "{st}");
    let replay = h
        .http
        .get(format!("{}/real/customers/42", h.data))
        .send()
        .await
        .unwrap();
    assert_eq!(replay.headers()["x-up"], "1");
    assert_eq!(
        replay.json::<Value>().await.unwrap(),
        json!({"id": 42, "name": "Ann"})
    );
    assert_eq!(
        h.http
            .post(format!("{}/real/orders", h.data))
            .json(&json!({"sku": "y"}))
            .send()
            .await
            .unwrap()
            .status(),
        404,
        "body is part of the recorded match"
    );
    let exp: Value = h
        .admin(reqwest::Method::GET, "/export?tag=rec", VIEWER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(exp["stubs"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn admin_ui_is_served_with_strict_csp() {
    let (rx_url, _) = receiver().await;
    let h = Harness::start(Config::for_tests(StorageKind::Memory, tokens()), &rx_url).await;
    let r = h.http.get(format!("{}/ui", h.admin)).send().await.unwrap();
    let built = fastmock_server::ui::built();
    assert_eq!(r.status(), if built { 200 } else { 404 });
    let csp = r.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        csp.contains("script-src 'self';")
            && !csp.contains("unsafe-inline")
            && !csp.contains("unsafe-eval")
            && !csp.contains('*'),
        "{csp}"
    );
    assert_eq!(r.headers()["x-frame-options"], "DENY");
    assert_eq!(r.headers()["cache-control"], "no-cache");
    let html = r.text().await.unwrap();
    if built {
        assert!(html.contains("/ui/assets/"), "{html}");
        assert!(!html.contains("<script>"), "no inline scripts");
        let nonce = csp
            .split("'nonce-")
            .nth(1)
            .and_then(|r| r.split('\'').next())
            .unwrap()
            .to_string();
        assert!(
            html.contains(&format!("content=\"{nonce}\"")),
            "the page carries the style nonce of its CSP"
        );
        assert!(!html.contains("__FASTMOCK_STYLE_NONCE__"));
        let asset = html
            .split('"')
            .find(|p| p.starts_with("/ui/assets/") && p.ends_with(".js"))
            .unwrap()
            .to_string();
        let a = h
            .http
            .get(format!("{}{asset}", h.admin))
            .send()
            .await
            .unwrap();
        assert_eq!(a.status(), 200);
        assert_eq!(
            a.headers()["content-type"],
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            a.headers()["cache-control"],
            "public, max-age=31536000, immutable"
        );
        let deep = h
            .http
            .get(format!(
                "{}/ui/stubs/3f0c9a43-0000-4000-8000-000000000000",
                h.admin
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(deep.status(), 200, "client routes fall back to index.html");
        assert!(deep.text().await.unwrap().contains("/ui/assets/"));
    } else {
        assert!(html.contains("not available"), "{html}");
    }
    let missing = h
        .http
        .get(format!("{}/ui/assets/missing-0000.js", h.admin))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
    let cfg: serde_json::Value = h
        .http
        .get(format!("{}/ui/config.json", h.admin))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cfg["dataPlaneScheme"], "http");
    assert_eq!(
        h.http
            .get(format!("{}/ui", h.data))
            .send()
            .await
            .unwrap()
            .status(),
        404,
        "UI is not on the mock port"
    );
}

#[tokio::test]
async fn phase4_chaos_sse_xpath_graphql_and_kafka() {
    let (rx_url, _) = receiver().await;
    let mut cfg = Config::for_tests(StorageKind::Memory, tokens());
    let kafka = std::env::var("FASTMOCK_TEST_KAFKA").ok();
    let registry = std::env::var("FASTMOCK_TEST_SCHEMA_REGISTRY").ok();
    cfg.kafka_bootstrap = kafka.clone().map(|k| vec![k]);
    cfg.kafka_schema_registry = registry.clone();
    let h = Harness::start(cfg, &rx_url).await;
    h.create_stub(
        json!({"request": {"urlPathTemplate": "/flaky/{x}"}, "response": {"body": "fine"}}),
    )
    .await;
    let r = h.admin(reqwest::Method::PUT, "/chaos", EDITOR).json(&json!({"rules": [{"id": "down", "pathPrefix": "/flaky/a", "probability": 1.0, "status": 503, "body": "chaos"}, {"id": "slow", "pathPrefix": "/flaky/b", "probability": 1.0, "delay": {"fixedMs": 300}}]})).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let a = h
        .http
        .get(format!("{}/flaky/a", h.data))
        .send()
        .await
        .unwrap();
    assert_eq!(
        (
            a.status().as_u16(),
            a.headers()["x-fastmock-chaos"]
                .to_str()
                .unwrap()
                .to_string()
        ),
        (503, "down".to_string())
    );
    let t = std::time::Instant::now();
    let b = h
        .http
        .get(format!("{}/flaky/b", h.data))
        .send()
        .await
        .unwrap();
    assert!(t.elapsed() >= Duration::from_millis(290));
    assert_eq!(b.text().await.unwrap(), "fine");
    assert_eq!(
        h.http
            .get(format!("{}/flaky/c", h.data))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "fine"
    );
    let bad = h
        .admin(reqwest::Method::PUT, "/chaos", EDITOR)
        .json(&json!({"rules": [{"id": "x", "probability": 2.0, "status": 500}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    assert_eq!(
        h.admin(reqwest::Method::DELETE, "/chaos", EDITOR)
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    assert_eq!(
        h.http
            .get(format!("{}/flaky/a", h.data))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    h.create_stub(
        json!({"request": {"urlPath": "/events"}, "response": {"sse": [
        {"event": "status", "data": "queued {{request.query.id}}"},
        {"event": "status", "data": "sent", "delayMs": 250},
        {"id": "3", "data": "line1\nline2", "delayMs": 250}]}}),
    )
    .await;
    let t = std::time::Instant::now();
    let mut resp = h
        .http
        .get(format!("{}/events?id=7", h.data))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let first = resp.chunk().await.unwrap().unwrap();
    assert!(
        t.elapsed() < Duration::from_millis(200),
        "the first event is streamed before the delayed ones"
    );
    assert_eq!(
        String::from_utf8_lossy(&first),
        "event: status\ndata: queued 7\n\n"
    );
    let mut rest = Vec::new();
    while let Some(c) = resp.chunk().await.unwrap() {
        rest.extend_from_slice(&c);
    }
    assert_eq!(
        String::from_utf8_lossy(&rest),
        "event: status\ndata: sent\n\nid: 3\ndata: line1\ndata: line2\n\n"
    );
    assert!(t.elapsed() >= Duration::from_millis(490));

    h.create_stub(json!({"request": {"method": "POST", "urlPath": "/soap/balance", "body": [{"xpath": "//getBalance/msisdn", "value": {"matches": "380\\d{9}"}}]},
        "response": {"headers": {"content-type": "text/xml"}, "body": "<Envelope><Body><balance>100</balance></Body></Envelope>"}})).await;
    let soap = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" xmlns:b="urn:b"><s:Body><b:getBalance><b:msisdn>380501234567</b:msisdn></b:getBalance></s:Body></s:Envelope>"#;
    let r = h
        .http
        .post(format!("{}/soap/balance", h.data))
        .header("content-type", "text/xml")
        .body(soap)
        .send()
        .await
        .unwrap();
    assert!(r.text().await.unwrap().contains("<balance>100</balance>"));
    assert_eq!(
        h.http
            .post(format!("{}/soap/balance", h.data))
            .body(soap.replace("380501234567", "123"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    h.create_stub(json!({"request": {"method": "POST", "urlPath": "/graphql", "graphql": {"operationName": {"equalTo": "GetUser"}}},
        "response": {"jsonBody": {"data": {"user": {"id": "{{json request.json.variables.id}}"}}}}})).await;
    let g: Value = h.http.post(format!("{}/graphql", h.data)).json(&json!({"operationName": "GetUser", "query": "query GetUser($id: ID!) { user(id: $id) { id } }", "variables": {"id": 5}})).send().await.unwrap().json().await.unwrap();
    assert_eq!(g, json!({"data": {"user": {"id": 5}}}));
    assert_eq!(
        h.http
            .post(format!("{}/graphql", h.data))
            .json(&json!({"operationName": "Other"}))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    let (Some(kafka), Some(_)) = (kafka, registry) else {
        eprintln!(
            "FASTMOCK_TEST_KAFKA / FASTMOCK_TEST_SCHEMA_REGISTRY not set: Kafka part skipped"
        );
        return;
    };
    let topic = "orders-events";
    let client = rskafka::client::ClientBuilder::new(vec![kafka.clone()])
        .build()
        .await
        .unwrap();
    let controller = client.controller_client().unwrap();
    for t in [topic, "orders-events-avro"] {
        if let Err(e) = controller.create_topic(t, 3, 1, 30_000).await {
            assert!(
                e.to_string().contains("TopicAlreadyExists"),
                "create topic {t}: {e}"
            );
        }
    }
    let schema = json!({"type": "record", "name": "Event", "namespace": "fm.test", "fields": [{"name": "msisdn", "type": "string"}, {"name": "amount", "type": ["null", "long"], "default": null}]});
    h.create_stub(json!({"request": {"method": "POST", "urlPath": "/charge"}, "response": {"status": 202},
        "messages": [
            {"topic": topic, "key": "{{request.json.msisdn}}", "jsonValue": {"msisdn": "{{request.json.msisdn}}", "amount": "{{json request.json.amount}}"}, "headers": {"source": "fastmock"}},
            {"topic": "orders-events-avro", "key": "k", "jsonValue": {"msisdn": "{{request.json.msisdn}}", "amount": "{{json request.json.amount}}"}, "avroSchema": schema}
        ]})).await;
    let partitions = client
        .list_topics()
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.name == topic)
        .unwrap()
        .partitions
        .len() as i32;
    let p = fastmock_kafka::partition_for(Some(b"380501112233"), partitions);
    let pc = client
        .partition_client(
            topic,
            p,
            rskafka::client::partition::UnknownTopicHandling::Retry,
        )
        .await
        .unwrap();
    let before = pc
        .get_offset(rskafka::client::partition::OffsetAt::Latest)
        .await
        .unwrap();
    assert_eq!(
        h.http
            .post(format!("{}/charge", h.data))
            .json(&json!({"msisdn": "380501112233", "amount": 15}))
            .send()
            .await
            .unwrap()
            .status(),
        202
    );
    let (records, _) = pc.fetch_records(before, 1..1_000_000, 2_000).await.unwrap();
    let rec = &records.first().expect("message published").record;
    assert_eq!(rec.key.as_deref(), Some(&b"380501112233"[..]));
    assert_eq!(
        serde_json::from_slice::<Value>(rec.value.as_ref().unwrap()).unwrap(),
        json!({"msisdn": "380501112233", "amount": 15})
    );
    assert_eq!(rec.headers["source"], b"fastmock".to_vec());
    let lc = client
        .partition_client(
            "orders-events-avro",
            fastmock_kafka::partition_for(Some(b"k"), 3),
            rskafka::client::partition::UnknownTopicHandling::Retry,
        )
        .await
        .unwrap();
    let mut latest = 0;
    for _ in 0..50 {
        latest = lc
            .get_offset(rskafka::client::partition::OffsetAt::Latest)
            .await
            .unwrap();
        if latest > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(latest > 0, "avro record published");
    let (avro, _) = lc
        .fetch_records(latest - 1, 1..1_000_000, 2_000)
        .await
        .unwrap();
    let v = avro.last().unwrap().record.value.clone().unwrap();
    assert_eq!(v[0], 0, "Confluent magic byte");
    let parsed = apache_avro::Schema::parse(&schema).unwrap();
    #[allow(deprecated)]
    let decoded = apache_avro::from_avro_datum(&parsed, &mut &v[5..], None).unwrap();
    assert_eq!(
        apache_avro::from_value::<Value>(&decoded).unwrap(),
        json!({"msisdn": "380501112233", "amount": 15})
    );
}

#[tokio::test]
async fn review_hardening_regressions() {
    let (rx_url, rx) = receiver().await;
    let dir = std::env::temp_dir().join(format!("fm-sec-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("hook-key"), "allowed").unwrap();
    std::fs::write(dir.join("plugin-only"), "private").unwrap();
    let mut cfg = Config::for_tests(StorageKind::Memory, tokens());
    cfg.secrets_dir = Some(dir.clone());
    cfg.stub_secrets = vec!["hook-key".into()];
    cfg.max_inflight_body_bytes = 2 * 1024 * 1024;
    let h = Harness::start(cfg, &rx_url).await;

    h.create_stub(json!({"request": {"urlPath": "/loop"}, "response": {"proxy": {"baseUrl": "http://[::]:1"}}})).await;
    let r = h.http.get(format!("{}/loop", h.data)).send().await.unwrap();
    assert_eq!(r.status(), 502);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["detail"], "blocked by egress policy", "{body}");

    for (path, secret) in [("/ok", "hook-key"), ("/denied", "plugin-only")] {
        h.create_stub(json!({"request": {"urlPath": path}, "response": {"status": 200},
            "callbacks": [{"url": format!("{rx_url}{path}"), "body": "{{request.body}}", "hmac": {"secretRef": secret, "header": "x-sig"}}]})).await;
        h.http
            .post(format!("{}{path}", h.data))
            .body("payload")
            .send()
            .await
            .unwrap();
    }
    let got = rx.wait_for(1, Duration::from_secs(5)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let got: Vec<_> = got.into_iter().chain(rx.all()).collect();
    assert!(
        got.iter()
            .any(|c| c.0 == "/ok" && c.1.contains_key("x-sig"))
    );
    assert!(
        !got.iter().any(|c| c.0 == "/denied"),
        "a secret outside FASTMOCK_STUB_SECRETS must not sign"
    );
    let cbs: Value = h
        .admin(reqwest::Method::GET, "/callbacks", VIEWER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let text = cbs.to_string();
    assert!(text.contains("is not configured"), "{text}");
    assert!(
        !cbs["callbacks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["headers"]["x-sig"].as_str().is_some_and(|v| v != "***")),
        "{text}"
    );

    h.create_stub(json!({"request": {"urlPath": "/sse"}, "response": {"sse": [{"data": "hello {{request.query.n}}"}]}})).await;
    let txt = h
        .http
        .get(format!("{}/sse?n=x%0Devent:%20admin", h.data))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !txt.contains('\r') && txt.contains("data: event: admin"),
        "{txt:?}"
    );

    h.create_stub(
        json!({"request": {"urlPath": "/size"}, "response": {"body": "{{size request.json}}"}}),
    )
    .await;
    let big = format!("[{}0]", "0,".repeat(300_000));
    let r = h
        .http
        .post(format!("{}/size", h.data))
        .header("content-type", "application/json")
        .body(big)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "0", "oversized JSON is not parsed");
    let r = h
        .http
        .post(format!("{}/size", h.data))
        .body(vec![b'a'; 2 * 1024 * 1024])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);

    h.create_stub(json!({"request": {"urlPath": "/xml", "body": [{"xpath": "//a//a"}]}, "response": {"status": 200}})).await;
    let deep = format!("{}{}", "<a>".repeat(5000), "</a>".repeat(5000));
    let started = std::time::Instant::now();
    let r = h
        .http
        .post(format!("{}/xml", h.data))
        .header("content-type", "text/xml")
        .body(deep)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );

    h.http
        .post(format!("{}/nothing", h.data))
        .header("content-type", "text/xml")
        .body("<r><password>Hunter2xml</password></r>")
        .send()
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let journal = serde_json::to_string(&h.requests("pathPrefix=/nothing").await).unwrap();
    assert!(!journal.contains("Hunter2xml"), "{journal}");
    let r = h.admin(reqwest::Method::POST, "/stubs", EDITOR).json(&json!({"request": {"urlPath": "/boom"}, "response": {"body": "{{#*inline \"p\"}}{{> p}}{{/inline}}{{> p}}"}})).send().await.unwrap();
    assert_eq!(r.status(), 400);
    h.create_stub(json!({"request": {"urlPath": "/sec", "basicAuth": {"username": "u", "password": "BASIC-SECRET"}, "headers": {"Authorization": {"equalTo": "Bearer REQ-SECRET"}}}, "response": {"headers": {"x-api-key": "RESP-SECRET"}},
        "callbacks": [{"url": format!("{rx_url}/sec"), "headers": {"Authorization": "Bearer CB-SECRET"}}]})).await;
    let as_viewer = h
        .admin(reqwest::Method::GET, "/stubs", VIEWER)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !as_viewer.contains("RESP-SECRET")
            && !as_viewer.contains("CB-SECRET")
            && !as_viewer.contains("BASIC-SECRET")
            && !as_viewer.contains("REQ-SECRET"),
        "{as_viewer}"
    );
    let as_editor = h
        .admin(reqwest::Method::GET, "/stubs", EDITOR)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(as_editor.contains("RESP-SECRET"));
    let r = h
        .admin(reqwest::Method::POST, "/requests/count", VIEWER)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    std::fs::remove_dir_all(dir).ok();
}
