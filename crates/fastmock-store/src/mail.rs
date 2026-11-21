//! Captured mail in PostgreSQL: [`PgMailStore`] implements [`MailStore`] on the tables of migration 0011.
//!
//! Messages live in `fm_mail_messages` with the raw bytes, the parsed parts as `jsonb` and lowercase search
//! columns (`subject_lower`, `from_terms`, `to_terms`, `cc_terms`) that the search terms compare with
//! `strpos`, so no extension is needed. Mailboxes (`fm_mail_mailboxes`, keyed by area and address, the global
//! area being `''`), their folders (`fm_mail_folders`, with `uid_validity` and `uid_next`) and the folder
//! entries (`fm_mail_entries`, deleted with their message or folder) hold the IMAP/POP3 state.
//!
//! UIDs come from `uid_next` of the folder row, updated in the inserting transaction: the row lock
//! serialises writers of one folder across replicas, so UIDs are unique and become visible in increasing
//! order. A transaction that files into several folders locks them in a fixed order. `UIDVALIDITY` comes from
//! the sequence `fm_mail_uid_validity`, seeded with the Unix time when the migration ran.

use std::collections::BTreeSet;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use fastmock_core::SpaceFilter;
use fastmock_core::StoreResult;
use fastmock_core::mail::{
    FlagOp, INBOX, MailAddress, MailCursor, MailEntry, MailFilter, MailMessage, MailPage,
    MailQuery, MailRef, MailRetention, MailSource, MailStore, MailTerm, MailboxDelivery,
    MailboxEntry, MailboxInfo, MailboxKey, MailboxOwner, flag_name, folder_name,
};
use serde_json::Value;
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Postgres, QueryBuilder, Row, Transaction};
use uuid::Uuid;

use crate::postgres::no_nul;

/// The advisory lock a retention sweep holds for its transaction, so that one replica sweeps at a time.
pub const MAIL_SWEEP_LOCK: i64 = 0x666d_5f6d_6169_6c01;

/// Columns of a message as lists show it. Queries name their columns, never `*` (see the journal).
const ENTRY_COLUMNS: &str =
    "id, space, received_at, source, provider, envelope, summary, labels, provider_extras";

/// Columns of a whole message.
const MESSAGE_COLUMNS: &str = "id, space, received_at, source, provider, envelope, summary, labels, provider_extras, transcript, search_text, raw";

const SEEN: &str = "\\Seen";
const DELETED: &str = "\\Deleted";

/// Messages and bytes of raw mail stored for one area.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailUsage {
    pub space: Option<String>,
    pub messages: u64,
    pub bytes: u64,
}

/// [`MailStore`] on PostgreSQL; cheap to clone.
#[derive(Clone)]
pub struct PgMailStore {
    pool: PgPool,
}

impl PgMailStore {
    /// A store on the pool of a migrated database (see [`crate::PgStore::pool`]).
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Stored messages and raw bytes per area, sorted with the global area first.
    pub async fn usage(&self) -> StoreResult<Vec<MailUsage>> {
        let rows: Vec<(Option<String>, i64, i64)> = sqlx::query_as(
            "select space, count(*), coalesce(sum(raw_size), 0)::bigint from fm_mail_messages group by space order by space nulls first",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(space, messages, bytes)| MailUsage {
                space,
                messages: messages.max(0) as u64,
                bytes: bytes.max(0) as u64,
            })
            .collect())
    }
}

fn area(space: Option<&str>) -> String {
    space.map(no_nul).unwrap_or_default()
}

fn key_parts(key: &MailboxKey) -> (String, String, String) {
    (
        area(key.space.as_deref()),
        no_nul(&key.address),
        no_nul(&folder_name(&key.folder)),
    )
}

