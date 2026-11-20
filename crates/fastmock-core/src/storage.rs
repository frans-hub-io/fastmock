use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use crate::model::{
    AuditEvent, CallbackFilter, CallbackJob, JournalEntry, JournalFilter, SpaceFilter, Stub,
};

pub type StoreResult<T> = anyhow::Result<T>;

#[async_trait]
pub trait StubStore: Send + Sync {
    async fn list(&self) -> StoreResult<Vec<Stub>>;
    async fn get(&self, id: Uuid) -> StoreResult<Option<Stub>>;
    async fn upsert(&self, stub: &Stub) -> StoreResult<()>;
    async fn insert(&self, stub: &Stub) -> StoreResult<bool>;
    async fn delete(&self, id: Uuid) -> StoreResult<bool>;
    async fn delete_by_tag(&self, tag: &str, spaces: &SpaceFilter) -> StoreResult<u64>;
    /// Deletes the stubs carrying `tag` within `spaces` and inserts or replaces `stubs`, atomically.
    async fn replace_tag(
        &self,
        tag: Option<&str>,
        spaces: &SpaceFilter,
        stubs: &[Stub],
    ) -> StoreResult<u64>;
    async fn clear(&self, spaces: &SpaceFilter) -> StoreResult<u64>;
    async fn revision(&self) -> StoreResult<i64>;
}

#[async_trait]
pub trait JournalStore: Send + Sync {
    async fn append(&self, entry: &JournalEntry) -> StoreResult<i64>;
    async fn append_many(&self, entries: &[JournalEntry]) -> StoreResult<()> {
        for e in entries {
            self.append(e).await?;
        }
        Ok(())
    }
    async fn query(&self, filter: &JournalFilter) -> StoreResult<Vec<JournalEntry>>;
    async fn count(&self, filter: &JournalFilter) -> StoreResult<i64>;
    async fn clear(&self, spaces: &SpaceFilter) -> StoreResult<u64>;
    async fn purge_older_than(&self, at: DateTime<Utc>) -> StoreResult<u64>;
    async fn get(&self, id: i64) -> StoreResult<Option<JournalEntry>> {
        let all = self
            .query(&JournalFilter {
                limit: Some(10_000),
                ..Default::default()
            })
            .await?;
        Ok(all.into_iter().find(|e| e.id == id))
    }
    async fn query_page(
        &self,
        filter: &JournalFilter,
        offset: i64,
    ) -> StoreResult<Vec<JournalEntry>> {
        let limit = filter.limit.unwrap_or(100).clamp(1, 10_000);
        let mut f = filter.clone();
        f.limit = Some((limit + offset.max(0)).min(10_000));
        let v = self.query(&f).await?;
        Ok(v.into_iter()
            .skip(offset.max(0) as usize)
            .take(limit as usize)
            .collect())
    }
    async fn delete_ids(&self, _ids: &[i64]) -> StoreResult<u64> {
        anyhow::bail!("this journal store cannot delete single entries")
    }
    async fn stub_hits(&self, since: Option<DateTime<Utc>>) -> StoreResult<Vec<Uuid>> {
        let v = self
            .query(&JournalFilter {
                matched: Some(true),
                since,
                limit: Some(10_000),
                ..Default::default()
            })
            .await?;
        let mut ids: Vec<Uuid> = v.into_iter().filter_map(|e| e.stub_id).collect();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }
}

#[async_trait]
pub trait OutboxStore: Send + Sync {
    async fn enqueue(&self, job: &CallbackJob) -> StoreResult<()>;
    async fn claim_due(
        &self,
        now: DateTime<Utc>,
        limit: i64,
        lease: Duration,
    ) -> StoreResult<Vec<CallbackJob>>;
    async fn mark_sent(&self, id: Uuid, attempt: u32, status: u16) -> StoreResult<bool>;
    async fn mark_attempt_failed(
        &self,
        id: Uuid,
        attempt: u32,
        error: &str,
        status: Option<u16>,
        retry_at: Option<DateTime<Utc>>,
    ) -> StoreResult<bool>;
    async fn list(&self, filter: &CallbackFilter) -> StoreResult<Vec<CallbackJob>>;
    async fn pending_count(&self) -> StoreResult<i64>;
    async fn clear(&self, spaces: &SpaceFilter) -> StoreResult<u64>;
    async fn purge_finished_older_than(&self, at: DateTime<Utc>) -> StoreResult<u64>;
}

#[async_trait]
pub trait HotStore: Send + Sync {
    async fn get(&self, key: &str) -> StoreResult<Option<String>>;
    async fn set(&self, key: &str, value: &str, ttl: Option<Duration>) -> StoreResult<()>;
    async fn delete(&self, key: &str) -> StoreResult<bool>;
    async fn incr(&self, key: &str, ttl: Option<Duration>) -> StoreResult<i64>;
    async fn incr_by(&self, key: &str, delta: i64, ttl: Option<Duration>) -> StoreResult<i64> {
        if delta == 1 {
            return self.incr(key, ttl).await;
        }
        anyhow::bail!("incr_by is not supported by this store")
    }
    async fn compare_and_set(
        &self,
        key: &str,
        expected: Option<&str>,
        value: &str,
        ttl: Option<Duration>,
    ) -> StoreResult<bool>;
    async fn scan(&self, prefix: &str, limit: usize) -> StoreResult<Vec<(String, String)>>;
    async fn delete_prefix(&self, prefix: &str) -> StoreResult<u64>;
    async fn zadd(&self, key: &str, score: f64, member: &str) -> StoreResult<()>;
    async fn zpop_until(&self, key: &str, max_score: f64, limit: usize)
    -> StoreResult<Vec<String>>;
    async fn ping(&self) -> StoreResult<()>;
}

