//! Failed-login counters shared by SMTP AUTH, IMAP and POP3: a key (a client address or a mailbox)
//! that fails [`ThrottlePolicy::limit`] times within [`ThrottlePolicy::window`] is blocked for a backoff
//! that doubles with every further failure, up to [`ThrottlePolicy::max_backoff`]. Blocked keys are
//! refused without checking the password. [`MemoryLoginFailures`] keeps the counters in this process;
//! `fastmock-store` shares them between replicas through Valkey.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;

/// When failures block a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThrottlePolicy {
    /// Failures within the window before the key is blocked; `0` turns blocking off.
    pub limit: u32,
    pub window: Duration,
    /// The first block; each further failure doubles it.
    pub base_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for ThrottlePolicy {
    fn default() -> Self {
        Self {
            limit: 5,
            window: Duration::from_secs(300),
            base_backoff: Duration::from_secs(30),
            max_backoff: Duration::from_secs(900),
        }
    }
}

impl ThrottlePolicy {
    /// The block after `failures` failures within the window, `None` below the limit.
    pub fn backoff(&self, failures: u32) -> Option<Duration> {
        if self.limit == 0 || failures < self.limit {
            return None;
        }
        let doublings = (failures - self.limit).min(16);
        Some(
            self.base_backoff
                .saturating_mul(1u32 << doublings)
                .min(self.max_backoff),
        )
    }
}

/// Failed-login counters.
#[async_trait]
pub trait LoginFailures: Send + Sync {
    /// Whether `key` is blocked now.
    async fn blocked(&self, key: &str) -> bool;
    /// Counts a failure of `key` and blocks it when the policy says so.
    async fn failed(&self, key: &str, policy: &ThrottlePolicy);
    /// Forgets the failures of `key` (after a successful login).
    async fn clear(&self, key: &str);
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    failures: u32,
    since: Instant,
    blocked_until: Option<Instant>,
}

/// Counters of this process. At most [`MemoryLoginFailures::MAX_KEYS`] keys are kept; expired ones are
/// pruned first, then the oldest.
#[derive(Default)]
pub struct MemoryLoginFailures {
    map: Mutex<HashMap<String, Entry>>,
}

impl MemoryLoginFailures {
    pub const MAX_KEYS: usize = 100_000;

    pub fn new() -> Self {
        Self::default()
    }

    /// Keys currently counted.
    pub fn len(&self) -> usize {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl LoginFailures for MemoryLoginFailures {
    async fn blocked(&self, key: &str) -> bool {
        let map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        map.get(key)
            .and_then(|e| e.blocked_until)
            .is_some_and(|t| Instant::now() < t)
    }

    async fn failed(&self, key: &str, policy: &ThrottlePolicy) {
        if policy.limit == 0 {
            return;
        }
        let now = Instant::now();
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= Self::MAX_KEYS && !map.contains_key(key) {
            map.retain(|_, e| {
                now.duration_since(e.since) < policy.window
                    || e.blocked_until.is_some_and(|t| now < t)
            });
            if map.len() >= Self::MAX_KEYS
                && let Some(oldest) = map
                    .iter()
                    .min_by_key(|(_, e)| e.since)
                    .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
        let e = map.entry(key.to_string()).or_insert(Entry {
            failures: 0,
            since: now,
            blocked_until: None,
        });
        let blocked = e.blocked_until.is_some_and(|t| now < t);
        if !blocked && now.duration_since(e.since) >= policy.window {
            *e = Entry {
                failures: 0,
                since: now,
                blocked_until: None,
            };
        }
        e.failures = e.failures.saturating_add(1);
        if let Some(b) = policy.backoff(e.failures) {
            e.blocked_until = Some(now + b);
            e.since = now;
        }
    }

    async fn clear(&self, key: &str) {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_the_maximum() {
        let p = ThrottlePolicy::default();
        assert_eq!(p.backoff(4), None);
        assert_eq!(p.backoff(5), Some(Duration::from_secs(30)));
        assert_eq!(p.backoff(6), Some(Duration::from_secs(60)));
        assert_eq!(p.backoff(40), Some(Duration::from_secs(900)));
        let off = ThrottlePolicy { limit: 0, ..p };
        assert_eq!(off.backoff(100), None);
    }

    #[tokio::test]
    async fn keys_block_after_the_limit_and_clear() {
        let f = MemoryLoginFailures::new();
        let p = ThrottlePolicy::default();
        for _ in 0..4 {
            f.failed("ip:198.51.100.7", &p).await;
        }
        assert!(!f.blocked("ip:198.51.100.7").await);
        f.failed("ip:198.51.100.7", &p).await;
        assert!(f.blocked("ip:198.51.100.7").await);
        assert!(!f.blocked("ip:198.51.100.8").await);
        f.clear("ip:198.51.100.7").await;
        assert!(!f.blocked("ip:198.51.100.7").await);
        let off = ThrottlePolicy { limit: 0, ..p };
        for _ in 0..10 {
            f.failed("box:a@example.test", &off).await;
        }
        assert!(!f.blocked("box:a@example.test").await);
        assert!(f.is_empty());
    }

    #[tokio::test]
    async fn short_windows_expire() {
        let f = MemoryLoginFailures::new();
        let p = ThrottlePolicy {
            limit: 2,
            window: Duration::from_millis(20),
            base_backoff: Duration::from_millis(30),
            max_backoff: Duration::from_millis(30),
        };
        f.failed("k", &p).await;
        std::thread::sleep(Duration::from_millis(30));
        f.failed("k", &p).await;
        assert!(!f.blocked("k").await, "the first failure left the window");
        f.failed("k", &p).await;
        assert!(f.blocked("k").await);
        std::thread::sleep(Duration::from_millis(40));
        assert!(!f.blocked("k").await);
    }
}
