use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use fastmock_core::memory::memory_stores;
use fastmock_core::{
    ApiToken, ApiTokenStatus, AuditEvent, CallbackFilter, CallbackJob, CallbackStatus,
    JournalEntry, JournalFilter, SpaceFilter, Stores, Stub, TokenStore,
};
use fastmock_store::{PgStore, ValkeyStore};
use serde_json::json;
use sqlx::Connection;

async fn real_stores() -> Option<Stores> {
    let admin_url = std::env::var("FASTMOCK_TEST_DATABASE_URL").ok()?;
    let valkey_url = std::env::var("FASTMOCK_TEST_VALKEY_URL").ok()?;
    let db = format!("fm_{}", uuid::Uuid::new_v4().simple());
    let mut admin = sqlx::PgConnection::connect(&admin_url)
        .await
        .expect("admin connection");
    sqlx::query(&format!("create database {db}"))
        .execute(&mut admin)
        .await
        .unwrap();
    let url = format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db);
    let pg = PgStore::connect(&url, 5).await.unwrap();
    pg.migrate().await.unwrap();
    pg.migrate().await.unwrap();
    let prefix = format!("fmtest:{}:", uuid::Uuid::new_v4().simple());
    let hot = ValkeyStore::connect(&valkey_url, &prefix).await.unwrap();
    let pg = Arc::new(pg);
    Some(Stores {
        stubs: pg.clone(),
        journal: pg.clone(),
        outbox: pg.clone(),
        hot: Arc::new(hot),
        data: pg.clone(),
        audit: pg,
    })
}

async fn all_backends() -> Vec<(&'static str, Stores)> {
    let mut v = vec![("memory", memory_stores(10_000))];
    match real_stores().await {
        Some(s) => v.push(("postgres+valkey", s)),
        None => eprintln!(
            "FASTMOCK_TEST_DATABASE_URL/FASTMOCK_TEST_VALKEY_URL not set: real backends skipped"
        ),
    }
    v
}

fn stub(path: &str, priority: i32, tags: &[&str]) -> Stub {
    serde_json::from_value(json!({"priority": priority, "tags": tags, "request": {"urlPath": path}, "response": {"status": 200}})).unwrap()
}

#[tokio::test]
async fn stub_store_contract() {
    for (name, s) in all_backends().await {
        let r0 = s.stubs.revision().await.unwrap();
        let a = stub("/a", 5, &["t1"]);
        let b = stub("/b", 1, &["t1", "t2"]);
        s.stubs.upsert(&a).await.unwrap();
        s.stubs.upsert(&b).await.unwrap();
        assert!(s.stubs.revision().await.unwrap() > r0, "{name}");
        let list = s.stubs.list().await.unwrap();
        assert_eq!(list.len(), 2, "{name}");
        assert_eq!(list[0].id, b.id, "{name}: newest first");
        let mut a2 = a.clone();
        a2.name = Some("renamed".into());
        s.stubs.upsert(&a2).await.unwrap();
        let got = s.stubs.get(a.id).await.unwrap().unwrap();
        assert_eq!(got.name.as_deref(), Some("renamed"), "{name}");
        assert!(got.updated_at.is_some(), "{name}");
        let r0 = s.stubs.revision().await.unwrap();
        let batch: Vec<Stub> = (0..3)
            .map(|i| stub(&format!("/r{i}"), 5, &["batch"]))
            .collect();
        assert_eq!(
            s.stubs
                .replace_tag(Some("batch"), &SpaceFilter::Any, &batch)
                .await
                .unwrap(),
            0,
            "{name}"
        );
        assert_eq!(
            s.stubs
                .replace_tag(Some("batch"), &SpaceFilter::Any, &batch[..1])
                .await
                .unwrap(),
            3,
            "{name}"
        );
        assert_eq!(
            s.stubs.revision().await.unwrap(),
            r0 + 2,
            "{name}: one revision per replace"
        );
        assert_eq!(
            s.stubs
                .delete_by_tag("batch", &SpaceFilter::Any)
                .await
                .unwrap(),
            1,
            "{name}"
        );
        assert_eq!(
            s.stubs
                .delete_by_tag("t2", &SpaceFilter::Any)
                .await
                .unwrap(),
            1,
            "{name}"
        );
        assert!(s.stubs.delete(a.id).await.unwrap(), "{name}");
        assert!(!s.stubs.delete(a.id).await.unwrap(), "{name}");
        assert!(s.stubs.list().await.unwrap().is_empty(), "{name}");
    }
}

