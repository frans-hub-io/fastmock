use std::sync::OnceLock;

use axum::body::Body;
use axum::http::{Request, header};
use fastmock_core::mail::{Envelope, MailQuery, MailSource};
use fastmock_core::model::SpaceFilter;
use fastmock_mail::{Incoming, MailConfig};
use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use serde_json::Value;
use sqlx::Connection;
use tower::ServiceExt;

const ADMIN: &str = "fm_admin_token_for_mail_wiring_tests_0123";
const VIEWER: &str = "fm_viewer_token_for_mail_wiring_tests_012";

const ACCESS: &str = r#"
spaces:
  - name: billing
    mailDomains: [billing.example.test, "*.invoices.example.test"]
  - name: crm
"#;

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

async fn storages() -> Vec<StorageKind> {
    let mut v = vec![StorageKind::Memory];
    let (Ok(admin_url), Ok(valkey_url)) = (
        std::env::var("FASTMOCK_TEST_DATABASE_URL"),
        std::env::var("FASTMOCK_TEST_VALKEY_URL"),
    ) else {
        return v;
    };
    let db = format!("fm_mail_{}", uuid::Uuid::new_v4().simple());
    let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut c)
        .await
        .unwrap();
    v.push(StorageKind::Postgres {
        database_url: format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db),
        valkey_url,
        valkey_prefix: format!("mail:{}:", uuid::Uuid::new_v4().simple()),
        max_connections: 5,
        options: Default::default(),
    });
    v
}

fn config(storage: StorageKind, mail: Option<MailConfig>) -> Config {
    let dir = std::env::temp_dir().join(format!("fm-mail-wiring-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let access = dir.join("access.yaml");
    std::fs::write(&access, ACCESS).unwrap();
    let mut cfg = Config::for_tests(
        storage,
        vec![
            format!("ops:admin:{}", hash_token(ADMIN)),
            format!("dash:viewer:{}", hash_token(VIEWER)),
        ],
    );
    cfg.plugins = Some(vec![]);
    cfg.access_file = Some(access);
    cfg.mail = mail;
    cfg
}

async fn get(admin: &axum::Router, path: &str, token: &str) -> Value {
    let res = admin
        .clone()
        .oneshot(
            Request::get(format!("/__admin{path}"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "{path}");
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn features(v: &Value) -> Vec<String> {
    v["features"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f.as_str())
        .filter(|f| f.starts_with("mail"))
        .map(String::from)
        .collect()
}

#[tokio::test]
async fn mail_is_wired_only_when_enabled() {
    for storage in storages().await {
        let name = format!("{storage:?}");
        let mut mail = MailConfig::default();
        mail.listeners.smtp = None;
        mail.listeners.pop3 = Some("127.0.0.1:0".parse().unwrap());
        let (app, _data, admin, _) =
            fastmock_server::build(config(storage.clone(), Some(mail)), prom())
                .await
                .unwrap();
        let svc = app.mail.clone().expect("mail module");
        assert_eq!(
            features(&get(&admin, "/version", VIEWER).await),
            vec!["mail", "mail.pop3", "mail.providers"],
            "{name}"
        );
        let spaces = get(&admin, "/spaces", ADMIN).await;
        let billing = spaces["spaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "billing")
            .unwrap();
        assert_eq!(
            billing["mailDomains"],
            serde_json::json!(["billing.example.test", "*.invoices.example.test"])
        );
        let stored = svc
            .accept(Incoming::new(
                MailSource::Smtp,
                Envelope {
                    rcpt_to: vec![
                        "a@eu.invoices.example.test".into(),
                        "b@other.example".into(),
                    ],
                    auth_user: Some("crm".into()),
                    ..Default::default()
                },
                b"Subject: hi\r\n\r\nbody\r\n".to_vec(),
            ))
            .await
            .unwrap();
        let spaces: Vec<Option<&str>> = stored.iter().map(|e| e.space.as_deref()).collect();
        assert_eq!(spaces, vec![Some("billing"), Some("crm")], "{name}");
        let page = svc
            .store
            .list(&MailQuery {
                spaces: SpaceFilter::In(vec!["billing".into()]),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].summary.subject.as_deref(), Some("hi"));

        let (app, _data, admin, _) = fastmock_server::build(config(storage, None), prom())
            .await
            .unwrap();
        assert!(app.mail.is_none());
        assert!(features(&get(&admin, "/version", VIEWER).await).is_empty());
    }
}
