use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::model::{
    AuditEvent, CallbackFilter, CallbackJob, CallbackStatus, JournalEntry, JournalFilter,
    SpaceFilter, Stub,
};
use crate::storage::*;

#[derive(Default)]
pub struct MemoryStubs {
    stubs: Mutex<Vec<Stub>>,
    revision: AtomicI64,
}

#[async_trait]
impl StubStore for MemoryStubs {
    async fn list(&self) -> StoreResult<Vec<Stub>> {
        let mut v = self.stubs.lock().await.clone();
        v.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        Ok(v)
    }

    async fn get(&self, id: Uuid) -> StoreResult<Option<Stub>> {
        Ok(self.stubs.lock().await.iter().find(|s| s.id == id).cloned())
    }

    async fn upsert(&self, stub: &Stub) -> StoreResult<()> {
        let mut s = stub.clone();
        s.updated_at = Some(Utc::now());
        let mut stubs = self.stubs.lock().await;
        stubs.retain(|x| x.id != s.id);
        stubs.push(s);
        self.revision.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn insert(&self, stub: &Stub) -> StoreResult<bool> {
        let mut stubs = self.stubs.lock().await;
        if stubs.iter().any(|x| x.id == stub.id) {
            return Ok(false);
        }
        let mut s = stub.clone();
        s.updated_at = Some(Utc::now());
        stubs.push(s);
        self.revision.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }

    async fn delete(&self, id: Uuid) -> StoreResult<bool> {
        let mut stubs = self.stubs.lock().await;
        let before = stubs.len();
        stubs.retain(|x| x.id != id);
        let removed = stubs.len() != before;
        if removed {
            self.revision.fetch_add(1, Ordering::SeqCst);
        }
        Ok(removed)
    }

    async fn delete_by_tag(&self, tag: &str, spaces: &SpaceFilter) -> StoreResult<u64> {
        let mut stubs = self.stubs.lock().await;
        let before = stubs.len();
        stubs.retain(|x| !(x.tags.iter().any(|t| t == tag) && spaces.matches(x.space.as_deref())));
        let n = (before - stubs.len()) as u64;
        if n > 0 {
            self.revision.fetch_add(1, Ordering::SeqCst);
        }
        Ok(n)
    }

    async fn replace_tag(
        &self,
        tag: Option<&str>,
        spaces: &SpaceFilter,
        new: &[Stub],
    ) -> StoreResult<u64> {
        let mut stubs = self.stubs.lock().await;
        let before = stubs.len();
        if let Some(t) = tag {
            stubs
                .retain(|x| !(x.tags.iter().any(|y| y == t) && spaces.matches(x.space.as_deref())));
        }
        let removed = (before - stubs.len()) as u64;
        let now = Utc::now();
        for (i, s) in new.iter().enumerate() {
            let mut s = s.clone();
            s.updated_at = Some(now + chrono::Duration::nanoseconds(i as i64));
            stubs.retain(|x| x.id != s.id);
            stubs.push(s);
        }
        self.revision.fetch_add(1, Ordering::SeqCst);
        Ok(removed)
    }

    async fn clear(&self, spaces: &SpaceFilter) -> StoreResult<u64> {
        let mut stubs = self.stubs.lock().await;
        let before = stubs.len();
        stubs.retain(|x| !spaces.matches(x.space.as_deref()));
        self.revision.fetch_add(1, Ordering::SeqCst);
        Ok((before - stubs.len()) as u64)
    }

    async fn revision(&self) -> StoreResult<i64> {
        Ok(self.revision.load(Ordering::SeqCst))
    }
}

pub struct MemoryJournal {
    entries: Mutex<std::collections::VecDeque<JournalEntry>>,
    next: AtomicI64,
    capacity: usize,
}

impl MemoryJournal {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(std::collections::VecDeque::new()),
            next: AtomicI64::new(1),
            capacity: capacity.max(1),
        }
    }
}