fn entry(source: &str, path: &str, corr: Option<&str>, matched: bool) -> JournalEntry {
    JournalEntry {
        id: 0,
        at: Utc::now(),
        source: source.into(),
        method: "POST".into(),
        path: path.into(),
        query: "".into(),
        headers: BTreeMap::from([("content-type".into(), "application/json".into())]),
        body: "{\"phone\":\"380501234567\"}".into(),
        status: 200,
        response_body: "ok".into(),
        stub_id: None,
        correlation: corr.map(String::from),
        matched,
        duration_ms: 3,
        trace_id: Some("0af7651916cd43dd8448eb211c80319c".into()),
        space: None,
    }
}

#[tokio::test]
async fn journal_contract() {
    for (name, s) in all_backends().await {
        let id1 = s
            .journal
            .append(&entry("sms", "/sms/v1/phones/1", Some("m1"), true))
            .await
            .unwrap();
        let id2 = s
            .journal
            .append(&entry("chat", "/chat/send", None, false))
            .await
            .unwrap();
        assert!(id2 > id1, "{name}");
        let all = s.journal.query(&JournalFilter::default()).await.unwrap();
        assert_eq!(all.len(), 2, "{name}");
        assert_eq!(all[0].id, id2, "{name}: newest first");
        assert_eq!(all[1].headers["content-type"], "application/json", "{name}");
        let f = JournalFilter {
            correlation: Some("m1".into()),
            ..Default::default()
        };
        assert_eq!(s.journal.count(&f).await.unwrap(), 1, "{name}");
        let f = JournalFilter {
            path_prefix: Some("/chat".into()),
            matched: Some(false),
            ..Default::default()
        };
        assert_eq!(
            s.journal.query(&f).await.unwrap()[0].source,
            "chat",
            "{name}"
        );
        let f = JournalFilter {
            body_contains: Some("380501234567".into()),
            method: Some("post".into()),
            ..Default::default()
        };
        assert_eq!(s.journal.count(&f).await.unwrap(), 2, "{name}");
        let f = JournalFilter {
            body_contains: Some("%".into()),
            ..Default::default()
        };
        assert_eq!(
            s.journal.count(&f).await.unwrap(),
            0,
            "{name}: no LIKE wildcards"
        );
        assert_eq!(
            s.journal
                .purge_older_than(Utc::now() - chrono::Duration::hours(1))
                .await
                .unwrap(),
            0,
            "{name}"
        );
        assert_eq!(
            s.journal.clear(&SpaceFilter::Any).await.unwrap(),
            2,
            "{name}"
        );
    }
}

