//! Per-space capture allow-lists: a space narrows the server-wide lists and never widens them, sources do
//! not cross spaces, the allow-lists are visible where they apply, and the MCP tools go through the same
//! checks. Runs on memory storage and, with FASTMOCK_TEST_DATABASE_URL and FASTMOCK_TEST_VALKEY_URL, on
//! PostgreSQL and Valkey. No broker is needed: the sources never connect.

use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::Duration;

use fastmock_brokers::amqp::AmqpSettings;
use fastmock_brokers::mqtt::MqttSettings;
use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use fastmock_server::state::AppState;
use reqwest::Method;
use serde_json::{Value, json};
use sqlx::Connection;
use tokio::sync::watch;
use uuid::Uuid;

const ADMIN: &str = "fm_admin_token_for_capture_space_tests_0123";
const VIEWER: &str = "fm_viewer_token_for_capture_space_tests_012";
const PAY: &str = "fm_pay_token_for_capture_space_tests_0123456";
const BILL: &str = "fm_bill_token_for_capture_space_tests_012345";
const OPEN: &str = "fm_open_token_for_capture_space_tests_012345";
const CRM: &str = "fm_crm_token_for_capture_space_tests_0123456";

const ACCESS: &str = r#"
roles:
  - name: capture-writer
    permissions: [messages:read, messages:write]
spaces:
  - name: payments
    capture:
      kafkaTopics: ["payments.*"]
      amqpExchanges: [payments]
  - name: billing
    capture:
      kafkaTopics: ["billing.*"]
  - name: open
    capture:
      kafkaTopics: ["*"]
  - name: crm
bindings:
  - subject: token:pay
    role: capture-writer
    spaces: [payments]
  - subject: token:bill
    role: editor
    spaces: [billing]
  - subject: token:open
    role: capture-writer
    spaces: [open]
  - subject: token:crm
    role: capture-writer
    spaces: [crm]
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
        format!("bill::{}", hash_token(BILL)),
        format!("open::{}", hash_token(OPEN)),
        format!("crm::{}", hash_token(CRM)),
    ]
}

