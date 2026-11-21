//! Failed-login counters shared by every replica through Valkey: [`ValkeyLoginFailures`] implements
//! [`LoginFailures`]. A key's failures live in `<prefix>mail:login:n:<digest>` (expiring after the
//! window) and a block in `<prefix>mail:login:block:<digest>` (expiring after the backoff); the digest
//! is a SHA-256 of the key, so client addresses and user names are not stored. When Valkey cannot be
//! reached the counters fail open: nothing is blocked and the error is logged.

use async_trait::async_trait;
use fastmock_core::mail::{LoginFailures, ThrottlePolicy};
use redis::Script;
use redis::aio::ConnectionManager;
use sha2::{Digest, Sha256};

const FAILED: &str = r"
local n = redis.call('INCR', KEYS[1])
local window = tonumber(ARGV[1])
if n == 1 then redis.call('PEXPIRE', KEYS[1], window) end
local limit = tonumber(ARGV[2])
if limit > 0 and n >= limit then
  local d = math.min(tonumber(ARGV[3]) * 2 ^ math.min(n - limit, 16), tonumber(ARGV[4]))
  d = math.floor(d)
  redis.call('SET', KEYS[2], '1', 'PX', d)
  redis.call('PEXPIRE', KEYS[1], window + d)
end
return n
";

/// [`LoginFailures`] in Valkey.
#[derive(Clone)]
pub struct ValkeyLoginFailures {
    conn: ConnectionManager,
    prefix: String,
    failed: Script,
}

impl ValkeyLoginFailures {
    pub fn new(conn: ConnectionManager, prefix: &str) -> Self {
        Self {
            conn,
            prefix: format!("{prefix}mail:login:"),
            failed: Script::new(FAILED),
        }
    }

    fn keys(&self, key: &str) -> (String, String) {
        let d = hex::encode(&Sha256::digest(key.as_bytes())[..16]);
        (
            format!("{}n:{d}", self.prefix),
            format!("{}block:{d}", self.prefix),
        )
    }
}

fn warn(e: &redis::RedisError) {
    metrics::counter!("fastmock_store_errors_total", "store" => "mail_login").increment(1);
    tracing::warn!(error = %e, "Valkey login counters are unavailable");
}

#[async_trait]
impl LoginFailures for ValkeyLoginFailures {
    async fn blocked(&self, key: &str) -> bool {
        let (_, block) = self.keys(key);
        let mut c = self.conn.clone();
        match redis::cmd("EXISTS")
            .arg(&block)
            .query_async::<i64>(&mut c)
            .await
        {
            Ok(n) => n > 0,
            Err(e) => {
                warn(&e);
                false
            }
        }
    }

    async fn failed(&self, key: &str, policy: &ThrottlePolicy) {
        if policy.limit == 0 {
            return;
        }
        let (n, block) = self.keys(key);
        let mut c = self.conn.clone();
        let r = self
            .failed
            .key(&n)
            .key(&block)
            .arg(policy.window.as_millis() as u64)
            .arg(policy.limit)
            .arg(policy.base_backoff.as_millis() as u64)
            .arg(policy.max_backoff.as_millis() as u64)
            .invoke_async::<i64>(&mut c)
            .await;
        if let Err(e) = r {
            warn(&e);
        }
    }

    async fn clear(&self, key: &str) {
        let (n, block) = self.keys(key);
        let mut c = self.conn.clone();
        if let Err(e) = redis::cmd("DEL")
            .arg(&n)
            .arg(&block)
            .query_async::<i64>(&mut c)
            .await
        {
            warn(&e);
        }
    }
}
