//! AMQP over TLS with client certificates and SASL EXTERNAL against RabbitMQ (`rabbitmq_auth_mechanism_ssl`):
//! publishing from stubs and message capture, with the settings read the way `FASTMOCK_AMQP_*` are.
//!
//! Runs when `FASTMOCK_TEST_AMQPS` (an `amqps://` URL) and `FASTMOCK_TEST_AMQPS_TLS_DIR` are set; the
//! directory holds what `.github/rabbitmq/prepare.sh` generates: `ca.pem`, `client.pem`/`client.key`
//! (a user that exists in RabbitMQ) and `unknown.pem`/`unknown.key` (signed by the same CA, no such user).
//! The test also generates a certificate from a CA that RabbitMQ does not trust.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::Duration;

use fastmock_brokers::amqp::{AmqpMessage, AmqpPublisher, AmqpSettings, ExchangeDeclaration};
use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use reqwest::Method;
use serde_json::{Value, json};
use tokio::sync::watch;
use uuid::Uuid;

const ADMIN: &str = "fm_admin_token_for_amqp_external_tests_0123";

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

fn env() -> Option<(String, PathBuf)> {
    let url = std::env::var("FASTMOCK_TEST_AMQPS").ok()?;
    let dir = std::env::var("FASTMOCK_TEST_AMQPS_TLS_DIR").ok()?;
    Some((url, PathBuf::from(dir)))
}

/// The settings as `FASTMOCK_AMQP_*` would give them.
fn settings(url: &str, ca: &Path, cert: &Path, key: &Path) -> anyhow::Result<AmqpSettings> {
    let vars = [
        ("AMQP_URL", url.to_string()),
        ("AMQP_SASL_MECHANISM", "EXTERNAL".to_string()),
        ("AMQP_TLS_CA_FILE", ca.display().to_string()),
        ("AMQP_TLS_CERT_FILE", cert.display().to_string()),
        ("AMQP_TLS_KEY_FILE", key.display().to_string()),
        ("AMQP_DECLARE_EXCHANGES", "true".to_string()),
    ];
    let get = |n: &str| vars.iter().find(|(k, _)| *k == n).map(|(_, v)| v.clone());
    Ok(fastmock_server::broker_settings::amqp_settings(&get)?.unwrap())
}

struct H {
    admin: String,
    data: String,
    http: reqwest::Client,
    _stop: watch::Sender<bool>,
}

