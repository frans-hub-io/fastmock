use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use fastmock_server::state::AppState;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sqlx::Connection;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

const ADMIN: &str = "fm_admin_token_for_async_tests_01234";
const EDITOR: &str = "fm_editor_token_for_async_tests_0123";
const VIEWER: &str = "fm_viewer_token_for_async_tests_0123";

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

struct Harness {
    app: AppState,
    data: String,
    admin: String,
    http: reqwest::Client,
    shutdown: watch::Sender<bool>,
}

impl Harness {
    async fn start(cfg: Config) -> Harness {
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
        app.ready.store(true, Ordering::SeqCst);
        Harness {
            app,
            data: format!("http://{da}"),
            admin: format!("http://{aa}"),
            http: reqwest::Client::new(),
            shutdown: tx,
        }
    }

    fn admin(&self, method: reqwest::Method, path: &str, token: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}/__admin{path}", self.admin))
            .bearer_auth(token)
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        self.call_as(EDITOR, method, path, body).await
    }

    async fn call_as(
        &self,
        token: &str,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut rb = self.admin(method, path, token);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let r = rb.send().await.unwrap();
        let st = r.status().as_u16();
        let text = r.text().await.unwrap();
        (st, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    fn ws_url(&self, path: &str) -> String {
        format!("{}{path}", self.data.replace("http://", "ws://"))
    }

    async fn wait_run(&self, id: &str) -> Value {
        for _ in 0..200 {
            let (st, v) = self
                .call(reqwest::Method::GET, &format!("/contract-tests/{id}"), None)
                .await;
            assert_eq!(st, 200, "{v}");
            if v["status"] != "running" {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("contract test run {id} did not finish");
    }
}

async fn next_text<S>(ws: &mut S) -> Option<String>
where
    S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => return Some(t.to_string()),
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => return None,
            Ok(Some(Ok(_))) => continue,
            Err(_) => panic!("no WebSocket message within 5 s"),
        }
    }
}