#[async_trait]
impl JournalStore for MemoryJournal {
    async fn append(&self, entry: &JournalEntry) -> StoreResult<i64> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let mut e = entry.clone();
        e.id = id;
        let mut entries = self.entries.lock().await;
        entries.push_back(e);
        while entries.len() > self.capacity {
            entries.pop_front();
        }
        Ok(id)
    }

    async fn query(&self, filter: &JournalFilter) -> StoreResult<Vec<JournalEntry>> {
        let entries = self.entries.lock().await;
        let limit = filter.limit.unwrap_or(100).clamp(1, 10_000) as usize;
        Ok(entries
            .iter()
            .rev()
            .filter(|e| filter.accepts(e))
            .take(limit)
            .cloned()
            .collect())
    }

    async fn count(&self, filter: &JournalFilter) -> StoreResult<i64> {
        Ok(self
            .entries
            .lock()
            .await
            .iter()
            .filter(|e| filter.accepts(e))
            .count() as i64)
    }

    async fn clear(&self, spaces: &SpaceFilter) -> StoreResult<u64> {
        let mut entries = self.entries.lock().await;
        let before = entries.len();
        entries.retain(|e| !spaces.matches(e.space.as_deref()));
        Ok((before - entries.len()) as u64)
    }

    async fn purge_older_than(&self, at: DateTime<Utc>) -> StoreResult<u64> {
        let mut entries = self.entries.lock().await;
        let before = entries.len();
        entries.retain(|e| e.at >= at);
        Ok((before - entries.len()) as u64)
    }

    async fn get(&self, id: i64) -> StoreResult<Option<JournalEntry>> {
        Ok(self
            .entries
            .lock()
            .await
            .iter()
            .find(|e| e.id == id)
            .cloned())
    }

    async fn query_page(
        &self,
        filter: &JournalFilter,
        offset: i64,
    ) -> StoreResult<Vec<JournalEntry>> {
        let entries = self.entries.lock().await;
        let limit = filter.limit.unwrap_or(100).clamp(1, 10_000) as usize;
        Ok(entries
            .iter()
            .rev()
            .filter(|e| filter.accepts(e))
            .skip(offset.max(0) as usize)
            .take(limit)
            .cloned()
            .collect())
    }

    async fn delete_ids(&self, ids: &[i64]) -> StoreResult<u64> {
        let set: std::collections::HashSet<i64> = ids.iter().copied().collect();
        let mut entries = self.entries.lock().await;
        let before = entries.len();
        entries.retain(|e| !set.contains(&e.id));
        Ok((before - entries.len()) as u64)
    }

    async fn stub_hits(&self, since: Option<DateTime<Utc>>) -> StoreResult<Vec<Uuid>> {
        let entries = self.entries.lock().await;
        let set: std::collections::BTreeSet<Uuid> = entries
            .iter()
            .filter(|e| since.is_none_or(|s| e.at >= s))
            .filter_map(|e| e.stub_id)
            .collect();
        Ok(set.into_iter().collect())
    }
}

#[derive(Default)]
pub struct MemoryOutbox {
    jobs: Mutex<Vec<(CallbackJob, Option<DateTime<Utc>>)>>,
}

#[async_trait]
impl OutboxStore for MemoryOutbox {
    async fn enqueue(&self, job: &CallbackJob) -> StoreResult<()> {
        self.jobs.lock().await.push((job.clone(), None));
        Ok(())
    }

    async fn claim_due(
        &self,
        now: DateTime<Utc>,
        limit: i64,
        lease: Duration,
    ) -> StoreResult<Vec<CallbackJob>> {
        let mut jobs = self.jobs.lock().await;
        let lease_until = now + chrono::Duration::from_std(lease).unwrap_or_default();
        for (j, l) in jobs.iter_mut() {
            if j.status == CallbackStatus::Pending
                && j.attempts >= j.max_attempts
                && l.is_some_and(|x| x <= now)
            {
                j.status = CallbackStatus::Failed;
                j.last_error = Some("lease expired after the last attempt".into());
                *l = None;
            }
        }
        let mut due: Vec<usize> = jobs
            .iter()
            .enumerate()
            .filter(|(_, (j, l))| {
                j.status == CallbackStatus::Pending
                    && j.attempts < j.max_attempts
                    && j.due_at <= now
                    && l.is_none_or(|l| l <= now)
            })
            .map(|(i, _)| i)
            .collect();
        due.sort_by_key(|i| jobs[*i].0.due_at);
        due.truncate(limit.max(0) as usize);
        let mut out = Vec::new();
        for i in due {
            jobs[i].1 = Some(lease_until);
            jobs[i].0.attempts += 1;
            out.push(jobs[i].0.clone());
        }
        Ok(out)
    }

    async fn mark_sent(&self, id: Uuid, attempt: u32, status: u16) -> StoreResult<bool> {
        let mut jobs = self.jobs.lock().await;
        if let Some((j, l)) = jobs.iter_mut().find(|(j, _)| {
            j.id == id && j.status == CallbackStatus::Pending && j.attempts == attempt
        }) {
            j.status = CallbackStatus::Sent;
            j.last_status = Some(status);
            j.last_error = None;
            j.sent_at = Some(Utc::now());
            *l = None;
            return Ok(true);
        }
        Ok(false)
    }

