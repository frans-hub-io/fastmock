use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use fastmock_kafka::{
    BasicAuth, ClientCertFiles, KafkaPublisher, KafkaSecurity, KafkaSettings, RegistryOptions,
    SaslMechanism, SaslSettings, SchemaRegistry, SecurityProtocol, StartFrom, TlsFiles,
};
use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind, TlsSettings};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use serde_json::{Value, json};
use tokio::sync::watch;

const EDITOR: &str = "fm_editor_token_for_kafka_sec_tests_01";

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn credentials(kind: &str, mechanism: SaslMechanism) -> Option<SaslSettings> {
    Some(SaslSettings {
        mechanism,
        username: var(&format!("FASTMOCK_TEST_KAFKA_{kind}_USERNAME"))?,
        password: var(&format!("FASTMOCK_TEST_KAFKA_{kind}_PASSWORD"))?,
    })
}

fn tls(client_cert: bool) -> Option<TlsFiles> {
    let ca = var("FASTMOCK_TEST_KAFKA_TLS_CA_FILE")?;
    let client_cert = if client_cert {
        Some(ClientCertFiles {
            cert_file: var("FASTMOCK_TEST_KAFKA_TLS_CERT_FILE")?.into(),
            key_file: var("FASTMOCK_TEST_KAFKA_TLS_KEY_FILE")?.into(),
        })
    } else {
        None
    };
    Some(TlsFiles {
        ca_file: Some(ca.into()),
        client_cert,
    })
}

struct Server {
    data: String,
    admin: String,
    http: reqwest::Client,
    _shutdown: watch::Sender<bool>,
}