/// Canonical flags, sorted by bytes (as the memory store's `BTreeSet`) and without duplicates.
fn flag_list(flags: &[String]) -> Vec<String> {
    flags
        .iter()
        .map(|f| no_nul(&flag_name(f)))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// `jsonb` cannot hold U+0000; it becomes U+FFFD, as in the journal.
fn json_no_nul(v: Value) -> Value {
    match v {
        Value::String(s) if s.contains('\0') => Value::String(no_nul(&s)),
        Value::Array(a) => Value::Array(a.into_iter().map(json_no_nul).collect()),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, v)| (no_nul(&k), json_no_nul(v)))
                .collect(),
        ),
        other => other,
    }
}

fn to_json<T: serde::Serialize>(v: &T) -> StoreResult<Value> {
    Ok(json_no_nul(serde_json::to_value(v)?))
}

fn address_terms<'a>(
    list: &[MailAddress],
    extra: impl IntoIterator<Item = &'a String>,
) -> Vec<String> {
    let mut v = Vec::new();
    for a in list {
        v.push(no_nul(&a.address.to_lowercase()));
        if let Some(n) = &a.name {
            v.push(no_nul(&n.to_lowercase()));
        }
    }
    v.extend(extra.into_iter().map(|s| no_nul(&s.to_lowercase())));
    v
}

fn push_spaces<'a>(qb: &mut QueryBuilder<'a, Postgres>, f: &'a SpaceFilter) {
    match f {
        SpaceFilter::Any => {}
        SpaceFilter::Global => {
            qb.push(" and space is null");
        }
        SpaceFilter::In(list) if list.len() == 1 => {
            qb.push(" and space = ").push_bind(&list[0]);
        }
        SpaceFilter::In(list) => {
            qb.push(" and space = any(").push_bind(list).push(")");
        }
    }
}

fn push_areas<'a>(qb: &mut QueryBuilder<'a, Postgres>, f: &'a SpaceFilter) {
    match f {
        SpaceFilter::Any => {}
        SpaceFilter::Global => {
            qb.push(" and m.area = ''");
        }
        SpaceFilter::In(list) => {
            qb.push(" and m.area = any(").push_bind(list).push(")");
        }
    }
}

fn push_filter(qb: &mut QueryBuilder<'_, Postgres>, f: &MailFilter) {
    for t in &f.terms {
        match t {
            MailTerm::From(v) => {
                qb.push(" and exists (select 1 from unnest(from_terms) as t(v) where strpos(t.v, ")
                    .push_bind(no_nul(v))
                    .push(") > 0)");
            }
            MailTerm::To(v) => {
                qb.push(" and exists (select 1 from unnest(to_terms) as t(v) where strpos(t.v, ")
                    .push_bind(no_nul(v))
                    .push(") > 0)");
            }
            MailTerm::Cc(v) => {
                qb.push(" and exists (select 1 from unnest(cc_terms) as t(v) where strpos(t.v, ")
                    .push_bind(no_nul(v))
                    .push(") > 0)");
            }
            MailTerm::Subject(v) => {
                qb.push(" and coalesce(strpos(subject_lower, ")
                    .push_bind(no_nul(v))
                    .push("), 0) > 0");
            }
            MailTerm::HasAttachment => {
                qb.push(" and has_attachments");
            }
            MailTerm::Label(v) => {
                qb.push(" and ").push_bind(no_nul(v)).push(" = any(labels)");
            }
            MailTerm::After(at) => {
                qb.push(" and received_at >= ").push_bind(*at);
            }
            MailTerm::Before(at) => {
                qb.push(" and received_at < ").push_bind(*at);
            }
            MailTerm::Text(v) => {
                let v = no_nul(v);
                qb.push(" and (strpos(search_text, ")
                    .push_bind(v.clone())
                    .push(") > 0 or coalesce(strpos(subject_lower, ")
                    .push_bind(v)
                    .push("), 0) > 0)");
            }
        }
    }
}

fn source_of(r: &PgRow) -> StoreResult<MailSource> {
    let s: String = r.try_get("source")?;
    MailSource::parse(&s).ok_or_else(|| anyhow::anyhow!("unknown mail source `{s}`"))
}