    async fn mark_attempt_failed(
        &self,
        id: Uuid,
        attempt: u32,
        error: &str,
        status: Option<u16>,
        retry_at: Option<DateTime<Utc>>,
    ) -> StoreResult<bool> {
        let mut jobs = self.jobs.lock().await;
        if let Some((j, l)) = jobs.iter_mut().find(|(j, _)| {
            j.id == id && j.status == CallbackStatus::Pending && j.attempts == attempt
        }) {
            j.last_error = Some(error.to_string());
            j.last_status = status;
            *l = None;
            match retry_at {
                Some(at) => j.due_at = at,
                None => j.status = CallbackStatus::Failed,
            }
            return Ok(true);
        }
        Ok(false)
    }

    async fn list(&self, filter: &CallbackFilter) -> StoreResult<Vec<CallbackJob>> {
        let jobs = self.jobs.lock().await;
        let limit = filter.limit.unwrap_or(100).clamp(1, 10_000) as usize;
        Ok(jobs
            .iter()
            .rev()
            .map(|(j, _)| j)
            .filter(|j| filter.accepts(j))
            .take(limit)
            .cloned()
            .collect())
    }

    async fn pending_count(&self) -> StoreResult<i64> {
        Ok(self
            .jobs
            .lock()
            .await
            .iter()
            .filter(|(j, _)| j.status == CallbackStatus::Pending)
            .count() as i64)
    }

    async fn clear(&self, spaces: &SpaceFilter) -> StoreResult<u64> {
        let mut jobs = self.jobs.lock().await;
        let before = jobs.len();
        jobs.retain(|(j, _)| !spaces.matches(j.space.as_deref()));
        Ok((before - jobs.len()) as u64)
    }

    async fn purge_finished_older_than(&self, at: DateTime<Utc>) -> StoreResult<u64> {
        let mut jobs = self.jobs.lock().await;
        let before = jobs.len();
        jobs.retain(|(j, _)| j.status == CallbackStatus::Pending || j.created_at >= at);
        Ok((before - jobs.len()) as u64)
    }
}

#[derive(Default)]
pub struct MemoryHot {
    data: Mutex<HashMap<String, (String, Option<Instant>)>>,
    zsets: Mutex<HashMap<String, Vec<(f64, String)>>>,
    writes: std::sync::atomic::AtomicU64,
}

/// Expired entries are dropped every this many writes, so keys with a TTL do not pile up.
const SWEEP_EVERY: u64 = 1024;

impl MemoryHot {
    fn live(entry: &(String, Option<Instant>)) -> bool {
        entry.1.is_none_or(|t| t > Instant::now())
    }

    fn sweep_due(&self) -> bool {
        self.writes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .is_multiple_of(SWEEP_EVERY)
    }
}

#[async_trait]
impl HotStore for MemoryHot {
    async fn get(&self, key: &str) -> StoreResult<Option<String>> {
        let data = self.data.lock().await;
        Ok(data.get(key).filter(|e| Self::live(e)).map(|e| e.0.clone()))
    }

    async fn set(&self, key: &str, value: &str, ttl: Option<Duration>) -> StoreResult<()> {
        let mut data = self.data.lock().await;
        if self.sweep_due() {
            data.retain(|_, e| Self::live(e));
        }
        data.insert(
            key.to_string(),
            (value.to_string(), ttl.map(|t| Instant::now() + t)),
        );
        Ok(())
    }

    async fn delete(&self, key: &str) -> StoreResult<bool> {
        Ok(self
            .data
            .lock()
            .await
            .remove(key)
            .is_some_and(|e| Self::live(&e)))
    }

    async fn incr(&self, key: &str, ttl: Option<Duration>) -> StoreResult<i64> {
        self.incr_by(key, 1, ttl).await
    }

    async fn incr_by(&self, key: &str, delta: i64, ttl: Option<Duration>) -> StoreResult<i64> {
        let mut data = self.data.lock().await;
        let current = data
            .get(key)
            .filter(|e| Self::live(e))
            .and_then(|e| e.0.parse::<i64>().ok());
        let next = current.unwrap_or(0).saturating_add(delta);
        let expiry = match (current, data.get(key)) {
            (Some(_), Some(e)) => e.1,
            _ => ttl.map(|t| Instant::now() + t),
        };
        data.insert(key.to_string(), (next.to_string(), expiry));
        Ok(next)
    }