impl Server {
    async fn start(cfg: Config) -> Server {
        let (app, data, admin, policy) = fastmock_server::build(cfg, prom()).await.unwrap();
        let (tx, rx) = watch::channel(false);
        let dl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (da, aa) = (dl.local_addr().unwrap(), al.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(dl, data).await.unwrap() });
        tokio::spawn(async move { axum::serve(al, admin).await.unwrap() });
        tokio::spawn(fastmock_server::outbox::run(app.clone(), policy, rx));
        app.ready.store(true, Ordering::SeqCst);
        Server {
            data: format!("http://{da}"),
            admin: format!("http://{aa}"),
            http: reqwest::Client::new(),
            _shutdown: tx,
        }
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut rb = self
            .http
            .request(method, format!("{}/__admin{path}", self.admin))
            .bearer_auth(EDITOR);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let r = rb.send().await.unwrap();
        let st = r.status().as_u16();
        (st, r.json().await.unwrap_or(Value::Null))
    }

    async fn wait_run(&self, id: &str) -> Value {
        for _ in 0..200 {
            let (st, v) = self
                .call(reqwest::Method::GET, &format!("/contract-tests/{id}"), None)
                .await;
            assert_eq!(st, 200, "{v}");
            if v["status"] != "running" && v["status"] != "queued" {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("contract test run {id} did not finish");
    }
}

async fn next_value(
    client: &KafkaPublisher,
    cursor: &mut fastmock_kafka::TopicCursor,
) -> (Option<String>, Value) {
    let got = client
        .consume(cursor, 1, 1_000_000, Duration::from_secs(5))
        .await
        .unwrap();
    let r = got.first().expect("a record arrives");
    (
        r.key.clone().map(|k| String::from_utf8(k).unwrap()),
        serde_json::from_slice(r.value.as_ref().unwrap()).unwrap(),
    )
}

/// Publishes through a stub and an async mock, reads both records back with the same
/// settings, and runs an AsyncAPI contract test over the secured connection.
async fn round_trip(name: &str, bootstrap: String, security: KafkaSecurity) {
    security.validate().unwrap();
    let topic = format!("fm-sec-{name}-{}", uuid::Uuid::new_v4().simple());
    let client = KafkaPublisher::connect(&KafkaSettings {
        bootstrap: vec![bootstrap.clone()],
        client_id: "fastmock-test".into(),
        security: security.clone(),
    })
    .await
    .unwrap();
    client
        .ensure_topics(std::slice::from_ref(&topic), 1, 1)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut cfg = Config::for_tests(
        StorageKind::Memory,
        vec![format!("ci:editor:{}", hash_token(EDITOR))],
    );
    cfg.kafka_bootstrap = Some(vec![bootstrap]);
    cfg.kafka_security = security;
    cfg.kafka_allowed_topics = Some(vec!["fm-sec-*".into()]);
    cfg.kafka_contract_topics = vec!["fm-sec-*".into()];
    let s = Server::start(cfg).await;
    let mut cursor = client.cursor(&topic, StartFrom::Latest).await.unwrap();

    let (st, v) = s
        .call(
            reqwest::Method::POST,
            "/stubs",
            Some(json!({"request": {"method": "POST", "urlPath": "/orders"}, "response": {"status": 202},
                "messages": [{"topic": topic, "key": "{{request.json.id}}", "jsonValue": {"via": "stub", "id": "{{request.json.id}}"}}]})),
        )
        .await;
    assert_eq!(st, 201, "{v}");
    let r = s
        .http
        .post(format!("{}/orders", s.data))
        .json(&json!({"id": "A-1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    assert_eq!(
        next_value(&client, &mut cursor).await,
        (Some("A-1".into()), json!({"via": "stub", "id": "A-1"}))
    );

    let id = uuid::Uuid::now_v7();
    let (st, v) = s
        .call(
            reqwest::Method::PUT,
            &format!("/async-mocks/{id}"),
            Some(json!({"name": "secure", "channel": topic, "enabled": false,
                "messages": [{"payload": {"via": "async", "id": "B-{{sequence}}"}}]})),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    let (st, v) = s
        .call(
            reqwest::Method::POST,
            &format!("/async-mocks/{id}/trigger"),
            None,
        )
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        next_value(&client, &mut cursor).await.1,
        json!({"via": "async", "id": "B-1"})
    );

    let spec = json!({"asyncapi": "2.6.0", "info": {"title": "orders", "version": "1"},
        "channels": {topic.clone(): {"bindings": {"kafka": {}}, "subscribe": {"operationId": "orderEvents",
            "message": {"payload": {"type": "object", "required": ["via", "id"],
                "properties": {"via": {"type": "string"}, "id": {"type": "string"}}}}}}}});
    let (st, v) = s
        .call(
            reqwest::Method::POST,
            "/contract-tests",
            Some(json!({"spec": spec, "fromBeginning": true, "messageCount": 2, "waitMs": 5000})),
        )
        .await;
    assert_eq!(st, 202, "{v}");
    let run = s.wait_run(v["id"].as_str().unwrap()).await;
    assert_eq!(run["status"], "passed", "{run}");
    assert_eq!(
        run["results"][0]["messages"].as_array().unwrap().len(),
        2,
        "{run}"
    );
}

#[derive(Clone, Default)]
struct LogBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Connects with settings the broker rejects; returns the error and everything logged
/// meanwhile at trace level (rskafka logs each failed attempt and keeps retrying).
async fn rejected(bootstrap: &str, security: KafkaSecurity) -> (String, String) {
    let buf = LogBuf::default();
    let w = buf.clone();
    let sub = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || w.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(sub);
    let r = tokio::time::timeout(
        Duration::from_secs(4),
        KafkaPublisher::connect(&KafkaSettings {
            bootstrap: vec![bootstrap.to_string()],
            client_id: "fastmock-test".into(),
            security,
        }),
    )
    .await;
    let err = match r {
        Ok(Ok(_)) => panic!("connecting must fail"),
        Ok(Err(e)) => format!("{e:#} {e:?}"),
        Err(_) => "timed out".into(),
    };
    let logs = String::from_utf8_lossy(&buf.0.lock().unwrap()).to_string();
    (err, logs)
}

#[tokio::test]
async fn sasl_plaintext_with_plain() {
    let (Some(bootstrap), Some(sasl)) = (
        var("FASTMOCK_TEST_KAFKA_SASL_PLAINTEXT"),
        credentials("PLAIN", SaslMechanism::Plain),
    ) else {
        eprintln!(
            "FASTMOCK_TEST_KAFKA_SASL_PLAINTEXT / _PLAIN_USERNAME / _PLAIN_PASSWORD not set: skipped"
        );
        return;
    };
    let security = KafkaSecurity {
        protocol: SecurityProtocol::SaslPlaintext,
        sasl: Some(sasl.clone()),
        tls: TlsFiles::default(),
    };
    round_trip("plain", bootstrap.clone(), security).await;

    let wrong = format!("{}-wrong", sasl.password);
    let (e, logs) = rejected(
        &bootstrap,
        KafkaSecurity {
            protocol: SecurityProtocol::SaslPlaintext,
            sasl: Some(SaslSettings {
                password: wrong.clone(),
                ..sasl
            }),
            tls: TlsFiles::default(),
        },
    )
    .await;
    assert!(logs.contains("SaslAuthenticationFailed"), "{e}\n{logs}");
    assert!(!e.contains(&wrong), "the password is not in errors: {e}");
    assert!(!logs.contains(&wrong), "the password is not logged: {logs}");
}

#[tokio::test]
async fn sasl_plaintext_with_scram_sha_256() {
    let (Some(bootstrap), Some(sasl)) = (
        var("FASTMOCK_TEST_KAFKA_SASL_PLAINTEXT"),
        credentials("SCRAM", SaslMechanism::ScramSha256),
    ) else {
        eprintln!(
            "FASTMOCK_TEST_KAFKA_SASL_PLAINTEXT / _SCRAM_USERNAME / _SCRAM_PASSWORD not set: skipped"
        );
        return;
    };
    round_trip(
        "scram256",
        bootstrap.clone(),
        KafkaSecurity {
            protocol: SecurityProtocol::SaslPlaintext,
            sasl: Some(sasl.clone()),
            tls: TlsFiles::default(),
        },
    )
    .await;
    let wrong = format!("{}-wrong", sasl.password);
    let (e, logs) = rejected(
        &bootstrap,
        KafkaSecurity {
            protocol: SecurityProtocol::SaslPlaintext,
            sasl: Some(SaslSettings {
                password: wrong.clone(),
                ..sasl
            }),
            tls: TlsFiles::default(),
        },
    )
    .await;
    assert!(logs.contains("SaslAuthenticationFailed"), "{e}\n{logs}");
    assert!(!e.contains(&wrong), "the password is not in errors: {e}");
    assert!(!logs.contains(&wrong), "the password is not logged: {logs}");
}

#[tokio::test]
async fn sasl_ssl_with_scram_sha_512() {
    let (Some(bootstrap), Some(sasl), Some(tls)) = (
        var("FASTMOCK_TEST_KAFKA_SASL_SSL"),
        credentials("SCRAM", SaslMechanism::ScramSha512),
        tls(false),
    ) else {
        eprintln!(
            "FASTMOCK_TEST_KAFKA_SASL_SSL / _SCRAM_USERNAME / _SCRAM_PASSWORD / _TLS_CA_FILE not set: skipped"
        );
        return;
    };
    round_trip(
        "scram512",
        bootstrap.clone(),
        KafkaSecurity {
            protocol: SecurityProtocol::SaslSsl,
            sasl: Some(sasl.clone()),
            tls,
        },
    )
    .await;
    let (e, logs) = rejected(
        &bootstrap,
        KafkaSecurity {
            protocol: SecurityProtocol::SaslSsl,
            sasl: Some(sasl),
            tls: TlsFiles::default(),
        },
    )
    .await;
    assert!(
        logs.contains("UnknownIssuer"),
        "without the CA the broker certificate is not trusted: {e}\n{logs}"
    );
}

#[tokio::test]
async fn ssl_with_client_certificate() {
    let (Some(bootstrap), Some(mtls), Some(server_only)) =
        (var("FASTMOCK_TEST_KAFKA_SSL"), tls(true), tls(false))
    else {
        eprintln!(
            "FASTMOCK_TEST_KAFKA_SSL / _TLS_CA_FILE / _TLS_CERT_FILE / _TLS_KEY_FILE not set: skipped"
        );
        return;
    };
    round_trip(
        "mtls",
        bootstrap.clone(),
        KafkaSecurity {
            protocol: SecurityProtocol::Ssl,
            sasl: None,
            tls: mtls,
        },
    )
    .await;
    let (e, logs) = rejected(
        &bootstrap,
        KafkaSecurity {
            protocol: SecurityProtocol::Ssl,
            sasl: None,
            tls: server_only,
        },
    )
    .await;
    assert!(
        logs.contains("CertificateRequired"),
        "the broker requires a client certificate: {e}\n{logs}"
    );
}

type Seen = Arc<Mutex<Vec<Option<String>>>>;

async fn registry_handler(State(seen): State<Seen>, headers: HeaderMap) -> (StatusCode, String) {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let ok = auth.as_deref() == Some("Basic c3I6c3ItcGFzcy0xMjM0");
    seen.lock().unwrap().push(auth);
    if ok {
        (StatusCode::OK, r#"{"id": 42}"#.into())
    } else {
        (
            StatusCode::UNAUTHORIZED,
            r#"{"error_code": 401, "message": "Unauthorized"}"#.into(),
        )
    }
}

fn write_pki(dir: &std::path::Path) -> (PathBuf, PathBuf, PathBuf) {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(vec![]).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_pem = ca.self_signed(&ca_key).unwrap().pem();
    let issuer = Issuer::new(ca, ca_key);
    let sk = KeyPair::generate().unwrap();
    let server = CertificateParams::new(vec!["localhost".into()])
        .unwrap()
        .signed_by(&sk, &issuer)
        .unwrap();
    std::fs::write(dir.join("ca.pem"), ca_pem).unwrap();
    std::fs::write(dir.join("server.pem"), server.pem()).unwrap();
    std::fs::write(dir.join("server.key"), sk.serialize_pem()).unwrap();
    (
        dir.join("ca.pem"),
        dir.join("server.pem"),
        dir.join("server.key"),
    )
}

#[tokio::test]
async fn schema_registry_uses_basic_auth_and_the_kafka_ca() {
    let _ = prom();
    let dir = std::env::temp_dir().join(format!("fm-sr-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let (ca, cert, key) = write_pki(&dir);
    let seen: Seen = Arc::default();
    let router = Router::new()
        .route("/subjects/{subject}/versions", post(registry_handler))
        .with_state(seen.clone());
    let tls = fastmock_server::tls::server_config(&TlsSettings {
        cert_file: cert,
        key_file: key,
        client_ca_file: None,
        client_auth_required: false,
    })
    .unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let (_stop, rx) = watch::channel(false);
    tokio::spawn(fastmock_server::tls::serve(
        l,
        router,
        Some(tls),
        rx,
        Duration::from_secs(5),
        Duration::from_secs(1),
    ));
    let url = format!("https://localhost:{port}");
    let auth = BasicAuth {
        username: "sr".into(),
        password: "sr-pass-1234".into(),
    };

    let r = SchemaRegistry::with_options(
        &url,
        &RegistryOptions {
            ca_file: Some(ca.clone()),
            auth: Some(auth.clone()),
        },
    )
    .unwrap();
    assert_eq!(r.register("orders-value", "\"string\"").await.unwrap(), 42);

    let anonymous = SchemaRegistry::with_options(
        &url,
        &RegistryOptions {
            ca_file: Some(ca.clone()),
            auth: None,
        },
    )
    .unwrap();
    let e = format!(
        "{:#}",
        anonymous
            .register("orders-value", "\"string\"")
            .await
            .unwrap_err()
    );
    assert!(e.contains("401"), "{e}");

    let untrusted = SchemaRegistry::with_options(
        &url,
        &RegistryOptions {
            ca_file: None,
            auth: Some(auth),
        },
    )
    .unwrap();
    let e = format!(
        "{:#}",
        untrusted
            .register("orders-value", "\"string\"")
            .await
            .unwrap_err()
    );
    assert!(e.contains("unreachable"), "{e}");
    assert!(!e.contains("sr-pass-1234"));

    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![Some("Basic c3I6c3ItcGFzcy0xMjM0".to_string()), None],
        "the untrusted connection never reached the handler"
    );

    let empty = dir.join("empty.pem");
    std::fs::write(&empty, "").unwrap();
    assert!(
        SchemaRegistry::with_options(
            &url,
            &RegistryOptions {
                ca_file: Some(empty),
                auth: None,
            },
        )
        .is_err()
    );
    let _ = std::fs::remove_dir_all(dir);
}
