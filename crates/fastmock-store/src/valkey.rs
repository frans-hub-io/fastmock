use std::path::PathBuf;
use std::time::Duration;

use anyhow::bail;
use async_trait::async_trait;
use fastmock_core::{HotStore, StoreResult};
use redis::aio::ConnectionManager;
use redis::{
    AsyncCommands, ClientTlsConfig, ConnectionAddr, IntoConnectionInfo, Script, TlsCertificates,
};

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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ValkeyTls {
    pub ca_file: Option<PathBuf>,
    pub cert_file: Option<PathBuf>,
    pub key_file: Option<PathBuf>,
}

impl ValkeyTls {
    pub fn is_set(&self) -> bool {
        self.ca_file.is_some() || self.cert_file.is_some() || self.key_file.is_some()
    }
}

pub fn valkey_client(
    url: &str,
    username: Option<&str>,
    tls: &ValkeyTls,
) -> anyhow::Result<redis::Client> {
    let mut info = url
        .into_connection_info()
        .map_err(|e| anyhow::anyhow!("not a valid Valkey URL: {e}"))?;
    let is_tls = match &info.addr {
        ConnectionAddr::TcpTls { insecure: true, .. } => bail!(
            "the `#insecure` URL fragment disables certificate verification and is not supported: set FASTMOCK_VALKEY_TLS_CA_FILE instead"
        ),
        ConnectionAddr::TcpTls { .. } => true,
        _ => false,
    };
    if tls.is_set() && !is_tls {
        bail!(
            "FASTMOCK_VALKEY_TLS_CA_FILE, FASTMOCK_VALKEY_TLS_CERT_FILE and FASTMOCK_VALKEY_TLS_KEY_FILE need a rediss:// (or valkeys://) URL"
        );
    }
    if let Some(u) = username {
        info.redis.username = Some(u.to_string());
    }
    if info.redis.username.is_some() && info.redis.password.is_none() {
        bail!(
            "a Valkey ACL user is set without a password: set FASTMOCK_VALKEY_PASSWORD (or put the password in the URL)"
        );
    }
    let client_tls = match (&tls.cert_file, &tls.key_file) {
        (Some(c), Some(k)) => Some(ClientTlsConfig {
            client_cert: crate::pem::read_certs(
                "Valkey client certificate (FASTMOCK_VALKEY_TLS_CERT_FILE)",
                c,
            )?,
            client_key: crate::pem::read_key(
                "Valkey client key (FASTMOCK_VALKEY_TLS_KEY_FILE)",
                k,
            )?,
        }),
        (None, None) => None,
        _ => bail!("set both FASTMOCK_VALKEY_TLS_CERT_FILE and FASTMOCK_VALKEY_TLS_KEY_FILE"),
    };
    let root_cert = tls
        .ca_file
        .as_deref()
        .map(|p| crate::pem::read_certs("Valkey CA file (FASTMOCK_VALKEY_TLS_CA_FILE)", p))
        .transpose()?;
    if is_tls {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    let client = if tls.is_set() {
        redis::Client::build_with_tls(
            info,
            TlsCertificates {
                client_tls,
                root_cert,
            },
        )
    } else {
        redis::Client::open(info)
    };
    client.map_err(|e| anyhow::anyhow!("invalid Valkey settings: {e}"))
}

impl ValkeyStore {
    pub async fn connect(url: &str, prefix: &str) -> anyhow::Result<Self> {
        Self::connect_with(valkey_client(url, None, &ValkeyTls::default())?, prefix).await
    }

    pub async fn connect_with(client: redis::Client, prefix: &str) -> anyhow::Result<Self> {
        let config = redis::aio::ConnectionManagerConfig::new()
            .set_connection_timeout(Duration::from_secs(5))
            .set_response_timeout(Duration::from_secs(5))
            .set_number_of_retries(3);
        let explain = |e: redis::RedisError| {
            let msg = e.to_string();
            match crate::pem::tls_hint(&msg, "FASTMOCK_VALKEY_TLS_CA_FILE") {
                Some(hint) => anyhow::anyhow!("{msg} ({hint})"),
                None => anyhow::Error::from(e),
            }
        };
        let probe = redis::AsyncConnectionConfig::new()
            .set_connection_timeout(Duration::from_secs(5))
            .set_response_timeout(Duration::from_secs(5));
        drop(
            client
                .get_multiplexed_async_connection_with_config(&probe)
                .await
                .map_err(explain)?,
        );
        let conn = ConnectionManager::new_with_config(client, config)
            .await
            .map_err(explain)?;
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