fn row_to_entry(r: &PgRow) -> StoreResult<MailEntry> {
    let extras: Option<Value> = r.try_get("provider_extras")?;
    Ok(MailEntry {
        id: r.try_get("id")?,
        space: r.try_get("space")?,
        received_at: r.try_get("received_at")?,
        source: source_of(r)?,
        provider: r.try_get("provider")?,
        envelope: serde_json::from_value(r.try_get("envelope")?)?,
        summary: serde_json::from_value(r.try_get("summary")?)?,
        labels: r.try_get("labels")?,
        provider_extras: extras.map(serde_json::from_value).transpose()?,
    })
}

fn row_to_message(r: &PgRow) -> StoreResult<MailMessage> {
    let e = row_to_entry(r)?;
    let raw: Vec<u8> = r.try_get("raw")?;
    Ok(MailMessage {
        id: e.id,
        space: e.space,
        received_at: e.received_at,
        source: e.source,
        provider: e.provider,
        envelope: e.envelope,
        summary: e.summary,
        labels: e.labels,
        provider_extras: e.provider_extras,
        transcript: serde_json::from_value(r.try_get("transcript")?)?,
        search_text: r.try_get("search_text")?,
        raw: Bytes::from(raw),
    })
}

fn refs(rows: Vec<(Uuid, Option<String>)>) -> Vec<MailRef> {
    rows.into_iter()
        .map(|(id, space)| MailRef { space, id })
        .collect()
}