#[async_trait]
pub trait DataStore: Send + Sync {
    async fn get(&self, namespace: &str, key: &str) -> StoreResult<Option<Value>>;
    async fn put(&self, namespace: &str, key: &str, value: &Value) -> StoreResult<()>;
    async fn delete(&self, namespace: &str, key: &str) -> StoreResult<bool>;
    async fn list(&self, namespace: &str, limit: i64) -> StoreResult<Vec<(String, Value)>>;
    async fn clear(&self, namespace: &str) -> StoreResult<u64>;
    async fn ping(&self) -> StoreResult<()>;
    async fn list_without(
        &self,
        namespace: &str,
        limit: i64,
        omit_field: &str,
    ) -> StoreResult<Vec<(String, Value)>> {
        let mut v = self.list(namespace, limit).await?;
        for (_, x) in &mut v {
            if let Value::Object(m) = x {
                m.remove(omit_field);
            }
        }
        Ok(v)
    }
}

#[async_trait]
pub trait AuditStore: Send + Sync {
    async fn record(&self, event: &AuditEvent) -> StoreResult<()>;
    async fn list(&self, limit: i64) -> StoreResult<Vec<AuditEvent>>;
    async fn purge_older_than(&self, at: DateTime<Utc>) -> StoreResult<u64>;
    async fn query(&self, filter: &AuditFilter) -> StoreResult<Vec<AuditEvent>> {
        let limit = filter.limit.unwrap_or(100).clamp(1, 10_000);
        let mut v = self.list(10_000).await?;
        v.retain(|e| filter.accepts(e));
        v.truncate(limit as usize);
        Ok(v)
    }
}

/// A token managed through the admin API. Only the SHA-256 of the value is stored.
#[derive(Clone, Debug, PartialEq)]
pub struct ApiToken {
    pub id: Uuid,
    pub name: String,
    /// Lowercase hex SHA-256 of the token value.
    pub sha256: String,
    pub role: String,
    pub spaces: Vec<String>,
    pub global: bool,
    pub description: Option<String>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    /// When the token was revoked, or, for a token retired after expiry, its expiry.
    pub revoked_at: Option<DateTime<Utc>>,
    /// Set by an explicit revocation; the status never compares clocks of different replicas.
    pub revoked: bool,
    /// The managed token that created this one.
    pub parent_id: Option<Uuid>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiTokenStatus {
    Active,
    Expired,
    Revoked,
}

impl ApiToken {
    pub fn status(&self, now: DateTime<Utc>) -> ApiTokenStatus {
        if self.revoked {
            ApiTokenStatus::Revoked
        } else if self.expires_at <= now {
            ApiTokenStatus::Expired
        } else {
            ApiTokenStatus::Active
        }
    }
}

#[async_trait]
pub trait TokenStore: Send + Sync {
    /// Stores a new token. Returns false when an active token already has the name; expired tokens with
    /// the name are retired first, so the name can be reused.
    async fn create(&self, token: &ApiToken, now: DateTime<Utc>) -> StoreResult<bool>;
    async fn get(&self, id: Uuid) -> StoreResult<Option<ApiToken>>;
    async fn by_hash(&self, sha256: &str) -> StoreResult<Option<ApiToken>>;
    /// Every stored token, newest first.
    async fn list(&self) -> StoreResult<Vec<ApiToken>>;
    /// Marks the token revoked; false when it does not exist, is already revoked or retired, or has
    /// expired by `at`.
    async fn revoke(&self, id: Uuid, at: DateTime<Utc>) -> StoreResult<bool>;
    /// Whether the store holds any token at all, whatever its status.
    async fn has_any(&self) -> StoreResult<bool> {
        Ok(!self.list().await?.is_empty())
    }
    /// Records a use, unless one was recorded less than a minute before `at`.
    async fn touch(&self, id: Uuid, at: DateTime<Utc>) -> StoreResult<()>;
    async fn count_active(&self, now: DateTime<Utc>) -> StoreResult<i64>;
    /// Deletes tokens revoked or expired before `at`.
    async fn purge_inactive_before(&self, at: DateTime<Utc>) -> StoreResult<u64>;
}

#[derive(Clone, Debug, Default)]
pub struct AuditFilter {
    pub action: Option<String>,
    pub principal: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub limit: Option<i64>,
}

impl AuditFilter {
    pub fn action_prefix(&self) -> Option<&str> {
        self.action.as_deref().and_then(|a| a.strip_suffix('*'))
    }

    pub fn accepts(&self, e: &AuditEvent) -> bool {
        let action_ok = match (self.action_prefix(), &self.action) {
            (Some(p), _) => e.action.starts_with(p),
            (None, Some(a)) => e.action == *a,
            (None, None) => true,
        };
        action_ok
            && self.principal.as_ref().is_none_or(|p| *p == e.principal)
            && self.since.is_none_or(|s| e.at >= s)
    }
}

#[derive(Clone)]
pub struct Stores {
    pub stubs: Arc<dyn StubStore>,
    pub journal: Arc<dyn JournalStore>,
    pub outbox: Arc<dyn OutboxStore>,
    pub hot: Arc<dyn HotStore>,
    pub data: Arc<dyn DataStore>,
    pub audit: Arc<dyn AuditStore>,
}

pub trait SecretProvider: Send + Sync {
    fn get(&self, name: &str) -> Option<String>;
}

#[derive(Default)]
pub struct NoSecrets;

impl SecretProvider for NoSecrets {
    fn get(&self, _name: &str) -> Option<String> {
        None
    }
}
