//! Captured broker messages in PostgreSQL: [`PgCaptureStore`] implements [`CaptureStore`] on the table of
//! migrations 0016 and 0017. The body is `bytea`; everything else of a message is one `jsonb` column, with
//! the space, source, protocol, topic, time and counted bytes as columns for filters and retention.

use std::collections::HashMap;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use fastmock_core::SpaceFilter;
use fastmock_core::StoreResult;
use fastmock_core::capture::{
    CapturePage, CaptureQuery, CaptureStore, CapturedMessage, InsertLimits, MAX_PAGE,
};
use serde_json::Value;
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use uuid::Uuid;

use crate::postgres::no_nul;

/// [`CaptureStore`] on PostgreSQL; cheap to clone.
#[derive(Clone)]
pub struct PgCaptureStore {
    pool: PgPool,
}

impl PgCaptureStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn clean(v: Value) -> Value {
    match v {
        Value::String(s) => Value::String(no_nul(&s)),
        Value::Array(a) => Value::Array(a.into_iter().map(clean).collect()),
        Value::Object(o) => {
            Value::Object(o.into_iter().map(|(k, v)| (no_nul(&k), clean(v))).collect())
        }
        other => other,
    }
}

fn entry(m: &CapturedMessage) -> StoreResult<Value> {
    let mut v = serde_json::to_value(m.without_body())?;
    if let Value::Object(o) = &mut v {
        o.remove("body");
    }
    Ok(clean(v))
}

fn push_spaces(qb: &mut QueryBuilder<'_, Postgres>, spaces: &SpaceFilter) {
    match spaces {
        SpaceFilter::Any => {
            qb.push(" and true");
        }
        SpaceFilter::Global => {
            qb.push(" and space is null");
        }
        SpaceFilter::In(list) => {
            qb.push(" and space = any(");
            qb.push_bind(list.clone());
            qb.push(")");
        }
    }
}

fn row_message(row: &sqlx::postgres::PgRow, with_body: bool) -> StoreResult<CapturedMessage> {
    let mut v: Value = row.try_get("entry")?;
    if let Value::Object(o) = &mut v {
        o.insert("body".into(), Value::String(String::new()));
    }
    let mut m: CapturedMessage = serde_json::from_value(v)?;
    if with_body {
        let b: Vec<u8> = row.try_get("body")?;
        m.body = Bytes::from(b);
    }
    Ok(m)
}