    async fn compare_and_set(
        &self,
        key: &str,
        expected: Option<&str>,
        value: &str,
        ttl: Option<Duration>,
    ) -> StoreResult<bool> {
        let mut data = self.data.lock().await;
        let current = data
            .get(key)
            .filter(|e| Self::live(e))
            .map(|e| e.0.as_str());
        if current != expected {
            return Ok(false);
        }
        data.insert(
            key.to_string(),
            (value.to_string(), ttl.map(|t| Instant::now() + t)),
        );
        Ok(true)
    }

    async fn scan(&self, prefix: &str, limit: usize) -> StoreResult<Vec<(String, String)>> {
        let data = self.data.lock().await;
        let mut v: Vec<(String, String)> = data
            .iter()
            .filter(|(k, e)| k.starts_with(prefix) && Self::live(e))
            .map(|(k, e)| (k.clone(), e.0.clone()))
            .collect();
        v.sort();
        v.truncate(limit);
        Ok(v)
    }

    async fn delete_prefix(&self, prefix: &str) -> StoreResult<u64> {
        let mut data = self.data.lock().await;
        let before = data.len();
        data.retain(|k, _| !k.starts_with(prefix));
        let removed = (before - data.len()) as u64;
        let mut z = self.zsets.lock().await;
        let zb = z.len();
        z.retain(|k, _| !k.starts_with(prefix));
        Ok(removed + (zb - z.len()) as u64)
    }

    async fn zadd(&self, key: &str, score: f64, member: &str) -> StoreResult<()> {
        let mut z = self.zsets.lock().await;
        let set = z.entry(key.to_string()).or_default();
        set.retain(|(_, m)| m != member);
        set.push((score, member.to_string()));
        set.sort_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });
        Ok(())
    }

    async fn zpop_until(
        &self,
        key: &str,
        max_score: f64,
        limit: usize,
    ) -> StoreResult<Vec<String>> {
        let mut z = self.zsets.lock().await;
        let Some(set) = z.get_mut(key) else {
            return Ok(vec![]);
        };
        let n = set
            .iter()
            .take_while(|(s, _)| *s <= max_score)
            .count()
            .min(limit);
        Ok(set.drain(0..n).map(|(_, m)| m).collect())
    }

    async fn ping(&self) -> StoreResult<()> {
        Ok(())
    }
}

#[derive(Default)]
pub struct MemoryData {
    data: Mutex<BTreeMap<(String, String), Value>>,
}

#[async_trait]
impl DataStore for MemoryData {
    async fn get(&self, namespace: &str, key: &str) -> StoreResult<Option<Value>> {
        Ok(self
            .data
            .lock()
            .await
            .get(&(namespace.to_string(), key.to_string()))
            .cloned())
    }

    async fn put(&self, namespace: &str, key: &str, value: &Value) -> StoreResult<()> {
        self.data
            .lock()
            .await
            .insert((namespace.to_string(), key.to_string()), value.clone());
        Ok(())
    }

    async fn delete(&self, namespace: &str, key: &str) -> StoreResult<bool> {
        Ok(self
            .data
            .lock()
            .await
            .remove(&(namespace.to_string(), key.to_string()))
            .is_some())
    }

    async fn list(&self, namespace: &str, limit: i64) -> StoreResult<Vec<(String, Value)>> {
        Ok(self
            .data
            .lock()
            .await
            .iter()
            .filter(|((ns, _), _)| ns == namespace)
            .take(limit.max(0) as usize)
            .map(|((_, k), v)| (k.clone(), v.clone()))
            .collect())
    }

    async fn clear(&self, namespace: &str) -> StoreResult<u64> {
        let mut data = self.data.lock().await;
        let before = data.len();
        data.retain(|(ns, _), _| ns != namespace);
        Ok((before - data.len()) as u64)
    }

    async fn ping(&self) -> StoreResult<()> {
        Ok(())
    }
}

#[derive(Default)]
pub struct MemoryAudit {
    events: Mutex<Vec<AuditEvent>>,
}

#[async_trait]
impl AuditStore for MemoryAudit {
    async fn record(&self, event: &AuditEvent) -> StoreResult<()> {
        let mut e = self.events.lock().await;
        let mut ev = event.clone();
        ev.id = e.len() as i64 + 1;
        e.push(ev);
        Ok(())
    }

