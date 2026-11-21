use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use fastmock_core::SpaceFilter;
use fastmock_core::mail::{
    FlagOp, MailAddress, MailEvent, MailEventKind, MailEvents, MailFilter, MailQuery,
    MailRetention, MailStore, MailTerm, MailboxDelivery, MailboxKey, MemoryMailStore, conformance,
    micros,
};
use fastmock_store::mail::MAIL_SWEEP_LOCK;
use fastmock_store::{PgMailStore, PgStore, ValkeyMailEvents, valkey_client};
use sqlx::Connection;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use uuid::Uuid;

async fn fresh_database() -> Option<String> {
    let Ok(admin_url) = std::env::var("FASTMOCK_TEST_DATABASE_URL") else {
        eprintln!("FASTMOCK_TEST_DATABASE_URL not set: skipped");
        return None;
    };
    let db = format!("fm_mail_{}", Uuid::new_v4().simple());
    let mut admin = sqlx::PgConnection::connect(&admin_url)
        .await
        .expect("admin connection");
    sqlx::query(&format!("create database {db}"))
        .execute(&mut admin)
        .await
        .unwrap();
    let url = format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db);
    PgStore::connect(&url, 2)
        .await
        .unwrap()
        .migrate()
        .await
        .unwrap();
    Some(url)
}

async fn store(url: &str, connections: u32) -> PgMailStore {
    PgMailStore::new(
        PgStore::connect(url, connections)
            .await
            .unwrap()
            .pool()
            .clone(),
    )
}

fn valkey_url() -> Option<String> {
    let v = std::env::var("FASTMOCK_TEST_VALKEY_URL").ok();
    if v.is_none() {
        eprintln!("FASTMOCK_TEST_VALKEY_URL not set: skipped");
    }
    v
}

#[tokio::test]
async fn pg_mail_store_passes_the_conformance_checks() {
    let Some(url) = fresh_database().await else {
        return;
    };
    let s = store(&url, 5).await;
    conformance::messages(&s).await;
    conformance::mailboxes(&s).await;
    let Some(url) = fresh_database().await else {
        return;
    };
    conformance::retention(&store(&url, 5).await).await;
    let Some(url) = fresh_database().await else {
        return;
    };
    conformance::growth(&store(&url, 5).await).await;
}

