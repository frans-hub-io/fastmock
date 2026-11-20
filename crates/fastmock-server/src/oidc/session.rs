//! Server-side sessions and pending logins in the hot store (Valkey, or process memory in memory
//! mode). Keys hold only SHA-256 hashes of the browser-held secrets.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use fastmock_core::HotStore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SESSION_PREFIX: &str = "oidc:session:";
const LOGIN_PREFIX: &str = "oidc:login:";
pub const LOGIN_TTL: Duration = Duration::from_secs(600);
pub const MAX_GROUPS: usize = 256;
pub const MAX_NAME_BYTES: usize = 256;

/// 256 random bits, base64url (43 characters).
pub fn random_secret() -> String {
    let b: [u8; 32] = rand::random();
    URL_SAFE_NO_PAD.encode(b)
}

pub fn looks_like_secret(v: &str) -> bool {
    v.len() == 43
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn digest(v: &str) -> String {
    hex::encode(Sha256::digest(v.as_bytes()))
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionRecord {
    pub name: String,
    pub groups: Vec<String>,
    /// Unix milliseconds.
    pub created: i64,
    pub last_seen: i64,
}

#[derive(Clone, Copy, Debug)]
pub struct Lifetimes {
    pub ttl: Duration,
    pub idle: Duration,
}

impl Lifetimes {
    /// How often `last_seen` is written back: at most once a minute, and often enough that the idle
    /// timeout is not overshot by more than a quarter.
    pub fn touch_every(&self) -> Duration {
        (self.idle / 4).min(Duration::from_secs(60))
    }

    fn remaining(&self, r: &SessionRecord, now: i64) -> Option<Duration> {
        let abs = r.created.saturating_add(self.ttl.as_millis() as i64) - now;
        let idle = r.last_seen.saturating_add(self.idle.as_millis() as i64) - now;
        let left = abs.min(idle);
        (left > 0).then(|| Duration::from_millis(left as u64))
    }
}

pub enum Found {
    Live(SessionRecord),
    Missing,
}

pub struct Sessions<'a> {
    pub hot: &'a dyn HotStore,
    pub life: Lifetimes,
}

impl Sessions<'_> {
    fn key(id: &str) -> String {
        format!("{SESSION_PREFIX}{}", digest(id))
    }

    /// Creates a session and returns its id (the cookie value).
    pub async fn create(&self, name: &str, groups: Vec<String>) -> anyhow::Result<String> {
        let id = random_secret();
        let now = now_ms();
        let r = SessionRecord {
            name: name.to_string(),
            groups,
            created: now,
            last_seen: now,
        };
        let ttl = self
            .life
            .remaining(&r, now)
            .unwrap_or(Duration::from_millis(1));
        self.hot
            .set(&Self::key(&id), &serde_json::to_string(&r)?, Some(ttl))
            .await?;
        Ok(id)
    }

    /// The live session of a cookie value. Updates `last_seen` (throttled); expired sessions are
    /// deleted and reported missing.
    pub async fn get(&self, id: &str) -> anyhow::Result<Found> {
        if !looks_like_secret(id) {
            return Ok(Found::Missing);
        }
        let key = Self::key(id);
        let Some(raw) = self.hot.get(&key).await? else {
            return Ok(Found::Missing);
        };
        let Ok(mut r) = serde_json::from_str::<SessionRecord>(&raw) else {
            self.hot.delete(&key).await?;
            return Ok(Found::Missing);
        };
        let now = now_ms();
        if self.life.remaining(&r, now).is_none() {
            self.hot.delete(&key).await?;
            return Ok(Found::Missing);
        }
        if now - r.last_seen >= self.life.touch_every().as_millis() as i64 {
            let old = raw;
            r.last_seen = now;
            if let Some(ttl) = self.life.remaining(&r, now) {
                let _ = self
                    .hot
                    .compare_and_set(&key, Some(&old), &serde_json::to_string(&r)?, Some(ttl))
                    .await?;
            }
        }
        Ok(Found::Live(r))
    }

    /// Deletes a session; returns its record when it existed.
    pub async fn delete(&self, id: &str) -> anyhow::Result<Option<SessionRecord>> {
        if !looks_like_secret(id) {
            return Ok(None);
        }
        let key = Self::key(id);
        let raw = self.hot.get(&key).await?;
        self.hot.delete(&key).await?;
        Ok(raw.and_then(|r| serde_json::from_str(&r).ok()))
    }
}

/// A login between the redirect to the IdP and the callback.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingLogin {
    pub nonce: String,
    pub verifier: String,
    pub return_to: String,
    /// SHA-256 of the login cookie that ties the callback to the browser that started the login.
    pub browser: String,
}

pub async fn put_pending(hot: &dyn HotStore, state: &str, p: &PendingLogin) -> anyhow::Result<()> {
    hot.set(
        &format!("{LOGIN_PREFIX}{}", digest(state)),
        &serde_json::to_string(p)?,
        Some(LOGIN_TTL),
    )
    .await
}

/// Takes a pending login out of the store: only the first caller with a state gets it.
pub async fn take_pending(hot: &dyn HotStore, state: &str) -> anyhow::Result<Option<PendingLogin>> {
    if !looks_like_secret(state) {
        return Ok(None);
    }
    let key = format!("{LOGIN_PREFIX}{}", digest(state));
    let Some(raw) = hot.get(&key).await? else {
        return Ok(None);
    };
    if !hot.delete(&key).await? {
        return Ok(None);
    }
    Ok(serde_json::from_str(&raw).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastmock_core::memory::MemoryHot;

    #[tokio::test]
    async fn idle_and_absolute_expiry() {
        let hot = MemoryHot::default();
        let s = Sessions {
            hot: &hot,
            life: Lifetimes {
                ttl: Duration::from_millis(900),
                idle: Duration::from_millis(400),
            },
        };
        let id = s.create("alice", vec!["g".into()]).await.unwrap();
        assert!(looks_like_secret(&id));
        assert!(matches!(s.get(&id).await.unwrap(), Found::Live(r) if r.name == "alice"));
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            assert!(matches!(s.get(&id).await.unwrap(), Found::Live(_)));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(matches!(s.get(&id).await.unwrap(), Found::Missing));
        let id = s.create("bob", vec![]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(450)).await;
        assert!(matches!(s.get(&id).await.unwrap(), Found::Missing));
        let id = s.create("carol", vec![]).await.unwrap();
        assert_eq!(s.delete(&id).await.unwrap().unwrap().name, "carol");
        assert!(matches!(s.get(&id).await.unwrap(), Found::Missing));
        assert!(matches!(s.get("short").await.unwrap(), Found::Missing));
    }

    #[tokio::test]
    async fn pending_logins_are_single_use() {
        let hot = MemoryHot::default();
        let state = random_secret();
        let p = PendingLogin {
            nonce: "n".into(),
            verifier: "v".into(),
            return_to: "/ui/".into(),
            browser: "b".into(),
        };
        put_pending(&hot, &state, &p).await.unwrap();
        assert!(take_pending(&hot, &state).await.unwrap().is_some());
        assert!(take_pending(&hot, &state).await.unwrap().is_none());
        assert!(take_pending(&hot, "x").await.unwrap().is_none());
    }
}