    async fn list(&self, limit: i64) -> StoreResult<Vec<AuditEvent>> {
        Ok(self
            .events
            .lock()
            .await
            .iter()
            .rev()
            .take(limit.clamp(1, 10_000) as usize)
            .cloned()
            .collect())
    }

    async fn purge_older_than(&self, at: DateTime<Utc>) -> StoreResult<u64> {
        let mut e = self.events.lock().await;
        let before = e.len();
        e.retain(|x| x.at >= at);
        Ok((before - e.len()) as u64)
    }
}

/// Managed tokens of memory mode; they are lost on restart.
#[derive(Default)]
pub struct MemoryTokens {
    tokens: Mutex<Vec<ApiToken>>,
}

#[async_trait]
impl TokenStore for MemoryTokens {
    async fn create(&self, token: &ApiToken, now: DateTime<Utc>) -> StoreResult<bool> {
        let mut v = self.tokens.lock().await;
        for t in v.iter_mut() {
            if t.name == token.name && t.revoked_at.is_none() && t.expires_at <= now {
                t.revoked_at = Some(t.expires_at);
            }
        }
        if v.iter()
            .any(|t| t.name == token.name && t.revoked_at.is_none())
        {
            return Ok(false);
        }
        if v.iter()
            .any(|t| t.id == token.id || t.sha256 == token.sha256)
        {
            anyhow::bail!("duplicate token id or hash");
        }
        v.push(token.clone());
        Ok(true)
    }

    async fn get(&self, id: Uuid) -> StoreResult<Option<ApiToken>> {
        Ok(self
            .tokens
            .lock()
            .await
            .iter()
            .find(|t| t.id == id)
            .cloned())
    }

    async fn by_hash(&self, sha256: &str) -> StoreResult<Option<ApiToken>> {
        Ok(self
            .tokens
            .lock()
            .await
            .iter()
            .find(|t| t.sha256 == sha256)
            .cloned())
    }

    async fn list(&self) -> StoreResult<Vec<ApiToken>> {
        let mut v = self.tokens.lock().await.clone();
        v.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        Ok(v)
    }

    async fn revoke(&self, id: Uuid, at: DateTime<Utc>) -> StoreResult<bool> {
        let mut v = self.tokens.lock().await;
        match v.iter_mut().find(|t| t.id == id && t.revoked_at.is_none()) {
            Some(t) => {
                t.revoked_at = Some(at);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn touch(&self, id: Uuid, at: DateTime<Utc>) -> StoreResult<()> {
        let mut v = self.tokens.lock().await;
        if let Some(t) = v.iter_mut().find(|t| t.id == id)
            && t.last_used_at
                .is_none_or(|l| l <= at - chrono::Duration::minutes(1))
        {
            t.last_used_at = Some(at);
        }
        Ok(())
    }

    async fn count_active(&self, now: DateTime<Utc>) -> StoreResult<i64> {
        Ok(self
            .tokens
            .lock()
            .await
            .iter()
            .filter(|t| t.status(now) == ApiTokenStatus::Active)
            .count() as i64)
    }

    async fn purge_inactive_before(&self, at: DateTime<Utc>) -> StoreResult<u64> {
        let mut v = self.tokens.lock().await;
        let before = v.len();
        v.retain(|t| t.revoked_at.is_none_or(|r| r >= at) && t.expires_at >= at);
        Ok((before - v.len()) as u64)
    }
}

pub fn memory_stores(journal_capacity: usize) -> Stores {
    Stores {
        stubs: Arc::new(MemoryStubs::default()),
        journal: Arc::new(MemoryJournal::new(journal_capacity)),
        outbox: Arc::new(MemoryOutbox::default()),
        hot: Arc::new(MemoryHot::default()),
        data: Arc::new(MemoryData::default()),
        audit: Arc::new(MemoryAudit::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn expired_hot_entries_are_swept() {
        let hot = MemoryHot::default();
        for i in 0..SWEEP_EVERY {
            hot.set(&format!("k{i}"), "v", Some(Duration::from_millis(1)))
                .await
                .unwrap();
        }
        hot.set("keep", "v", None).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        for i in 0..SWEEP_EVERY {
            hot.set(&format!("n{i}"), "v", Some(Duration::from_secs(60)))
                .await
                .unwrap();
        }
        let n = hot.data.lock().await.len() as u64;
        assert!(n <= SWEEP_EVERY + 1, "{n} entries");
        assert_eq!(hot.get("keep").await.unwrap().as_deref(), Some("v"));
    }
}