fn u32_of(v: i64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

fn uids_param(uids: &[u32]) -> Vec<i64> {
    uids.iter().map(|u| i64::from(*u)).collect()
}

fn sorted_uids(mut v: Vec<i64>) -> Vec<u32> {
    v.sort_unstable();
    v.into_iter().map(u32_of).collect()
}

async fn mailbox_id(
    tx: &mut Transaction<'_, Postgres>,
    area: &str,
    address: &str,
) -> StoreResult<i64> {
    let find = "select id from fm_mail_mailboxes where area = $1 and address = $2";
    if let Some(id) = sqlx::query_scalar(find)
        .bind(area)
        .bind(address)
        .fetch_optional(&mut **tx)
        .await?
    {
        return Ok(id);
    }
    sqlx::query(
        "insert into fm_mail_mailboxes (area, address) values ($1, $2) on conflict (area, address) do nothing",
    )
    .bind(area)
    .bind(address)
    .execute(&mut **tx)
    .await?;
    Ok(sqlx::query_scalar(find)
        .bind(area)
        .bind(address)
        .fetch_one(&mut **tx)
        .await?)
}

const FIND_FOLDER: &str = "select f.id from fm_mail_folders f join fm_mail_mailboxes m on m.id = f.mailbox_id where m.area = $1 and m.address = $2 and f.name = $3";

/// The folder's id, creating the mailbox and folder when missing; `true` when this call created the folder.
async fn ensure_folder(
    tx: &mut Transaction<'_, Postgres>,
    area: &str,
    address: &str,
    folder: &str,
) -> StoreResult<(i64, bool)> {
    if let Some(id) = sqlx::query_scalar(FIND_FOLDER)
        .bind(area)
        .bind(address)
        .bind(folder)
        .fetch_optional(&mut **tx)
        .await?
    {
        return Ok((id, false));
    }
    let mailbox = mailbox_id(tx, area, address).await?;
    let created: Option<i64> = sqlx::query_scalar(
        "insert into fm_mail_folders (mailbox_id, name, uid_validity, uid_next) values ($1, $2, nextval('fm_mail_uid_validity'), 1)
         on conflict (mailbox_id, name) do nothing returning id",
    )
    .bind(mailbox)
    .bind(folder)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(id) = created {
        return Ok((id, true));
    }
    Ok((
        sqlx::query_scalar("select id from fm_mail_folders where mailbox_id = $1 and name = $2")
            .bind(mailbox)
            .bind(folder)
            .fetch_one(&mut **tx)
            .await?,
        false,
    ))
}

/// Reserves `n` UIDs of a folder; the row stays locked until the transaction ends.
async fn reserve_uids(tx: &mut Transaction<'_, Postgres>, folder: i64, n: i64) -> StoreResult<i64> {
    let next: i64 = sqlx::query_scalar(
        "update fm_mail_folders set uid_next = uid_next + $2 where id = $1 returning uid_next - $2",
    )
    .bind(folder)
    .bind(n)
    .fetch_one(&mut **tx)
    .await?;
    Ok(next)
}

const ENTRY_SELECT: &str = "select e.uid, e.message_id, e.flags, msg.received_at, msg.raw_size from fm_mail_entries e join fm_mail_messages msg on msg.id = e.message_id";

fn row_to_mailbox_entry(r: &PgRow) -> StoreResult<MailboxEntry> {
    Ok(MailboxEntry {
        uid: u32_of(r.try_get("uid")?),
        message_id: r.try_get("message_id")?,
        flags: r.try_get("flags")?,
        received_at: r.try_get("received_at")?,
        size: r.try_get::<i64, _>("raw_size")?.max(0) as u64,
    })
}

const FOLDER_INFO: &str = "select f.name, f.uid_validity, f.uid_next, count(e.uid) as exists_count, count(e.uid) filter (where not ($3 = any(e.flags))) as unseen_count
     from fm_mail_folders f join fm_mail_mailboxes m on m.id = f.mailbox_id left join fm_mail_entries e on e.folder_id = f.id
     where m.area = $1 and m.address = $2";

fn row_to_info(r: &PgRow) -> StoreResult<MailboxInfo> {
    Ok(MailboxInfo {
        folder: r.try_get("name")?,
        uid_validity: u32_of(r.try_get("uid_validity")?),
        uid_next: u32_of(r.try_get("uid_next")?),
        exists: u32_of(r.try_get("exists_count")?),
        unseen: u32_of(r.try_get("unseen_count")?),
    })
}

impl PgMailStore {
    async fn folder_id(&self, key: &MailboxKey) -> StoreResult<Option<i64>> {
        let (area, address, folder) = key_parts(key);
        Ok(sqlx::query_scalar(FIND_FOLDER)
            .bind(area)
            .bind(address)
            .bind(folder)
            .fetch_optional(&self.pool)
            .await?)
    }
}

#[async_trait]
impl MailStore for PgMailStore {
    async fn insert(&self, msg: &MailMessage, deliveries: &[MailboxDelivery]) -> StoreResult<()> {
        let s = &msg.summary;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "insert into fm_mail_messages (id, space, received_at, source, provider, envelope, summary, labels, provider_extras, transcript, search_text, subject_lower, from_terms, to_terms, cc_terms, has_attachments, raw_size, raw)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)",
        )
        .bind(msg.id)
        .bind(msg.space.as_deref().map(no_nul))
        .bind(msg.received_at)
        .bind(msg.source.as_str())
        .bind(msg.provider.as_deref().map(no_nul))
        .bind(to_json(&msg.envelope)?)
        .bind(to_json(&msg.summary)?)
        .bind(msg.labels.iter().map(|l| no_nul(l)).collect::<Vec<_>>())
        .bind(msg.provider_extras.as_ref().map(to_json).transpose()?)
        .bind(to_json(&msg.transcript)?)
        .bind(no_nul(&msg.search_text))
        .bind(s.subject.as_deref().map(|x| no_nul(&x.to_lowercase())))
        .bind(address_terms(&s.from, [&msg.envelope.mail_from]))
        .bind(address_terms(&s.to, &msg.envelope.rcpt_to))
        .bind(address_terms(&s.cc, []))
        .bind(s.has_attachments)
        .bind(msg.raw.len() as i64)
        .bind(msg.raw.as_ref())
        .execute(&mut *tx)
        .await?;
        let area = area(msg.space.as_deref());
        let mut ds: Vec<(String, String, Vec<String>)> = deliveries
            .iter()
            .map(|d| {
                (
                    no_nul(&d.address),
                    no_nul(&folder_name(&d.folder)),
                    flag_list(&d.flags),
                )
            })
            .collect();
        ds.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        for (address, folder, flags) in ds {
            let (folder_id, _) = ensure_folder(&mut tx, &area, &address, &folder).await?;
            let uid = reserve_uids(&mut tx, folder_id, 1).await?;
            sqlx::query(
                "insert into fm_mail_entries (folder_id, uid, message_id, flags) values ($1, $2, $3, $4)",
            )
            .bind(folder_id)
            .bind(uid)
            .bind(msg.id)
            .bind(flags)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn get(&self, id: Uuid, spaces: &SpaceFilter) -> StoreResult<Option<MailMessage>> {
        let mut qb = QueryBuilder::new(format!(
            "select {MESSAGE_COLUMNS} from fm_mail_messages where id = "
        ));
        qb.push_bind(id);
        push_spaces(&mut qb, spaces);
        let row = qb.build().fetch_optional(&self.pool).await?;
        row.as_ref().map(row_to_message).transpose()
    }

    async fn list(&self, query: &MailQuery) -> StoreResult<MailPage> {
        let limit = query.page_size();
        let mut qb = QueryBuilder::new(format!(
            "select {ENTRY_COLUMNS} from fm_mail_messages where true"
        ));
        push_spaces(&mut qb, &query.spaces);
        push_filter(&mut qb, &query.filter);
        if let Some(c) = query.cursor {
            qb.push(if query.oldest_first {
                " and (received_at, id) > ("
            } else {
                " and (received_at, id) < ("
            })
            .push_bind(c.received_at)
            .push(", ")
            .push_bind(c.id)
            .push(")");
        }
        qb.push(if query.oldest_first {
            " order by received_at, id limit "
        } else {
            " order by received_at desc, id desc limit "
        })
        .push_bind(limit as i64 + 1);
        let rows = qb.build().fetch_all(&self.pool).await?;
        let mut items = rows
            .iter()
            .map(row_to_entry)
            .collect::<StoreResult<Vec<_>>>()?;
        let more = items.len() > limit;
        items.truncate(limit);
        Ok(MailPage {
            next: if more {
                items.last().map(MailCursor::of)
            } else {
                None
            },
            items,
        })
    }

    async fn delete(&self, id: Uuid, spaces: &SpaceFilter) -> StoreResult<Option<MailRef>> {
        let mut qb = QueryBuilder::new("delete from fm_mail_messages where id = ");
        qb.push_bind(id);
        push_spaces(&mut qb, spaces);
        qb.push(" returning id, space");
        let row: Option<(Uuid, Option<String>)> =
            qb.build_query_as().fetch_optional(&self.pool).await?;
        Ok(row.map(|(id, space)| MailRef { space, id }))
    }

    async fn delete_matching(
        &self,
        spaces: &SpaceFilter,
        filter: &MailFilter,
    ) -> StoreResult<Vec<MailRef>> {
        let mut qb = QueryBuilder::new("delete from fm_mail_messages where true");
        push_spaces(&mut qb, spaces);
        push_filter(&mut qb, filter);
        qb.push(" returning id, space");
        Ok(refs(qb.build_query_as().fetch_all(&self.pool).await?))
    }

    async fn set_labels(
        &self,
        id: Uuid,
        spaces: &SpaceFilter,
        labels: &[String],
    ) -> StoreResult<Option<MailEntry>> {
        let mut qb = QueryBuilder::new("update fm_mail_messages set labels = ");
        qb.push_bind(labels.iter().map(|l| no_nul(l)).collect::<Vec<_>>())
            .push(" where id = ")
            .push_bind(id);
        push_spaces(&mut qb, spaces);
        qb.push(format!(" returning {ENTRY_COLUMNS}"));
        let row = qb.build().fetch_optional(&self.pool).await?;
        row.as_ref().map(row_to_entry).transpose()
    }

    async fn sweep(&self, policy: &MailRetention, now: DateTime<Utc>) -> StoreResult<Vec<MailRef>> {
        let mut tx = self.pool.begin().await?;
        let locked: bool = sqlx::query_scalar("select pg_try_advisory_xact_lock($1)")
            .bind(MAIL_SWEEP_LOCK)
            .fetch_one(&mut *tx)
            .await?;
        if !locked {
            tx.rollback().await?;
            return Ok(vec![]);
        }
        let mut out = Vec::new();
        let cutoff = chrono::Duration::from_std(policy.max_age)
            .ok()
            .and_then(|d| now.checked_sub_signed(d))
            .filter(|c| c.timestamp() > -210_000_000_000);
        if let Some(cutoff) = cutoff {
            out.extend(refs(
                sqlx::query_as(
                    "delete from fm_mail_messages where received_at < $1 returning id, space",
                )
                .bind(cutoff)
                .fetch_all(&mut *tx)
                .await?,
            ));
        }
        let max = i64::try_from(policy.max_messages.max(1)).unwrap_or(i64::MAX);
        out.extend(refs(
            sqlx::query_as(
                "delete from fm_mail_messages m using (
                    select id from (
                        select id, row_number() over (partition by space order by received_at desc, id desc) as n
                        from fm_mail_messages
                    ) ranked where ranked.n > $1
                 ) doomed where m.id = doomed.id returning m.id, m.space",
            )
            .bind(max)
            .fetch_all(&mut *tx)
            .await?,
        ));
        tx.commit().await?;
        Ok(out)
    }

    async fn mailbox_folders(
        &self,
        space: Option<&str>,
        address: &str,
    ) -> StoreResult<Vec<MailboxInfo>> {
        let rows = sqlx::query(&format!(
            "{FOLDER_INFO} group by f.id, f.name, f.uid_validity, f.uid_next order by f.name <> $4, f.name collate \"C\""
        ))
        .bind(area(space))
        .bind(no_nul(address))
        .bind(SEEN)
        .bind(INBOX)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_info).collect()
    }

    async fn mailboxes(&self, spaces: &SpaceFilter) -> StoreResult<Vec<MailboxOwner>> {
        let mut qb = QueryBuilder::new(
            "select m.area, m.address, m.password_hash is not null as has_password,
                    coalesce(array_agg(f.name order by f.name collate \"C\") filter (where f.name is not null), '{}') as folders
             from fm_mail_mailboxes m left join fm_mail_folders f on f.mailbox_id = m.id where true",
        );
        push_areas(&mut qb, spaces);
        qb.push(
            " group by m.id, m.area, m.address, m.password_hash order by m.area collate \"C\", m.address collate \"C\"",
        );
        let rows = qb.build().fetch_all(&self.pool).await?;
        rows.iter()
            .map(|r| {
                let area: String = r.try_get("area")?;
                Ok(MailboxOwner {
                    space: (!area.is_empty()).then_some(area),
                    address: r.try_get("address")?,
                    folders: r.try_get("folders")?,
                    has_password: r.try_get("has_password")?,
                })
            })
            .collect()
    }

    async fn mailbox_create(&self, key: &MailboxKey) -> StoreResult<bool> {
        let (area, address, folder) = key_parts(key);
        let mut tx = self.pool.begin().await?;
        let (_, created) = ensure_folder(&mut tx, &area, &address, &folder).await?;
        tx.commit().await?;
        Ok(created)
    }

    async fn mailbox_delete(&self, key: &MailboxKey) -> StoreResult<bool> {
        let (area, address, folder) = key_parts(key);
        if folder == INBOX {
            return Ok(false);
        }
        let n = sqlx::query(
            "delete from fm_mail_folders f using fm_mail_mailboxes m where f.mailbox_id = m.id and m.area = $1 and m.address = $2 and f.name = $3",
        )
        .bind(area)
        .bind(address)
        .bind(folder)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    async fn mailbox_info(&self, key: &MailboxKey) -> StoreResult<Option<MailboxInfo>> {
        let (area, address, folder) = key_parts(key);
        let row = sqlx::query(&format!(
            "{FOLDER_INFO} and f.name = $4 group by f.id, f.name, f.uid_validity, f.uid_next"
        ))
        .bind(area)
        .bind(address)
        .bind(SEEN)
        .bind(folder)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(row_to_info).transpose()
    }

    async fn mailbox_entries(&self, key: &MailboxKey) -> StoreResult<Vec<MailboxEntry>> {
        let Some(folder) = self.folder_id(key).await? else {
            return Ok(vec![]);
        };
        let rows = sqlx::query(&format!(
            "{ENTRY_SELECT} where e.folder_id = $1 order by e.uid"
        ))
        .bind(folder)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_mailbox_entry).collect()
    }

    async fn mailbox_store_flags(
        &self,
        key: &MailboxKey,
        uids: &[u32],
        op: FlagOp,
        flags: &[String],
    ) -> StoreResult<Vec<MailboxEntry>> {
        let Some(folder) = self.folder_id(key).await? else {
            return Ok(vec![]);
        };
        let set = match op {
            FlagOp::Add => {
                "array(select distinct x.f collate \"C\" from unnest(e.flags || $3::text[]) as x(f) order by 1)"
            }
            FlagOp::Remove => {
                "array(select x.f from unnest(e.flags) as x(f) where x.f <> all($3::text[]) order by x.f collate \"C\")"
            }
            FlagOp::Replace => "$3::text[]",
        };
        let rows = sqlx::query(&format!(
            "update fm_mail_entries e set flags = {set} from fm_mail_messages msg
             where msg.id = e.message_id and e.folder_id = $1 and e.uid = any($2)
             returning e.uid, e.message_id, e.flags, msg.received_at, msg.raw_size"
        ))
        .bind(folder)
        .bind(uids_param(uids))
        .bind(flag_list(flags))
        .fetch_all(&self.pool)
        .await?;
        let mut v = rows
            .iter()
            .map(row_to_mailbox_entry)
            .collect::<StoreResult<Vec<_>>>()?;
        v.sort_by_key(|e| e.uid);
        Ok(v)
    }

    async fn mailbox_copy(
        &self,
        key: &MailboxKey,
        uids: &[u32],
        target: &str,
    ) -> StoreResult<Option<Vec<(u32, u32)>>> {
        let target_key = MailboxKey {
            folder: folder_name(target),
            ..key.clone()
        };
        let Some(target_id) = self.folder_id(&target_key).await? else {
            return Ok(None);
        };
        let mut tx = self.pool.begin().await?;
        let locked: Option<i64> =
            sqlx::query_scalar("select id from fm_mail_folders where id = $1 for update")
                .bind(target_id)
                .fetch_optional(&mut *tx)
                .await?;
        if locked.is_none() {
            tx.rollback().await?;
            return Ok(None);
        }
        let source: Vec<(i64, Uuid, Vec<String>)> = match self.folder_id(key).await? {
            Some(src) => {
                sqlx::query_as(
                    "select e.uid, e.message_id, e.flags from fm_mail_entries e join fm_mail_messages msg on msg.id = e.message_id
                     where e.folder_id = $1 and e.uid = any($2) order by e.uid for key share of msg",
                )
                .bind(src)
                .bind(uids_param(uids))
                .fetch_all(&mut *tx)
                .await?
            }
            None => vec![],
        };
        if source.is_empty() {
            tx.commit().await?;
            return Ok(Some(vec![]));
        }
        let first = reserve_uids(&mut tx, target_id, source.len() as i64).await?;
        let mut pairs = Vec::with_capacity(source.len());
        let mut qb = QueryBuilder::<Postgres>::new(
            "insert into fm_mail_entries (folder_id, uid, message_id, flags) ",
        );
        qb.push_values(
            source.into_iter().enumerate(),
            |mut b, (i, (uid, message_id, flags))| {
                let new = first + i as i64;
                pairs.push((u32_of(uid), u32_of(new)));
                b.push_bind(target_id)
                    .push_bind(new)
                    .push_bind(message_id)
                    .push_bind(flags);
            },
        );
        qb.build().execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Some(pairs))
    }

    async fn mailbox_expunge(
        &self,
        key: &MailboxKey,
        only: Option<&[u32]>,
    ) -> StoreResult<Vec<u32>> {
        let Some(folder) = self.folder_id(key).await? else {
            return Ok(vec![]);
        };
        let mut qb = QueryBuilder::new("delete from fm_mail_entries where folder_id = ");
        qb.push_bind(folder)
            .push(" and ")
            .push_bind(DELETED)
            .push(" = any(flags)");
        if let Some(o) = only {
            qb.push(" and uid = any(")
                .push_bind(uids_param(o))
                .push(")");
        }
        qb.push(" returning uid");
        Ok(sorted_uids(
            qb.build_query_scalar().fetch_all(&self.pool).await?,
        ))
    }

    async fn mailbox_remove(&self, key: &MailboxKey, uids: &[u32]) -> StoreResult<Vec<u32>> {
        let Some(folder) = self.folder_id(key).await? else {
            return Ok(vec![]);
        };
        Ok(sorted_uids(
            sqlx::query_scalar(
                "delete from fm_mail_entries where folder_id = $1 and uid = any($2) returning uid",
            )
            .bind(folder)
            .bind(uids_param(uids))
            .fetch_all(&self.pool)
            .await?,
        ))
    }

    async fn mailbox_password(
        &self,
        space: Option<&str>,
        address: &str,
    ) -> StoreResult<Option<String>> {
        let hash: Option<Option<String>> = sqlx::query_scalar(
            "select password_hash from fm_mail_mailboxes where area = $1 and address = $2",
        )
        .bind(area(space))
        .bind(no_nul(address))
        .fetch_optional(&self.pool)
        .await?;
        Ok(hash.flatten())
    }

    async fn set_mailbox_password(
        &self,
        space: Option<&str>,
        address: &str,
        hash: Option<&str>,
    ) -> StoreResult<bool> {
        let (area, address) = (area(space), no_nul(address));
        match hash {
            Some(h) => {
                let mut tx = self.pool.begin().await?;
                ensure_folder(&mut tx, &area, &address, INBOX).await?;
                sqlx::query(
                    "update fm_mail_mailboxes set password_hash = $3 where area = $1 and address = $2",
                )
                .bind(&area)
                .bind(&address)
                .bind(no_nul(h))
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(true)
            }
            None => {
                let n = sqlx::query(
                    "update fm_mail_mailboxes set password_hash = null where area = $1 and address = $2 and password_hash is not null",
                )
                .bind(area)
                .bind(address)
                .execute(&self.pool)
                .await?
                .rows_affected();
                Ok(n > 0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_are_canonical_sorted_by_bytes_and_unique() {
        let f = flag_list(&[
            "\\seen".into(),
            "$Label1".into(),
            "\\Seen".into(),
            "a\0b".into(),
        ]);
        assert_eq!(f, vec!["$Label1", "\\Seen", "a\u{FFFD}b"]);
    }

    #[test]
    fn nul_characters_are_replaced_in_json() {
        let v = json_no_nul(serde_json::json!({"a\0": ["x\0y", 1, {"k": "v\0"}], "b": null}));
        assert_eq!(
            v,
            serde_json::json!({"a\u{FFFD}": ["x\u{FFFD}y", 1, {"k": "v\u{FFFD}"}], "b": null})
        );
    }

    #[test]
    fn address_terms_hold_lowercase_names_and_addresses() {
        let list = vec![
            MailAddress {
                name: Some("Billing Team".into()),
                address: "Billing@Example.com".into(),
            },
            MailAddress {
                name: None,
                address: "x@example.test".into(),
            },
        ];
        let extra = "Bounce@Example.com".to_string();
        assert_eq!(
            address_terms(&list, [&extra]),
            vec![
                "billing@example.com",
                "billing team",
                "x@example.test",
                "bounce@example.com"
            ]
        );
    }
}
