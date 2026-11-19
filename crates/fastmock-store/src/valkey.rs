use std::time::Duration;

use async_trait::async_trait;
use fastmock_core::{HotStore, StoreResult};
use redis::aio::ConnectionManager;
use redis::{AsyncCommands, Script};

const CAS: &str = r#"
local cur = redis.call('GET', KEYS[1])
local ok
if ARGV[1] == '1' then ok = (cur == false) else ok = (cur ~= false and cur == ARGV[2]) end
if not ok then return 0 end
if ARGV[4] ~= '' then redis.call('SET', KEYS[1], ARGV[3], 'PX', ARGV[4]) else redis.call('SET', KEYS[1], ARGV[3]) end
return 1
"#;

const INCR: &str = r#"
local v = redis.call('INCRBY', KEYS[1], ARGV[2])
if v == tonumber(ARGV[2]) and ARGV[1] ~= '' then redis.call('PEXPIRE', KEYS[1], ARGV[1]) end
return v
"#;

const ZPOP: &str = r#"
local m = redis.call('ZRANGEBYSCORE', KEYS[1], '-inf', ARGV[1], 'LIMIT', 0, ARGV[2])
if #m > 0 then redis.call('ZREM', KEYS[1], unpack(m)) end
return m
"#;

#[derive(Clone)]
pub struct ValkeyStore {
    conn: ConnectionManager,
    prefix: String,
    cas: Script,
    incr: Script,
    zpop: Script,
}

fn escape_glob(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn ttl_ms(ttl: Option<Duration>) -> String {
    ttl.map(|t| t.as_millis().max(1).to_string())
        .unwrap_or_default()
}

impl ValkeyStore {
    pub async fn connect(url: &str, prefix: &str) -> anyhow::Result<Self> {
        let client = redis::Client::open(url)?;
        let config = redis::aio::ConnectionManagerConfig::new()
            .set_connection_timeout(Duration::from_secs(5))
            .set_response_timeout(Duration::from_secs(5))
            .set_number_of_retries(3);
        let conn = ConnectionManager::new_with_config(client, config).await?;
        Ok(Self {
            conn,
            prefix: prefix.to_string(),
            cas: Script::new(CAS),
            incr: Script::new(INCR),
            zpop: Script::new(ZPOP),
        })
    }

    fn k(&self, key: &str) -> String {
        format!("{}{}", self.prefix, key)
    }

    async fn scan_keys(&self, prefix: &str, limit: usize) -> StoreResult<Vec<String>> {
        let mut conn = self.conn.clone();
        let pattern = format!("{}*", escape_glob(&self.k(prefix)));
        let mut cursor: u64 = 0;
        let mut keys = Vec::new();
        loop {
            let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(500)
                .query_async(&mut conn)
                .await?;
            keys.extend(batch);
            cursor = next;
            if cursor == 0 || keys.len() >= limit {
                break;
            }
        }
        keys.sort();
        keys.dedup();
        keys.truncate(limit);
        Ok(keys)
    }
}

#[async_trait]
impl HotStore for ValkeyStore {
    async fn get(&self, key: &str) -> StoreResult<Option<String>> {
        let mut c = self.conn.clone();
        Ok(c.get(self.k(key)).await?)
    }

    async fn set(&self, key: &str, value: &str, ttl: Option<Duration>) -> StoreResult<()> {
        let mut c = self.conn.clone();
        match ttl {
            Some(t) => {
                c.pset_ex::<_, _, ()>(self.k(key), value, t.as_millis().max(1) as u64)
                    .await?
            }
            None => c.set::<_, _, ()>(self.k(key), value).await?,
        }
        Ok(())
    }

    async fn delete(&self, key: &str) -> StoreResult<bool> {
        let mut c = self.conn.clone();
        let n: i64 = c.del(self.k(key)).await?;
        Ok(n > 0)
    }

    async fn incr(&self, key: &str, ttl: Option<Duration>) -> StoreResult<i64> {
        self.incr_by(key, 1, ttl).await
    }

    async fn incr_by(&self, key: &str, delta: i64, ttl: Option<Duration>) -> StoreResult<i64> {
        let mut c = self.conn.clone();
        Ok(self
            .incr
            .key(self.k(key))
            .arg(ttl_ms(ttl))
            .arg(delta)
            .invoke_async(&mut c)
            .await?)
    }

    async fn compare_and_set(
        &self,
        key: &str,
        expected: Option<&str>,
        value: &str,
        ttl: Option<Duration>,
    ) -> StoreResult<bool> {
        let mut c = self.conn.clone();
        let r: i64 = self
            .cas
            .key(self.k(key))
            .arg(if expected.is_none() { "1" } else { "0" })
            .arg(expected.unwrap_or(""))
            .arg(value)
            .arg(ttl_ms(ttl))
            .invoke_async(&mut c)
            .await?;
        Ok(r == 1)
    }

    async fn scan(&self, prefix: &str, limit: usize) -> StoreResult<Vec<(String, String)>> {
        let keys = self.scan_keys(prefix, limit).await?;
        if keys.is_empty() {
            return Ok(vec![]);
        }
        let mut c = self.conn.clone();
        let values: Vec<Option<String>> = redis::cmd("MGET").arg(&keys).query_async(&mut c).await?;
        Ok(keys
            .into_iter()
            .zip(values)
            .filter_map(|(k, v)| {
                v.map(|v| (k.strip_prefix(&self.prefix).unwrap_or(&k).to_string(), v))
            })
            .collect())
    }

    async fn delete_prefix(&self, prefix: &str) -> StoreResult<u64> {
        let keys = self.scan_keys(prefix, usize::MAX).await?;
        let mut c = self.conn.clone();
        let mut n = 0u64;
        for chunk in keys.chunks(500) {
            let d: i64 = redis::cmd("UNLINK").arg(chunk).query_async(&mut c).await?;
            n += d as u64;
        }
        Ok(n)
    }

    async fn zadd(&self, key: &str, score: f64, member: &str) -> StoreResult<()> {
        let mut c = self.conn.clone();
        let _: i64 = c.zadd(self.k(key), member, score).await?;
        Ok(())
    }

    async fn zpop_until(
        &self,
        key: &str,
        max_score: f64,
        limit: usize,
    ) -> StoreResult<Vec<String>> {
        let mut c = self.conn.clone();
        let mut out = Vec::new();
        while out.len() < limit {
            let batch = (limit - out.len()).min(1000);
            let got: Vec<String> = self
                .zpop
                .key(self.k(key))
                .arg(max_score)
                .arg(batch)
                .invoke_async(&mut c)
                .await?;
            let n = got.len();
            out.extend(got);
            if n < batch {
                break;
            }
        }
        Ok(out)
    }

    async fn ping(&self) -> StoreResult<()> {
        let mut c = self.conn.clone();
        let _: String = redis::cmd("PING").query_async(&mut c).await?;
        Ok(())
    }
}
