//! MQTT and AMQP publishing and message capture.
//!
//! The capture API tests always run (messages are written to the store directly). The broker tests run
//! when `FASTMOCK_TEST_MQTT` (an MQTT 5 broker URL, for example Mosquitto), `FASTMOCK_TEST_AMQP` (RabbitMQ
//! or LavinMQ, user `guest` unless `FASTMOCK_TEST_AMQP_USER`/`_PASSWORD`) or `FASTMOCK_TEST_KAFKA` are
//! set, and are skipped otherwise. With `FASTMOCK_TEST_DATABASE_URL` and `FASTMOCK_TEST_VALKEY_URL` the
//! capture API and the two-replica test also run on PostgreSQL.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use chrono::Utc;
use fastmock_brokers::amqp::{AmqpCapture, AmqpMessage, AmqpPublisher, AmqpSettings};
use fastmock_brokers::mqtt::{MqttConnection, MqttPublish, MqttSettings};
use fastmock_brokers::{CaptureLimits, Credentials, LinkStatus, capture_channel};
use fastmock_core::MessageProtocol;
use fastmock_core::capture::{CapturedMessage, InsertLimits};
use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use fastmock_server::state::AppState;
use reqwest::Method;
use serde_json::{Value, json};
use sqlx::Connection;
use tokio::sync::watch;
use uuid::Uuid;

const ADMIN: &str = "fm_admin_token_for_broker_tests_0123456789";
const EDITOR: &str = "fm_editor_token_for_broker_tests_0123456789";
const VIEWER: &str = "fm_viewer_token_for_broker_tests_0123456789";
const BEDIT: &str = "fm_bedit_token_for_broker_tests_0123456789";
const BVIEW: &str = "fm_bview_token_for_broker_tests_0123456789";
const BCAP: &str = "fm_bcap_token_for_broker_tests_0123456789";

const ACCESS: &str = r#"
roles:
  - name: capture-writer
    permissions: [messages:read, messages:write]
spaces:
  - name: billing
  - name: crm
bindings:
  - subject: token:bcap
    role: capture-writer
    spaces: [billing]
  - subject: token:bedit
    role: editor
    spaces: [billing]
  - subject: token:bview
    role: viewer
    spaces: [billing]
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
        format!("ci:editor:{}", hash_token(EDITOR)),
        format!("dash:viewer:{}", hash_token(VIEWER)),
        format!("bedit::{}", hash_token(BEDIT)),
        format!("bview::{}", hash_token(BVIEW)),
        format!("bcap::{}", hash_token(BCAP)),
    ]
}

async fn shared_storage() -> Option<StorageKind> {
    let admin_url = std::env::var("FASTMOCK_TEST_DATABASE_URL").ok()?;
    let valkey_url = std::env::var("FASTMOCK_TEST_VALKEY_URL").ok()?;
    let db = format!("fm_brokers_{}", Uuid::new_v4().simple());
    let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut c)
        .await
        .unwrap();
    Some(StorageKind::Postgres {
        database_url: format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db),
        valkey_url,
        valkey_prefix: format!("brokers:{}:", Uuid::new_v4().simple()),
        max_connections: 5,
        options: Default::default(),
    })
}

async fn storages() -> Vec<StorageKind> {
    let mut v = vec![StorageKind::Memory];
    match shared_storage().await {
        Some(s) => v.push(s),
        None => eprintln!(
            "FASTMOCK_TEST_DATABASE_URL/FASTMOCK_TEST_VALKEY_URL not set: PostgreSQL part skipped"
        ),
    }
    v
}

fn fast() -> fastmock_server::capture::runtime::RuntimeOptions {
    fastmock_server::capture::runtime::RuntimeOptions {
        lease_ttl: Duration::from_secs(3),
        renew_every: Duration::from_millis(300),
        tick: Duration::from_millis(50),
        reload_every: Duration::from_millis(200),
    }
}

struct H {
    app: AppState,
    admin: String,
    data: String,
    http: reqwest::Client,
    _stop: watch::Sender<bool>,
}