async fn shared_storage() -> Option<StorageKind> {
    let admin_url = std::env::var("FASTMOCK_TEST_DATABASE_URL").ok()?;
    let valkey_url = std::env::var("FASTMOCK_TEST_VALKEY_URL").ok()?;
    let db = format!("fm_capspaces_{}", Uuid::new_v4().simple());
    let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut c)
        .await
        .unwrap();
    Some(StorageKind::Postgres {
        database_url: format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db),
        valkey_url,
        valkey_prefix: format!("capspaces:{}:", Uuid::new_v4().simple()),
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

fn write_access(text: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("fm-capspaces-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("access.yaml");
    std::fs::write(&f, text).unwrap();
    f
}

fn config(storage: StorageKind) -> Config {
    let mut cfg = Config::for_tests(storage, tokens());
    cfg.plugins = Some(vec![]);
    cfg.access_file = Some(write_access(ACCESS));
    cfg.kafka_bootstrap = Some(vec!["127.0.0.1:1".into()]);
    cfg.mqtt = Some(MqttSettings::new("mqtt://127.0.0.1:1").unwrap());
    cfg.amqp = Some(AmqpSettings::new("amqp://127.0.0.1:1/%2f").unwrap());
    cfg.capture.enabled = true;
    cfg.capture.kafka_topics = Some(vec![
        "payments.*".into(),
        "billing.*".into(),
        "shared.*".into(),
    ]);
    cfg.capture.mqtt_topics = Some(vec!["*".into()]);
    cfg.capture.amqp_exchanges = Some(vec!["payments".into(), "billing".into()]);
    cfg
}

struct H {
    app: AppState,
    admin: String,
    http: reqwest::Client,
    _stop: watch::Sender<bool>,
}

impl H {
    async fn start(cfg: Config) -> H {
        let (app, data, admin, _policy) = fastmock_server::build(cfg, prom()).await.unwrap();
        let dl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let aa = al.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(dl, data).await.unwrap() });
        tokio::spawn(async move { axum::serve(al, admin).await.unwrap() });
        app.ready.store(true, Ordering::SeqCst);
        let (stop, _) = watch::channel(false);
        H {
            app,
            admin: format!("http://{aa}"),
            http: reqwest::Client::new(),
            _stop: stop,
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

    async fn put(&self, token: &str, query: &str, body: Value) -> (u16, Value) {
        self.call(
            Method::PUT,
            &format!("/capture/sources/{}{query}", Uuid::now_v7()),
            token,
            Some(body),
        )
        .await
    }

    async fn mcp(&self, token: &str, method: &str, params: Value) -> Value {
        let r = self
            .http
            .post(format!("{}/mcp", self.admin))
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-06-18")
            .bearer_auth(token)
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let text = r.text().await.unwrap();
        let body = text
            .lines()
            .find_map(|l| l.strip_prefix("data:"))
            .map(|d| d.trim().to_string())
            .unwrap_or(text);
        serde_json::from_str::<Value>(&body).unwrap()["result"].clone()
    }

    async fn tool(&self, token: &str, name: &str, args: Value) -> Value {
        self.mcp(
            token,
            "tools/call",
            json!({"name": name, "arguments": args}),
        )
        .await
    }

    async fn tool_names(&self, token: &str) -> Vec<String> {
        self.mcp(token, "tools/list", json!({})).await["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }
}

fn kafka(topic: &str) -> Value {
    json!({"name": format!("k-{}", Uuid::new_v4().simple()), "protocol": "kafka", "topics": [topic]})
}

fn area<'a>(v: &'a Value, space: Option<&str>) -> &'a Value {
    v["allowLists"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["space"].as_str() == space)
        .unwrap_or_else(|| panic!("no allow-list entry for {space:?}: {v}"))
}

#[tokio::test]
async fn space_allow_lists_narrow_and_never_widen() {
    for storage in storages().await {
        let h = H::start(config(storage)).await;

        let (st, v) = h.put(PAY, "", kafka("payments.orders")).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["space"], "payments");

        for topic in ["shared.events", "billing.invoices"] {
            let (st, v) = h.put(PAY, "", kafka(topic)).await;
            assert_eq!(st, 400, "{topic}: {v}");
            let e = v["error"].as_str().unwrap();
            assert!(
                e.contains("not in the capture allow-list of space payments")
                    && e.contains("kafkaTopics"),
                "{e}"
            );
        }

        let (st, v) = h.put(OPEN, "", kafka("audit.log")).await;
        assert_eq!(
            st, 400,
            "a space list of * cannot widen the server-wide list: {v}"
        );
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .contains("audit.log is not in FASTMOCK_CAPTURE_KAFKA_TOPICS"),
            "{v}"
        );
        let (st, v) = h.put(OPEN, "", kafka("shared.events")).await;
        assert_eq!(st, 200, "{v}");
        let (st, v) = h.put(CRM, "", kafka("shared.events")).await;
        assert_eq!(
            st, 200,
            "a space without lists keeps the server-wide ones: {v}"
        );
        let (st, v) = h.put(CRM, "", kafka("audit.log")).await;
        assert_eq!(st, 400, "{v}");

        let (st, v) = h
            .put(
                PAY,
                "",
                json!({"name": "devices", "protocol": "mqtt", "topics": ["devices/+/state"]}),
            )
            .await;
        assert_eq!(st, 200, "the space sets no MQTT list: {v}");
        let (st, v) = h
            .put(
                PAY,
                "",
                json!({"name": "bills", "protocol": "amqp", "exchange": "billing"}),
            )
            .await;
        assert_eq!(st, 400, "{v}");
        assert!(
            v["error"].as_str().unwrap().contains("amqpExchanges"),
            "{v}"
        );
        let (st, v) = h
            .put(
                PAY,
                "",
                json!({"name": "pays", "protocol": "amqp", "exchange": "payments"}),
            )
            .await;
        assert_eq!(st, 200, "{v}");

        let (st, v) = h.put(ADMIN, "?space=payments", kafka("billing.x")).await;
        assert_eq!(st, 400, "the space list binds global admins too: {v}");
        let (st, v) = h.put(ADMIN, "", kafka("billing.x")).await;
        assert_eq!(st, 200, "the global area has the server-wide list: {v}");
        let (st, v) = h.put(ADMIN, "", kafka("audit.log")).await;
        assert_eq!(st, 400, "{v}");

        let (st, mine) = h.call(Method::GET, "/capture/sources", PAY, None).await;
        assert_eq!(st, 200, "{mine}");
        assert_eq!(
            mine["allowLists"],
            json!([{"space": "payments", "kafkaTopics": ["payments.*"], "mqttTopics": ["*"], "amqpExchanges": ["payments"]}])
        );
        assert!(
            mine["sources"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["space"] == "payments"),
            "{mine}"
        );
        let (_, all) = h.call(Method::GET, "/capture/sources", ADMIN, None).await;
        assert_eq!(all["allowLists"].as_array().unwrap().len(), 5, "{all}");
        let server = json!(["payments.*", "billing.*", "shared.*"]);
        assert_eq!(area(&all, None)["kafkaTopics"], server);
        assert_eq!(area(&all, Some("open"))["kafkaTopics"], server);
        assert_eq!(area(&all, Some("crm"))["kafkaTopics"], server);
        assert_eq!(
            area(&all, Some("billing"))["kafkaTopics"],
            json!(["billing.*"])
        );
        assert_eq!(
            area(&all, Some("billing"))["amqpExchanges"],
            json!(["payments", "billing"])
        );

        let (_, spaces) = h.call(Method::GET, "/spaces", PAY, None).await;
        assert_eq!(
            spaces["spaces"],
            json!([{
                "name": "payments", "pathPrefixes": [], "hosts": [], "mailDomains": [],
                "capture": {"kafkaTopics": ["payments.*"], "amqpExchanges": ["payments"]},
                "permissions": ["messages:read", "messages:write"]
            }])
        );
        let (_, access) = h.call(Method::GET, "/access", ADMIN, None).await;
        let crm = access["spaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "crm")
            .unwrap();
        assert!(crm.get("capture").is_none(), "{crm}");
        let open = access["spaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "open")
            .unwrap();
        assert_eq!(open["capture"], json!({"kafkaTopics": ["*"]}));
    }
}

#[tokio::test]
async fn capture_sources_do_not_cross_spaces() {
    for storage in storages().await {
        let h = H::start(config(storage)).await;
        let id = Uuid::now_v7();
        let (st, v) = h
            .call(
                Method::PUT,
                &format!("/capture/sources/{id}"),
                BILL,
                Some(json!({"name": "invoices", "protocol": "kafka", "topics": ["billing.invoices"]})),
            )
            .await;
        assert_eq!(st, 200, "{v}");

        let (st, v) = h
            .call(
                Method::PUT,
                &format!("/capture/sources/{id}"),
                PAY,
                Some(json!({"name": "invoices", "protocol": "kafka", "topics": ["billing.invoices"]})),
            )
            .await;
        assert_eq!(st, 400, "the allow-list of the caller's space applies: {v}");
        let (st, v) = h
            .call(
                Method::PUT,
                &format!("/capture/sources/{id}"),
                PAY,
                Some(json!({"name": "invoices", "protocol": "kafka", "topics": ["payments.x"]})),
            )
            .await;
        assert_eq!(st, 404, "another space's source is not found: {v}");
        let (st, v) = h
            .call(
                Method::PUT,
                &format!("/capture/sources/{id}?space=billing"),
                PAY,
                Some(json!({"name": "invoices", "protocol": "kafka", "topics": ["billing.invoices"]})),
            )
            .await;
        assert!(st == 403 || st == 404, "{st} {v}");
        let (st, _) = h
            .call(Method::GET, &format!("/capture/sources/{id}"), PAY, None)
            .await;
        assert_eq!(st, 404);
        let (st, _) = h
            .call(Method::DELETE, &format!("/capture/sources/{id}"), PAY, None)
            .await;
        assert_eq!(st, 404);
        let (_, v) = h
            .call(Method::GET, &format!("/capture/sources/{id}"), ADMIN, None)
            .await;
        assert_eq!(v["space"], "billing");
        assert_eq!(v["topics"], json!(["billing.invoices"]));

        let (_, list) = h.call(Method::GET, "/capture/sources", PAY, None).await;
        assert!(list["sources"].as_array().unwrap().is_empty(), "{list}");
        let (st, _) = h
            .call(Method::GET, "/capture/sources?space=billing", PAY, None)
            .await;
        assert!(st == 403 || st == 404, "{st}");
    }
}

#[tokio::test]
async fn mcp_tools_honour_the_space_allow_lists() {
    let h = H::start(config(StorageKind::Memory)).await;
    let names = h.tool_names(PAY).await;
    for t in [
        "list_capture_sources",
        "put_capture_source",
        "find_messages",
    ] {
        assert!(names.contains(&t.to_string()), "{names:?}");
    }
    let viewer = h.tool_names(VIEWER).await;
    assert!(viewer.contains(&"list_capture_sources".to_string()));
    assert!(
        !viewer.contains(&"put_capture_source".to_string()),
        "a viewer cannot create sources: {viewer:?}"
    );

    let r = h
        .tool(
            PAY,
            "put_capture_source",
            json!({"source": kafka("billing.invoices")}),
        )
        .await;
    assert_eq!(r["isError"], true, "{r}");
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("400") && text.contains("space payments"),
        "{text}"
    );

    let r = h
        .tool(
            PAY,
            "put_capture_source",
            json!({"source": kafka("billing.invoices"), "space": "billing"}),
        )
        .await;
    assert_eq!(r["isError"], true, "{r}");

    let r = h
        .tool(
            PAY,
            "put_capture_source",
            json!({"source": kafka("payments.orders")}),
        )
        .await;
    assert_ne!(r["isError"], true, "{r}");
    assert_eq!(r["structuredContent"]["space"], "payments");

    let r = h.tool(PAY, "list_capture_sources", json!({})).await;
    assert_ne!(r["isError"], true, "{r}");
    let v = &r["structuredContent"];
    assert_eq!(v["sources"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(
        v["allowLists"],
        json!([{"space": "payments", "kafkaTopics": ["payments.*"], "mqttTopics": ["*"], "amqpExchanges": ["payments"]}])
    );
    let r = h
        .tool(PAY, "list_capture_sources", json!({"space": "billing"}))
        .await;
    assert_eq!(r["isError"], true, "{r}");

    let (_, audit) = h.call(Method::GET, "/audit?limit=50", ADMIN, None).await;
    assert!(
        audit.to_string().contains("capture.source.put"),
        "MCP writes are audited: {audit}"
    );
}

#[tokio::test]
async fn sources_outside_a_space_list_do_not_start() {
    let mut cfg = config(StorageKind::Memory);
    cfg.capture.sources = fastmock_server::broker_settings::capture_settings(&|n| match n {
        "CAPTURE_ENABLED" => Some("true".into()),
        "CAPTURE_SOURCES" => Some(
            r#"[{"name": "leak", "space": "payments", "protocol": "kafka", "topics": ["billing.invoices"]}]"#
                .into(),
        ),
        _ => None,
    })
    .unwrap()
    .sources;
    let Err(e) = fastmock_server::build(cfg, prom()).await else {
        panic!("a configured source outside its space's list must stop the start");
    };
    let e = format!("{e:#}");
    assert!(
        e.contains("capture source leak") && e.contains("space payments"),
        "{e}"
    );

    for (doc, want) in [
        (
            "spaces:\n  - name: a\n    capture:\n      kafkaTopics: [\"pay*ments\"]\n",
            "capture.kafkaTopics",
        ),
        (
            "spaces:\n  - name: a\n    capture:\n      kafka: [x]\n",
            "unknown field",
        ),
        (
            "spaces:\n  - name: a\n    capture:\n      mqttTopics: [\"\"]\n",
            "capture.mqttTopics",
        ),
    ] {
        let mut cfg = config(StorageKind::Memory);
        cfg.access_file = Some(write_access(doc));
        let Err(e) = fastmock_server::build(cfg, prom()).await else {
            panic!("{doc} must be refused");
        };
        let e = format!("{e:#}");
        assert!(e.contains(want) && e.contains("spaces[0]"), "{e}");
    }

    let h = H::start(config(StorageKind::Memory)).await;
    let id = Uuid::now_v7();
    h.app
        .stores
        .data
        .put(
            fastmock_server::capture::NAMESPACE,
            &id.to_string(),
            &json!({"id": id, "name": "stale", "space": "payments", "protocol": "kafka", "topics": ["billing.invoices"]}),
        )
        .await
        .unwrap();
    let (stop, rx) = watch::channel(false);
    tokio::spawn(fastmock_server::capture::runtime::run_with(
        h.app.clone(),
        rx,
        fastmock_server::capture::runtime::RuntimeOptions {
            lease_ttl: Duration::from_secs(3),
            renew_every: Duration::from_millis(300),
            tick: Duration::from_millis(50),
            reload_every: Duration::from_millis(200),
        },
    ));
    let mut last = Value::Null;
    for _ in 0..100 {
        let (_, v) = h
            .call(Method::GET, &format!("/capture/sources/{id}"), ADMIN, None)
            .await;
        if v["status"]["state"] == "error" {
            last = v;
            break;
        }
        last = v;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(last["status"]["state"], "error", "{last}");
    assert!(
        last["status"]["lastError"]
            .as_str()
            .unwrap()
            .contains("capture allow-list of space payments"),
        "{last}"
    );
    let _ = stop.send(true);
}
