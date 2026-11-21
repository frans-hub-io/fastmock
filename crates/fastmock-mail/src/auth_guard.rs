//! Login protection shared by SMTP AUTH, IMAP and POP3: failed logins are counted per client address
//! and per user name ([`LoginFailures`]); a blocked address or user is refused without checking the
//! password. Password hashing (PBKDF2) runs at most [`MAX_CONCURRENT_HASHES`] at a time per mail
//! service (one per server process).

use std::net::IpAddr;
use std::sync::Arc;

use fastmock_core::mail::{LoginFailures, MemoryLoginFailures, ThrottlePolicy};
use tokio::sync::Semaphore;

/// Password hashes computed at the same time.
pub const MAX_CONCURRENT_HASHES: usize = 2;

/// Failed-login counters with their policy, and the password-hashing limit.
#[derive(Clone)]
pub struct AuthGuard {
    failures: Arc<dyn LoginFailures>,
    policy: ThrottlePolicy,
    hashing: Arc<Semaphore>,
}

impl AuthGuard {
    /// Counters in this process.
    pub fn memory(policy: ThrottlePolicy) -> Self {
        Self::new(Arc::new(MemoryLoginFailures::new()), policy)
    }

    pub fn new(failures: Arc<dyn LoginFailures>, policy: ThrottlePolicy) -> Self {
        Self {
            failures,
            policy,
            hashing: Arc::new(Semaphore::new(MAX_CONCURRENT_HASHES)),
        }
    }

    /// The same hashing limit with other counters.
    pub fn with_failures(&self, failures: Arc<dyn LoginFailures>) -> Self {
        Self {
            failures,
            policy: self.policy,
            hashing: self.hashing.clone(),
        }
    }

    /// Runs a password hash (or another CPU-heavy credential check) on the blocking pool, at most
    /// [`MAX_CONCURRENT_HASHES`] at a time.
    pub async fn hash<T, F>(&self, f: F) -> Option<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let _permit = self.hashing.acquire().await.ok()?;
        tokio::task::spawn_blocking(f).await.ok()
    }

    pub fn policy(&self) -> &ThrottlePolicy {
        &self.policy
    }

    fn keys(ip: IpAddr, user: &str) -> [String; 2] {
        let user: String = user.trim().to_lowercase().chars().take(320).collect();
        [format!("ip:{ip}"), format!("user:{user}")]
    }

    /// Whether the client address or the user is blocked now.
    pub async fn blocked(&self, ip: IpAddr, user: &str) -> bool {
        if self.policy.limit == 0 {
            return false;
        }
        let [a, b] = Self::keys(ip, user);
        let blocked = self.failures.blocked(&a).await || self.failures.blocked(&b).await;
        if blocked {
            metrics::counter!("fastmock_mail_rejected_total", "reason" => "login_throttled")
                .increment(1);
        }
        blocked
    }

    /// Counts a failed login for the address and the user.
    pub async fn failed(&self, ip: IpAddr, user: &str) {
        if self.policy.limit == 0 {
            return;
        }
        let [a, b] = Self::keys(ip, user);
        self.failures.failed(&a, &self.policy).await;
        self.failures.failed(&b, &self.policy).await;
    }

    /// Forgets the failures of the user after a successful login (the address keeps its count).
    pub async fn succeeded(&self, user: &str) {
        if self.policy.limit == 0 {
            return;
        }
        let [_, b] = Self::keys(IpAddr::from([0, 0, 0, 0]), user);
        self.failures.clear(&b).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn addresses_and_users_block_separately() {
        let g = AuthGuard::memory(ThrottlePolicy {
            limit: 2,
            ..Default::default()
        });
        let ip: IpAddr = "198.51.100.7".parse().unwrap();
        let other: IpAddr = "198.51.100.8".parse().unwrap();
        g.failed(ip, "a@example.test").await;
        g.failed(ip, "b@example.test").await;
        assert!(g.blocked(ip, "c@example.test").await, "the address");
        assert!(!g.blocked(other, "b@example.test").await);
        g.failed(other, "B@Example.test").await;
        assert!(g.blocked(other, "b@example.test").await, "the user");
        g.succeeded("b@example.test").await;
        assert!(!g.blocked(other, "b@example.test").await);
    }

    #[tokio::test]
    async fn hashing_is_limited() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let g = AuthGuard::memory(ThrottlePolicy::default());
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = vec![];
        for _ in 0..8 {
            let (r, p) = (running.clone(), peak.clone());
            let g = g.clone();
            tasks.push(tokio::spawn(async move {
                g.hash(move || {
                    let n = r.fetch_add(1, Ordering::SeqCst) + 1;
                    p.fetch_max(n, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    r.fetch_sub(1, Ordering::SeqCst);
                })
                .await
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert!(peak.load(Ordering::SeqCst) <= MAX_CONCURRENT_HASHES);
    }
}
