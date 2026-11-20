//! Behaviour every [`MailStore`] must show, as async checks that panic on a difference. The memory store
//! runs them in its unit tests; the PostgreSQL store runs the same checks. Each check works in spaces of
//! its own, except [`retention`], whose sweep acts on every area: give it a store no other test uses.

use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::model::{Envelope, MailMessage, MailSource, MailSummary, micros};
use super::query::{MailFilter, MailQuery, MailTerm};
use super::store::*;
use crate::model::SpaceFilter;

/// A space name no other check uses.
pub fn space() -> String {
    format!("t{}", &Uuid::new_v4().simple().to_string()[..12])
}

/// A small message in `space`, received at `at`.
pub fn message(space: Option<&str>, at: DateTime<Utc>, subject: &str) -> MailMessage {
    let raw = format!(
        "From: sender@example.com\r\nTo: rcpt@example.test\r\nSubject: {subject}\r\n\r\n{subject} body text\r\n"
    );
    MailMessage {
        id: Uuid::now_v7(),
        space: space.map(String::from),
        received_at: micros(at),
        source: MailSource::Smtp,
        provider: None,
        envelope: Envelope {
            mail_from: "sender@example.com".into(),
            rcpt_to: vec!["rcpt@example.test".into()],
            ..Default::default()
        },
        summary: MailSummary {
            subject: Some(subject.to_string()),
            size: raw.len() as u64,
            ..Default::default()
        },
        labels: vec![],
        provider_extras: None,
        transcript: vec![],
        search_text: format!("{subject} body text").to_lowercase(),
        raw: Bytes::from(raw),
    }
}

/// Inserts `n` messages a second apart, ending now, each filed into the INBOX of `rcpt@example.test`;
/// returns their ids, oldest first.
pub async fn fill(store: &dyn MailStore, space: Option<&str>, n: usize) -> Vec<Uuid> {
    let start = Utc::now() - chrono::Duration::seconds(n as i64);
    let mut ids = Vec::new();
    for i in 0..n {
        let m = message(
            space,
            start + chrono::Duration::seconds(i as i64),
            &format!("message {i}"),
        );
        store
            .insert(&m, &[MailboxDelivery::inbox("rcpt@example.test")])
            .await
            .unwrap();
        ids.push(m.id);
    }
    ids
}

fn only(space: &str) -> SpaceFilter {
    SpaceFilter::In(vec![space.to_string()])
}

async fn ids_of(store: &dyn MailStore, q: &MailQuery) -> (Vec<Uuid>, Option<super::MailCursor>) {
    let p = store.list(q).await.unwrap();
    (p.items.iter().map(|e| e.id).collect(), p.next)
}