impl H {
    async fn start(mut cfg: Config, run_capture: bool) -> H {
        cfg.plugins = Some(vec![]);
        let dir = std::env::temp_dir().join(format!("fm-brokers-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("access.yaml");
        std::fs::write(&f, ACCESS).unwrap();
        cfg.access_file = Some(f);
        let (app, data, admin, _policy) = fastmock_server::build(cfg, prom()).await.unwrap();
        let dl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (da, aa) = (dl.local_addr().unwrap(), al.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(dl, data).await.unwrap() });
        tokio::spawn(async move { axum::serve(al, admin).await.unwrap() });
        app.ready.store(true, Ordering::SeqCst);
        let (stop, rx) = watch::channel(false);
        if run_capture {
            tokio::spawn(fastmock_server::capture::runtime::run_with(
                app.clone(),
                rx,
                fast(),
            ));
        }
        H {
            app,
            admin: format!("http://{aa}/__admin"),
            data: format!("http://{da}"),
            http: reqwest::Client::new(),
            _stop: stop,
        }
    }

    async fn call(&self, m: Method, path: &str, token: &str, body: Option<Value>) -> (u16, Value) {
        let mut rb = self
            .http
            .request(m, format!("{}{path}", self.admin))
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

    async fn put_source(&self, query: &str, body: Value) -> Value {
        let id = Uuid::now_v7();
        self.ok(
            Method::PUT,
            &format!("/capture/sources/{id}{query}"),
            EDITOR,
            Some(body),
        )
        .await
    }

    async fn wait_running(&self, id: &str) {
        for _ in 0..200 {
            let v = self
                .ok(Method::GET, &format!("/capture/sources/{id}"), VIEWER, None)
                .await;
            if v["status"]["state"] == "running" {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let v = self
            .ok(Method::GET, &format!("/capture/sources/{id}"), VIEWER, None)
            .await;
        panic!("source {id} is not running: {v}");
    }

    async fn wait_for(&self, body: Value) -> Value {
        let (st, v) = self
            .call(Method::POST, "/capture/messages/wait", VIEWER, Some(body))
            .await;
        assert_eq!(st, 200, "{v}");
        v
    }

    async fn metrics(&self) -> String {
        self.http
            .get(self.admin.replace("/__admin", "/metrics"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    }
}

fn capture_cfg(storage: StorageKind) -> Config {
    let mut cfg = Config::for_tests(storage, tokens());
    cfg.capture.enabled = true;
    cfg.capture.kafka_topics = Some(vec!["*".into()]);
    cfg.capture.mqtt_topics = Some(vec!["*".into()]);
    cfg.capture.amqp_exchanges = Some(vec!["*".into()]);
    cfg
}

fn stored(
    source: Uuid,
    name: &str,
    space: Option<&str>,
    topic: &str,
    body: &[u8],
) -> CapturedMessage {
    CapturedMessage {
        id: Uuid::now_v7(),
        space: space.map(String::from),
        source_id: source,
        source: name.into(),
        protocol: MessageProtocol::Kafka,
        topic: topic.into(),
        exchange: None,
        key: Some("A-1".into()),
        headers: BTreeMap::from([("event-type".to_string(), "OrderShipped".to_string())]),
        properties: serde_json::Map::from_iter([("partition".to_string(), json!(0))]),
        content_type: None,
        size: body.len() as u64,
        truncated: false,
        preview: std::str::from_utf8(body).ok().map(String::from),
        received_at: Utc::now(),
        body: Bytes::copy_from_slice(body),
    }
}

async fn insert(h: &H, msgs: &[CapturedMessage]) {
    let svc = h.app.capture.clone().unwrap();
    svc.store
        .insert(msgs, &InsertLimits::keep(1000))
        .await
        .unwrap();
    svc.notify();
}

#[tokio::test]
async fn capture_is_off_by_default() {
    let h = H::start(Config::for_tests(StorageKind::Memory, tokens()), false).await;
    let (st, v) = h.call(Method::GET, "/capture/sources", VIEWER, None).await;
    assert_eq!(st, 404);
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("FASTMOCK_CAPTURE_ENABLED")
    );
    let v = h.ok(Method::GET, "/version", VIEWER, None).await;
    let features: Vec<&str> = v["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert!(features.contains(&"mqtt") && features.contains(&"amqp"));
    assert!(!features.contains(&"capture"));
}

#[tokio::test]
async fn capture_api_finds_waits_verifies_and_scopes_messages() {
    for storage in storages().await {
        let mut cfg = capture_cfg(storage);
        cfg.kafka_bootstrap = Some(vec!["127.0.0.1:1".into()]);
        cfg.capture.kafka_topics = Some(vec!["orders.*".into()]);
        cfg.capture.sources = fastmock_server::broker_settings::capture_settings(&|n| match n {
            "CAPTURE_ENABLED" => Some("true".into()),
            "CAPTURE_SOURCES" => Some(
                r#"[{"name": "static-orders", "protocol": "kafka", "topics": ["orders.static"]}]"#
                    .into(),
            ),
            _ => None,
        })
        .unwrap()
        .sources;
        let h = H::start(cfg, false).await;
        let v = h.ok(Method::GET, "/version", VIEWER, None).await;
        assert!(
            v["features"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f == "capture")
        );

        // Sources: validation, allow-list, permissions, spaces, static sources.
        let src = h
            .put_source(
                "",
                json!({"name": "orders", "protocol": "kafka", "topics": ["orders.events"]}),
            )
            .await;
        let src_id: Uuid = src["id"].as_str().unwrap().parse().unwrap();
        assert_eq!(src["status"]["state"], "stopped");
        let (st, v) = h
            .call(
                Method::PUT,
                &format!("/capture/sources/{}", Uuid::now_v7()),
                EDITOR,
                Some(json!({"name": "audit", "protocol": "kafka", "topics": ["audit"]})),
            )
            .await;
        assert_eq!(st, 400);
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .contains("FASTMOCK_CAPTURE_KAFKA_TOPICS"),
            "{v}"
        );
        let (st, v) = h
            .call(
                Method::PUT,
                &format!("/capture/sources/{}", Uuid::now_v7()),
                EDITOR,
                Some(json!({"name": "devices", "protocol": "mqtt", "topics": ["devices/#"]})),
            )
            .await;
        assert_eq!(st, 400);
        assert!(
            v["error"].as_str().unwrap().contains("FASTMOCK_MQTT_URL"),
            "{v}"
        );
        let (st, _) = h
            .call(
                Method::PUT,
                &format!("/capture/sources/{}", Uuid::now_v7()),
                EDITOR,
                Some(json!({"name": "orders", "protocol": "kafka", "topics": ["orders.x"]})),
            )
            .await;
        assert_eq!(st, 400, "names are unique per space");
        let (st, _) = h
            .call(
                Method::PUT,
                &format!("/capture/sources/{}", Uuid::now_v7()),
                VIEWER,
                Some(json!({"name": "v", "protocol": "kafka", "topics": ["orders.v"]})),
            )
            .await;
        assert_eq!(st, 403);
        let billing = h
            .call(
                Method::PUT,
                &format!("/capture/sources/{}", Uuid::now_v7()),
                BEDIT,
                Some(json!({"name": "orders", "protocol": "kafka", "topics": ["orders.billing"]})),
            )
            .await;
        assert_eq!(billing.0, 200, "{}", billing.1);
        assert_eq!(billing.1["space"], "billing");
        let billing_id: Uuid = billing.1["id"].as_str().unwrap().parse().unwrap();
        let list = h.ok(Method::GET, "/capture/sources", VIEWER, None).await;
        assert_eq!(list["sources"].as_array().unwrap().len(), 3);
        let static_src = list["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "static-orders")
            .unwrap()
            .clone();
        assert_eq!(static_src["fromConfig"], true);
        let (st, _) = h
            .call(
                Method::DELETE,
                &format!("/capture/sources/{}", static_src["id"].as_str().unwrap()),
                EDITOR,
                None,
            )
            .await;
        assert_eq!(st, 409);
        let mine = h.ok(Method::GET, "/capture/sources", BVIEW, None).await;
        assert_eq!(mine["sources"].as_array().unwrap().len(), 1);
        assert_eq!(mine["sources"][0]["space"], "billing");

        // Messages: list, find, get decoded, body.
        let json_msg = stored(
            src_id,
            "orders",
            None,
            "orders.events",
            br#"{"orderId":"A-1","status":"SHIPPED"}"#,
        );
        let mut text_msg = stored(src_id, "orders", None, "orders.events", b"plain text");
        text_msg.headers.insert("event-type".into(), "Other".into());
        let mut bin_msg = stored(src_id, "orders", None, "orders.other", &[0xff, 0x00, 0x01]);
        bin_msg.preview = None;
        let billing_msg = stored(
            billing_id,
            "orders",
            Some("billing"),
            "orders.billing",
            br#"{"orderId":"B-1"}"#,
        );
        insert(&h, &[json_msg.clone(), text_msg.clone(), bin_msg.clone()]).await;
        insert(&h, std::slice::from_ref(&billing_msg)).await;

        let all = h.ok(Method::GET, "/capture/messages", VIEWER, None).await;
        assert_eq!(all["messages"].as_array().unwrap().len(), 4);
        assert_eq!(
            all["messages"][0]["id"],
            billing_msg.id.to_string(),
            "newest first"
        );
        assert!(all["messages"][0].get("body").is_none());
        let page = h
            .ok(
                Method::GET,
                "/capture/messages?limit=2&order=oldest",
                VIEWER,
                None,
            )
            .await;
        assert_eq!(page["messages"][0]["id"], json_msg.id.to_string());
        let next = page["nextCursor"].as_str().unwrap();
        let page2 = h
            .ok(
                Method::GET,
                &format!("/capture/messages?limit=2&order=oldest&cursor={next}"),
                VIEWER,
                None,
            )
            .await;
        assert_eq!(page2["messages"].as_array().unwrap().len(), 2);
        assert!(page2.get("nextCursor").is_none());
        let by_topic = h
            .ok(
                Method::GET,
                "/capture/messages?topic=orders.other",
                VIEWER,
                None,
            )
            .await;
        assert_eq!(by_topic["messages"].as_array().unwrap().len(), 1);
        let by_source = h
            .ok(Method::GET, "/capture/messages?source=orders", VIEWER, None)
            .await;
        assert_eq!(
            by_source["messages"].as_array().unwrap().len(),
            4,
            "a name selects the sources of every readable space"
        );
        let billing_only = h.ok(Method::GET, "/capture/messages", BVIEW, None).await;
        assert_eq!(billing_only["messages"].as_array().unwrap().len(), 1);
        let (st, _) = h
            .call(
                Method::GET,
                &format!("/capture/messages/{}", json_msg.id),
                BVIEW,
                None,
            )
            .await;
        assert_eq!(st, 404, "messages of other areas are invisible");

        let found = h
            .ok(
                Method::POST,
                "/capture/messages/find",
                VIEWER,
                Some(json!({"match": {
                    "topic": {"equalTo": "orders.events"},
                    "headers": {"Event-Type": {"equalTo": "OrderShipped"}},
                    "key": {"equalTo": "A-1"},
                    "body": [{"jsonPath": "$.status", "value": {"equalTo": "SHIPPED"}}]
                }})),
            )
            .await;
        assert_eq!(found["count"], 1, "{found}");
        assert_eq!(found["messages"][0]["id"], json_msg.id.to_string());
        let (st, _) = h
            .call(
                Method::POST,
                "/capture/messages/find",
                VIEWER,
                Some(json!({"match": {"topic": {"matches": "("}}})),
            )
            .await;
        assert_eq!(st, 400);

        let d = h
            .ok(
                Method::GET,
                &format!("/capture/messages/{}", json_msg.id),
                VIEWER,
                None,
            )
            .await;
        assert_eq!(d["body"]["encoding"], "json");
        assert_eq!(d["body"]["value"]["orderId"], "A-1");
        assert_eq!(d["properties"]["partition"], 0);
        let d = h
            .ok(
                Method::GET,
                &format!("/capture/messages/{}", text_msg.id),
                VIEWER,
                None,
            )
            .await;
        assert_eq!(
            d["body"],
            json!({"encoding": "text", "value": "plain text"})
        );
        let d = h
            .ok(
                Method::GET,
                &format!("/capture/messages/{}", bin_msg.id),
                VIEWER,
                None,
            )
            .await;
        assert_eq!(d["body"]["encoding"], "base64");
        assert_eq!(d["body"]["value"], "/wAB");
        let raw = h
            .http
            .get(format!("{}/capture/messages/{}/body", h.admin, bin_msg.id))
            .bearer_auth(VIEWER)
            .send()
            .await
            .unwrap();
        assert_eq!(raw.headers()["content-type"], "application/octet-stream");
        assert_eq!(raw.bytes().await.unwrap().as_ref(), &[0xff, 0x00, 0x01]);

        // Verify with near misses.
        let v = h
            .ok(
                Method::POST,
                "/capture/verify",
                VIEWER,
                Some(json!({"match": {"topic": {"equalTo": "orders.events"}}, "count": 2})),
            )
            .await;
        assert_eq!(v["verified"], true);
        let (st, v) = h
            .call(
                Method::POST,
                "/capture/verify",
                VIEWER,
                Some(json!({"match": {
                    "topic": {"equalTo": "orders.events"},
                    "body": [{"jsonPath": "$.status", "value": {"equalTo": "CANCELLED"}}]
                }})),
            )
            .await;
        assert_eq!(st, 422, "{v}");
        assert_eq!(v["count"], 0);
        assert_eq!(v["expected"], "at least 1");
        assert_eq!(v["nearMisses"][0]["mismatches"], json!(["body[0]"]));
        assert_eq!(v["nearMisses"][0]["score"], 0.5);
        let (st, _) = h
            .call(
                Method::POST,
                "/capture/verify",
                VIEWER,
                Some(json!({"count": 1, "atLeast": 1})),
            )
            .await;
        assert_eq!(st, 400);

        // Wait: existing messages need includeExisting; new ones are found as they arrive.
        let (st, v) = h
            .call(
                Method::POST,
                "/capture/messages/wait",
                VIEWER,
                Some(json!({"match": {"topic": {"equalTo": "orders.events"}}, "timeoutMs": 300})),
            )
            .await;
        assert_eq!(st, 408, "{v}");
        assert_eq!(v["expected"], 1);
        let v = h
            .wait_for(json!({"match": {"topic": {"equalTo": "orders.events"}}, "includeExisting": true, "count": 2, "timeoutMs": 1000}))
            .await;
        assert_eq!(v["count"], 2);
        assert_eq!(
            v["messages"][0]["id"],
            json_msg.id.to_string(),
            "oldest first"
        );
        let h2 = h.app.clone();
        let late = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let m = stored(
                src_id,
                "orders",
                None,
                "orders.events",
                br#"{"orderId":"A-2"}"#,
            );
            let svc = h2.capture.clone().unwrap();
            svc.store
                .insert(std::slice::from_ref(&m), &InsertLimits::keep(1000))
                .await
                .unwrap();
            svc.notify();
            m
        });
        let started = std::time::Instant::now();
        let v = h
            .wait_for(json!({"match": {"body": [{"jsonPath": "$.orderId", "value": {"equalTo": "A-2"}}]}, "timeoutMs": 5000}))
            .await;
        let late = late.await.unwrap();
        assert_eq!(v["messages"][0]["id"], late.id.to_string());
        assert!(started.elapsed() < Duration::from_secs(3));

        // Deletes and audit.
        let (st, _) = h
            .call(
                Method::DELETE,
                &format!("/capture/messages/{}", text_msg.id),
                VIEWER,
                None,
            )
            .await;
        assert_eq!(st, 403);
        let (st, _) = h
            .call(
                Method::DELETE,
                &format!("/capture/messages/{}", text_msg.id),
                EDITOR,
                None,
            )
            .await;
        assert_eq!(st, 204);
        let v = h
            .ok(
                Method::DELETE,
                "/capture/messages?source=orders",
                BEDIT,
                None,
            )
            .await;
        assert_eq!(v["deleted"], 1, "only the billing messages of the caller");
        let v = h
            .ok(
                Method::DELETE,
                &format!("/capture/messages?source={src_id}"),
                EDITOR,
                None,
            )
            .await;
        assert_eq!(v["deleted"], 3);
        let (st, _) = h
            .call(
                Method::DELETE,
                &format!("/capture/sources/{src_id}?purge=true"),
                EDITOR,
                None,
            )
            .await;
        assert_eq!(st, 204);
        let audit = h.ok(Method::GET, "/audit?limit=50", ADMIN, None).await;
        let actions: Vec<String> = audit["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["action"].as_str().unwrap().to_string())
            .collect();
        for a in [
            "capture.source.put",
            "capture.source.delete",
            "capture.messages.delete",
            "capture.message.delete",
        ] {
            assert!(actions.iter().any(|x| x == a), "{a} in {actions:?}");
        }
        assert!(h.metrics().await.contains("fastmock_capture_waits_total"));
    }
}

#[tokio::test]
async fn per_source_limits_keep_the_newest_messages() {
    let mut cfg = capture_cfg(StorageKind::Memory);
    cfg.kafka_bootstrap = Some(vec!["127.0.0.1:1".into()]);
    cfg.capture.max_messages = 3;
    let h = H::start(cfg, false).await;
    let (st, v) = h
        .call(
            Method::PUT,
            &format!("/capture/sources/{}", Uuid::now_v7()),
            EDITOR,
            Some(json!({"name": "o", "protocol": "kafka", "topics": ["t"], "maxMessages": 4})),
        )
        .await;
    assert_eq!(st, 400, "{v}");
    let src = h
        .put_source(
            "",
            json!({"name": "o", "protocol": "kafka", "topics": ["t"], "maxMessages": 2}),
        )
        .await;
    let id: Uuid = src["id"].as_str().unwrap().parse().unwrap();
    let svc = h.app.capture.clone().unwrap();
    let msgs: Vec<CapturedMessage> = (0..5)
        .map(|i| stored(id, "o", None, "t", format!("{i}").as_bytes()))
        .collect();
    svc.store
        .insert(&msgs, &InsertLimits::keep(2))
        .await
        .unwrap();
    let v = h.ok(Method::GET, "/capture/sources", VIEWER, None).await;
    assert_eq!(v["sources"][0]["status"]["stored"], 2);
    assert_eq!(v["maxMessages"], 3);
}

/// A TCP proxy whose connections can be cut, to simulate a broker restart.
struct Proxy {
    addr: std::net::SocketAddr,
    cut: watch::Sender<u64>,
    open: Arc<AtomicBool>,
}

impl Proxy {
    async fn start(upstream: String) -> Proxy {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let (cut, cut_rx) = watch::channel(0u64);
        let open = Arc::new(AtomicBool::new(true));
        let o = open.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut inbound, _)) = l.accept().await else {
                    return;
                };
                if !o.load(Ordering::SeqCst) {
                    continue;
                }
                let up = upstream.clone();
                let mut rx = cut_rx.clone();
                rx.mark_unchanged();
                tokio::spawn(async move {
                    let Ok(mut outbound) = tokio::net::TcpStream::connect(up).await else {
                        return;
                    };
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound) => {}
                        _ = rx.changed() => {}
                    }
                });
            }
        });
        Proxy { addr, cut, open }
    }

    fn restart(&self, down: Duration) {
        self.open.store(false, Ordering::SeqCst);
        self.cut.send_modify(|n| *n += 1);
        let o = self.open.clone();
        tokio::spawn(async move {
            tokio::time::sleep(down).await;
            o.store(true, Ordering::SeqCst);
        });
    }
}