#[async_trait]
impl CaptureStore for PgCaptureStore {
    async fn insert(&self, msgs: &[CapturedMessage], limits: &InsertLimits) -> StoreResult<u64> {
        if msgs.is_empty() {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await?;
        let mut sources = Vec::new();
        let mut areas: Vec<Option<String>> = Vec::new();
        for m in msgs {
            sqlx::query(
                "insert into fm_captured_messages (id, space, source_id, protocol, topic, received_at, entry, body, bytes)
                 values ($1, $2, $3, $4, $5, $6, $7, $8, $9) on conflict (id) do nothing",
            )
            .bind(m.id)
            .bind(m.space.as_deref().map(no_nul))
            .bind(m.source_id)
            .bind(m.protocol.as_str())
            .bind(no_nul(&m.topic))
            .bind(m.received_at)
            .bind(entry(m)?)
            .bind(m.body.as_ref())
            .bind(i64::try_from(m.stored_bytes()).unwrap_or(i64::MAX))
            .execute(&mut *tx)
            .await?;
            if !sources.contains(&m.source_id) {
                sources.push(m.source_id);
            }
            let area = m.space.as_deref().map(no_nul);
            if !areas.contains(&area) {
                areas.push(area);
            }
        }
        let keep = i64::try_from(limits.keep.max(1)).unwrap_or(i64::MAX);
        let mut dropped = 0;
        for s in sources {
            let r = sqlx::query(
                "delete from fm_captured_messages where source_id = $1 and id < (
                    select id from fm_captured_messages where source_id = $1 order by id desc offset $2 - 1 limit 1
                 )",
            )
            .bind(s)
            .bind(keep)
            .execute(&mut *tx)
            .await?;
            dropped += r.rows_affected();
        }
        let cap = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
        for area in areas {
            let used: i64 = sqlx::query_scalar(
                "select coalesce(sum(bytes), 0)::bigint from fm_captured_messages where space is not distinct from $1",
            )
            .bind(&area)
            .fetch_one(&mut *tx)
            .await?;
            if used > cap(limits.area_bytes) {
                let r = sqlx::query(
                    "delete from fm_captured_messages where id in (
                        select id from (
                            select id, sum(bytes) over (order by id) - bytes as before
                            from fm_captured_messages where space is not distinct from $1
                        ) t where before < $2
                     )",
                )
                .bind(&area)
                .bind(used - cap(limits.area_bytes))
                .execute(&mut *tx)
                .await?;
                dropped += r.rows_affected();
            }
        }
        let used: i64 =
            sqlx::query_scalar("select coalesce(sum(bytes), 0)::bigint from fm_captured_messages")
                .fetch_one(&mut *tx)
                .await?;
        if used > cap(limits.total_bytes) {
            let r = sqlx::query(
                "delete from fm_captured_messages where id in (
                    select id from (
                        select id, sum(bytes) over (order by id) - bytes as before from fm_captured_messages
                    ) t where before < $1
                 )",
            )
            .bind(used - cap(limits.total_bytes))
            .execute(&mut *tx)
            .await?;
            dropped += r.rows_affected();
        }
        tx.commit().await?;
        Ok(dropped)
    }

    async fn bodies(&self, ids: &[Uuid]) -> StoreResult<HashMap<Uuid, Bytes>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<(Uuid, Vec<u8>)> =
            sqlx::query_as("select id, body from fm_captured_messages where id = any($1)")
                .bind(ids.to_vec())
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(id, b)| (id, Bytes::from(b)))
            .collect())
    }

    async fn total_bytes(&self) -> StoreResult<u64> {
        let n: i64 =
            sqlx::query_scalar("select coalesce(sum(bytes), 0)::bigint from fm_captured_messages")
                .fetch_one(&self.pool)
                .await?;
        Ok(n.max(0) as u64)
    }

    async fn get(&self, id: Uuid, spaces: &SpaceFilter) -> StoreResult<Option<CapturedMessage>> {
        let mut qb = QueryBuilder::<Postgres>::new(
            "select entry, body from fm_captured_messages where id = ",
        );
        qb.push_bind(id);
        push_spaces(&mut qb, spaces);
        match qb.build().fetch_optional(&self.pool).await? {
            Some(row) => Ok(Some(row_message(&row, true)?)),
            None => Ok(None),
        }
    }

    async fn list(&self, q: &CaptureQuery) -> StoreResult<CapturePage> {
        let limit = q.limit.clamp(1, MAX_PAGE * 20);
        let mut qb = QueryBuilder::<Postgres>::new(if q.with_body {
            "select entry, body from fm_captured_messages where true"
        } else {
            "select entry from fm_captured_messages where true"
        });
        push_spaces(&mut qb, &q.spaces);
        if let Some(s) = q.source_id {
            qb.push(" and source_id = ").push_bind(s);
        }
        if let Some(p) = q.protocol {
            qb.push(" and protocol = ").push_bind(p.as_str());
        }
        if let Some(t) = &q.topic {
            qb.push(" and topic = ").push_bind(no_nul(t));
        }
        if let Some(s) = q.since {
            qb.push(" and received_at >= ").push_bind(s);
        }
        if let Some(u) = q.until {
            qb.push(" and received_at < ").push_bind(u);
        }
        if let Some(c) = q.cursor {
            qb.push(if q.oldest_first {
                " and id > "
            } else {
                " and id < "
            })
            .push_bind(c);
        }
        qb.push(if q.oldest_first {
            " order by id asc limit "
        } else {
            " order by id desc limit "
        })
        .push_bind((limit + 1) as i64);
        let rows = qb.build().fetch_all(&self.pool).await?;
        let mut items = rows
            .iter()
            .map(|r| row_message(r, q.with_body))
            .collect::<StoreResult<Vec<_>>>()?;
        let next = if items.len() > limit {
            items.truncate(limit);
            items.last().map(|m| m.id)
        } else {
            None
        };
        Ok(CapturePage { items, next })
    }

    async fn delete(&self, id: Uuid, spaces: &SpaceFilter) -> StoreResult<Option<Option<String>>> {
        let mut qb = QueryBuilder::<Postgres>::new("delete from fm_captured_messages where id = ");
        qb.push_bind(id);
        push_spaces(&mut qb, spaces);
        qb.push(" returning space");
        let row = qb.build().fetch_optional(&self.pool).await?;
        Ok(match row {
            Some(r) => Some(r.try_get::<Option<String>, _>("space")?),
            None => None,
        })
    }

    async fn delete_matching(
        &self,
        spaces: &SpaceFilter,
        source: Option<Uuid>,
    ) -> StoreResult<u64> {
        let mut qb = QueryBuilder::<Postgres>::new("delete from fm_captured_messages where true");
        push_spaces(&mut qb, spaces);
        if let Some(s) = source {
            qb.push(" and source_id = ").push_bind(s);
        }
        Ok(qb.build().execute(&self.pool).await?.rows_affected())
    }

    async fn sweep(&self, before: DateTime<Utc>) -> StoreResult<u64> {
        Ok(
            sqlx::query("delete from fm_captured_messages where received_at < $1")
                .bind(before)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    async fn counts(&self) -> StoreResult<HashMap<Uuid, u64>> {
        let rows: Vec<(Uuid, i64)> = sqlx::query_as(
            "select source_id, count(*) from fm_captured_messages group by source_id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(s, n)| (s, n.max(0) as u64))
            .collect())
    }
}