/// Insert, get, list with pages, filters, labels and deletes.
pub async fn messages(store: &dyn MailStore) {
    let (a, b) = (space(), space());
    let t0 = micros(Utc::now()) - chrono::Duration::seconds(60);
    let one = message(Some(&a), t0, "one");
    let two = message(Some(&a), t0 + chrono::Duration::seconds(1), "two");
    let three = message(Some(&a), t0 + chrono::Duration::seconds(2), "three");
    let tie = message(Some(&a), t0 + chrono::Duration::seconds(2), "tie");
    let other = message(Some(&b), t0, "other");
    for m in [&one, &two, &three, &tie, &other] {
        store.insert(m, &[]).await.unwrap();
    }
    assert!(store.insert(&one, &[]).await.is_err(), "duplicate id");
    let got = store.get(one.id, &only(&a)).await.unwrap().unwrap();
    assert_eq!(got, one);
    assert!(store.get(one.id, &only(&b)).await.unwrap().is_none());
    assert!(
        store
            .get(one.id, &SpaceFilter::Any)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .get(one.id, &SpaceFilter::Global)
            .await
            .unwrap()
            .is_none()
    );

    let newest = [tie.id, three.id, two.id, one.id];
    let mut q = MailQuery {
        spaces: only(&a),
        limit: 3,
        ..Default::default()
    };
    let (page1, next) = ids_of(store, &q).await;
    assert_eq!(page1, newest[..3].to_vec());
    q.cursor = Some(next.expect("a second page"));
    let (page2, next) = ids_of(store, &q).await;
    assert_eq!(page2, newest[3..].to_vec());
    assert!(next.is_none());

    let mut q = MailQuery {
        spaces: only(&a),
        limit: 1,
        oldest_first: true,
        ..Default::default()
    };
    let mut seen = Vec::new();
    loop {
        let (page, next) = ids_of(store, &q).await;
        seen.extend(page);
        match next {
            Some(c) => q.cursor = Some(c),
            None => break,
        }
    }
    let mut oldest = newest.to_vec();
    oldest.reverse();
    assert_eq!(seen, oldest);

    let both = SpaceFilter::In(vec![a.clone(), b.clone()]);
    let q = MailQuery {
        spaces: both.clone(),
        limit: 10,
        ..Default::default()
    };
    assert_eq!(ids_of(store, &q).await.0.len(), 5);
    let search = |terms: Vec<MailTerm>| MailQuery {
        spaces: both.clone(),
        filter: MailFilter { terms },
        limit: 10,
        ..Default::default()
    };
    assert_eq!(
        ids_of(store, &search(vec![MailTerm::Subject("tw".into())]))
            .await
            .0,
        vec![two.id]
    );
    assert_eq!(
        ids_of(store, &search(vec![MailTerm::Text("other body".into())]))
            .await
            .0,
        vec![other.id]
    );
    assert_eq!(
        ids_of(store, &search(vec![MailTerm::After(three.received_at)]))
            .await
            .0,
        vec![tie.id, three.id]
    );

    let labels = vec!["otp".to_string(), "welcome".to_string()];
    let e = store
        .set_labels(two.id, &only(&a), &labels)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(e.labels, labels);
    assert!(
        store
            .set_labels(two.id, &only(&b), &labels)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        ids_of(store, &search(vec![MailTerm::Label("otp".into())]))
            .await
            .0,
        vec![two.id]
    );
    assert_eq!(
        store
            .get(two.id, &SpaceFilter::Any)
            .await
            .unwrap()
            .unwrap()
            .labels,
        labels
    );

    assert!(store.delete(other.id, &only(&a)).await.unwrap().is_none());
    assert_eq!(
        store.delete(other.id, &only(&b)).await.unwrap(),
        Some(MailRef {
            space: Some(b.clone()),
            id: other.id
        })
    );
    assert!(
        store
            .get(other.id, &SpaceFilter::Any)
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.delete(other.id, &only(&b)).await.unwrap().is_none());

    let gone = store
        .delete_matching(
            &only(&a),
            &MailFilter {
                terms: vec![MailTerm::Subject("one".into())],
            },
        )
        .await
        .unwrap();
    assert_eq!(gone.iter().map(|r| r.id).collect::<Vec<_>>(), vec![one.id]);
    let mut gone: Vec<Uuid> = store
        .delete_matching(&only(&a), &MailFilter::default())
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    gone.sort();
    let mut want = vec![two.id, three.id, tie.id];
    want.sort();
    assert_eq!(gone, want);
    assert!(ids_of(store, &q).await.0.is_empty());
}

async fn uids(store: &dyn MailStore, key: &MailboxKey) -> Vec<u32> {
    store
        .mailbox_entries(key)
        .await
        .unwrap()
        .iter()
        .map(|e| e.uid)
        .collect()
}

