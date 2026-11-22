//! Round trips against real brokers. They run when `FASTMOCK_TEST_MQTT` (an `mqtt://` URL of an MQTT 5
//! broker such as Mosquitto) and `FASTMOCK_TEST_AMQP` (an `amqp://` URL of RabbitMQ or LavinMQ, user
//! `guest`) are set, and are skipped otherwise. `FASTMOCK_TEST_MQTT311` (with `_USER` and `_PASSWORD`) points
//! the MQTT 3.1.1 test at another broker, for example LavinMQ.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use fastmock_brokers::amqp::{
    AmqpCapture, AmqpMessage, AmqpPublisher, AmqpSettings, ExchangeDeclaration,
};
use fastmock_brokers::mqtt::{MqttConnection, MqttPublish, MqttSettings, MqttVersion};
use fastmock_brokers::{Credentials, LinkStatus, Received};
use tokio::sync::{mpsc, watch};

fn init() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

async fn next(rx: &mut mpsc::Receiver<Received>) -> Received {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("no message within 10 s")
        .expect("channel closed")
}

async fn wait_connected(f: impl Fn() -> LinkStatus) {
    for _ in 0..100 {
        if f().connected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("not connected: {:?}", f());
}

async fn mqtt_round_trip(url: &str, version: MqttVersion, credentials: Option<Credentials>) {
    let mut s = MqttSettings::new(url).unwrap();
    s.version = version;
    s.credentials = credentials;
    let base = format!("fm-test/{}", rand_suffix());
    let (tx, mut rx) = mpsc::channel(16);
    let sub = MqttConnection::start(
        &s,
        s.client_id("cap"),
        vec![(format!("{base}/+/state"), 1)],
        Some(tx),
    )
    .unwrap();
    wait_connected(|| sub.status()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let publisher = MqttConnection::start(&s, s.client_id("pub"), vec![], None).unwrap();
    for qos in 0..=2u8 {
        publisher
            .publish(&MqttPublish {
                topic: format!("{base}/d{qos}/state"),
                payload: format!("{{\"qos\":{qos}}}").into_bytes(),
                qos,
                retain: false,
                user_properties: vec![("source".into(), "fastmock".into())],
                content_type: Some("application/json".into()),
                correlation_data: Some(b"corr-1".to_vec()),
                ..Default::default()
            })
            .await
            .unwrap();
        let m = next(&mut rx).await;
        assert_eq!(m.topic, format!("{base}/d{qos}/state"));
        assert_eq!(m.payload, format!("{{\"qos\":{qos}}}").into_bytes());
        if version == MqttVersion::V5 {
            assert_eq!(
                m.headers,
                vec![("source".to_string(), "fastmock".to_string())]
            );
            assert_eq!(m.content_type.as_deref(), Some("application/json"));
            assert_eq!(m.properties["correlationData"], "corr-1");
        }
    }
    publisher
        .publish(&MqttPublish {
            topic: format!("{base}/other/x"),
            payload: b"ignored".to_vec(),
            qos: 1,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .is_err(),
        "a topic outside the filter is not received"
    );
}

fn rand_suffix() -> String {
    format!("{:08x}", rand_u32())
}

fn rand_u32() -> u32 {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    n ^ std::process::id().rotate_left(7)
}

#[tokio::test]
async fn mqtt5_publish_and_subscribe() {
    init();
    let Ok(url) = std::env::var("FASTMOCK_TEST_MQTT") else {
        eprintln!("FASTMOCK_TEST_MQTT not set: skipped");
        return;
    };
    mqtt_round_trip(&url, MqttVersion::V5, None).await;
}

#[tokio::test]
async fn mqtt311_publish_and_subscribe() {
    init();
    let Ok(url) =
        std::env::var("FASTMOCK_TEST_MQTT311").or_else(|_| std::env::var("FASTMOCK_TEST_MQTT"))
    else {
        eprintln!("FASTMOCK_TEST_MQTT not set: skipped");
        return;
    };
    let credentials = std::env::var("FASTMOCK_TEST_MQTT311_USER")
        .ok()
        .map(|u| Credentials {
            username: u,
            password: std::env::var("FASTMOCK_TEST_MQTT311_PASSWORD").unwrap_or_default(),
        });
    mqtt_round_trip(&url, MqttVersion::V311, credentials).await;
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
async fn amqp_publish_and_capture() {
    init();
    let Ok(url) = std::env::var("FASTMOCK_TEST_AMQP") else {
        eprintln!("FASTMOCK_TEST_AMQP not set: skipped");
        return;
    };
    let s = amqp_settings(&url);
    let exchange = format!("fm-test-{}", rand_suffix());
    let publisher = AmqpPublisher::new(s.clone(), "fastmock test");
    let decl = ExchangeDeclaration {
        kind: "topic".into(),
        durable: false,
    };
    let msg = AmqpMessage {
        exchange: exchange.clone(),
        routing_key: "order.created".into(),
        payload: br#"{"id":1}"#.to_vec(),
        headers: vec![("x-event".into(), "created".into())],
        content_type: Some("application/json".into()),
        message_id: Some("m-1".into()),
        correlation_id: Some("c-1".into()),
        reply_to: Some("replies".into()),
        expiration: Some("60000".into()),
        persistent: true,
        ..Default::default()
    };
    let e = publisher.publish(&msg, None).await.unwrap_err();
    assert!(
        e.to_string().contains("FASTMOCK_AMQP_DECLARE_EXCHANGES"),
        "{e}"
    );
    publisher.publish(&msg, Some(&decl)).await.unwrap();

    let (tx, mut rx) = mpsc::channel(16);
    let status = Arc::new(Mutex::new(LinkStatus::default()));
    let (stop, stopped) = watch::channel(false);
    let task = tokio::spawn(fastmock_brokers::amqp::consume(
        s.clone(),
        AmqpCapture {
            exchange: exchange.clone(),
            binding_keys: vec!["order.*".into()],
            max_queue_length: 100,
            label: "test".into(),
        },
        tx,
        status.clone(),
        stopped,
    ));
    wait_connected(|| status.lock().unwrap().clone()).await;
    publisher.publish(&msg, Some(&decl)).await.unwrap();
    publisher
        .publish(
            &AmqpMessage {
                routing_key: "invoice.created".into(),
                ..msg.clone()
            },
            None,
        )
        .await
        .unwrap();
    let m = next(&mut rx).await;
    assert_eq!(m.topic, "order.created");
    assert_eq!(m.exchange.as_deref(), Some(exchange.as_str()));
    assert_eq!(m.payload, br#"{"id":1}"#.to_vec());
    assert_eq!(m.content_type.as_deref(), Some("application/json"));
    assert_eq!(m.properties["messageId"], "m-1");
    assert_eq!(m.properties["correlationId"], "c-1");
    assert_eq!(m.properties["replyTo"], "replies");
    assert_eq!(m.properties["expiration"], "60000");
    assert_eq!(m.properties["deliveryMode"], 2);
    assert_eq!(
        m.headers,
        vec![("x-event".to_string(), "created".to_string())]
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .is_err(),
        "a routing key outside the binding is not captured"
    );
    let unroutable = publisher
        .publish(
            &AmqpMessage {
                routing_key: "nobody.listens".into(),
                mandatory: true,
                ..msg.clone()
            },
            None,
        )
        .await
        .unwrap_err();
    assert!(
        unroutable.to_string().contains("unroutable"),
        "{unroutable}"
    );
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    publisher.close().await;
}

fn env_path(name: &str) -> Option<std::path::PathBuf> {
    std::env::var(name).ok().map(std::path::PathBuf::from)
}

/// `FASTMOCK_TEST_MQTT_MTLS` is an `mqtts://` listener that requires a client certificate issued by
/// `FASTMOCK_TEST_TLS_CA`; `FASTMOCK_TEST_TLS_CLIENT_CERT` and `_KEY` are such a certificate.
#[tokio::test]
async fn mqtt_over_mutual_tls() {
    init();
    let (Ok(url), Some(ca), Some(cert), Some(key)) = (
        std::env::var("FASTMOCK_TEST_MQTT_MTLS"),
        env_path("FASTMOCK_TEST_TLS_CA"),
        env_path("FASTMOCK_TEST_TLS_CLIENT_CERT"),
        env_path("FASTMOCK_TEST_TLS_CLIENT_KEY"),
    ) else {
        eprintln!("FASTMOCK_TEST_MQTT_MTLS and the TLS files not set: skipped");
        return;
    };
    let mut s = MqttSettings::new(&url).unwrap();
    s.tls_files = fastmock_brokers::TlsFiles {
        ca_file: Some(ca.clone()),
        client_cert: Some(fastmock_brokers::ClientCertFiles {
            cert_file: cert,
            key_file: key,
        }),
    };
    s.validate().unwrap();
    let topic = format!("fm-tls/{}", rand_suffix());
    let (tx, mut rx) = mpsc::channel(4);
    let sub =
        MqttConnection::start(&s, s.client_id("cap"), vec![(topic.clone(), 1)], Some(tx)).unwrap();
    wait_connected(|| sub.status()).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let p = MqttConnection::start(&s, s.client_id("pub"), vec![], None).unwrap();
    p.publish(&MqttPublish {
        topic: topic.clone(),
        payload: b"over tls".to_vec(),
        qos: 1,
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(next(&mut rx).await.payload, b"over tls");

    let mut no_cert = s.clone();
    no_cert.tls_files.client_cert = None;
    let c = MqttConnection::start(&no_cert, no_cert.client_id("x"), vec![], None).unwrap();
    assert!(
        c.publish(&MqttPublish {
            topic,
            payload: vec![],
            ..Default::default()
        })
        .await
        .is_err()
    );
    let mut no_ca = s.clone();
    no_ca.tls_files.ca_file = None;
    let c = MqttConnection::start(&no_ca, no_ca.client_id("x"), vec![], None).unwrap();
    let e = c
        .publish(&MqttPublish {
            topic: "a".into(),
            payload: vec![],
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(e.to_string().contains("UnknownIssuer"), "{e}");
}

/// `FASTMOCK_TEST_MQTT_AUTH` needs `FASTMOCK_TEST_MQTT_AUTH_USER` and `_PASSWORD`.
#[tokio::test]
async fn mqtt_with_a_password() {
    init();
    let (Ok(url), Ok(user), Ok(password)) = (
        std::env::var("FASTMOCK_TEST_MQTT_AUTH"),
        std::env::var("FASTMOCK_TEST_MQTT_AUTH_USER"),
        std::env::var("FASTMOCK_TEST_MQTT_AUTH_PASSWORD"),
    ) else {
        eprintln!("FASTMOCK_TEST_MQTT_AUTH not set: skipped");
        return;
    };
    let mut s = MqttSettings::new(&url).unwrap();
    s.credentials = Some(Credentials {
        username: user,
        password: password.clone(),
    });
    let c = MqttConnection::start(&s, s.client_id("pub"), vec![], None).unwrap();
    c.publish(&MqttPublish {
        topic: "fm/auth".into(),
        payload: b"x".to_vec(),
        qos: 1,
        ..Default::default()
    })
    .await
    .unwrap();
    s.credentials.as_mut().unwrap().password = format!("{password}-wrong");
    let c = MqttConnection::start(&s, s.client_id("pub"), vec![], None).unwrap();
    let e = c
        .publish(&MqttPublish {
            topic: "fm/auth".into(),
            payload: b"x".to_vec(),
            qos: 1,
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(
        !e.to_string().contains(&password),
        "the password is never in errors"
    );
    assert!(c.status().last_error.is_some());
}

/// `FASTMOCK_TEST_AMQPS` is an `amqps://` listener whose certificate `FASTMOCK_TEST_TLS_CA` issued.
#[tokio::test]
async fn amqp_over_tls_with_a_custom_ca() {
    init();
    let (Ok(url), Some(ca)) = (
        std::env::var("FASTMOCK_TEST_AMQPS"),
        env_path("FASTMOCK_TEST_TLS_CA"),
    ) else {
        eprintln!("FASTMOCK_TEST_AMQPS and FASTMOCK_TEST_TLS_CA not set: skipped");
        return;
    };
    let mut s = amqp_settings(&url);
    s.tls_files.ca_file = Some(ca);
    s.validate().unwrap();
    let msg = AmqpMessage {
        exchange: "amq.topic".into(),
        routing_key: "fm.tls".into(),
        payload: b"x".to_vec(),
        ..Default::default()
    };
    AmqpPublisher::new(s.clone(), "tls test")
        .publish(&msg, None)
        .await
        .unwrap();
    let mut no_ca = s.clone();
    no_ca.tls_files.ca_file = None;
    let e = AmqpPublisher::new(no_ca, "tls test")
        .publish(&msg, None)
        .await
        .unwrap_err();
    assert!(
        format!("{e:#}").to_lowercase().contains("certificate")
            || format!("{e:#}").contains("UnknownIssuer"),
        "{e:#}"
    );
}
