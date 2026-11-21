use std::sync::{Arc, OnceLock};
use std::time::Duration;

use fastmock_core::mail::{
    Envelope, MailEventKind, MailQuery, MailSource, MailSubscription, MailboxKey,
};
use fastmock_core::model::SpaceFilter;
use fastmock_mail::{Incoming, MailConfig, MailService};
use fastmock_server::config::{Config, StorageKind};
use sqlx::Connection;

const ACCESS: &str = r#"
spaces:
  - name: billing
    mailDomains: [billing.example.test]
  - name: crm
    mailDomains: [crm.example.test]
"#;

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

async fn shared_storage() -> Option<StorageKind> {
    let (Ok(admin_url), Ok(valkey_url)) = (
        std::env::var("FASTMOCK_TEST_DATABASE_URL"),
        std::env::var("FASTMOCK_TEST_VALKEY_URL"),
    ) else {
        eprintln!("FASTMOCK_TEST_DATABASE_URL/FASTMOCK_TEST_VALKEY_URL not set: skipped");
        return None;
    };
    let db = format!("fm_mailrep_{}", uuid::Uuid::new_v4().simple());
    let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut c)
        .await
        .unwrap();
    Some(StorageKind::Postgres {
        database_url: format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db),
        valkey_url,
        valkey_prefix: format!("mailrep:{}:", uuid::Uuid::new_v4().simple()),
        max_connections: 5,
        options: Default::default(),
    })
}

async fn replica(storage: &StorageKind, max_messages: usize) -> Arc<MailService> {
    let dir = std::env::temp_dir().join(format!("fm-mail-replicas-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let access = dir.join("access.yaml");
    std::fs::write(&access, ACCESS).unwrap();
    let mut cfg = Config::for_tests(storage.clone(), vec![]);
    cfg.auth_disabled = true;
    cfg.plugins = Some(vec![]);
    cfg.access_file = Some(access);
    let mut mail = MailConfig::default();
    mail.listeners.smtp = None;
    mail.retention.max_messages = max_messages;
    cfg.mail = Some(mail);
    let (app, _data, _admin, _) = fastmock_server::build(cfg, prom()).await.unwrap();
    app.mail.expect("mail module")
}

fn incoming(rcpt: &str, subject: &str) -> Incoming {
    Incoming::new(
        MailSource::Smtp,
        Envelope {
            mail_from: "shop@example.com".into(),
            rcpt_to: vec![rcpt.into()],
            ..Default::default()
        },
        format!("From: shop@example.com\r\nTo: {rcpt}\r\nSubject: {subject}\r\n\r\nbody\r\n")
            .into_bytes(),
    )
}

async fn recv(sub: &mut MailSubscription) -> Arc<fastmock_core::mail::MailEvent> {
    tokio::time::timeout(Duration::from_secs(5), sub.recv())
        .await
        .expect("an event within 5s")
        .unwrap()
}

/// The next event of this kind; events published before the subscription may still arrive first.
async fn recv_kind(
    sub: &mut MailSubscription,
    kind: MailEventKind,
) -> Arc<fastmock_core::mail::MailEvent> {
    loop {
        let e = recv(sub).await;
        if e.kind == kind {
            return e;
        }
    }
}

#[tokio::test]
async fn a_message_accepted_on_one_replica_is_seen_on_the_other() {
    let Some(storage) = shared_storage().await else {
        return;
    };
    let a = replica(&storage, 1000).await;
    let b = replica(&storage, 1000).await;
    assert_eq!(b.events.poll_interval(), Some(Duration::from_secs(2)));
    let mut on_b = b.events.subscribe(SpaceFilter::In(vec!["billing".into()]));
    let stored = a
        .accept(incoming("Alice+Signup@billing.example.test", "hello"))
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    let e = recv(&mut on_b).await;
    assert_eq!(e.kind, MailEventKind::Received);
    assert_eq!(e.ids, vec![stored[0].id]);
    assert_eq!(e.mailboxes, vec!["alice@billing.example.test"]);
    let full = b
        .store
        .get(stored[0].id, &SpaceFilter::Any)
        .await
        .unwrap()
        .expect("stored in the shared database");
    assert_eq!(full.summary.subject.as_deref(), Some("hello"));
    let inbox = MailboxKey::inbox(Some("billing"), "alice@billing.example.test");
    assert_eq!(b.store.mailbox_entries(&inbox).await.unwrap().len(), 1);

    let refs = b
        .store
        .delete_matching(&SpaceFilter::Any, &Default::default())
        .await
        .unwrap();
    b.publish_deleted(&refs).await;
    let mut on_a = a.events.subscribe(SpaceFilter::In(vec!["crm".into()]));
    a.accept(incoming("x@crm.example.test", "second"))
        .await
        .unwrap();
    assert_eq!(recv(&mut on_a).await.space.as_deref(), Some("crm"));
}

#[tokio::test]
async fn retention_runs_once_and_both_replicas_hear_the_deletions() {
    let Some(storage) = shared_storage().await else {
        return;
    };
    let a = replica(&storage, 2).await;
    let b = replica(&storage, 2).await;
    let mut ids = Vec::new();
    for i in 0..5 {
        ids.push(
            a.accept(incoming("bob@billing.example.test", &format!("m{i}")))
                .await
                .unwrap()[0]
                .id,
        );
    }
    let mut on_a = a.events.subscribe(SpaceFilter::Any);
    let mut on_b = b.events.subscribe(SpaceFilter::Any);
    let (x, y) = tokio::join!(a.sweep(chrono::Utc::now()), b.sweep(chrono::Utc::now()));
    assert_eq!(
        x.unwrap() + y.unwrap(),
        3,
        "the oldest three are dropped once"
    );
    for sub in [&mut on_a, &mut on_b] {
        let e = recv_kind(sub, MailEventKind::Deleted).await;
        assert_eq!(e.kind, MailEventKind::Deleted);
        assert_eq!(e.space.as_deref(), Some("billing"));
        assert_eq!(e.ids.len(), 3);
        let mut got = e.ids.clone();
        got.sort();
        let mut want = ids[..3].to_vec();
        want.sort();
        assert_eq!(got, want);
    }
    let left = b
        .store
        .list(&MailQuery {
            oldest_first: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        left.items.iter().map(|e| e.id).collect::<Vec<_>>(),
        ids[3..].to_vec()
    );

    let _c = replica(&storage, 2).await;
    let t0 = std::time::Instant::now();
    loop {
        let text = prom().render();
        if text
            .lines()
            .any(|l| l.starts_with("fastmock_mail_messages{space=\"billing\"} 2"))
        {
            assert!(
                text.lines()
                    .any(|l| l.starts_with("fastmock_mail_storage_bytes{space=\"billing\"}")),
                "{text}"
            );
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "{text}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