/// Mailboxes, folders, UIDs, flags, copy, expunge and passwords.
pub async fn mailboxes(store: &dyn MailStore) {
    let s = space();
    let sp = Some(s.as_str());
    let (a, b) = ("a@example.test", "b@example.test");
    let inbox = MailboxKey::inbox(sp, a);
    let now = micros(Utc::now());
    let m1 = message(sp, now - chrono::Duration::seconds(2), "m1");
    let m2 = message(sp, now - chrono::Duration::seconds(1), "m2");
    store
        .insert(&m1, &[MailboxDelivery::inbox(a), MailboxDelivery::inbox(b)])
        .await
        .unwrap();
    store
        .insert(
            &m2,
            &[MailboxDelivery {
                address: a.into(),
                folder: "inbox".into(),
                flags: vec!["\\seen".into()],
            }],
        )
        .await
        .unwrap();
    let folders = store.mailbox_folders(sp, a).await.unwrap();
    assert_eq!(folders.len(), 1);
    let first = &folders[0];
    assert_eq!(
        (
            first.folder.as_str(),
            first.exists,
            first.unseen,
            first.uid_next
        ),
        (INBOX, 2, 1, 3)
    );
    assert!(first.uid_validity > 0);
    let entries = store.mailbox_entries(&inbox).await.unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|e| (e.uid, e.message_id))
            .collect::<Vec<_>>(),
        vec![(1, m1.id), (2, m2.id)]
    );
    assert_eq!(entries[1].flags, vec!["\\Seen".to_string()]);
    assert_eq!(entries[0].size, m1.raw.len() as u64);
    assert_eq!(entries[0].received_at, m1.received_at);
    assert!(
        store
            .mailbox_entries(&MailboxKey::inbox(Some(&space()), a))
            .await
            .unwrap()
            .is_empty()
    );

    let flagged = store
        .mailbox_store_flags(
            &inbox,
            &[1, 9],
            FlagOp::Add,
            &["\\Deleted".into(), "$Label1".into()],
        )
        .await
        .unwrap();
    assert_eq!(flagged.len(), 1);
    assert_eq!(flagged[0].flags, vec!["$Label1", "\\Deleted"]);
    let flagged = store
        .mailbox_store_flags(
            &inbox,
            &[1],
            FlagOp::Remove,
            &["$label1".into(), "$Label1".into()],
        )
        .await
        .unwrap();
    assert_eq!(flagged[0].flags, vec!["\\Deleted"]);
    let flagged = store
        .mailbox_store_flags(&inbox, &[2], FlagOp::Replace, &["\\Flagged".into()])
        .await
        .unwrap();
    assert_eq!(flagged[0].flags, vec!["\\Flagged"]);
    assert_eq!(store.mailbox_info(&inbox).await.unwrap().unwrap().unseen, 2);

    let archive = MailboxKey {
        folder: "Archive".into(),
        ..inbox.clone()
    };
    assert!(store.mailbox_create(&archive).await.unwrap());
    assert!(!store.mailbox_create(&archive).await.unwrap());
    assert!(
        !store
            .mailbox_create(&MailboxKey {
                folder: "Inbox".into(),
                ..inbox.clone()
            })
            .await
            .unwrap()
    );
    let names: Vec<String> = store
        .mailbox_folders(sp, a)
        .await
        .unwrap()
        .into_iter()
        .map(|f| f.folder)
        .collect();
    assert_eq!(names, vec![INBOX.to_string(), "Archive".to_string()]);
    assert_eq!(
        store
            .mailbox_copy(&inbox, &[2, 1], "Archive")
            .await
            .unwrap(),
        Some(vec![(1, 1), (2, 2)])
    );
    assert_eq!(
        store.mailbox_copy(&inbox, &[1], "Missing").await.unwrap(),
        None
    );
    assert_eq!(
        store.mailbox_entries(&archive).await.unwrap()[0].flags,
        vec!["\\Deleted"]
    );

    assert_eq!(
        store.mailbox_expunge(&inbox, Some(&[2])).await.unwrap(),
        Vec::<u32>::new()
    );
    assert_eq!(store.mailbox_expunge(&inbox, None).await.unwrap(), vec![1]);
    assert_eq!(uids(store, &inbox).await, vec![2]);
    assert!(store.get(m1.id, &SpaceFilter::Any).await.unwrap().is_some());
    assert_eq!(uids(store, &MailboxKey::inbox(sp, b)).await, vec![1]);
    assert_eq!(
        store.mailbox_remove(&inbox, &[2, 7]).await.unwrap(),
        vec![2]
    );
    assert!(uids(store, &inbox).await.is_empty());

    let m3 = message(sp, now, "m3");
    store
        .insert(&m3, &[MailboxDelivery::inbox(a)])
        .await
        .unwrap();
    assert_eq!(uids(store, &inbox).await, vec![3]);

    store
        .delete(m1.id, &SpaceFilter::Any)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(uids(store, &archive).await, vec![2]);
    assert!(uids(store, &MailboxKey::inbox(sp, b)).await.is_empty());

    let before = store.mailbox_info(&archive).await.unwrap().unwrap();
    assert!(store.mailbox_delete(&archive).await.unwrap());
    assert!(!store.mailbox_delete(&archive).await.unwrap());
    assert!(!store.mailbox_delete(&inbox).await.unwrap());
    assert!(store.mailbox_info(&archive).await.unwrap().is_none());
    assert!(store.mailbox_create(&archive).await.unwrap());
    let after = store.mailbox_info(&archive).await.unwrap().unwrap();
    assert!(after.uid_validity > before.uid_validity);
    assert_eq!((after.uid_next, after.exists), (1, 0));

    let owners = store.mailboxes(&only(&s)).await.unwrap();
    assert_eq!(
        owners
            .iter()
            .map(|o| (o.address.as_str(), o.has_password))
            .collect::<Vec<_>>(),
        vec![(a, false), (b, false)]
    );
    assert_eq!(owners[0].space.as_deref(), sp);
    assert!(store.set_mailbox_password(sp, a, Some("h1")).await.unwrap());
    assert_eq!(
        store.mailbox_password(sp, a).await.unwrap().as_deref(),
        Some("h1")
    );
    assert!(store.mailbox_password(None, a).await.unwrap().is_none());
    assert!(store.mailboxes(&only(&s)).await.unwrap()[0].has_password);
    assert!(store.set_mailbox_password(sp, a, None).await.unwrap());
    assert!(!store.set_mailbox_password(sp, a, None).await.unwrap());
    let c = "c@example.test";
    assert!(store.set_mailbox_password(sp, c, Some("h2")).await.unwrap());
    let folders = store.mailbox_folders(sp, c).await.unwrap();
    assert_eq!(
        folders
            .iter()
            .map(|f| f.folder.as_str())
            .collect::<Vec<_>>(),
        vec![INBOX]
    );
}