impl H {
    async fn start(amqp: AmqpSettings) -> H {
        let mut cfg = Config::for_tests(
            StorageKind::Memory,
            vec![format!("ops:admin:{}", hash_token(ADMIN))],
        );
        cfg.plugins = Some(vec![]);
        cfg.amqp = Some(amqp);
        cfg.amqp_allowed_exchanges = Some(vec!["fm-ext-*".into()]);
        cfg.capture.enabled = true;
        cfg.capture.amqp_exchanges = Some(vec!["fm-ext-*".into()]);
        let (app, data, admin, _policy) = fastmock_server::build(cfg, prom()).await.unwrap();
        let dl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (da, aa) = (dl.local_addr().unwrap(), al.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(dl, data).await.unwrap() });
        tokio::spawn(async move { axum::serve(al, admin).await.unwrap() });
        app.ready.store(true, Ordering::SeqCst);
        let (stop, rx) = watch::channel(false);
        tokio::spawn(fastmock_server::capture::runtime::run_with(
            app.clone(),
            rx,
            fastmock_server::capture::runtime::RuntimeOptions {
                lease_ttl: Duration::from_secs(3),
                renew_every: Duration::from_millis(300),
                tick: Duration::from_millis(50),
                reload_every: Duration::from_millis(200),
            },
        ));
        H {
            admin: format!("http://{aa}/__admin"),
            data: format!("http://{da}"),
            http: reqwest::Client::new(),
            _stop: stop,
        }
    }

    async fn call(&self, m: Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut rb = self
            .http
            .request(m, format!("{}{path}", self.admin))
            .bearer_auth(ADMIN);
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

    /// The source once its state is no longer `stopped` or `connecting`.
    async fn settled(&self, id: &str) -> Value {
        let mut last = Value::Null;
        for _ in 0..300 {
            let (_, v) = self
                .call(Method::GET, &format!("/capture/sources/{id}"), None)
                .await;
            let state = v["status"]["state"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            last = v;
            if state == "running" || state == "error" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        last
    }
}

async fn declare(settings: &AmqpSettings, exchange: &str) -> anyhow::Result<()> {
    AmqpPublisher::new(settings.clone(), "test setup")
        .publish(
            &AmqpMessage {
                exchange: exchange.to_string(),
                routing_key: "init".into(),
                ..Default::default()
            },
            Some(&ExchangeDeclaration::default()),
        )
        .await
}

#[tokio::test]
async fn external_auth_publishes_and_captures_over_mtls() {
    let Some((url, dir)) = env() else {
        eprintln!("FASTMOCK_TEST_AMQPS / FASTMOCK_TEST_AMQPS_TLS_DIR not set: skipped");
        return;
    };
    let _ = prom();
    let ca = dir.join("ca.pem");
    let s = settings(&url, &ca, &dir.join("client.pem"), &dir.join("client.key")).unwrap();
    assert!(s.credentials.is_none());
    let exchange = format!("fm-ext-{}", Uuid::new_v4().simple());
    declare(&s, &exchange)
        .await
        .expect("publishing with SASL EXTERNAL and a client certificate");

    let h = H::start(s).await;
    let id = Uuid::now_v7();
    let (st, v) = h
        .call(
            Method::PUT,
            &format!("/capture/sources/{id}"),
            Some(json!({"name": "orders", "protocol": "amqp", "exchange": exchange, "bindingKeys": ["order.#"]})),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    let v = h.settled(&id.to_string()).await;
    assert_eq!(v["status"]["state"], "running", "{v}");

    let (st, v) = h
        .call(
            Method::POST,
            "/stubs",
            Some(json!({
                "request": {"method": "POST", "urlPathTemplate": "/orders/{id}"},
                "response": {"status": 201},
                "messages": [{"protocol": "amqp", "exchange": exchange, "routingKey": "order.created",
                              "correlationId": "{{request.pathParams.id}}",
                              "jsonValue": {"id": "{{request.pathParams.id}}"}}]
            })),
        )
        .await;
    assert_eq!(st, 201, "{v}");
    let r = h
        .http
        .post(format!("{}/orders/x-1", h.data))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let (st, v) = h
        .call(
            Method::POST,
            "/capture/messages/wait",
            Some(json!({"match": {"exchange": {"equalTo": exchange}, "properties": {"correlationId": {"equalTo": "x-1"}}},
                        "lookbackMs": 10000, "timeoutMs": 15000})),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    let m = &v["messages"][0];
    assert_eq!(m["topic"], "order.created");
    assert_eq!(m["preview"], r#"{"id":"x-1"}"#);
    let (_, list) = h
        .call(Method::GET, &format!("/capture/sources/{id}"), None)
        .await;
    assert_eq!(list["status"]["connects"], 1, "{list}");
}

#[tokio::test]
async fn external_auth_failures_are_reported_without_details() {
    let Some((url, dir)) = env() else {
        eprintln!("FASTMOCK_TEST_AMQPS / FASTMOCK_TEST_AMQPS_TLS_DIR not set: skipped");
        return;
    };
    let _ = prom();
    let ca = dir.join("ca.pem");
    let exchange = format!("fm-ext-{}", Uuid::new_v4().simple());

    let unknown = settings(
        &url,
        &ca,
        &dir.join("unknown.pem"),
        &dir.join("unknown.key"),
    )
    .unwrap();
    let e = declare(&unknown, &exchange)
        .await
        .expect_err("a certificate whose name is no RabbitMQ user must be refused");
    let text = format!("{e:#}");
    assert!(!text.contains("unknown.key"), "{text}");

    let tmp = std::env::temp_dir().join(format!("fm-amqps-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).unwrap();
    let mut other_ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    other_ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    other_ca
        .distinguished_name
        .push(rcgen::DnType::CommonName, "untrusted test CA");
    let issuer = rcgen::Issuer::new(other_ca, rcgen::KeyPair::generate().unwrap());
    let mut leaf = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    leaf.distinguished_name
        .push(rcgen::DnType::CommonName, "fastmock-capture");
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let leaf_cert = leaf.signed_by(&leaf_key, &issuer).unwrap();
    std::fs::write(tmp.join("untrusted.pem"), leaf_cert.pem()).unwrap();
    std::fs::write(tmp.join("untrusted.key"), leaf_key.serialize_pem()).unwrap();
    let untrusted = settings(
        &url,
        &ca,
        &tmp.join("untrusted.pem"),
        &tmp.join("untrusted.key"),
    )
    .unwrap();
    declare(&untrusted, &exchange)
        .await
        .expect_err("a certificate from a CA RabbitMQ does not trust must be refused");

    let good = settings(&url, &ca, &dir.join("client.pem"), &dir.join("client.key")).unwrap();
    declare(&good, &exchange).await.unwrap();

    for (s, want) in [
        (unknown, "refused the credentials"),
        (
            untrusted,
            "TLS handshake failed: the broker refused FastMock's client certificate",
        ),
    ] {
        let h = H::start(s).await;
        let id = Uuid::now_v7();
        let (st, v) = h
            .call(
                Method::PUT,
                &format!("/capture/sources/{id}"),
                Some(json!({"name": "orders", "protocol": "amqp", "exchange": exchange})),
            )
            .await;
        assert_eq!(st, 200, "{v}");
        let v = h.settled(&id.to_string()).await;
        assert_eq!(v["status"]["state"], "error", "{v}");
        let e = v["status"]["lastError"].as_str().unwrap();
        assert!(e.contains(want), "{want}: {e}");
        assert!(!e.contains("fastmock-") && !e.contains(".pem"), "{e}");
    }
}