fn host_port(url: &str) -> String {
    let u = url::Url::parse(url).unwrap();
    format!(
        "{}:{}",
        u.host_str().unwrap(),
        u.port_or_known_default().unwrap()
    )
}

#[tokio::test]
async fn mqtt_stubs_async_mocks_and_capture() {
    let Ok(url) = std::env::var("FASTMOCK_TEST_MQTT") else {
        eprintln!("FASTMOCK_TEST_MQTT not set: skipped");
        return;
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let proxy = Proxy::start(host_port(&url)).await;
    let base = format!("fm-it/{}", Uuid::new_v4().simple());
    let mut cfg = capture_cfg(StorageKind::Memory);
    cfg.mqtt = Some(MqttSettings::new(&format!("mqtt://{}", proxy.addr)).unwrap());
    cfg.mqtt_allowed_topics = Some(vec![format!("{base}/*")]);
    cfg.capture.mqtt_topics = Some(vec![format!("{base}/*")]);
    let h = H::start(cfg, true).await;

    // An independent subscriber checks what reaches the broker.
    let direct = MqttSettings::new(&url).unwrap();
    let (tx, mut rx) = capture_channel(
        "mqtt",
        CaptureLimits {
            max_messages: 32,
            ..Default::default()
        },
    );
    let sub = MqttConnection::start(
        &direct,
        direct.client_id("test"),
        vec![(format!("{base}/#"), 1)],
        Some(tx),
    )
    .unwrap();
    for _ in 0..100 {
        if sub.status().connected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let src = h
        .put_source("", json!({"name": "devices", "protocol": "mqtt", "topics": [format!("{base}/+/state")], "qos": 1}))
        .await;
    let src_id = src["id"].as_str().unwrap().to_string();
    h.wait_running(&src_id).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (st, v) = h
        .call(Method::POST, "/stubs", EDITOR, Some(json!({
            "request": {"method": "POST", "urlPathTemplate": "/devices/{id}/state"},
            "response": {"status": 202},
            "messages": [
                {"protocol": "mqtt", "topic": format!("{base}/{{{{request.pathParams.id}}}}/state"), "qos": 1,
                 "headers": {"source": "fastmock"}, "contentType": "application/json",
                 "correlationData": "{{request.headers.x-request-id}}", "responseTopic": format!("{base}/replies"),
                 "jsonValue": {"device": "{{request.pathParams.id}}", "on": true}, "delayMs": 300},
                {"protocol": "mqtt", "topic": "outside/allowlist", "value": "x"}
            ]
        })))
        .await;
    assert_eq!(st, 201, "{v}");
    let started = std::time::Instant::now();
    let r = h
        .http
        .post(format!("{}/devices/d1/state", h.data))
        .header("x-request-id", "req-7")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    let v = h
        .wait_for(json!({"match": {"source": {"equalTo": "devices"}, "properties": {"correlationData": {"equalTo": "req-7"}}}, "lookbackMs": 5000, "timeoutMs": 10000}))
        .await;
    assert!(
        started.elapsed() >= Duration::from_millis(300),
        "delayMs is honoured"
    );
    let m = &v["messages"][0];
    assert_eq!(m["topic"], format!("{base}/d1/state"));
    assert_eq!(m["protocol"], "mqtt");
    assert_eq!(m["headers"]["source"], "fastmock");
    assert_eq!(m["contentType"], "application/json");
    assert_eq!(m["properties"]["qos"], 1);
    assert_eq!(m["properties"]["responseTopic"], format!("{base}/replies"));
    let d = h
        .ok(
            Method::GET,
            &format!("/capture/messages/{}", m["id"].as_str().unwrap()),
            VIEWER,
            None,
        )
        .await;
    assert_eq!(d["body"]["value"], json!({"device": "d1", "on": true}));
    let direct_msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(direct_msg.payload, br#"{"device":"d1","on":true}"#);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let metrics = h.metrics().await;
    assert!(
        metrics
            .lines()
            .any(|l| l.starts_with("fastmock_messages_total")
                && l.contains("protocol=\"mqtt\"")
                && l.contains("result=\"failed\"")),
        "the allow-list refused the second message"
    );

    // Async mock, triggered.
    let mock = Uuid::now_v7();
    let (st, v) = h
        .call(Method::PUT, &format!("/async-mocks/{mock}"), EDITOR, Some(json!({
            "name": "lights", "protocol": "mqtt", "channel": format!("{base}/lamp/state"), "frequencyMs": 60000,
            "enabled": false, "contentType": "application/json", "mqtt": {"qos": 2, "retain": false},
            "messages": [{"headers": {"seq": "{{sequence}}"}, "payload": {"lumens": "{{json sequence}}"}}]
        })))
        .await;
    assert_eq!(st, 200, "{v}");
    let t = h
        .ok(
            Method::POST,
            &format!("/async-mocks/{mock}/trigger"),
            EDITOR,
            None,
        )
        .await;
    assert_eq!(t["protocol"], "mqtt");
    assert!(t.get("partition").is_none());
    let v = h
        .wait_for(json!({"match": {"topic": {"equalTo": format!("{base}/lamp/state")}}, "lookbackMs": 5000}))
        .await;
    assert_eq!(
        v["messages"][0]["properties"]["qos"], 1,
        "delivered with the lower of the published QoS (2) and the subscription QoS (1)"
    );
    assert_eq!(v["messages"][0]["contentType"], "application/json");

    // The broker goes away and comes back: the capture and the publisher reconnect.
    proxy.restart(Duration::from_millis(800));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut reconnected = false;
    for _ in 0..60 {
        let r = h
            .http
            .post(format!("{}/devices/d2/state", h.data))
            .header("x-request-id", "req-8")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 202, "the stub answers while the broker is away");
        let (st, _) = h
            .call(Method::POST, "/capture/messages/wait", VIEWER, Some(json!({"match": {"properties": {"correlationData": {"equalTo": "req-8"}}}, "includeExisting": true, "timeoutMs": 700})))
            .await;
        if st == 200 {
            reconnected = true;
            break;
        }
    }
    assert!(
        reconnected,
        "messages are captured again after the broker came back"
    );
    h.wait_running(&src_id).await;
    let s = h
        .ok(
            Method::GET,
            &format!("/capture/sources/{src_id}"),
            VIEWER,
            None,
        )
        .await;
    assert!(
        s["status"]["connects"].as_u64().unwrap() >= 2,
        "the consumer reconnected: {s}"
    );

    // Disabling the source stops the consumer.
    let mut body = src.clone();
    body.as_object_mut().unwrap().remove("status");
    body["enabled"] = json!(false);
    h.ok(
        Method::PUT,
        &format!("/capture/sources/{src_id}"),
        EDITOR,
        Some(body),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let s = h
        .ok(
            Method::GET,
            &format!("/capture/sources/{src_id}"),
            VIEWER,
            None,
        )
        .await;
    assert_eq!(s["status"]["state"], "disabled");
}

#[tokio::test]
async fn mqtt_publishes_reach_shared_subscriptions_and_capture_does_not_join_a_group() {
    let Ok(url) = std::env::var("FASTMOCK_TEST_MQTT") else {
        eprintln!("FASTMOCK_TEST_MQTT not set: skipped");
        return;
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let base = format!("fm-it/{}", Uuid::new_v4().simple());
    let group = format!("workers-{}", Uuid::new_v4().simple());
    let mut cfg = capture_cfg(StorageKind::Memory);
    cfg.mqtt = Some(MqttSettings::new(&url).unwrap());
    cfg.mqtt_allowed_topics = Some(vec![format!("{base}/*")]);
    cfg.capture.mqtt_topics = Some(vec![format!("{base}/*")]);
    let h = H::start(cfg, true).await;

    let direct = MqttSettings::new(&url).unwrap();
    let mut workers = Vec::new();
    for n in 0..2 {
        let (tx, rx) = capture_channel(
            "mqtt",
            CaptureLimits {
                max_messages: 64,
                ..Default::default()
            },
        );
        let conn = MqttConnection::start(
            &direct,
            direct.client_id(&format!("worker{n}")),
            vec![(format!("$share/{group}/{base}/#"), 1)],
            Some(tx),
        )
        .unwrap();
        workers.push((conn, rx));
    }
    for (conn, _) in &workers {
        for _ in 0..100 {
            if conn.status().connected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(conn.status().connected);
    }

    let src = h
        .put_source(
            "",
            json!({"name": "all", "protocol": "mqtt", "topics": [format!("{base}/#")], "qos": 1}),
        )
        .await;
    h.wait_running(src["id"].as_str().unwrap()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (st, v) = h
        .call(Method::POST, "/stubs", EDITOR, Some(json!({
            "request": {"method": "POST", "urlPathTemplate": "/jobs/{id}"},
            "response": {"status": 202},
            "messages": [{"protocol": "mqtt", "topic": format!("{base}/jobs/{{{{request.pathParams.id}}}}"), "qos": 1,
                          "jsonValue": {"job": "{{request.pathParams.id}}"}}]
        })))
        .await;
    assert_eq!(st, 201, "{v}");
    let jobs: Vec<String> = (0..8).map(|i| format!("job{i}")).collect();
    for j in &jobs {
        let r = h
            .http
            .post(format!("{}/jobs/{j}", h.data))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 202);
    }

    let mut got: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while got.len() < jobs.len() && tokio::time::Instant::now() < deadline {
        for (_, rx) in workers.iter_mut() {
            if let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
                let v: Value = serde_json::from_slice(&m.payload).unwrap();
                assert_eq!(
                    m.topic,
                    format!("{base}/jobs/{}", v["job"].as_str().unwrap())
                );
                got.push(v["job"].as_str().unwrap().to_string());
            }
        }
    }
    got.sort();
    assert_eq!(
        got, jobs,
        "every publication reaches exactly one member of the shared group"
    );

    let v = h
        .wait_for(json!({"match": {"source": {"equalTo": "all"}}, "count": 8, "lookbackMs": 20000, "timeoutMs": 10000}))
        .await;
    assert_eq!(
        v["count"], 8,
        "the capture source got its own copy of every message, so the group took nothing away"
    );
}

type Registered = Arc<Mutex<(Vec<String>, Vec<(String, i32)>)>>;

async fn fake_registry() -> (String, Registered) {
    use axum::extract::{Path, State};
    use axum::routing::{get, post};
    let state: Registered = Arc::default();
    async fn register(
        State(s): State<Registered>,
        Path(subject): Path<String>,
        axum::Json(b): axum::Json<Value>,
    ) -> axum::Json<Value> {
        let schema = b["schema"].as_str().unwrap().to_string();
        let mut g = s.lock().unwrap();
        let id = match g.0.iter().position(|x| *x == schema) {
            Some(i) => i,
            None => {
                g.0.push(schema);
                g.0.len() - 1
            }
        } as i32
            + 1;
        g.1.push((subject, id));
        axum::Json(json!({"id": id}))
    }
    async fn by_id(
        State(s): State<Registered>,
        Path(id): Path<usize>,
    ) -> Result<axum::Json<Value>, axum::http::StatusCode> {
        let g = s.lock().unwrap();
        match g.0.get(id.wrapping_sub(1)) {
            Some(schema) => Ok(axum::Json(json!({"schema": schema}))),
            None => Err(axum::http::StatusCode::NOT_FOUND),
        }
    }
    let router = axum::Router::new()
        .route("/subjects/{subject}/versions", post(register))
        .route("/schemas/ids/{id}", get(by_id))
        .with_state(state.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    (url, state)
}

fn reading_schema() -> Value {
    json!({"type": "record", "name": "Reading", "fields": [
        {"name": "device", "type": "string"}, {"name": "temp", "type": "int"}]})
}

#[tokio::test]
async fn mqtt_stubs_publish_avro_that_capture_decodes() {
    let Ok(url) = std::env::var("FASTMOCK_TEST_MQTT") else {
        eprintln!("FASTMOCK_TEST_MQTT not set: skipped");
        return;
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (registry, seen) = fake_registry().await;
    let base = format!("fm-it/{}", Uuid::new_v4().simple());
    let mut cfg = capture_cfg(StorageKind::Memory);
    cfg.mqtt = Some(MqttSettings::new(&url).unwrap());
    cfg.mqtt_allowed_topics = Some(vec![format!("{base}/*")]);
    cfg.capture.mqtt_topics = Some(vec![format!("{base}/*")]);
    cfg.kafka_schema_registry = Some(registry);
    let h = H::start(cfg, true).await;
    let src = h
        .put_source("", json!({"name": "readings", "protocol": "mqtt", "topics": [format!("{base}/#")], "qos": 1}))
        .await;
    h.wait_running(src["id"].as_str().unwrap()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (st, v) = h
        .call(Method::POST, "/stubs", EDITOR, Some(json!({
            "request": {"method": "POST", "urlPathTemplate": "/readings/{id}"},
            "response": {"status": 202},
            "messages": [
                {"protocol": "mqtt", "topic": format!("{base}/fixed/state"), "qos": 1, "avroSchema": reading_schema(),
                 "jsonValue": {"device": "{{request.pathParams.id}}", "temp": "{{json 21}}"}},
                {"protocol": "mqtt", "topic": format!("{base}/by-id/{{{{request.pathParams.id}}}}"), "qos": 1,
                 "avroSchema": reading_schema(), "avroSubject": "readings-by-id-value",
                 "jsonValue": {"device": "{{request.pathParams.id}}", "temp": 30}}
            ]
        })))
        .await;
    assert_eq!(st, 201, "{v}");
    let (st, v) = h
        .call(Method::POST, "/stubs", EDITOR, Some(json!({
            "request": {"method": "POST", "urlPathTemplate": "/bad/{id}"},
            "response": {"status": 202},
            "messages": [{"protocol": "mqtt", "topic": format!("{base}/{{{{request.pathParams.id}}}}"), "avroSchema": reading_schema(), "jsonValue": {}}]
        })))
        .await;
    assert_eq!(st, 400, "{v}");
    assert!(v.to_string().contains("avroSubject is required"), "{v}");

    let r = h
        .http
        .post(format!("{}/readings/d7", h.data))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    let v = h
        .wait_for(json!({"match": {"source": {"equalTo": "readings"}, "body": [{"jsonPath": "$.temp", "value": {"equalTo": "30"}}]},
                         "count": 1, "lookbackMs": 20000, "timeoutMs": 10000}))
        .await;
    let d = h
        .ok(
            Method::GET,
            &format!(
                "/capture/messages/{}",
                v["messages"][0]["id"].as_str().unwrap()
            ),
            VIEWER,
            None,
        )
        .await;
    assert_eq!(d["topic"], format!("{base}/by-id/d7"));
    assert_eq!(d["body"]["encoding"], "avro", "{d}");
    assert_eq!(d["body"]["value"], json!({"device": "d7", "temp": 30}));
    let v = h
        .wait_for(json!({"match": {"topic": {"equalTo": format!("{base}/fixed/state")}, "body": [{"jsonPath": "$.device", "value": {"equalTo": "d7"}}]},
                         "lookbackMs": 20000, "timeoutMs": 10000}))
        .await;
    assert_eq!(v["count"], 1);
    let mock = Uuid::now_v7();
    let (st, v) = h
        .call(Method::PUT, &format!("/async-mocks/{mock}"), EDITOR, Some(json!({
            "name": "avro lamp", "protocol": "mqtt", "channel": format!("{base}/lamp/reading"), "frequencyMs": 60000,
            "enabled": false, "avroSchema": reading_schema(), "mqtt": {"qos": 1, "retain": false},
            "messages": [{"payload": {"device": "lamp", "temp": 5}}]
        })))
        .await;
    assert_eq!(st, 200, "{v}");
    h.ok(
        Method::POST,
        &format!("/async-mocks/{mock}/trigger"),
        EDITOR,
        None,
    )
    .await;
    let v = h
        .wait_for(json!({"match": {"topic": {"equalTo": format!("{base}/lamp/reading")}, "body": [{"jsonPath": "$.temp", "value": {"equalTo": "5"}}]},
                         "lookbackMs": 20000, "timeoutMs": 10000}))
        .await;
    assert_eq!(v["count"], 1, "the async mock publishes Avro on MQTT too");
    let subjects: Vec<String> = seen
        .lock()
        .unwrap()
        .1
        .iter()
        .map(|(s, _)| s.clone())
        .collect();
    assert!(
        subjects.contains(&format!("{base}/fixed/state-value")),
        "the default subject is the topic and stays one path segment: {subjects:?}"
    );
    assert!(
        subjects.contains(&"readings-by-id-value".to_string()),
        "{subjects:?}"
    );
}

#[tokio::test]
async fn amqp_stubs_publish_avro_that_capture_decodes() {
    let Ok(url) = std::env::var("FASTMOCK_TEST_AMQP") else {
        eprintln!("FASTMOCK_TEST_AMQP not set: skipped");
        return;
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (registry, seen) = fake_registry().await;
    let exchange = format!("fm-it-avro-{}", Uuid::new_v4().simple());
    let mut settings = amqp_settings(&url);
    settings.declare_exchanges = true;
    let mut cfg = capture_cfg(StorageKind::Memory);
    cfg.amqp = Some(settings.clone());
    cfg.amqp_allowed_exchanges = Some(vec!["fm-it-avro-*".into()]);
    cfg.kafka_schema_registry = Some(registry);
    let h = H::start(cfg, true).await;
    AmqpPublisher::new(settings, "test setup")
        .publish(
            &AmqpMessage {
                exchange: exchange.clone(),
                routing_key: "init".into(),
                ..Default::default()
            },
            Some(&fastmock_brokers::amqp::ExchangeDeclaration::default()),
        )
        .await
        .unwrap();
    let src = h
        .put_source("", json!({"name": "readings", "protocol": "amqp", "exchange": exchange, "bindingKeys": ["reading.#"]}))
        .await;
    h.wait_running(src["id"].as_str().unwrap()).await;

    let (st, v) = h
        .call(Method::POST, "/stubs", EDITOR, Some(json!({
            "request": {"method": "POST", "urlPathTemplate": "/readings/{id}"},
            "response": {"status": 202},
            "messages": [{"protocol": "amqp", "exchange": exchange, "routingKey": "reading.created",
                          "avroSchema": reading_schema(), "contentType": "avro/binary",
                          "jsonValue": {"device": "{{request.pathParams.id}}", "temp": 18}}]
        })))
        .await;
    assert_eq!(st, 201, "{v}");
    let r = h
        .http
        .post(format!("{}/readings/d9", h.data))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    let v = h
        .wait_for(json!({"match": {"exchange": {"equalTo": exchange}, "body": [{"jsonPath": "$.device", "value": {"equalTo": "d9"}}]},
                         "lookbackMs": 20000, "timeoutMs": 10000}))
        .await;
    let d = h
        .ok(
            Method::GET,
            &format!(
                "/capture/messages/{}",
                v["messages"][0]["id"].as_str().unwrap()
            ),
            VIEWER,
            None,
        )
        .await;
    assert_eq!(d["contentType"], "avro/binary");
    assert_eq!(d["body"]["encoding"], "avro", "{d}");
    assert_eq!(d["body"]["value"], json!({"device": "d9", "temp": 18}));
    let subjects: Vec<String> = seen
        .lock()
        .unwrap()
        .1
        .iter()
        .map(|(s, _)| s.clone())
        .collect();
    assert_eq!(subjects, vec![format!("{exchange}.reading.created-value")]);
}

fn amqp_settings(url: &str) -> AmqpSettings {
    let mut s = AmqpSettings::new(url).unwrap();
    s.credentials = Some(Credentials {
        username: std::env::var("FASTMOCK_TEST_AMQP_USER").unwrap_or_else(|_| "guest".into()),
        password: std::env::var("FASTMOCK_TEST_AMQP_PASSWORD").unwrap_or_else(|_| "guest".into()),
    });
    s
}

#[tokio::test]
async fn amqp_stubs_async_mocks_and_capture() {
    let Ok(url) = std::env::var("FASTMOCK_TEST_AMQP") else {
        eprintln!("FASTMOCK_TEST_AMQP not set: skipped");
        return;
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let exchange = format!("fm-it-{}", Uuid::new_v4().simple());
    let mut settings = amqp_settings(&url);
    settings.declare_exchanges = true;
    let mut cfg = capture_cfg(StorageKind::Memory);
    cfg.amqp = Some(settings.clone());
    cfg.amqp_allowed_exchanges = Some(vec!["fm-it-*".into()]);
    let h = H::start(cfg, true).await;

    // The exchange exists once something declared it.
    AmqpPublisher::new(settings.clone(), "test setup")
        .publish(
            &AmqpMessage {
                exchange: exchange.clone(),
                routing_key: "init".into(),
                ..Default::default()
            },
            Some(&fastmock_brokers::amqp::ExchangeDeclaration::default()),
        )
        .await
        .unwrap();

    // The application's own consumer, on its own queue.
    let (app_tx, mut app_rx) = capture_channel(
        "amqp",
        CaptureLimits {
            max_messages: 16,
            ..Default::default()
        },
    );
    let app_status = Arc::new(Mutex::new(LinkStatus::default()));
    let (app_stop, app_stopped) = watch::channel(false);
    tokio::spawn(fastmock_brokers::amqp::consume(
        settings.clone(),
        AmqpCapture {
            exchange: exchange.clone(),
            binding_keys: vec!["order.#".into()],
            max_queue_length: 100,
            label: "app".into(),
        },
        app_tx,
        app_status.clone(),
        app_stopped,
    ));

    let src = h
        .put_source("", json!({"name": "orders", "protocol": "amqp", "exchange": exchange, "bindingKeys": ["order.*"]}))
        .await;
    h.wait_running(src["id"].as_str().unwrap()).await;
    for _ in 0..100 {
        if app_status.lock().unwrap().connected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let (st, v) = h
        .call(Method::POST, "/stubs", EDITOR, Some(json!({
            "request": {"method": "POST", "urlPathTemplate": "/orders/{id}"},
            "response": {"status": 201},
            "messages": [
                {"protocol": "amqp", "exchange": exchange, "routingKey": "order.created",
                 "headers": {"x-event": "created"}, "contentType": "application/json",
                 "messageId": "{{request.pathParams.id}}", "correlationId": "{{request.headers.x-request-id}}",
                 "replyTo": "replies", "expiration": "60000", "persistent": true, "priority": 4, "messageType": "order.created",
                 "jsonValue": {"id": "{{request.pathParams.id}}"}},
                {"protocol": "amqp", "exchange": "not-allowed", "routingKey": "x", "value": "x"}
            ]
        })))
        .await;
    assert_eq!(st, 201, "{v}");
    let r = h
        .http
        .post(format!("{}/orders/o-1", h.data))
        .header("x-request-id", "c-9")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let v = h
        .wait_for(json!({"match": {"exchange": {"equalTo": exchange}, "properties": {"correlationId": {"equalTo": "c-9"}}}, "lookbackMs": 5000}))
        .await;
    let m = &v["messages"][0];
    assert_eq!(m["topic"], "order.created");
    assert_eq!(m["protocol"], "amqp");
    assert_eq!(m["headers"]["x-event"], "created");
    assert_eq!(m["contentType"], "application/json");
    assert_eq!(m["properties"]["messageId"], "o-1");
    assert_eq!(m["properties"]["replyTo"], "replies");
    assert_eq!(m["properties"]["expiration"], "60000");
    assert_eq!(m["properties"]["deliveryMode"], 2);
    assert_eq!(m["properties"]["priority"], 4);
    assert_eq!(m["properties"]["type"], "order.created");
    let app_msg = tokio::time::timeout(Duration::from_secs(5), app_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        app_msg.payload, br#"{"id":"o-1"}"#,
        "the application's queue still gets the message"
    );

    // Async mock to the same exchange.
    let mock = Uuid::now_v7();
    let (st, v) = h
        .call(
            Method::PUT,
            &format!("/async-mocks/{mock}"),
            EDITOR,
            Some(json!({
                "name": "orders", "protocol": "amqp", "channel": "order.paid", "enabled": false,
                "amqp": {"exchange": exchange, "persistent": true},
                "messages": [{"payload": {"seq": "{{json sequence}}"}}]
            })),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    let t = h
        .ok(
            Method::POST,
            &format!("/async-mocks/{mock}/trigger"),
            EDITOR,
            None,
        )
        .await;
    assert_eq!(t["exchange"], exchange);
    let v = h
        .wait_for(json!({"match": {"topic": {"equalTo": "order.paid"}}, "lookbackMs": 5000}))
        .await;
    assert_eq!(v["messages"][0]["properties"]["deliveryMode"], 2);
    let bad = Uuid::now_v7();
    h.ok(Method::PUT, &format!("/async-mocks/{bad}"), EDITOR, Some(json!({
        "name": "x", "protocol": "amqp", "channel": "k", "amqp": {"exchange": "other"}, "messages": [{"payload": 1}]
    }))).await;
    let (st, v) = h
        .call(
            Method::POST,
            &format!("/async-mocks/{bad}/trigger"),
            EDITOR,
            None,
        )
        .await;
    assert_eq!(st, 400);
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("FASTMOCK_AMQP_ALLOWED_EXCHANGES"),
        "{v}"
    );

    let v = h
        .ok(
            Method::POST,
            "/capture/verify",
            VIEWER,
            Some(json!({"match": {"source": {"equalTo": "orders"}}, "count": 2})),
        )
        .await;
    assert_eq!(v["verified"], true, "{v}");
    let _ = app_stop.send(true);
}

#[tokio::test]
async fn kafka_capture_reads_without_a_consumer_group() {
    let Ok(bootstrap) = std::env::var("FASTMOCK_TEST_KAFKA") else {
        eprintln!("FASTMOCK_TEST_KAFKA not set: skipped");
        return;
    };
    let topic = format!("fm-capture-{}", Uuid::new_v4().simple());
    let client = fastmock_kafka::KafkaPublisher::connect(&fastmock_kafka::KafkaSettings {
        bootstrap: vec![bootstrap.clone()],
        client_id: "fastmock-test".into(),
        ..Default::default()
    })
    .await
    .unwrap();
    client
        .ensure_topics(std::slice::from_ref(&topic), 2, 1)
        .await
        .unwrap();
    let mut cfg = capture_cfg(StorageKind::Memory);
    cfg.kafka_bootstrap = Some(vec![bootstrap]);
    cfg.capture.max_message_bytes = 16;
    let h = H::start(cfg, true).await;
    let src = h
        .put_source(
            "",
            json!({"name": "orders", "protocol": "kafka", "topics": [topic]}),
        )
        .await;
    h.wait_running(src["id"].as_str().unwrap()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (st, v) = h
        .call(Method::POST, "/stubs", EDITOR, Some(json!({
            "request": {"method": "POST", "urlPathTemplate": "/orders/{id}/ship"},
            "response": {"status": 202},
            "messages": [{"topic": topic, "key": "{{request.pathParams.id}}", "headers": {"event-type": "OrderShipped"},
                          "contentType": "application/json", "jsonValue": {"orderId": "{{request.pathParams.id}}", "padding": "0123456789"}}]
        })))
        .await;
    assert_eq!(st, 201, "{v}");
    h.http
        .post(format!("{}/orders/A-1/ship", h.data))
        .send()
        .await
        .unwrap();
    let v = h
        .wait_for(json!({"match": {"key": {"equalTo": "A-1"}, "headers": {"event-type": {"equalTo": "OrderShipped"}}}, "lookbackMs": 10000, "timeoutMs": 15000}))
        .await;
    let m = &v["messages"][0];
    assert_eq!(m["topic"], topic);
    assert_eq!(m["contentType"], "application/json");
    assert_eq!(m["truncated"], true);
    assert_eq!(m["size"], 40);
    assert!(m["properties"]["offset"].is_number());
    let d = h
        .ok(
            Method::GET,
            &format!("/capture/messages/{}", m["id"].as_str().unwrap()),
            VIEWER,
            None,
        )
        .await;
    assert_eq!(
        d["body"]["encoding"], "text",
        "a truncated JSON body is text"
    );
}

#[tokio::test]
async fn one_replica_captures_and_any_replica_answers() {
    let Ok(url) = std::env::var("FASTMOCK_TEST_MQTT") else {
        eprintln!("FASTMOCK_TEST_MQTT not set: skipped");
        return;
    };
    let Some(storage) = shared_storage().await else {
        eprintln!("FASTMOCK_TEST_DATABASE_URL/FASTMOCK_TEST_VALKEY_URL not set: skipped");
        return;
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let base = format!("fm-rep/{}", Uuid::new_v4().simple());
    let mut cfg = capture_cfg(storage);
    cfg.mqtt = Some(MqttSettings::new(&url).unwrap());
    let a = H::start(cfg.clone(), true).await;
    let b = H::start(cfg, true).await;
    let src = a
        .put_source(
            "",
            json!({"name": "devices", "protocol": "mqtt", "topics": [format!("{base}/#")]}),
        )
        .await;
    b.wait_running(src["id"].as_str().unwrap()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let s = MqttSettings::new(&url).unwrap();
    let publisher = MqttConnection::start(&s, s.client_id("app"), vec![], None).unwrap();
    for i in 0..3 {
        publisher
            .publish(&MqttPublish {
                topic: format!("{base}/m{i}"),
                payload: b"x".to_vec(),
                qos: 1,
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let v = b.wait_for(json!({"match": {"topic": {"matches": format!("{base}/m.")}}, "count": 3, "lookbackMs": 10000, "timeoutMs": 10000})).await;
    assert_eq!(v["count"], 3);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let v = a
        .ok(
            Method::POST,
            "/capture/verify",
            VIEWER,
            Some(json!({"match": {"source": {"equalTo": "devices"}}, "count": 3})),
        )
        .await;
    assert_eq!(v["verified"], true, "each message is stored once: {v}");
}

#[tokio::test]
async fn asyncapi_import_creates_mqtt_and_amqp_mocks() {
    let mut cfg = Config::for_tests(StorageKind::Memory, tokens());
    cfg.amqp = Some(AmqpSettings::new("amqp://127.0.0.1:1").unwrap());
    cfg.amqp_allowed_exchanges = Some(vec!["orders".into()]);
    let h = H::start(cfg, false).await;
    let doc = json!({"asyncapi": "2.6.0", "info": {"title": "Shop", "version": "1"},
    "servers": {"m": {"url": "mqtt://broker.example.com", "protocol": "mqtt"}},
    "channels": {
        "lights/{id}": {"parameters": {"id": {"schema": {"type": "string", "example": "l1"}}},
            "subscribe": {"operationId": "light", "bindings": {"mqtt": {"qos": 1}},
                "message": {"payload": {"type": "object", "properties": {"on": {"type": "boolean"}}}}}},
        "order.created": {"bindings": {"amqp": {"is": "routingKey", "exchange": {"name": "orders", "type": "topic"}}},
            "subscribe": {"operationId": "orderCreated", "message": {"payload": {"type": "string", "example": "x"}}}},
        "audit.event": {"bindings": {"amqp": {"exchange": {"name": "audit"}}},
            "subscribe": {"operationId": "audited", "message": {"payload": {"type": "string"}}}}
    }});
    let (st, v) = h
        .call(
            Method::POST,
            "/import/asyncapi",
            EDITOR,
            Some(json!({"spec": doc, "tag": "shop"})),
        )
        .await;
    assert_eq!(st, 201, "{v}");
    let mocks = v["asyncMocks"].as_array().unwrap();
    let light = mocks.iter().find(|m| m["name"] == "light").unwrap();
    assert_eq!(light["protocol"], "mqtt");
    assert_eq!(light["channel"], "lights/l1");
    assert_eq!(light["mqtt"]["qos"], 1);
    let order = mocks.iter().find(|m| m["name"] == "orderCreated").unwrap();
    assert_eq!(order["protocol"], "amqp");
    assert_eq!(order["amqp"]["exchange"], "orders");
    let warnings: Vec<&str> = v["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap())
        .collect();
    assert!(
        warnings.iter().any(|w| w.contains(
            "FASTMOCK_MQTT_URL is not set; MQTT async mocks are stored but not published"
        )),
        "{warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("exchange audit is not in FASTMOCK_AMQP_ALLOWED_EXCHANGES")),
        "{warnings:?}"
    );
    let list = h
        .ok(Method::GET, "/async-mocks?tag=shop", VIEWER, None)
        .await;
    assert_eq!(list["asyncMocks"].as_array().unwrap().len(), 3);
    let id = light["id"].as_str().unwrap();
    let (st, v) = h
        .call(
            Method::POST,
            &format!("/async-mocks/{id}/trigger"),
            EDITOR,
            None,
        )
        .await;
    assert_eq!(st, 400);
    assert!(
        v["error"].as_str().unwrap().contains("FASTMOCK_MQTT_URL"),
        "{v}"
    );
}

#[tokio::test]
async fn capture_sources_are_limited_to_allowed_application_destinations() {
    let mut cfg = Config::for_tests(StorageKind::Memory, tokens());
    cfg.capture.enabled = true;
    cfg.kafka_bootstrap = Some(vec!["127.0.0.1:1".into()]);
    cfg.mqtt = Some(MqttSettings::new("mqtt://127.0.0.1:1").unwrap());
    let h = H::start(cfg, false).await;
    let put = |body: Value| {
        let h = &h;
        async move {
            h.call(
                Method::PUT,
                &format!("/capture/sources/{}", Uuid::now_v7()),
                EDITOR,
                Some(body),
            )
            .await
        }
    };
    let (st, v) = put(json!({"name": "o", "protocol": "kafka", "topics": ["orders"]})).await;
    assert_eq!(st, 400, "no allow-list: API sources are refused: {v}");
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("FASTMOCK_CAPTURE_KAFKA_TOPICS"),
        "{v}"
    );

    let mut cfg = capture_cfg(StorageKind::Memory);
    cfg.kafka_bootstrap = Some(vec!["127.0.0.1:1".into()]);
    cfg.mqtt = Some(MqttSettings::new("mqtt://127.0.0.1:1").unwrap());
    let h = H::start(cfg, false).await;
    for (body, want) in [
        (
            json!({"name": "s", "protocol": "mqtt", "topics": ["$share/app/orders/#"]}),
            "starts with $",
        ),
        (
            json!({"name": "s", "protocol": "mqtt", "topics": ["$SYS/#"]}),
            "starts with $",
        ),
        (
            json!({"name": "k", "protocol": "kafka", "topics": ["__consumer_offsets"], "startFrom": "earliest"}),
            "internal topic",
        ),
    ] {
        let (st, v) = h
            .call(
                Method::PUT,
                &format!("/capture/sources/{}", Uuid::now_v7()),
                EDITOR,
                Some(body.clone()),
            )
            .await;
        assert_eq!(st, 400, "{body}: {v}");
        assert!(v["error"].as_str().unwrap().contains(want), "{body}: {v}");
    }

    // Purging needs messages:delete, checked before the source is deleted.
    let (st, src) = h
        .call(
            Method::PUT,
            &format!("/capture/sources/{}?space=billing", Uuid::now_v7()),
            BCAP,
            Some(json!({"name": "o", "protocol": "kafka", "topics": ["orders"]})),
        )
        .await;
    assert_eq!(st, 200, "{src}");
    let id = src["id"].as_str().unwrap();
    let (st, _) = h
        .call(
            Method::DELETE,
            &format!("/capture/sources/{id}?purge=true"),
            BCAP,
            None,
        )
        .await;
    assert_eq!(st, 403);
    let (st, v) = h
        .call(Method::GET, &format!("/capture/sources/{id}"), BCAP, None)
        .await;
    assert_eq!(st, 200, "a refused purge leaves the source: {v}");
    let (st, _) = h
        .call(
            Method::DELETE,
            &format!("/capture/sources/{id}"),
            BCAP,
            None,
        )
        .await;
    assert_eq!(st, 204);
}

#[tokio::test]
async fn searches_are_bounded_by_body_bytes_and_skip_bodies_they_do_not_need() {
    let mut cfg = capture_cfg(StorageKind::Memory);
    cfg.kafka_bootstrap = Some(vec!["127.0.0.1:1".into()]);
    cfg.capture.max_message_bytes = 4 * 1024 * 1024;
    let h = H::start(cfg, false).await;
    let src = h
        .put_source(
            "",
            json!({"name": "big", "protocol": "kafka", "topics": ["big"]}),
        )
        .await;
    let id: Uuid = src["id"].as_str().unwrap().parse().unwrap();
    let body = format!("{{\"n\":\"{}\"}}", "x".repeat(3 * 1024 * 1024));
    let msgs: Vec<CapturedMessage> = (0..30)
        .map(|_| stored(id, "big", None, "big", body.as_bytes()))
        .collect();
    let mut other = stored(id, "big", None, "small", br#"{"n":"y"}"#);
    other.headers.insert("kind".into(), "small".into());
    insert(&h, &msgs).await;
    insert(&h, std::slice::from_ref(&other)).await;
    let (st, v) = h
        .call(
            Method::POST,
            "/capture/messages/find",
            VIEWER,
            Some(json!({"match": {"body": [{"jsonPath": "$.n", "value": {"equalTo": "y"}}]}})),
        )
        .await;
    assert_eq!(st, 422, "90 MiB of bodies to match: {v}");
    assert!(v["error"].as_str().unwrap().contains("64 MiB"), "{v}");
    let v = h
        .ok(
            Method::POST,
            "/capture/messages/find",
            VIEWER,
            Some(json!({"match": {"topic": {"equalTo": "small"}, "body": [{"jsonPath": "$.n", "value": {"equalTo": "y"}}]}})),
        )
        .await;
    assert_eq!(
        v["count"], 1,
        "only bodies of messages meeting the other conditions are read: {v}"
    );
    let v = h
        .ok(
            Method::POST,
            "/capture/messages/find",
            VIEWER,
            Some(json!({"match": {"topic": {"equalTo": "big"}}, "limit": 2})),
        )
        .await;
    assert_eq!(v["count"], 30);
    assert_eq!(v["messages"].as_array().unwrap().len(), 2);
    assert_eq!(
        v["messages"][0]["id"],
        msgs[29].id.to_string(),
        "newest first"
    );
    let (st, v) = h
        .call(
            Method::POST,
            "/capture/verify",
            VIEWER,
            Some(json!({"match": {"topic": {"equalTo": "small"}, "body": [{"contains": "z"}]}, "count": 1})),
        )
        .await;
    assert_eq!(st, 422);
    assert_eq!(v["nearMisses"][0]["id"], other.id.to_string(), "{v}");
    assert_eq!(v["nearMisses"][0]["mismatches"], json!(["body[0]"]));
}

#[tokio::test]
async fn one_space_cannot_take_every_search_slot() {
    let mut cfg = capture_cfg(StorageKind::Memory);
    cfg.kafka_bootstrap = Some(vec!["127.0.0.1:1".into()]);
    let h = Arc::new(H::start(cfg, false).await);
    let waits: Vec<_> = (0..6)
        .map(|_| {
            let h = h.clone();
            tokio::spawn(async move {
                h.call(
                    Method::POST,
                    "/capture/messages/wait?space=billing",
                    VIEWER,
                    Some(json!({"match": {"topic": {"equalTo": "never"}}, "timeoutMs": 3000})),
                )
                .await
                .0
            })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    let (st, v) = h
        .call(
            Method::POST,
            "/capture/messages/find?space=crm",
            VIEWER,
            Some(json!({"match": {}})),
        )
        .await;
    assert_eq!(st, 200, "another space still gets a slot: {v}");
    assert!(started.elapsed() < Duration::from_secs(2));
    for w in waits {
        assert!(matches!(w.await.unwrap(), 408 | 503));
    }
}

#[tokio::test]
async fn trigger_errors_name_no_broker_address() {
    let mut cfg = Config::for_tests(StorageKind::Memory, tokens());
    cfg.amqp = Some(AmqpSettings::new("amqp://127.0.0.1:1/private-vhost").unwrap());
    let h = H::start(cfg, false).await;
    let (st, v) = h
        .call(
            Method::PUT,
            &format!("/async-mocks/{}", Uuid::now_v7()),
            EDITOR,
            Some(
                json!({"name": "o", "protocol": "amqp", "channel": "order.created",
                "amqp": {"exchange": "orders"}, "messages": [{"name": "e", "payload": "x"}]}),
            ),
        )
        .await;
    assert!((200..300).contains(&st), "{v}");
    let id = v["id"].as_str().unwrap();
    let (st, v) = h
        .call(
            Method::POST,
            &format!("/async-mocks/{id}/trigger"),
            EDITOR,
            None,
        )
        .await;
    assert_eq!(st, 502, "{v}");
    let e = v["error"].as_str().unwrap();
    assert!(e.contains("broker connection refused"), "{e}");
    assert!(
        !e.contains("127.0.0.1") && !e.contains("private-vhost"),
        "{e}"
    );
}