/// Retention by age and by count per area. Sweeps every area of the store.
pub async fn retention(store: &dyn MailStore) {
    let (r1, r2) = (space(), space());
    let now = micros(Utc::now());
    let day = chrono::Duration::days(1);
    let mut old = Vec::new();
    let mut r1_ids = Vec::new();
    for (i, back) in [9, 8, 3, 2, 1].into_iter().enumerate() {
        let m = message(Some(&r1), now - day * back, &format!("r1 {i}"));
        store
            .insert(&m, &[MailboxDelivery::inbox("rcpt@example.test")])
            .await
            .unwrap();
        if back > 7 {
            old.push(m.id);
        }
        r1_ids.push(m.id);
    }
    let r2_ids = fill(store, Some(&r2), 4).await;
    let policy = MailRetention {
        max_messages: 3,
        max_age: Duration::from_secs(7 * 86_400),
    };
    let ours = |refs: Vec<MailRef>| -> Vec<(String, Uuid)> {
        let mut v: Vec<(String, Uuid)> = refs
            .into_iter()
            .filter_map(|r| r.space.filter(|s| *s == r1 || *s == r2).map(|s| (s, r.id)))
            .collect();
        v.sort();
        v
    };
    let mut want = vec![
        (r1.clone(), old[0]),
        (r1.clone(), old[1]),
        (r2.clone(), r2_ids[0]),
    ];
    want.sort();
    assert_eq!(ours(store.sweep(&policy, now).await.unwrap()), want);
    assert!(ours(store.sweep(&policy, now).await.unwrap()).is_empty());
    let q = MailQuery {
        spaces: only(&r1),
        oldest_first: true,
        ..Default::default()
    };
    assert_eq!(ids_of(store, &q).await.0, r1_ids[2..].to_vec());
    let inbox = MailboxKey::inbox(Some(&r1), "rcpt@example.test");
    assert_eq!(uids(store, &inbox).await, vec![3, 4, 5]);
}