#[tokio::test]
async fn pg_mailboxes_per_area_are_capped() {
    let Some(url) = fresh_database().await else {
        return;
    };
    let s = store(&url, 3).await.with_max_mailboxes(2);
    let space = conformance::space();
    let sp = Some(space.as_str());
    for (i, rcpt) in ["a@example.test", "b@example.test", "c@example.test"]
        .iter()
        .enumerate()
    {
        let m = conformance::message(sp, Utc::now(), &format!("m{i}"));
        s.insert(&m, &[MailboxDelivery::inbox(rcpt)]).await.unwrap();
        assert!(s.get(m.id, &SpaceFilter::Any).await.unwrap().is_some());
    }
    let owners = s
        .mailboxes(&SpaceFilter::In(vec![space.clone()]), None, 10)
        .await
        .unwrap();
    let names: Vec<&str> = owners.iter().map(|o| o.address.as_str()).collect();
    assert_eq!(names, vec!["a@example.test", "b@example.test"]);
    assert!(
        s.mailbox_create(&MailboxKey::inbox(sp, "d@example.test"))
            .await
            .is_err()
    );
    let m = conformance::message(sp, Utc::now(), "again");
    s.insert(&m, &[MailboxDelivery::inbox("a@example.test")])
        .await
        .unwrap();
    assert_eq!(
        s.mailbox_entries(&MailboxKey::inbox(sp, "a@example.test"))
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn parallel_inserts_from_two_replicas_get_unique_increasing_uids() {
    let Some(url) = fresh_database().await else {
        return;
    };
    let (r1, r2) = (
        Arc::new(store(&url, 8).await),
        Arc::new(store(&url, 8).await),
    );
    let space = conformance::space();
    let per_task = 10;
    let mut tasks = Vec::new();
    for t in 0..8 {
        let s = if t % 2 == 0 { r1.clone() } else { r2.clone() };
        let space = space.clone();
        tasks.push(tokio::spawn(async move {
            let mut ids = Vec::new();
            for i in 0..per_task {
                let m = conformance::message(Some(&space), Utc::now(), &format!("t{t} m{i}"));
                let mut d = vec![
                    MailboxDelivery::inbox("a@example.test"),
                    MailboxDelivery::inbox("b@example.test"),
                ];
                if t % 2 == 1 {
                    d.reverse();
                }
                s.insert(&m, &d).await.unwrap();
                ids.push(m.id);
            }
            ids
        }));
    }
    let mut all = BTreeSet::new();
    for t in tasks {
        all.extend(t.await.unwrap());
    }
    let total = 8 * per_task;
    assert_eq!(all.len(), total);
    for addr in ["a@example.test", "b@example.test"] {
        let key = MailboxKey::inbox(Some(&space), addr);
        let entries = r1.mailbox_entries(&key).await.unwrap();
        let uids: Vec<u32> = entries.iter().map(|e| e.uid).collect();
        assert_eq!(uids, (1..=total as u32).collect::<Vec<_>>(), "{addr}");
        let ids: BTreeSet<Uuid> = entries.iter().map(|e| e.message_id).collect();
        assert_eq!(ids, all, "{addr}");
        let info = r2.mailbox_info(&key).await.unwrap().unwrap();
        assert_eq!(
            (info.uid_next, info.exists),
            (total as u32 + 1, total as u32)
        );
    }
}

#[tokio::test]
async fn uids_become_visible_in_increasing_order() {
    let Some(url) = fresh_database().await else {
        return;
    };
    let (r1, r2) = (
        Arc::new(store(&url, 4).await),
        Arc::new(store(&url, 4).await),
    );
    let space = conformance::space();
    let key = MailboxKey::inbox(Some(&space), "a@example.test");
    let writers: Vec<_> = [r1.clone(), r2.clone()]
        .into_iter()
        .map(|s| {
            let space = space.clone();
            tokio::spawn(async move {
                for i in 0..25 {
                    let m = conformance::message(Some(&space), Utc::now(), &format!("m{i}"));
                    s.insert(&m, &[MailboxDelivery::inbox("a@example.test")])
                        .await
                        .unwrap();
                }
            })
        })
        .collect();
    let reader = {
        let s = r1.clone();
        let key = key.clone();
        tokio::spawn(async move {
            let mut seen_max = 0;
            for _ in 0..200 {
                let uids: Vec<u32> = s
                    .mailbox_entries(&key)
                    .await
                    .unwrap()
                    .iter()
                    .map(|e| e.uid)
                    .collect();
                assert_eq!(
                    uids,
                    (1..=uids.len() as u32).collect::<Vec<_>>(),
                    "a reader never sees a gap below a visible UID"
                );
                assert!(uids.len() as u32 >= seen_max);
                seen_max = uids.len() as u32;
                if seen_max == 50 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
    };
    for w in writers {
        w.await.unwrap();
    }
    reader.await.unwrap();
}

#[tokio::test]
async fn concurrent_flag_updates_keep_every_flag() {
    let Some(url) = fresh_database().await else {
        return;
    };
    let (r1, r2) = (
        Arc::new(store(&url, 8).await),
        Arc::new(store(&url, 8).await),
    );
    let space = conformance::space();
    let key = MailboxKey::inbox(Some(&space), "a@example.test");
    let m = conformance::message(Some(&space), Utc::now(), "flags");
    r1.insert(&m, &[MailboxDelivery::inbox("a@example.test")])
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for i in 0..20u32 {
        let s = if i.is_multiple_of(2) {
            r1.clone()
        } else {
            r2.clone()
        };
        let key = key.clone();
        tasks.push(tokio::spawn(async move {
            s.mailbox_store_flags(&key, &[1], FlagOp::Add, &[format!("k{i:02}")])
                .await
                .unwrap()
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap().len(), 1);
    }
    let flags = r2.mailbox_entries(&key).await.unwrap()[0].flags.clone();
    let want: Vec<String> = (0..20).map(|i| format!("k{i:02}")).collect();
    assert_eq!(flags, want);

    let mut tasks = Vec::new();
    for i in 0..20u32 {
        let s = if i.is_multiple_of(2) {
            r1.clone()
        } else {
            r2.clone()
        };
        let key = key.clone();
        tasks.push(tokio::spawn(async move {
            s.mailbox_store_flags(&key, &[1], FlagOp::Remove, &[format!("k{i:02}")])
                .await
                .unwrap()
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert!(r1.mailbox_entries(&key).await.unwrap()[0].flags.is_empty());
}

#[tokio::test]
async fn expunge_races_with_fetch_and_with_itself() {
    let Some(url) = fresh_database().await else {
        return;
    };
    let (r1, r2) = (
        Arc::new(store(&url, 8).await),
        Arc::new(store(&url, 8).await),
    );
    let space = conformance::space();
    let key = MailboxKey::inbox(Some(&space), "rcpt@example.test");
    let ids = conformance::fill(r1.as_ref(), Some(&space), 40).await;
    let all: Vec<u32> = (1..=40).collect();
    r1.mailbox_store_flags(&key, &all, FlagOp::Add, &["\\Deleted".into()])
        .await
        .unwrap();
    let reader = {
        let s = r2.clone();
        let key = key.clone();
        tokio::spawn(async move {
            for _ in 0..50 {
                let entries = s.mailbox_entries(&key).await.unwrap();
                let uids: Vec<u32> = entries.iter().map(|e| e.uid).collect();
                let mut sorted = uids.clone();
                sorted.sort_unstable();
                sorted.dedup();
                assert_eq!(uids, sorted);
                for e in &entries {
                    assert!(e.size > 0);
                }
                if entries.is_empty() {
                    break;
                }
            }
        })
    };
    let mut expungers = Vec::new();
    for (i, chunk) in all.chunks(5).enumerate() {
        let s = if i.is_multiple_of(2) {
            r1.clone()
        } else {
            r2.clone()
        };
        let key = key.clone();
        let only: Vec<u32> = chunk.to_vec();
        expungers.push(tokio::spawn(async move {
            let a = s.mailbox_expunge(&key, Some(&only)).await.unwrap();
            let b = s.mailbox_expunge(&key, None).await.unwrap();
            [a, b].concat()
        }));
    }
    let mut gone = Vec::new();
    for t in expungers {
        gone.extend(t.await.unwrap());
    }
    reader.await.unwrap();
    gone.sort_unstable();
    assert_eq!(gone, all, "each entry is expunged exactly once");
    assert!(r1.mailbox_entries(&key).await.unwrap().is_empty());
    for id in ids {
        assert!(r2.get(id, &SpaceFilter::Any).await.unwrap().is_some());
    }
}

#[tokio::test]
async fn deleting_a_message_while_copying_it_leaves_no_entries() {
    let Some(url) = fresh_database().await else {
        return;
    };
    let (r1, r2) = (
        Arc::new(store(&url, 4).await),
        Arc::new(store(&url, 4).await),
    );
    let space = conformance::space();
    let key = MailboxKey::inbox(Some(&space), "rcpt@example.test");
    let archive = MailboxKey {
        folder: "Archive".into(),
        ..key.clone()
    };
    let ids = conformance::fill(r1.as_ref(), Some(&space), 10).await;
    assert!(r1.mailbox_create(&archive).await.unwrap());
    let copier = {
        let s = r1.clone();
        let key = key.clone();
        tokio::spawn(async move {
            for uid in 1..=10 {
                s.mailbox_copy(&key, &[uid], "Archive")
                    .await
                    .expect("a copy never fails on a deleted message");
            }
        })
    };
    for id in &ids {
        r2.delete(*id, &SpaceFilter::Any).await.unwrap();
    }
    copier.await.unwrap();
    assert!(r1.mailbox_entries(&key).await.unwrap().is_empty());
    assert!(r1.mailbox_entries(&archive).await.unwrap().is_empty());
}

#[tokio::test]
async fn sweeps_run_on_one_replica_at_a_time() {
    let Some(url) = fresh_database().await else {
        return;
    };
    let (r1, r2) = (store(&url, 4).await, store(&url, 4).await);
    let space = conformance::space();
    let ids = conformance::fill(&r1, Some(&space), 6).await;
    let policy = MailRetention {
        max_messages: 2,
        max_age: Duration::from_secs(86_400),
        ..Default::default()
    };
    let mut holder = sqlx::PgConnection::connect(&url).await.unwrap();
    sqlx::query("select pg_advisory_lock($1)")
        .bind(MAIL_SWEEP_LOCK)
        .execute(&mut holder)
        .await
        .unwrap();
    assert!(
        r1.sweep(&policy, Utc::now()).await.unwrap().is_empty(),
        "another replica sweeps"
    );
    sqlx::query("select pg_advisory_unlock($1)")
        .bind(MAIL_SWEEP_LOCK)
        .execute(&mut holder)
        .await
        .unwrap();
    let (a, b) = tokio::join!(r1.sweep(&policy, Utc::now()), r2.sweep(&policy, Utc::now()));
    let mut gone: Vec<Uuid> = a
        .unwrap()
        .into_iter()
        .chain(b.unwrap())
        .map(|r| r.id)
        .collect();
    gone.sort();
    let mut want = ids[..4].to_vec();
    want.sort();
    assert_eq!(gone, want, "each message is swept once");
    let left = r2
        .list(&MailQuery {
            spaces: SpaceFilter::area(Some(&space)),
            oldest_first: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        left.items.iter().map(|e| e.id).collect::<Vec<_>>(),
        ids[4..].to_vec()
    );
    let key = MailboxKey::inbox(Some(&space), "rcpt@example.test");
    let uids: Vec<u32> = r1
        .mailbox_entries(&key)
        .await
        .unwrap()
        .iter()
        .map(|e| e.uid)
        .collect();
    assert_eq!(uids, vec![5, 6]);
}

fn rich(space: &str, i: usize) -> fastmock_core::mail::MailMessage {
    let subjects = [
        "Your Invoice 42",
        "Welcome aboard",
        "Reset password",
        "ÄRGER über Rechnung",
    ];
    let mut m = conformance::message(
        Some(space),
        Utc::now() - chrono::Duration::seconds(100 - i as i64),
        subjects[i % subjects.len()],
    );
    m.envelope.mail_from = if i.is_multiple_of(3) {
        "Bounce@Example.com".into()
    } else {
        String::new()
    };
    m.envelope.rcpt_to = vec![format!("hidden{i}@example.test")];
    m.summary.from = vec![MailAddress {
        name: (i.is_multiple_of(2)).then(|| "Billing Team".into()),
        address: format!("billing{}@example.com", i % 4),
    }];
    m.summary.to = if i.is_multiple_of(5) {
        vec![]
    } else {
        vec![MailAddress {
            name: None,
            address: "Alice@Example.test".into(),
        }]
    };
    m.summary.cc = if i % 4 == 1 {
        vec![MailAddress {
            name: Some("Carol Ünal".into()),
            address: "carol@example.test".into(),
        }]
    } else {
        vec![]
    };
    if i.is_multiple_of(7) {
        m.summary.subject = None;
    }
    m.summary.has_attachments = i % 3 == 1;
    m.labels = match i % 3 {
        0 => vec![],
        1 => vec!["otp".into()],
        _ => vec!["otp".into(), "welcome".into()],
    };
    m.search_text = format!(
        "{} pay now {}",
        m.summary.subject.clone().unwrap_or_default(),
        i % 2
    )
    .to_lowercase();
    m
}

#[tokio::test]
async fn search_terms_select_what_the_memory_store_selects() {
    let Some(url) = fresh_database().await else {
        return;
    };
    let pg = store(&url, 4).await;
    let mem = MemoryMailStore::new(1000);
    let space = conformance::space();
    let mut mid = None;
    for i in 0..24 {
        let m = rich(&space, i);
        if i == 12 {
            mid = Some(m.received_at);
        }
        pg.insert(&m, &[]).await.unwrap();
        mem.insert(&m, &[]).await.unwrap();
    }
    let mid = mid.unwrap();
    let cases: Vec<Vec<MailTerm>> = vec![
        vec![],
        vec![MailTerm::From("billing team".into())],
        vec![MailTerm::From("bounce@".into())],
        vec![MailTerm::From("".into())],
        vec![MailTerm::From("billing1".into())],
        vec![MailTerm::To("hidden1".into())],
        vec![MailTerm::To("alice@".into())],
        vec![MailTerm::To("".into())],
        vec![MailTerm::Cc("ünal".into())],
        vec![MailTerm::Cc("carol".into())],
        vec![MailTerm::Subject("invoice".into())],
        vec![MailTerm::Subject("ärger".into())],
        vec![MailTerm::Subject("".into())],
        vec![MailTerm::HasAttachment],
        vec![MailTerm::Label("otp".into())],
        vec![MailTerm::Label("ot".into())],
        vec![MailTerm::Label("welcome".into()), MailTerm::HasAttachment],
        vec![MailTerm::After(mid)],
        vec![MailTerm::Before(mid)],
        vec![MailTerm::Text("pay now 1".into())],
        vec![MailTerm::Text("welcome".into())],
        vec![MailTerm::Text("100%_x".into())],
        vec![
            MailTerm::Text("pay".into()),
            MailTerm::From("billing".into()),
            MailTerm::Before(mid),
        ],
    ];
    for terms in cases {
        for oldest_first in [false, true] {
            let mut q = MailQuery {
                spaces: SpaceFilter::area(Some(&space)),
                filter: MailFilter {
                    terms: terms.clone(),
                },
                limit: 5,
                oldest_first,
                ..Default::default()
            };
            loop {
                let a = pg.list(&q).await.unwrap();
                let b = mem.list(&q).await.unwrap();
                assert_eq!(a, b, "{terms:?} oldest_first={oldest_first}");
                match a.next {
                    Some(c) => q.cursor = Some(c),
                    None => break,
                }
            }
        }
    }
    let f = MailFilter {
        terms: vec![MailTerm::Label("welcome".into())],
    };
    let mut a: Vec<Uuid> = pg
        .delete_matching(&SpaceFilter::area(Some(&space)), &f)
        .await
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    let mut b: Vec<Uuid> = mem
        .delete_matching(&SpaceFilter::area(Some(&space)), &f)
        .await
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    a.sort();
    b.sort();
    assert_eq!(a, b);
    assert_eq!(a.len(), 8);
}

#[tokio::test]
async fn messages_round_trip_with_provider_fields_and_nul_characters() {
    let Some(url) = fresh_database().await else {
        return;
    };
    let s = store(&url, 2).await;
    let mut m = conformance::message(None, micros(Utc::now()), "plain");
    m.source = fastmock_core::mail::MailSource::Provider;
    m.provider = Some("sendgrid".into());
    m.provider_extras = Some(fastmock_core::mail::ProviderExtras {
        provider_message_id: Some("abc".into()),
        template_data: Some(serde_json::json!({"code": "1234"})),
        tags: vec!["t".into()],
        test_mode: true,
        ..Default::default()
    });
    m.transcript = vec![fastmock_core::mail::TranscriptLine {
        at: micros(Utc::now()),
        direction: fastmock_core::mail::TranscriptDirection::Client,
        text: "EHLO client.example.test".into(),
    }];
    m.raw = bytes::Bytes::from_static(b"Subject: x\r\n\r\n\0binary\xff\r\n");
    s.insert(&m, &[]).await.unwrap();
    assert_eq!(s.get(m.id, &SpaceFilter::Global).await.unwrap().unwrap(), m);

    let mut odd = conformance::message(None, micros(Utc::now()), "a\0b");
    odd.envelope.helo = Some("h\0".into());
    odd.search_text = "a\0b".into();
    s.insert(&odd, &[MailboxDelivery::inbox("x\0@example.test")])
        .await
        .unwrap();
    let got = s.get(odd.id, &SpaceFilter::Any).await.unwrap().unwrap();
    assert_eq!(got.summary.subject.as_deref(), Some("a\u{FFFD}b"));
    assert_eq!(got.envelope.helo.as_deref(), Some("h\u{FFFD}"));
    assert_eq!(got.raw, odd.raw);
    let found = s
        .list(&MailQuery {
            spaces: SpaceFilter::Global,
            filter: MailFilter {
                terms: vec![MailTerm::Subject("a\0b".into())],
            },
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(found.items.len(), 1);
    assert_eq!(
        s.mailbox_entries(&MailboxKey::inbox(None, "x\0@example.test"))
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn usage_counts_messages_and_bytes_per_area() {
    let Some(url) = fresh_database().await else {
        return;
    };
    let s = store(&url, 2).await;
    let space = conformance::space();
    conformance::fill(&s, Some(&space), 3).await;
    let g = conformance::message(None, Utc::now(), "global");
    s.insert(&g, &[]).await.unwrap();
    let usage = s.usage().await.unwrap();
    assert_eq!(usage.len(), 2);
    assert_eq!(usage[0].space, None);
    assert_eq!(usage[0].messages, 1);
    assert_eq!(usage[0].bytes, g.raw.len() as u64);
    assert_eq!(usage[1].space.as_deref(), Some(space.as_str()));
    assert_eq!(usage[1].messages, 3);
    assert!(usage[1].bytes > 0);
}

#[tokio::test]
async fn migrations_create_the_mail_tables_and_indexes() {
    let Some(url) = fresh_database().await else {
        return;
    };
    let pg = PgStore::connect(&url, 2).await.unwrap();
    let names: Vec<String> = sqlx::query_scalar(
        "select indexname::text from pg_indexes where tablename like 'fm_mail_%' order by 1",
    )
    .fetch_all(pg.pool())
    .await
    .unwrap();
    for want in [
        "fm_mail_entries_message",
        "fm_mail_messages_received",
        "fm_mail_messages_space",
    ] {
        assert!(names.iter().any(|n| n == want), "{want} in {names:?}");
    }
    let invalid: i64 = sqlx::query_scalar(
        "select count(*) from pg_index i join pg_class c on c.oid = i.indexrelid where c.relname like 'fm_mail_%' and not i.indisvalid",
    )
    .fetch_one(pg.pool())
    .await
    .unwrap();
    assert_eq!(invalid, 0);
    let validity: i64 = sqlx::query_scalar("select nextval('fm_mail_uid_validity')")
        .fetch_one(pg.pool())
        .await
        .unwrap();
    assert!(validity > 1_700_000_000, "seeded with the Unix time");
    assert!(validity < i64::from(u32::MAX));
}

async fn events(url: &str, prefix: &str) -> ValkeyMailEvents {
    ValkeyMailEvents::connect(
        valkey_client(url, None, &Default::default()).unwrap(),
        prefix,
        64,
    )
    .await
    .unwrap()
}

async fn recv(
    sub: &mut fastmock_core::mail::MailSubscription,
) -> Arc<fastmock_core::mail::MailEvent> {
    tokio::time::timeout(Duration::from_secs(5), sub.recv())
        .await
        .expect("an event within 5s")
        .unwrap()
}

#[tokio::test]
async fn valkey_events_reach_every_replica_of_the_prefix() {
    let Some(url) = valkey_url() else {
        return;
    };
    let prefix = format!("fmtest:{}:", Uuid::new_v4().simple());
    let (a, b) = (events(&url, &prefix).await, events(&url, &prefix).await);
    let other = events(&url, &format!("fmtest:{}:", Uuid::new_v4().simple())).await;
    assert!(a.is_connected());
    assert_eq!(a.poll_interval(), Some(Duration::from_secs(2)));
    assert_eq!(a.channel(None), format!("{prefix}mail:events:_global"));
    let mut on_a = a.subscribe(SpaceFilter::Any);
    let mut on_b = b.subscribe(SpaceFilter::area(Some("billing")));
    let mut on_other = other.subscribe(SpaceFilter::Any);
    let msg = conformance::message(Some("billing"), Utc::now(), "hello");
    a.publish(&MailEvent::deleted(Some("crm".into()), vec![Uuid::nil()]))
        .await
        .unwrap();
    a.publish(&MailEvent::received(&msg, vec!["rcpt@example.test".into()]))
        .await
        .unwrap();
    let first = recv(&mut on_a).await;
    assert_eq!(first.space.as_deref(), Some("crm"));
    let got = recv(&mut on_b).await;
    assert_eq!(got.kind, MailEventKind::Received);
    assert_eq!(got.ids, vec![msg.id]);
    assert_eq!(got.entry.as_ref().unwrap(), &msg.entry());
    assert_eq!(got.mailboxes, vec!["rcpt@example.test"]);
    assert_eq!(recv(&mut on_a).await.ids, vec![msg.id]);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), on_other.recv())
            .await
            .is_err(),
        "another prefix hears nothing"
    );
}

#[tokio::test]
async fn valkey_events_ignore_foreign_messages_on_the_channels() {
    let Some(url) = valkey_url() else {
        return;
    };
    let prefix = format!("fmtest:{}:", Uuid::new_v4().simple());
    let a = events(&url, &prefix).await;
    let mut sub = a.subscribe(SpaceFilter::Any);
    let mut conn = valkey_client(&url, None, &Default::default())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let wrong = serde_json::to_vec(&MailEvent::deleted(Some("crm".into()), vec![])).unwrap();
    for (channel, payload) in [
        (a.channel(Some("billing")), b"not json".to_vec()),
        (a.channel(Some("billing")), wrong),
    ] {
        redis::cmd("PUBLISH")
            .arg(channel)
            .arg(payload)
            .query_async::<i64>(&mut conn)
            .await
            .unwrap();
    }
    a.publish(&MailEvent::deleted(
        Some("billing".into()),
        vec![Uuid::nil()],
    ))
    .await
    .unwrap();
    let e = recv(&mut sub).await;
    assert_eq!(e.space.as_deref(), Some("billing"));
    assert_eq!(e.ids, vec![Uuid::nil()]);
}

struct Proxy {
    addr: std::net::SocketAddr,
    cut: tokio::sync::watch::Sender<u64>,
    down: Arc<std::sync::atomic::AtomicBool>,
}

async fn proxy(upstream: String) -> Proxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (cut, _) = tokio::sync::watch::channel(0u64);
    let down = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (cut2, down2) = (cut.clone(), down.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut client, _)) = listener.accept().await else {
                return;
            };
            if down2.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = client.shutdown().await;
                continue;
            }
            let mut rx = cut2.subscribe();
            let upstream = upstream.clone();
            tokio::spawn(async move {
                let Ok(mut server) = TcpStream::connect(&upstream).await else {
                    return;
                };
                tokio::select! {
                    _ = tokio::io::copy_bidirectional(&mut client, &mut server) => {}
                    _ = rx.changed() => {}
                }
            });
        }
    });
    Proxy { addr, cut, down }
}

#[tokio::test]
async fn valkey_events_resume_after_the_connection_drops() {
    let Some(url) = valkey_url() else {
        return;
    };
    let hostport = url
        .trim_start_matches("redis://")
        .trim_end_matches('/')
        .to_string();
    let p = proxy(hostport).await;
    let prefix = format!("fmtest:{}:", Uuid::new_v4().simple());
    let via_proxy = events(&format!("redis://{}/", p.addr), &prefix).await;
    let direct = events(&url, &prefix).await;
    let mut sub = via_proxy.subscribe(SpaceFilter::Any);
    direct
        .publish(&MailEvent::deleted(None, vec![Uuid::from_u128(1)]))
        .await
        .unwrap();
    assert_eq!(recv(&mut sub).await.ids, vec![Uuid::from_u128(1)]);

    p.down.store(true, std::sync::atomic::Ordering::SeqCst);
    p.cut.send_modify(|n| *n += 1);
    let t0 = std::time::Instant::now();
    while via_proxy.is_connected() {
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "the drop is noticed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let lost = MailEvent::deleted(None, vec![Uuid::from_u128(2)]);
    direct.publish(&lost).await.unwrap();
    let local = MailEvent::deleted(None, vec![Uuid::from_u128(3)]);
    assert!(
        via_proxy.publish(&local).await.is_err(),
        "publishing through a broken connection fails"
    );
    assert_eq!(
        recv(&mut sub).await.ids,
        vec![Uuid::from_u128(3)],
        "this replica's subscribers still get what it published"
    );

    p.down.store(false, std::sync::atomic::Ordering::SeqCst);
    let t0 = std::time::Instant::now();
    while !via_proxy.is_connected() {
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "the listener reconnects"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    direct
        .publish(&MailEvent::deleted(None, vec![Uuid::from_u128(4)]))
        .await
        .unwrap();
    assert_eq!(
        recv(&mut sub).await.ids,
        vec![Uuid::from_u128(4)],
        "events published while disconnected are lost, later ones arrive"
    );
    via_proxy
        .publish(&MailEvent::deleted(None, vec![Uuid::from_u128(5)]))
        .await
        .unwrap();
    assert_eq!(recv(&mut sub).await.ids, vec![Uuid::from_u128(5)]);
}

#[tokio::test]
async fn valkey_login_failures_are_shared_and_expire() {
    use fastmock_core::mail::{LoginFailures, ThrottlePolicy};
    let Some(url) = valkey_url() else {
        return;
    };
    let prefix = format!("fmtest:{}:", Uuid::new_v4().simple());
    let (a, b) = (events(&url, &prefix).await, events(&url, &prefix).await);
    let (fa, fb) = (a.login_failures(&prefix), b.login_failures(&prefix));
    let policy = ThrottlePolicy {
        limit: 2,
        window: Duration::from_secs(60),
        base_backoff: Duration::from_millis(400),
        max_backoff: Duration::from_millis(400),
    };
    fa.failed("ip:198.51.100.7", &policy).await;
    assert!(!fb.blocked("ip:198.51.100.7").await);
    fb.failed("ip:198.51.100.7", &policy).await;
    assert!(
        fa.blocked("ip:198.51.100.7").await,
        "counted across replicas"
    );
    assert!(!fa.blocked("ip:198.51.100.8").await);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!fa.blocked("ip:198.51.100.7").await, "the block expires");
    fa.failed("user:a@example.test", &policy).await;
    fa.failed("user:a@example.test", &policy).await;
    assert!(fb.blocked("user:a@example.test").await);
    fb.clear("user:a@example.test").await;
    assert!(!fa.blocked("user:a@example.test").await);
    let mut c = valkey_client(&url, None, &Default::default())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg(format!("{prefix}mail:login:*"))
        .query_async(&mut c)
        .await
        .unwrap();
    assert!(
        keys.iter()
            .all(|k| !k.contains("198.51.100") && !k.contains("example.test")),
        "{keys:?}"
    );
}