#[tokio::test]
async fn websocket_stubs_stream_reply_echo_and_close() {
    let h = Harness::start(Config::for_tests(StorageKind::Memory, tokens())).await;
    let (st, v) = h
        .call(
            reqwest::Method::POST,
            "/stubs",
            Some(json!({
                "request": {"method": "GET", "urlPathTemplate": "/rooms/{room}"},
                "response": {"status": 426, "body": "upgrade required", "websocket": {
                    "messages": [{"data": "hello {{request.pathParams.room}} {{request.query.user}}"}],
                    "onMessage": [
                        {"match": {"contains": "bye"}, "reply": "ciao {{message.text}}", "close": true},
                        {"match": {"matches": "^\\{.*"}, "reply": "id={{message.json.id}}"}
                    ],
                    "echo": true,
                    "maxMessages": 10
                }}
            })),
        )
        .await;
    assert_eq!(st, 201, "{v}");
    let (mut ws, resp) = tokio_tungstenite::connect_async(h.ws_url("/rooms/r1?user=ann"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 101);
    assert_eq!(next_text(&mut ws).await.as_deref(), Some("hello r1 ann"));
    ws.send(Message::text("ping-1")).await.unwrap();
    assert_eq!(next_text(&mut ws).await.as_deref(), Some("ping-1"));
    ws.send(Message::text(r#"{"id": 42}"#)).await.unwrap();
    assert_eq!(next_text(&mut ws).await.as_deref(), Some("id=42"));
    ws.send(Message::binary(b"raw".to_vec())).await.unwrap();
    match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
        Ok(Some(Ok(Message::Binary(b)))) => assert_eq!(&b[..], b"raw"),
        other => panic!("expected the binary echo, got {other:?}"),
    }
    ws.send(Message::text("bye now")).await.unwrap();
    assert_eq!(next_text(&mut ws).await.as_deref(), Some("ciao bye now"));
    assert_eq!(next_text(&mut ws).await, None, "closed after the rule");

    let plain = h
        .http
        .get(format!("{}/rooms/r1", h.data))
        .send()
        .await
        .unwrap();
    assert_eq!(plain.status(), 426);
    assert_eq!(plain.text().await.unwrap(), "upgrade required");

    let (st, v) = h
        .call(
            reqwest::Method::POST,
            "/stubs",
            Some(json!({
                "request": {"method": "GET", "urlPath": "/ticks"},
                "response": {"websocket": {
                    "messages": [{"data": "tick {{randomInt 1 1}}", "delayMs": 20}, {"data": "tock", "delayMs": 20}],
                    "repeat": true, "maxMessages": 5
                }}
            })),
        )
        .await;
    assert_eq!(st, 201, "{v}");
    let (mut ws, _) = tokio_tungstenite::connect_async(h.ws_url("/ticks"))
        .await
        .unwrap();
    let mut got = vec![];
    while let Some(t) = next_text(&mut ws).await {
        got.push(t);
    }
    assert_eq!(got, vec!["tick 1", "tock", "tick 1", "tock", "tick 1"]);

    let (st, _) = h
        .call(
            reqwest::Method::POST,
            "/stubs",
            Some(json!({
                "request": {"method": "GET", "urlPath": "/short"},
                "response": {"websocket": {"closeAfterMs": 200, "echo": true}}
            })),
        )
        .await;
    assert_eq!(st, 201);
    let (mut ws, _) = tokio_tungstenite::connect_async(h.ws_url("/short"))
        .await
        .unwrap();
    let started = std::time::Instant::now();
    assert_eq!(next_text(&mut ws).await, None);
    assert!(started.elapsed() < Duration::from_secs(3));

    let requests: Value = h
        .admin(reqwest::Method::GET, "/requests?pathPrefix=/rooms", VIEWER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let statuses: Vec<u64> = requests["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["status"].as_u64().unwrap())
        .collect();
    assert!(statuses.contains(&101), "{statuses:?}");
    let metrics = h
        .http
        .get(format!("{}/metrics", h.admin))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("fastmock_ws_messages_total"));
    assert!(metrics.contains("fastmock_ws_connections"));
}

const ASYNC_DOC: &str = r##"{
  "asyncapi": "3.0.0",
  "info": {"title": "Street Lights", "version": "1.0"},
  "servers": {"socket": {"host": "ws.example.com", "protocol": "ws"}, "broker": {"host": "kafka.example.com", "protocol": "kafka"}},
  "channels": {
    "live": {"address": "/live/{city}", "servers": [{"$ref": "#/servers/socket"}],
      "messages": {"tick": {"payload": {"type": "object"}, "examples": [{"name": "one", "payload": {"city": "{{request.pathParams.city}}", "n": 1}}]}}},
    "measured": {"address": "lights.measured", "servers": [{"$ref": "#/servers/broker"}],
      "messages": {"m": {"payload": {"type": "object", "required": ["lumens"], "properties": {"lumens": {"type": "integer"}}}}}}
  },
  "operations": {
    "streamLive": {"action": "send", "channel": {"$ref": "#/channels/live"}},
    "publishMeasured": {"action": "send", "channel": {"$ref": "#/channels/measured"}}
  }
}"##;

#[tokio::test]
async fn asyncapi_import_creates_async_mocks_and_websocket_stubs() {
    let h = Harness::start(Config::for_tests(StorageKind::Memory, tokens())).await;
    let r = h
        .admin(reqwest::Method::POST, "/import/asyncapi", VIEWER)
        .json(&json!({"spec": ASYNC_DOC}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "viewers cannot import");
    let (st, v) = h
        .call(
            reqwest::Method::POST,
            "/import/asyncapi",
            Some(json!({"spec": ASYNC_DOC, "dryRun": true, "frequencyMs": 1000})),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["dryRun"], true);
    assert_eq!(v["tag"], "asyncapi-street-lights");
    assert_eq!(v["asyncMocks"].as_array().unwrap().len(), 1);
    assert_eq!(v["websocketStubs"].as_array().unwrap().len(), 1);
    let (_, list) = h.call(reqwest::Method::GET, "/async-mocks", None).await;
    assert!(
        list["asyncMocks"].as_array().unwrap().is_empty(),
        "dry run stores nothing"
    );

    let (st, v) = h
        .call(
            reqwest::Method::POST,
            "/import/asyncapi",
            Some(json!({"spec": ASYNC_DOC, "frequencyMs": 1000, "tag": "lights"})),
        )
        .await;
    assert_eq!(st, 201, "{v}");
    assert!(
        v["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("FASTMOCK_KAFKA_BOOTSTRAP")),
        "{v}"
    );
    let (st, again) = h
        .call(
            reqwest::Method::POST,
            "/import/asyncapi",
            Some(json!({"spec": ASYNC_DOC, "frequencyMs": 1000, "tag": "lights"})),
        )
        .await;
    assert_eq!(st, 201, "{again}");
    let (_, list) = h
        .call(reqwest::Method::GET, "/async-mocks?tag=lights", None)
        .await;
    let mocks = list["asyncMocks"].as_array().unwrap();
    assert_eq!(mocks.len(), 1, "re-import replaces by tag");
    assert_eq!(mocks[0]["channel"], "lights.measured");
    assert_eq!(mocks[0]["messages"][0]["payload"], json!({"lumens": 0}));
    let id = mocks[0]["id"].as_str().unwrap().to_string();
    let (_, stubs) = h.call(reqwest::Method::GET, "/stubs", None).await;
    let ws_stubs: Vec<&Value> = stubs
        .as_array()
        .or_else(|| stubs["stubs"].as_array())
        .unwrap()
        .iter()
        .filter(|s| s["tags"] == json!(["lights"]))
        .collect();
    assert_eq!(ws_stubs.len(), 1, "{stubs}");

    let (mut ws, _) = tokio_tungstenite::connect_async(h.ws_url("/live/kyiv"))
        .await
        .unwrap();
    let first = next_text(&mut ws).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&first).unwrap(),
        json!({"city": "kyiv", "n": 1})
    );

    let (st, one) = h
        .call(reqwest::Method::GET, &format!("/async-mocks/{id}"), None)
        .await;
    assert_eq!(st, 200);
    let mut changed = one.clone();
    changed["frequencyMs"] = json!(500);
    let (st, err) = h
        .call(
            reqwest::Method::PUT,
            &format!("/async-mocks/{id}"),
            Some(changed.clone()),
        )
        .await;
    assert_eq!(st, 400, "{err}");
    changed["frequencyMs"] = json!(2000);
    changed["enabled"] = json!(false);
    let (st, saved) = h
        .call(
            reqwest::Method::PUT,
            &format!("/async-mocks/{id}"),
            Some(changed),
        )
        .await;
    assert_eq!(st, 200, "{saved}");
    assert_eq!(saved["frequencyMs"], 2000);
    let (st, err) = h
        .call(
            reqwest::Method::POST,
            &format!("/async-mocks/{id}/trigger"),
            None,
        )
        .await;
    assert_eq!(st, 400, "no Kafka configured: {err}");
    let (st, _) = h
        .call(reqwest::Method::DELETE, &format!("/async-mocks/{id}"), None)
        .await;
    assert_eq!(st, 204);
    let (st, _) = h
        .call(reqwest::Method::GET, &format!("/async-mocks/{id}"), None)
        .await;
    assert_eq!(st, 404);
    let (st, err) = h
        .call(
            reqwest::Method::POST,
            "/import/asyncapi",
            Some(json!({"spec": {"openapi": "3.0.0"}})),
        )
        .await;
    assert_eq!(st, 400, "{err}");
}

type Seen = Arc<Mutex<Vec<(String, HeaderMap)>>>;

async fn target_server() -> (String, Seen) {
    let seen: Seen = Arc::default();
    async fn pet(State(s): State<Seen>, headers: HeaderMap) -> impl IntoResponse {
        s.lock().unwrap().push(("pet".into(), headers));
        axum::Json(json!({"id": "seven", "name": "rex"}))
    }
    async fn create(State(s): State<Seen>, headers: HeaderMap, body: String) -> impl IntoResponse {
        s.lock().unwrap().push((body, headers));
        (
            StatusCode::CREATED,
            axum::Json(json!({"id": 1, "name": "rex"})),
        )
    }
    async fn health() -> impl IntoResponse {
        (StatusCode::INTERNAL_SERVER_ERROR, "boom")
    }
    async fn text() -> impl IntoResponse {
        ([("content-type", "text/plain")], "hello")
    }
    let app = Router::new()
        .route("/api/pets/{id}", get(pet))
        .route("/api/pets", post(create))
        .route("/api/health", get(health))
        .route("/api/text", get(text))
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}/api"), seen)
}

const PETS: &str = r##"
openapi: 3.0.3
info: {title: Pets, version: "1"}
paths:
  /pets/{id}:
    get:
      operationId: getPet
      parameters:
        - {name: id, in: path, required: true, schema: {type: integer}, example: 7}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Pet"}
  /pets:
    post:
      operationId: createPet
      requestBody:
        content:
          application/json:
            schema: {$ref: "#/components/schemas/Pet"}
            examples:
              rex: {value: {id: 1, name: rex}}
      responses:
        "201":
          description: created
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Pet"}
  /health:
    get:
      operationId: health
      responses:
        "200": {description: ok}
  /text:
    get:
      operationId: text
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {type: object}
components:
  schemas:
    Pet:
      type: object
      required: [id, name]
      properties:
        id: {type: integer}
        name: {type: string}
"##;

#[tokio::test]
async fn contract_tests_report_schema_violations() {
    let dir = std::env::temp_dir().join(format!("fm-ct-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("pets-token"), "Bearer s3cr3t-value\n").unwrap();
    let mut cfg = Config::for_tests(StorageKind::Memory, tokens());
    cfg.secrets_dir = Some(dir.clone());
    cfg.contract_secrets = vec![fastmock_server::config::ContractSecret {
        name: "pets-token".into(),
        hosts: vec!["127.0.0.1".into()],
    }];
    let h = Harness::start(cfg).await;
    let (target, seen) = target_server().await;

    let (st, v) = h
        .call_as(
            ADMIN,
            reqwest::Method::POST,
            "/contract-tests",
            Some(
                json!({"spec": PETS, "targetUrl": target, "headers": {"x-run": "ci"},
                "secretHeaders": {"Authorization": "pets-token"}, "timeoutMs": 2000}),
            ),
        )
        .await;
    assert_eq!(st, 202, "{v}");
    assert_eq!(v["operations"], 4);
    let run = h.wait_run(v["id"].as_str().unwrap()).await;
    assert_eq!(run["status"], "failed", "{run}");
    assert_eq!(
        run["summary"],
        json!({"total": 4, "passed": 1, "failed": 3, "errors": 0})
    );
    let by = |op: &str| {
        run["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["operation"] == op)
            .unwrap()
            .clone()
    };
    let get_pet = by("getPet");
    assert_eq!(get_pet["verdict"], "failed");
    assert!(
        get_pet["request"]["url"]
            .as_str()
            .unwrap()
            .ends_with("/api/pets/7")
    );
    assert!(
        get_pet["errors"][0].as_str().unwrap().contains("/id"),
        "{get_pet}"
    );
    assert_eq!(get_pet["request"]["headers"]["authorization"], "***");
    assert_eq!(get_pet["request"]["headers"]["x-run"], "ci");
    assert_eq!(get_pet["response"]["status"], 200);
    assert_eq!(by("createPet")["verdict"], "passed");
    assert_eq!(by("createPet")["example"], "rex");
    assert!(by("health")["errors"][0].as_str().unwrap().contains("500"));
    assert!(
        by("text")["errors"][0]
            .as_str()
            .unwrap()
            .contains("content type")
    );
    let calls = seen.lock().unwrap().clone();
    assert!(
        calls
            .iter()
            .all(|(_, hm)| hm["authorization"] == "Bearer s3cr3t-value")
    );
    assert!(calls.iter().any(|(b, _)| b == r#"{"id":1,"name":"rex"}"#));
    assert!(!run.to_string().contains("s3cr3t"), "secrets never stored");

    h.app
        .stores
        .data
        .put(
            "specs",
            "pets:1",
            &json!({"name": "pets", "version": "1", "type": "openapi", "document": PETS}),
        )
        .await
        .unwrap();
    let (st, v) = h
        .call(
            reqwest::Method::POST,
            "/contract-tests",
            Some(json!({"specRef": "pets:1", "targetUrl": target, "operations": ["createPet"]})),
        )
        .await;
    assert_eq!(st, 202, "{v}");
    let second = h.wait_run(v["id"].as_str().unwrap()).await;
    assert_eq!(second["status"], "passed", "{second}");
    assert_eq!(second["specRef"], "pets:1");

    let (_, list) = h.call(reqwest::Method::GET, "/contract-tests", None).await;
    let runs = list["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["id"], second["id"], "newest first");
    assert!(runs[0]["results"].as_array().unwrap().is_empty());

    for (body, needle) in [
        (json!({"spec": PETS}), "targetUrl"),
        (
            json!({"spec": PETS, "specRef": "x", "targetUrl": target}),
            "exactly one",
        ),
        (
            json!({"spec": PETS, "targetUrl": "http://169.254.169.254/"}),
            "denied",
        ),
        (
            json!({"spec": PETS, "targetUrl": target, "timeoutMs": 0}),
            "timeoutMs",
        ),
        (
            json!({"spec": PETS, "targetUrl": target, "operations": ["nope"]}),
            "no operations",
        ),
    ] {
        let (st, v) = h
            .call(reqwest::Method::POST, "/contract-tests", Some(body))
            .await;
        assert_eq!(st, 400, "{v}");
        assert!(
            v["error"].as_str().unwrap().contains(needle),
            "{needle}: {v}"
        );
    }
    let (st, v) = h
        .call_as(
            ADMIN,
            reqwest::Method::POST,
            "/contract-tests",
            Some(json!({"spec": PETS, "targetUrl": target, "secretHeaders": {"x": "unknown"}})),
        )
        .await;
    assert_eq!(st, 400, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("not configured"),
        "{v}"
    );
    let (st, v) = h
        .call(
            reqwest::Method::POST,
            "/contract-tests",
            Some(json!({"spec": PETS, "targetUrl": target, "secretHeaders": {"Authorization": "pets-token"}})),
        )
        .await;
    assert_eq!(st, 403, "secretHeaders need the admin role: {v}");
    let (st, _) = h
        .call(
            reqwest::Method::POST,
            "/contract-tests",
            Some(json!({"specRef": "missing", "targetUrl": target})),
        )
        .await;
    assert_eq!(st, 404);
    let r = h
        .admin(reqwest::Method::POST, "/contract-tests", VIEWER)
        .json(&json!({"spec": PETS, "targetUrl": target}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let id = second["id"].as_str().unwrap();
    let (st, _) = h
        .call(
            reqwest::Method::DELETE,
            &format!("/contract-tests/{id}"),
            None,
        )
        .await;
    assert_eq!(st, 204);
    let (st, _) = h
        .call(reqwest::Method::GET, &format!("/contract-tests/{id}"), None)
        .await;
    assert_eq!(st, 404);
    let metrics = h
        .http
        .get(format!("{}/metrics", h.admin))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("fastmock_contract_tests_total"));
    assert!(metrics.contains("fastmock_contract_test_duration_seconds"));
    std::fs::remove_dir_all(dir).unwrap();
}

async fn shared_storage() -> Option<StorageKind> {
    let admin_url = std::env::var("FASTMOCK_TEST_DATABASE_URL").ok()?;
    let valkey_url = std::env::var("FASTMOCK_TEST_VALKEY_URL").ok()?;
    let db = format!("fm_async_{}", uuid::Uuid::new_v4().simple());
    let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut c)
        .await
        .unwrap();
    Some(StorageKind::Postgres {
        database_url: format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db),
        valkey_url,
        valkey_prefix: format!("async:{}:", uuid::Uuid::new_v4().simple()),
        max_connections: 5,
        options: Default::default(),
    })
}

async fn count_messages(
    client: &fastmock_kafka::KafkaPublisher,
    cursor: &mut fastmock_kafka::TopicCursor,
    wait: Duration,
) -> Vec<fastmock_kafka::ConsumedRecord> {
    client.consume(cursor, 1000, 1_000_000, wait).await.unwrap()
}

#[tokio::test]
async fn async_mocks_publish_to_kafka_from_one_leader() {
    let Some(bootstrap) = std::env::var("FASTMOCK_TEST_KAFKA").ok() else {
        eprintln!("FASTMOCK_TEST_KAFKA not set: skipped");
        return;
    };
    let Some(storage) = shared_storage().await else {
        eprintln!("FASTMOCK_TEST_DATABASE_URL / FASTMOCK_TEST_VALKEY_URL not set: skipped");
        return;
    };
    let topic = format!("fm-async-{}", uuid::Uuid::new_v4().simple());
    let mut cfg = Config::for_tests(storage, tokens());
    cfg.kafka_bootstrap = Some(vec![bootstrap.clone()]);
    cfg.kafka_allowed_topics = Some(vec!["fm-async-*".into()]);
    cfg.kafka_contract_topics = vec!["fm-async-*".into()];
    cfg.kafka_schema_registry = std::env::var("FASTMOCK_TEST_SCHEMA_REGISTRY").ok();
    let a = Harness::start(cfg.clone()).await;
    let b = Harness::start(cfg).await;
    let client = fastmock_kafka::KafkaPublisher::connect(&fastmock_kafka::KafkaSettings {
        bootstrap: vec![bootstrap],
        client_id: "fastmock-test".into(),
    })
    .await
    .unwrap();
    client
        .ensure_topics(std::slice::from_ref(&topic), 3, 1)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let id = uuid::Uuid::now_v7();
    let (st, v) = a
        .call(
            reqwest::Method::PUT,
            &format!("/async-mocks/{id}"),
            Some(json!({"name": "measured", "channel": topic, "frequencyMs": 1000,
                "contentType": "application/json",
                "messages": [
                    {"name": "good", "key": "k-{{sequence}}", "headers": {"x-seq": "{{sequence}}"}, "payload": {"lumens": "{{json sequence}}", "id": "{{uuid}}"}},
                    {"name": "bad", "payload": {"lumens": "not-a-number"}}
                ]})),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    let (st, v) = a
        .call(
            reqwest::Method::PUT,
            &format!("/async-mocks/{}", uuid::Uuid::now_v7()),
            Some(
                json!({"name": "blocked", "channel": "other-topic", "messages": [{"payload": 1}]}),
            ),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    let blocked = v["id"].as_str().unwrap().to_string();
    let (st, v) = a
        .call(
            reqwest::Method::POST,
            &format!("/async-mocks/{blocked}/trigger"),
            None,
        )
        .await;
    assert_eq!(st, 400, "allowlist enforced: {v}");

    let mut cursor = client
        .cursor(&topic, fastmock_kafka::StartFrom::Latest)
        .await
        .unwrap();
    let opts = fastmock_server::asyncmock::PublisherOptions {
        lease_ttl: Duration::from_secs(3),
        renew_every: Duration::from_millis(500),
        tick: Duration::from_millis(50),
        reload_every: Duration::from_millis(300),
    };
    let (stop_a, rx_a) = watch::channel(false);
    let (_stop_b, rx_b) = watch::channel(false);
    let pa = tokio::spawn(fastmock_server::asyncmock::run_with(
        a.app.clone(),
        rx_a,
        opts.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    tokio::spawn(fastmock_server::asyncmock::run_with(
        b.app.clone(),
        rx_b,
        opts,
    ));
    let records = count_messages(&client, &mut cursor, Duration::from_millis(4200)).await;
    assert!(
        (3..=6).contains(&records.len()),
        "one leader publishes about one message per second, got {}",
        records.len()
    );
    let payloads: Vec<Value> = records
        .iter()
        .map(|r| serde_json::from_slice(r.value.as_ref().unwrap()).unwrap())
        .collect();
    let mut seqs: Vec<i64> = payloads
        .iter()
        .filter_map(|p| p["lumens"].as_i64())
        .collect();
    seqs.sort();
    assert!(!seqs.is_empty(), "{payloads:?}");
    assert!(
        seqs.iter().all(|s| s % 2 == 1),
        "round robin: odd sequences are the first example"
    );
    assert!(payloads.iter().any(|p| p["lumens"] == "not-a-number"));
    let good = records
        .iter()
        .find(|r| r.headers.contains_key("x-seq"))
        .unwrap();
    assert_eq!(good.headers["content-type"], b"application/json".to_vec());
    assert!(String::from_utf8_lossy(good.key.as_ref().unwrap()).starts_with("k-"));

    stop_a.send(true).unwrap();
    pa.await.unwrap();
    let after = count_messages(&client, &mut cursor, Duration::from_millis(3500)).await;
    assert!(
        !after.is_empty() && after.len() <= 5,
        "the other replica takes over: {}",
        after.len()
    );

    let (st, v) = b
        .call(
            reqwest::Method::POST,
            &format!("/async-mocks/{id}/trigger"),
            None,
        )
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["topic"], topic);

    let spec = json!({"asyncapi": "2.6.0", "info": {"title": "lights", "version": "1"},
        "channels": {topic.clone(): {"bindings": {"kafka": {}}, "subscribe": {"operationId": "measured",
            "message": {"payload": {"type": "object", "required": ["lumens"], "properties": {"lumens": {"type": "integer"}}}}}}}});
    let (st, v) = b
        .call(
            reqwest::Method::POST,
            "/contract-tests",
            Some(json!({"spec": spec, "messageCount": 2, "waitMs": 5000})),
        )
        .await;
    assert_eq!(st, 202, "{v}");
    let run = b.wait_run(v["id"].as_str().unwrap()).await;
    assert_eq!(run["kind"], "asyncapi");
    assert_eq!(
        run["status"], "failed",
        "one example violates the schema: {run}"
    );
    let msgs = run["results"][0]["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "{run}");
    assert!(
        msgs.iter()
            .any(|m| m["errors"].as_array().unwrap().is_empty())
    );
    assert!(
        msgs.iter()
            .any(|m| !m["errors"].as_array().unwrap().is_empty())
    );

    if std::env::var("FASTMOCK_TEST_SCHEMA_REGISTRY").is_ok() {
        let avro_topic = format!("{topic}-avro");
        client
            .ensure_topics(std::slice::from_ref(&avro_topic), 1, 1)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let schema = json!({"type": "record", "name": "Measured", "namespace": "lights", "fields": [
            {"name": "lumens", "type": "long"}, {"name": "note", "type": ["null", "string"], "default": null}]});
        let avro_id = uuid::Uuid::now_v7();
        let (st, v) = b
            .call(
                reqwest::Method::PUT,
                &format!("/async-mocks/{avro_id}"),
                Some(json!({"name": "avro", "channel": avro_topic, "enabled": false, "avroSchema": schema,
                    "messages": [{"payload": {"lumens": "{{json sequence}}", "note": "n-{{sequence}}"}}]})),
            )
            .await;
        assert_eq!(st, 200, "{v}");
        let (st, v) = b
            .call(
                reqwest::Method::POST,
                &format!("/async-mocks/{avro_id}/trigger"),
                None,
            )
            .await;
        assert_eq!(st, 200, "{v}");
        let mut avro_cursor = client
            .cursor(&avro_topic, fastmock_kafka::StartFrom::Earliest)
            .await
            .unwrap();
        let got = count_messages(&client, &mut avro_cursor, Duration::from_secs(3)).await;
        assert_eq!(got.len(), 1);
        let parsed = apache_avro::Schema::parse(&schema).unwrap();
        let decoded = fastmock_kafka::decode_avro(&parsed, got[0].value.as_ref().unwrap()).unwrap();
        assert_eq!(decoded, json!({"lumens": 1, "note": "n-1"}));
        let spec = json!({"asyncapi": "2.6.0", "info": {"title": "lights", "version": "1"},
            "channels": {avro_topic.clone(): {"bindings": {"kafka": {}}, "subscribe": {"operationId": "avro",
                "message": {"schemaFormat": "application/vnd.apache.avro;version=1.9.0", "payload": schema}}}}});
        let (st, v) = b
            .call(
                reqwest::Method::POST,
                "/contract-tests",
                Some(json!({"spec": spec, "fromBeginning": true, "waitMs": 3000})),
            )
            .await;
        assert_eq!(st, 202, "{v}");
        let run = b.wait_run(v["id"].as_str().unwrap()).await;
        assert_eq!(run["status"], "passed", "{run}");
    }
    let _ = a.shutdown.send(true);
    let _ = b.shutdown.send(true);
}