#[tokio::test]
async fn outbox_contract() {
    for (name, s) in all_backends().await {
        let now = Utc::now();
        let due = CallbackJob::new(
            "sms",
            "http://x/cb".into(),
            "POST".into(),
            BTreeMap::new(),
            "{}".into(),
            now - chrono::Duration::seconds(1),
            2,
        )
        .with_correlation("m1");
        let later = CallbackJob::new(
            "sms",
            "http://x/cb".into(),
            "POST".into(),
            BTreeMap::new(),
            "{}".into(),
            now + chrono::Duration::hours(1),
            2,
        );
        s.outbox.enqueue(&due).await.unwrap();
        s.outbox.enqueue(&later).await.unwrap();
        assert_eq!(s.outbox.pending_count().await.unwrap(), 2, "{name}");
        let claimed = s
            .outbox
            .claim_due(Utc::now(), 10, Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1, "{name}");
        assert_eq!(claimed[0].attempts, 1, "{name}");
        assert!(
            s.outbox
                .claim_due(Utc::now(), 10, Duration::from_secs(30))
                .await
                .unwrap()
                .is_empty(),
            "{name}: leased job is not claimed twice"
        );
        s.outbox
            .mark_attempt_failed(
                due.id,
                1,
                "boom",
                Some(503),
                Some(Utc::now() - chrono::Duration::milliseconds(1)),
            )
            .await
            .unwrap();
        let again = s
            .outbox
            .claim_due(Utc::now(), 10, Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(again.len(), 1, "{name}");
        assert_eq!(again[0].attempts, 2, "{name}");
        assert_eq!(again[0].last_status, Some(503), "{name}");
        assert!(
            !s.outbox.mark_sent(due.id, 1, 204).await.unwrap(),
            "{name}: a stale attempt cannot complete the job"
        );
        assert!(s.outbox.mark_sent(due.id, 2, 204).await.unwrap(), "{name}");
        let sent = s
            .outbox
            .list(&CallbackFilter {
                correlation: Some("m1".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(sent[0].status, CallbackStatus::Sent, "{name}");
        assert_eq!(sent[0].last_status, Some(204), "{name}");
        assert!(sent[0].sent_at.is_some(), "{name}");
        s.outbox
            .mark_attempt_failed(later.id, 0, "gave up", None, None)
            .await
            .unwrap();
        let failed = s
            .outbox
            .list(&CallbackFilter {
                status: Some(CallbackStatus::Failed),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(failed.len(), 1, "{name}");
        assert_eq!(s.outbox.pending_count().await.unwrap(), 0, "{name}");
        assert_eq!(
            s.outbox
                .purge_finished_older_than(Utc::now() + chrono::Duration::seconds(1))
                .await
                .unwrap(),
            2,
            "{name}"
        );
    }
}

#[tokio::test]
async fn concurrent_claims_do_not_overlap() {
    for (name, s) in all_backends().await {
        for _ in 0..200 {
            let j = CallbackJob::new(
                "x",
                "http://x".into(),
                "POST".into(),
                BTreeMap::new(),
                "".into(),
                Utc::now() - chrono::Duration::seconds(1),
                1,
            );
            s.outbox.enqueue(&j).await.unwrap();
        }
        let mut handles = vec![];
        for _ in 0..8 {
            let o = s.outbox.clone();
            handles.push(tokio::spawn(async move {
                let mut got = vec![];
                loop {
                    let b = o
                        .claim_due(Utc::now(), 7, Duration::from_secs(60))
                        .await
                        .unwrap();
                    if b.is_empty() {
                        break;
                    }
                    got.extend(b.into_iter().map(|j| j.id));
                }
                got
            }));
        }
        let mut ids = vec![];
        for h in handles {
            ids.extend(h.await.unwrap());
        }
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(n, 200, "{name}");
        assert_eq!(ids.len(), 200, "{name}: no job claimed twice");
    }
}

#[tokio::test]
async fn hot_store_contract() {
    for (name, s) in all_backends().await {
        let h = &s.hot;
        assert_eq!(h.get("k").await.unwrap(), None, "{name}");
        h.set("k", "v", None).await.unwrap();
        assert_eq!(h.get("k").await.unwrap().as_deref(), Some("v"), "{name}");
        assert!(
            !h.compare_and_set("k", Some("x"), "w", None).await.unwrap(),
            "{name}"
        );
        assert!(
            h.compare_and_set("k", Some("v"), "w", None).await.unwrap(),
            "{name}"
        );
        assert!(
            !h.compare_and_set("k", None, "z", None).await.unwrap(),
            "{name}"
        );
        assert!(
            h.compare_and_set("new", None, "z", Some(Duration::from_millis(200)))
                .await
                .unwrap(),
            "{name}"
        );
        assert_eq!(
            h.incr("cnt", Some(Duration::from_secs(10))).await.unwrap(),
            1,
            "{name}"
        );
        assert_eq!(h.incr("cnt", None).await.unwrap(), 2, "{name}");
        assert_eq!(h.incr_by("cnt", -1, None).await.unwrap(), 1, "{name}");
        assert_eq!(h.incr_by("cnt", 5, None).await.unwrap(), 6, "{name}");
        h.set("ttl", "x", Some(Duration::from_millis(150)))
            .await
            .unwrap();
        h.set("scenario:a*b", "1", None).await.unwrap();
        h.set("scenario:ab", "2", None).await.unwrap();
        let sc = h.scan("scenario:a*", 100).await.unwrap();
        assert_eq!(
            sc,
            vec![("scenario:a*b".to_string(), "1".to_string())],
            "{name}: glob characters are literal"
        );
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(h.get("ttl").await.unwrap(), None, "{name}: ttl expired");
        assert_eq!(h.get("new").await.unwrap(), None, "{name}: cas ttl expired");
        assert_eq!(h.delete_prefix("scenario:").await.unwrap(), 2, "{name}");
        assert!(h.delete("k").await.unwrap(), "{name}");
        assert!(!h.delete("k").await.unwrap(), "{name}");
        h.ping().await.unwrap();
    }
}

#[tokio::test]
async fn data_store_contract() {
    for (name, s) in all_backends().await {
        let d = &s.data;
        d.put("registry", "100", &json!({"usr": 1})).await.unwrap();
        d.put("registry", "100", &json!({"usr": 2})).await.unwrap();
        d.put("other", "100", &json!(3)).await.unwrap();
        assert_eq!(
            d.get("registry", "100").await.unwrap(),
            Some(json!({"usr": 2})),
            "{name}"
        );
        assert_eq!(d.list("registry", 10).await.unwrap().len(), 1, "{name}");
        assert!(d.delete("registry", "100").await.unwrap(), "{name}");
        assert_eq!(d.clear("other").await.unwrap(), 1, "{name}");
        d.ping().await.unwrap();
    }
}

#[tokio::test]
async fn audit_contract() {
    for (name, s) in all_backends().await {
        for i in 0..3 {
            let e = AuditEvent {
                id: 0,
                at: Utc::now(),
                principal: "ci".into(),
                role: "admin".into(),
                principal_kind: Some("token".into()),
                space: (i == 1).then(|| "payments".to_string()),
                action: format!("stub.create.{i}"),
                detail: json!({"i": i}),
            };
            s.audit.record(&e).await.unwrap();
        }
        let l = s.audit.list(2).await.unwrap();
        assert_eq!(l.len(), 2, "{name}");
        assert_eq!(l[0].action, "stub.create.2", "{name}: newest first");
        assert_eq!(l[0].detail, json!({"i": 2}), "{name}");
        assert_eq!(l[0].principal_kind.as_deref(), Some("token"), "{name}");
        assert_eq!(l[1].space.as_deref(), Some("payments"), "{name}");
        assert_eq!(
            s.audit
                .purge_older_than(Utc::now() + chrono::Duration::seconds(1))
                .await
                .unwrap(),
            3,
            "{name}"
        );
    }
}

#[tokio::test]
async fn delayed_queue_contract() {
    for (name, s) in all_backends().await {
        let h = &s.hot;
        h.zadd("q:a", 30.0, "third").await.unwrap();
        h.zadd("q:a", 10.0, "first").await.unwrap();
        h.zadd("q:a", 20.0, "second").await.unwrap();
        h.zadd("q:a", 99.0, "late").await.unwrap();
        assert!(
            h.zpop_until("q:a", 5.0, 100).await.unwrap().is_empty(),
            "{name}"
        );
        assert_eq!(
            h.zpop_until("q:a", 25.0, 100).await.unwrap(),
            vec!["first", "second"],
            "{name}"
        );
        assert_eq!(
            h.zpop_until("q:a", 50.0, 100).await.unwrap(),
            vec!["third"],
            "{name}"
        );
        assert!(
            h.zpop_until("q:a", 50.0, 100).await.unwrap().is_empty(),
            "{name}: popped items are gone"
        );
        assert!(
            h.zpop_until("q:none", 50.0, 100).await.unwrap().is_empty(),
            "{name}"
        );
        for i in 0..2500 {
            h.zadd("q:big", i as f64, &format!("m{i}")).await.unwrap();
        }
        assert_eq!(
            h.zpop_until("q:big", 1e9, 2200).await.unwrap().len(),
            2200,
            "{name}"
        );
        assert_eq!(
            h.zpop_until("q:big", 1e9, 5000).await.unwrap().len(),
            300,
            "{name}"
        );
    }
}

#[tokio::test]
async fn outbox_lease_expiry_respects_max_attempts() {
    for (name, s) in all_backends().await {
        let j = CallbackJob::new(
            "x",
            "http://x".into(),
            "POST".into(),
            BTreeMap::new(),
            "".into(),
            Utc::now() - chrono::Duration::seconds(1),
            1,
        );
        s.outbox.enqueue(&j).await.unwrap();
        let first = s
            .outbox
            .claim_due(Utc::now(), 10, Duration::from_millis(100))
            .await
            .unwrap();
        assert_eq!(first.len(), 1, "{name}");
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            s.outbox
                .claim_due(Utc::now(), 10, Duration::from_millis(100))
                .await
                .unwrap()
                .is_empty(),
            "{name}: no attempt beyond maxAttempts"
        );
        let failed = s
            .outbox
            .list(&CallbackFilter {
                status: Some(CallbackStatus::Failed),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            failed.len(),
            1,
            "{name}: an expired last attempt becomes failed"
        );
        assert!(
            !s.outbox.mark_sent(j.id, 1, 204).await.unwrap(),
            "{name}: the late worker cannot flip it back"
        );
    }
}

fn in_space(mut s: Stub, space: Option<&str>) -> Stub {
    s.space = space.map(String::from);
    s
}

#[tokio::test]
async fn space_scoped_contract() {
    for (name, s) in all_backends().await {
        let a = in_space(stub("/a", 5, &["t"]), Some("pay"));
        let b = in_space(stub("/b", 5, &["t"]), Some("bill"));
        let g = in_space(stub("/g", 5, &["t"]), None);
        for x in [&a, &b, &g] {
            s.stubs.upsert(x).await.unwrap();
        }
        assert_eq!(
            s.stubs.get(a.id).await.unwrap().unwrap().space.as_deref(),
            Some("pay"),
            "{name}"
        );
        let fresh = in_space(stub("/a2", 5, &["t"]), Some("pay"));
        assert_eq!(
            s.stubs
                .replace_tag(
                    Some("t"),
                    &SpaceFilter::area(Some("pay")),
                    std::slice::from_ref(&fresh)
                )
                .await
                .unwrap(),
            1,
            "{name}: only the space's tagged stubs are replaced"
        );
        assert!(s.stubs.get(b.id).await.unwrap().is_some(), "{name}");
        assert!(s.stubs.get(g.id).await.unwrap().is_some(), "{name}");
        assert_eq!(
            s.stubs
                .delete_by_tag("t", &SpaceFilter::Global)
                .await
                .unwrap(),
            1,
            "{name}"
        );
        assert_eq!(
            s.stubs
                .clear(&SpaceFilter::In(vec!["bill".into()]))
                .await
                .unwrap(),
            1,
            "{name}"
        );
        let left: Vec<_> = s
            .stubs
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|x| x.id)
            .collect();
        assert_eq!(left, vec![fresh.id], "{name}");
        assert_eq!(s.stubs.clear(&SpaceFilter::Any).await.unwrap(), 1, "{name}");

        for i in 0..30 {
            let mut e = entry("stubs", &format!("/p{i}"), None, true);
            e.space = match i % 3 {
                0 => Some("pay".into()),
                1 => Some("bill".into()),
                _ => None,
            };
            s.journal.append(&e).await.unwrap();
        }
        let pay = JournalFilter {
            spaces: SpaceFilter::In(vec!["pay".into()]),
            limit: Some(4),
            ..Default::default()
        };
        assert_eq!(s.journal.count(&pay).await.unwrap(), 10, "{name}");
        let p1 = s.journal.query_page(&pay, 0).await.unwrap();
        let p3 = s.journal.query_page(&pay, 8).await.unwrap();
        assert_eq!(p1.len(), 4, "{name}");
        assert_eq!(p3.len(), 2, "{name}: last page of the space");
        assert!(
            p1.iter()
                .chain(p3.iter())
                .all(|e| e.space.as_deref() == Some("pay")),
            "{name}"
        );
        assert_eq!(p1[0].path, "/p27", "{name}: newest first");
        let both = JournalFilter {
            spaces: SpaceFilter::In(vec!["pay".into(), "bill".into()]),
            ..Default::default()
        };
        assert_eq!(s.journal.count(&both).await.unwrap(), 20, "{name}");
        assert_eq!(
            s.journal
                .clear(&SpaceFilter::In(vec!["bill".into()]))
                .await
                .unwrap(),
            10,
            "{name}"
        );
        assert_eq!(
            s.journal.count(&JournalFilter::default()).await.unwrap(),
            20,
            "{name}"
        );

        let mut j1 = CallbackJob::new(
            "stubs",
            "http://127.0.0.1:9/x".into(),
            "POST".into(),
            BTreeMap::new(),
            String::new(),
            Utc::now(),
            1,
        );
        j1.space = Some("pay".into());
        let j2 = CallbackJob::new(
            "stubs",
            "http://127.0.0.1:9/y".into(),
            "POST".into(),
            BTreeMap::new(),
            String::new(),
            Utc::now(),
            1,
        );
        s.outbox.enqueue(&j1).await.unwrap();
        s.outbox.enqueue(&j2).await.unwrap();
        let only_pay = CallbackFilter {
            spaces: SpaceFilter::In(vec!["pay".into()]),
            ..Default::default()
        };
        let l = s.outbox.list(&only_pay).await.unwrap();
        assert_eq!(l.len(), 1, "{name}");
        assert_eq!(l[0].space.as_deref(), Some("pay"), "{name}");
        assert_eq!(
            s.outbox.clear(&SpaceFilter::Global).await.unwrap(),
            1,
            "{name}"
        );
        assert_eq!(
            s.outbox
                .list(&CallbackFilter::default())
                .await
                .unwrap()
                .len(),
            1,
            "{name}"
        );
    }
}

async fn token_backends() -> Vec<(&'static str, Arc<dyn TokenStore>)> {
    let mut v: Vec<(&'static str, Arc<dyn TokenStore>)> = vec![(
        "memory",
        Arc::new(fastmock_core::memory::MemoryTokens::default()),
    )];
    if let Ok(admin_url) = std::env::var("FASTMOCK_TEST_DATABASE_URL") {
        let db = format!("fm_{}", uuid::Uuid::new_v4().simple());
        let mut admin = sqlx::PgConnection::connect(&admin_url).await.unwrap();
        sqlx::query(&format!("create database {db}"))
            .execute(&mut admin)
            .await
            .unwrap();
        let url = format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db);
        let pg = PgStore::connect(&url, 5).await.unwrap();
        pg.migrate().await.unwrap();
        v.push(("postgres", Arc::new(pg)));
    }
    v
}

fn api_token(name: &str, hash: &str, expires_in: chrono::Duration) -> ApiToken {
    let now = Utc::now();
    ApiToken {
        id: uuid::Uuid::now_v7(),
        name: name.into(),
        sha256: hash.into(),
        role: "editor".into(),
        spaces: vec!["alpha".into(), "beta".into()],
        global: false,
        description: Some("d".into()),
        created_by: "ops".into(),
        created_at: now,
        expires_at: now + expires_in,
        last_used_at: None,
        revoked_at: None,
    }
}

#[tokio::test]
async fn token_store_contract() {
    for (name, s) in token_backends().await {
        let now = Utc::now();
        let a = api_token("ci", &"a".repeat(64), chrono::Duration::days(1));
        assert!(s.create(&a, now).await.unwrap(), "{name}");
        let dup = api_token("ci", &"b".repeat(64), chrono::Duration::days(1));
        assert!(
            !s.create(&dup, now).await.unwrap(),
            "{name}: active names are unique"
        );
        let same_hash = api_token("other", &"a".repeat(64), chrono::Duration::days(1));
        assert!(
            s.create(&same_hash, now).await.is_err(),
            "{name}: hashes are unique"
        );
        let got = s.by_hash(&"a".repeat(64)).await.unwrap().unwrap();
        assert_eq!(got.id, a.id, "{name}");
        assert_eq!(got.spaces, vec!["alpha", "beta"], "{name}");
        assert_eq!(got.description.as_deref(), Some("d"), "{name}");
        assert!(
            s.by_hash(&"c".repeat(64)).await.unwrap().is_none(),
            "{name}"
        );
        assert_eq!(s.count_active(now).await.unwrap(), 1, "{name}");

        s.touch(a.id, now).await.unwrap();
        let first = s.get(a.id).await.unwrap().unwrap().last_used_at.unwrap();
        s.touch(a.id, now + chrono::Duration::seconds(30))
            .await
            .unwrap();
        let t = s.get(a.id).await.unwrap().unwrap().last_used_at.unwrap();
        assert_eq!(
            t.timestamp(),
            first.timestamp(),
            "{name}: at most once per minute"
        );
        s.touch(a.id, now + chrono::Duration::seconds(61))
            .await
            .unwrap();
        let t = s.get(a.id).await.unwrap().unwrap().last_used_at.unwrap();
        assert!(t > first, "{name}");

        assert!(s.revoke(a.id, now).await.unwrap(), "{name}");
        assert!(!s.revoke(a.id, now).await.unwrap(), "{name}");
        let r = s.get(a.id).await.unwrap().unwrap();
        assert_eq!(r.status(Utc::now()), ApiTokenStatus::Revoked, "{name}");
        assert_eq!(s.count_active(Utc::now()).await.unwrap(), 0, "{name}");
        assert!(
            s.create(&dup, Utc::now()).await.unwrap(),
            "{name}: name free again"
        );

        let gone = api_token("gone", &"d".repeat(64), chrono::Duration::seconds(-5));
        assert!(s.create(&gone, Utc::now()).await.unwrap(), "{name}");
        let again = api_token("gone", &"e".repeat(64), chrono::Duration::days(1));
        assert!(
            s.create(&again, Utc::now()).await.unwrap(),
            "{name}: expired names are reused"
        );
        let old = s.get(gone.id).await.unwrap().unwrap();
        assert_eq!(old.status(Utc::now()), ApiTokenStatus::Expired, "{name}");
        let list = s.list().await.unwrap();
        assert_eq!(list.len(), 4, "{name}");
        assert_eq!(list[0].id, again.id, "{name}: newest first");

        assert_eq!(
            s.purge_inactive_before(Utc::now() + chrono::Duration::seconds(1))
                .await
                .unwrap(),
            2,
            "{name}"
        );
        assert_eq!(s.list().await.unwrap().len(), 2, "{name}");
    }
}
