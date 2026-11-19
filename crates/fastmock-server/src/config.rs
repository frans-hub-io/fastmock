use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use fastmock_store::{MigrateMode, PgTls, StorageOptions, ValkeyTls};
use ipnet::IpNet;

#[derive(Clone, PartialEq, Eq)]
pub enum StorageKind {
    Memory,
    Postgres {
        database_url: String,
        valkey_url: String,
        valkey_prefix: String,
        max_connections: u32,
        options: Box<StorageOptions>,
    },
}

impl std::fmt::Debug for StorageKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageKind::Memory => f.write_str("Memory"),
            StorageKind::Postgres {
                database_url,
                valkey_url,
                valkey_prefix,
                max_connections,
                options,
            } => f
                .debug_struct("Postgres")
                .field("database_url", &redact_url(database_url))
                .field("valkey_url", &redact_url(valkey_url))
                .field("valkey_prefix", valkey_prefix)
                .field("max_connections", max_connections)
                .field("options", options)
                .finish(),
        }
    }
}

pub fn redact_url(url: &str) -> String {
    let Ok(mut u) = reqwest::Url::parse(url) else {
        return "<unparsable URL>".into();
    };
    if u.password().is_some() {
        let _ = u.set_password(Some("REDACTED"));
    }
    if u.query().is_some() {
        let pairs: Vec<(String, String)> = u
            .query_pairs()
            .map(|(k, v)| {
                let secret = matches!(
                    k.to_ascii_lowercase().as_str(),
                    "password" | "pass" | "passwd"
                );
                (
                    k.into_owned(),
                    if secret {
                        "REDACTED".into()
                    } else {
                        v.into_owned()
                    },
                )
            })
            .collect();
        u.query_pairs_mut().clear().extend_pairs(pairs);
    }
    u.to_string()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseSettings {
    pub url: String,
    pub tls: PgTls,
    pub migrate: MigrateMode,
    pub migration_timeout: Duration,
}

pub fn database_from_env() -> anyhow::Result<DatabaseSettings> {
    database_settings(&env)
}

fn database_settings(env: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<DatabaseSettings> {
    let url = with_password(
        &env("DATABASE_URL").context("FASTMOCK_DATABASE_URL is required for postgres storage")?,
        env("DATABASE_PASSWORD"),
        "FASTMOCK_DATABASE_URL",
    )?;
    let tls = PgTls {
        mode: env("DATABASE_SSL_MODE")
            .map(|m| {
                m.parse()
                    .map_err(|e| anyhow::anyhow!("FASTMOCK_DATABASE_SSL_MODE: {e}"))
            })
            .transpose()?,
        root_cert: env("DATABASE_SSL_ROOT_CERT").map(PathBuf::from),
        client_cert: env("DATABASE_SSL_CERT").map(PathBuf::from),
        client_key: env("DATABASE_SSL_KEY").map(PathBuf::from),
    };
    if tls.client_cert.is_some() != tls.client_key.is_some() {
        bail!("set both FASTMOCK_DATABASE_SSL_CERT and FASTMOCK_DATABASE_SSL_KEY, or neither");
    }
    let migrate = match env("DATABASE_MIGRATE").as_deref().map(str::trim) {
        None | Some("auto") => MigrateMode::Auto,
        Some("skip") => MigrateMode::Skip,
        Some(other) => bail!("FASTMOCK_DATABASE_MIGRATE must be auto or skip, got {other}"),
    };
    let migration_timeout = match env("DATABASE_MIGRATION_TIMEOUT") {
        Some(v) => parse_duration(&v).context("FASTMOCK_DATABASE_MIGRATION_TIMEOUT")?,
        None => fastmock_store::DEFAULT_MIGRATION_TIMEOUT,
    };
    if migration_timeout.is_zero() {
        bail!("FASTMOCK_DATABASE_MIGRATION_TIMEOUT must be greater than zero");
    }
    fastmock_store::pg_connect_options(&url, &tls)
        .context("invalid PostgreSQL connection settings")?;
    Ok(DatabaseSettings {
        url,
        tls,
        migrate,
        migration_timeout,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValkeySettings {
    pub url: String,
    pub username: Option<String>,
    pub tls: ValkeyTls,
}

pub fn valkey_from_env() -> anyhow::Result<ValkeySettings> {
    valkey_settings(&env)
}

fn valkey_settings(env: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<ValkeySettings> {
    let url = with_password(
        &env("VALKEY_URL").context("FASTMOCK_VALKEY_URL is required for postgres storage")?,
        env("VALKEY_PASSWORD"),
        "FASTMOCK_VALKEY_URL",
    )?;
    let username = env("VALKEY_USERNAME").map(|u| u.trim().to_string());
    let tls = ValkeyTls {
        ca_file: env("VALKEY_TLS_CA_FILE").map(PathBuf::from),
        cert_file: env("VALKEY_TLS_CERT_FILE").map(PathBuf::from),
        key_file: env("VALKEY_TLS_KEY_FILE").map(PathBuf::from),
    };
    fastmock_store::valkey_client(&url, username.as_deref(), &tls)
        .context("invalid Valkey connection settings")?;
    Ok(ValkeySettings { url, username, tls })
}

#[derive(Clone, Debug)]
pub struct Config {
    pub listen: SocketAddr,
    pub admin_listen: SocketAddr,
    pub storage: StorageKind,
    pub auth_disabled: bool,
    pub admin_tokens: Vec<String>,
    pub admin_tokens_file: Option<PathBuf>,
    pub admin_allowed_origins: Vec<String>,
    pub max_body_bytes: usize,
    pub max_delay: Duration,
    pub journal_enabled: bool,
    pub journal_max_body: usize,
    pub journal_retention: Duration,
    pub redact_headers: Vec<String>,
    pub redact_json_keys: Vec<String>,
    pub unmatched_details: bool,
    pub callback_workers: usize,
    pub callback_batch: i64,
    pub callback_timeout: Duration,
    pub callback_poll: Duration,
    pub callback_allowed_hosts: Vec<String>,
    pub callback_denied_cidrs: Vec<IpNet>,
    pub outbound_ca_file: Option<PathBuf>,
    pub callback_retention: Duration,
    pub plugins: Option<Vec<String>>,
    pub secrets_dir: Option<PathBuf>,
    pub stub_poll: Duration,
    pub log_format: LogFormat,
    pub tls: Option<TlsSettings>,
    pub kafka_bootstrap: Option<Vec<String>>,
    pub kafka_schema_registry: Option<String>,
    pub plugin_overrides:
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
    pub max_inflight_body_bytes: usize,
    pub journal_queue: usize,
    pub stub_secrets: Vec<String>,
    pub recording_max_stubs: i64,
    pub kafka_allowed_topics: Option<Vec<String>>,
    pub shutdown_delay: Duration,
    pub header_read_timeout: Duration,
    pub contract_secrets: Vec<ContractSecret>,
    pub kafka_contract_topics: Vec<String>,
    pub ui_data_plane_origins: Vec<String>,
    pub journal_scan_timeout: Duration,
    pub journal_scan_max_work: u64,
    pub journal_scan_concurrency: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractSecret {
    pub name: String,
    pub hosts: Vec<String>,
}

fn secret_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub fn parse_contract_secrets(
    raw: &str,
    stub_secrets: &[String],
) -> anyhow::Result<Vec<ContractSecret>> {
    let mut out: Vec<ContractSecret> = Vec::new();
    for item in list(raw) {
        let Some((name, hosts)) = item.split_once('=') else {
            bail!("FASTMOCK_CONTRACT_SECRETS: `{item}` must look like name=host1|host2");
        };
        let name = name.trim();
        if !secret_name_ok(name) {
            bail!("FASTMOCK_CONTRACT_SECRETS: invalid secret name `{name}`");
        }
        if stub_secrets.iter().any(|s| s == name) {
            bail!(
                "FASTMOCK_CONTRACT_SECRETS: `{name}` is also listed in FASTMOCK_STUB_SECRETS; use a separate secret for contract tests"
            );
        }
        if out.iter().any(|c| c.name == name) {
            bail!("FASTMOCK_CONTRACT_SECRETS: `{name}` is listed twice");
        }
        let hosts: Vec<String> = hosts
            .split('|')
            .map(|h| h.trim().trim_end_matches('.').to_ascii_lowercase())
            .collect();
        for h in &hosts {
            let bare = h.strip_prefix("*.").unwrap_or(h);
            if bare.is_empty()
                || bare.contains('*')
                || h.chars()
                    .any(|c| c.is_whitespace() || matches!(c, '/' | '@' | '?' | '#'))
            {
                bail!("FASTMOCK_CONTRACT_SECRETS: invalid host `{h}` for secret `{name}`");
            }
        }
        out.push(ContractSecret {
            name: name.to_string(),
            hosts,
        });
    }
    Ok(out)
}

pub fn parse_origins(raw: &str, what: &str) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    for item in list(raw) {
        let u = reqwest::Url::parse(&item).with_context(|| format!("{what}: `{item}`"))?;
        if !matches!(u.scheme(), "http" | "https")
            || u.host_str().is_none_or(|h| {
                !h.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
            })
            || !u.username().is_empty()
            || u.password().is_some()
            || u.query().is_some()
            || u.fragment().is_some()
            || u.path() != "/"
        {
            bail!("{what}: `{item}` must be an origin such as https://mock.example.com:8080");
        }
        let o = u.origin().ascii_serialization();
        if !out.contains(&o) {
            out.push(o);
        }
    }
    Ok(out)
}

#[derive(Clone, Debug)]
pub struct TlsSettings {
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
    pub client_ca_file: Option<PathBuf>,
    pub client_auth_required: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFormat {
    Json,
    Text,
}

pub const DEFAULT_REDACT_HEADERS: &str = "authorization,proxy-authorization,cookie,set-cookie,x-api-key,apikey,api-key,x-auth-token,x-callback-token,x-goog-signature,x-signature,x-hub-signature,x-hub-signature-256,sha2-signature,ocp-apim-subscription-key,x-amz-security-token,x-fastmock-signature";
pub const DEFAULT_REDACT_KEYS: &str = "password,passwd,secret,token,access_token,refresh_token,id_token,client_secret,otp,apikey,api_key,authorization,auth,pin,cvv,sig,signature,key,private_key,session";
pub const DEFAULT_JOURNAL_SCAN_MAX_WORK: u64 = 4 * 1024 * 1024 * 1024;
pub const DEFAULT_DENIED_CIDRS: &str = "127.0.0.0/8,::1/128,::/128,::ffff:127.0.0.0/104,::ffff:169.254.0.0/112,::ffff:0.0.0.0/104,169.254.0.0/16,fe80::/10,fd00:ec2::254/128,100.100.100.200/32,0.0.0.0/8,224.0.0.0/4,ff00::/8";

pub fn with_password(url: &str, password: Option<String>, what: &str) -> anyhow::Result<String> {
    let Some(pw) = password else {
        return Ok(url.to_string());
    };
    let mut u = reqwest::Url::parse(url).with_context(|| format!("{what} is not a valid URL"))?;
    u.set_password(Some(&pw))
        .map_err(|_| anyhow::anyhow!("{what} cannot carry a password"))?;
    Ok(u.to_string())
}

fn env(name: &str) -> Option<String> {
    std::env::var(format!("FASTMOCK_{name}"))
        .ok()
        .filter(|v| !v.trim().is_empty())
}

fn list(v: &str) -> Vec<String> {
    v.split([',', '\n'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse<T: std::str::FromStr>(name: &str, default: T) -> anyhow::Result<T>
where
    T::Err: std::fmt::Display,
{
    match env(name) {
        None => Ok(default),
        Some(v) => v
            .trim()
            .parse::<T>()
            .map_err(|e| anyhow::anyhow!("FASTMOCK_{name}: {e}")),
    }
}

fn bool_env(name: &str, default: bool) -> anyhow::Result<bool> {
    match env(name).as_deref().map(str::to_ascii_lowercase).as_deref() {
        None => Ok(default),
        Some("1" | "true" | "yes" | "on") => Ok(true),
        Some("0" | "false" | "no" | "off") => Ok(false),
        Some(other) => bail!("FASTMOCK_{name}: not a boolean: {other}"),
    }
}

pub fn parse_duration(s: &str) -> anyhow::Result<Duration> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num
        .parse()
        .with_context(|| format!("invalid duration `{s}`"))?;
    Ok(match unit {
        "ms" => Duration::from_millis(n),
        "" | "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        "h" => Duration::from_secs(n * 3600),
        "d" => Duration::from_secs(n * 86400),
        other => bail!("invalid duration unit `{other}` in `{s}`"),
    })
}

fn dur(name: &str, default: &str) -> anyhow::Result<Duration> {
    parse_duration(&env(name).unwrap_or_else(|| default.to_string()))
        .with_context(|| format!("FASTMOCK_{name}"))
}

impl Config {
    pub fn for_tests(storage: StorageKind, tokens: Vec<String>) -> Self {
        Self {
            listen: "127.0.0.1:0".parse().unwrap(),
            admin_listen: "127.0.0.1:1".parse().unwrap(),
            storage,
            auth_disabled: false,
            admin_tokens: tokens,
            admin_tokens_file: None,
            admin_allowed_origins: vec![],
            max_body_bytes: 1024 * 1024,
            max_delay: Duration::from_secs(5),
            journal_enabled: true,
            journal_max_body: 64 * 1024,
            journal_retention: Duration::from_secs(3600),
            redact_headers: list(DEFAULT_REDACT_HEADERS),
            redact_json_keys: list(DEFAULT_REDACT_KEYS),
            unmatched_details: true,
            callback_workers: 8,
            callback_batch: 50,
            callback_timeout: Duration::from_secs(5),
            callback_poll: Duration::from_millis(50),
            callback_allowed_hosts: vec![],
            callback_denied_cidrs: vec!["169.254.0.0/16".parse().unwrap()],
            outbound_ca_file: None,
            callback_retention: Duration::from_secs(3600),
            plugins: None,
            secrets_dir: None,
            stub_poll: Duration::from_millis(200),
            log_format: LogFormat::Text,
            tls: None,
            kafka_bootstrap: None,
            kafka_schema_registry: None,
            plugin_overrides: Default::default(),
            max_inflight_body_bytes: 64 * 1024 * 1024,
            journal_queue: 1024,
            stub_secrets: vec![],
            recording_max_stubs: 1000,
            kafka_allowed_topics: None,
            shutdown_delay: Duration::ZERO,
            header_read_timeout: Duration::from_secs(30),
            contract_secrets: vec![],
            kafka_contract_topics: vec![],
            ui_data_plane_origins: vec![],
            journal_scan_timeout: Duration::from_secs(10),
            journal_scan_max_work: DEFAULT_JOURNAL_SCAN_MAX_WORK,
            journal_scan_concurrency: 2,
        }
    }

    pub fn from_env() -> anyhow::Result<Self> {
        let storage = match env("STORAGE").as_deref().unwrap_or("postgres") {
            "memory" => StorageKind::Memory,
            "postgres" => {
                let db = database_from_env()?;
                let valkey = valkey_from_env()?;
                StorageKind::Postgres {
                    database_url: db.url,
                    valkey_url: valkey.url,
                    valkey_prefix: env("VALKEY_PREFIX").unwrap_or_else(|| "fastmock:".into()),
                    max_connections: parse("DB_MAX_CONNECTIONS", 10u32)?,
                    options: Box::new(StorageOptions {
                        database_tls: db.tls,
                        migrate: db.migrate,
                        migration_timeout: db.migration_timeout,
                        valkey_username: valkey.username,
                        valkey_tls: valkey.tls,
                    }),
                }
            }
            other => bail!("FASTMOCK_STORAGE must be memory or postgres, got {other}"),
        };
        let denied =
            list(&env("CALLBACK_DENIED_CIDRS").unwrap_or_else(|| DEFAULT_DENIED_CIDRS.into()))
                .iter()
                .map(|c| {
                    c.parse::<IpNet>()
                        .with_context(|| format!("FASTMOCK_CALLBACK_DENIED_CIDRS: {c}"))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
        let cfg = Self {
            listen: parse("LISTEN", "0.0.0.0:8080".parse()?)?,
            admin_listen: parse("ADMIN_LISTEN", "0.0.0.0:9090".parse()?)?,
            storage,
            auth_disabled: bool_env("AUTH_DISABLED", false)?,
            admin_tokens: env("ADMIN_TOKENS").map(|v| list(&v)).unwrap_or_default(),
            admin_tokens_file: env("ADMIN_TOKENS_FILE").map(PathBuf::from),
            admin_allowed_origins: env("ADMIN_ALLOWED_ORIGINS")
                .map(|v| list(&v))
                .unwrap_or_default(),
            max_body_bytes: parse("MAX_BODY_BYTES", 10 * 1024 * 1024usize)?,
            max_delay: dur("MAX_DELAY", "15m")?,
            journal_enabled: bool_env("JOURNAL_ENABLED", true)?,
            journal_max_body: parse("JOURNAL_MAX_BODY_BYTES", 64 * 1024usize)?,
            journal_retention: dur("JOURNAL_RETENTION", "24h")?,
            redact_headers: list(
                &env("REDACT_HEADERS").unwrap_or_else(|| DEFAULT_REDACT_HEADERS.into()),
            )
            .into_iter()
            .map(|h| h.to_ascii_lowercase())
            .collect(),
            redact_json_keys: list(
                &env("REDACT_JSON_KEYS").unwrap_or_else(|| DEFAULT_REDACT_KEYS.into()),
            )
            .into_iter()
            .map(|h| h.to_ascii_lowercase())
            .collect(),
            unmatched_details: bool_env("UNMATCHED_DETAILS", false)?,
            callback_workers: parse("CALLBACK_WORKERS", 32usize)?.max(1),
            callback_batch: parse("CALLBACK_BATCH", 50i64)?.max(1),
            callback_timeout: dur("CALLBACK_TIMEOUT", "10s")?,
            callback_poll: dur("CALLBACK_POLL", "200ms")?,
            callback_allowed_hosts: env("CALLBACK_ALLOWED_HOSTS")
                .map(|v| list(&v))
                .unwrap_or_default(),
            callback_denied_cidrs: denied,
            outbound_ca_file: env("OUTBOUND_CA_FILE").map(PathBuf::from),
            callback_retention: dur("CALLBACK_RETENTION", "24h")?,
            plugins: env("PLUGINS").map(|v| list(&v)),
            secrets_dir: env("SECRETS_DIR").map(PathBuf::from),
            stub_poll: dur("STUB_POLL", "5s")?,
            log_format: match env("LOG_FORMAT").as_deref().unwrap_or("json") {
                "json" => LogFormat::Json,
                "text" => LogFormat::Text,
                other => bail!("FASTMOCK_LOG_FORMAT must be json or text, got {other}"),
            },
            tls: match (env("TLS_CERT_FILE"), env("TLS_KEY_FILE")) {
                (Some(c), Some(k)) => Some(TlsSettings {
                    cert_file: c.into(),
                    key_file: k.into(),
                    client_ca_file: env("TLS_CLIENT_CA_FILE").map(PathBuf::from),
                    client_auth_required: match env("TLS_CLIENT_AUTH")
                        .as_deref()
                        .unwrap_or("required")
                    {
                        "required" => true,
                        "optional" => false,
                        other => bail!(
                            "FASTMOCK_TLS_CLIENT_AUTH must be required or optional, got {other}"
                        ),
                    },
                }),
                (None, None) => None,
                _ => bail!("set both FASTMOCK_TLS_CERT_FILE and FASTMOCK_TLS_KEY_FILE"),
            },
            kafka_bootstrap: env("KAFKA_BOOTSTRAP").map(|v| list(&v)),
            kafka_schema_registry: env("KAFKA_SCHEMA_REGISTRY_URL"),
            plugin_overrides: Default::default(),
            max_inflight_body_bytes: parse("MAX_INFLIGHT_BODY_BYTES", 256 * 1024 * 1024usize)?,
            journal_queue: parse("JOURNAL_QUEUE", 2048usize)?.max(1),
            stub_secrets: env("STUB_SECRETS").map(|v| list(&v)).unwrap_or_default(),
            recording_max_stubs: parse("RECORDING_MAX_STUBS", 1000i64)?.max(1),
            kafka_allowed_topics: env("KAFKA_ALLOWED_TOPICS").map(|v| list(&v)),
            shutdown_delay: dur("SHUTDOWN_DELAY", "5s")?,
            header_read_timeout: dur("HEADER_READ_TIMEOUT", "30s")?,
            contract_secrets: parse_contract_secrets(
                &env("CONTRACT_SECRETS").unwrap_or_default(),
                &env("STUB_SECRETS").map(|v| list(&v)).unwrap_or_default(),
            )?,
            kafka_contract_topics: env("KAFKA_CONTRACT_TOPICS")
                .map(|v| list(&v))
                .unwrap_or_default(),
            ui_data_plane_origins: parse_origins(
                &env("UI_DATA_PLANE_URL").unwrap_or_default(),
                "FASTMOCK_UI_DATA_PLANE_URL",
            )?,
            journal_scan_timeout: dur("JOURNAL_SCAN_TIMEOUT", "10s")?,
            journal_scan_max_work: parse("JOURNAL_SCAN_MAX_WORK", DEFAULT_JOURNAL_SCAN_MAX_WORK)?
                .max(1),
            journal_scan_concurrency: parse("JOURNAL_SCAN_CONCURRENCY", 2usize)?.max(1),
        };
        if cfg.max_inflight_body_bytes < cfg.max_body_bytes {
            bail!("FASTMOCK_MAX_INFLIGHT_BODY_BYTES must be at least FASTMOCK_MAX_BODY_BYTES");
        }
        if cfg.listen == cfg.admin_listen {
            bail!(
                "FASTMOCK_LISTEN and FASTMOCK_ADMIN_LISTEN must differ: the control plane is never served on the mock port"
            );
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_are_url_encoded() {
        let u = with_password(
            "postgres://fm@db:5432/fastmock",
            Some("p@ss:w/rd#".into()),
            "x",
        )
        .unwrap();
        assert_eq!(u, "postgres://fm:p%40ss%3Aw%2Frd%23@db:5432/fastmock");
        assert_eq!(
            with_password("redis://valkey:6379/", None, "x").unwrap(),
            "redis://valkey:6379/"
        );
        let v = with_password("redis://valkey:6379/", Some("a b".into()), "x").unwrap();
        assert_eq!(v, "redis://:a%20b@valkey:6379/");
    }

    #[test]
    fn contract_secrets_bind_names_to_hosts() {
        let v = parse_contract_secrets(
            "pets-token=api.example.com|*.pets.example.com, other=198.51.100.7",
            &["hook-key".into()],
        )
        .unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "pets-token");
        assert_eq!(v[0].hosts, vec!["api.example.com", "*.pets.example.com"]);
        assert!(parse_contract_secrets("hook-key=api.example.com", &["hook-key".into()]).is_err());
        assert!(parse_contract_secrets("x", &[]).is_err());
        assert!(parse_contract_secrets("x=", &[]).is_err());
        assert!(parse_contract_secrets("x=a.example.com|", &[]).is_err());
        assert!(parse_contract_secrets("x=*", &[]).is_err());
        assert!(parse_contract_secrets("x=https://a.example.com/", &[]).is_err());
        assert!(parse_contract_secrets("../x=a.example.com", &[]).is_err());
        assert!(parse_contract_secrets("x=a.example.com,x=b.example.com", &[]).is_err());
        assert!(parse_contract_secrets("", &[]).unwrap().is_empty());
    }

    #[test]
    fn ui_origins_are_exact() {
        assert_eq!(
            parse_origins(
                "https://mock.example.com:8443, http://Mock.example.com/",
                "x"
            )
            .unwrap(),
            vec!["https://mock.example.com:8443", "http://mock.example.com"]
        );
        assert!(parse_origins("https://*.example.com", "x").is_err());
        assert!(parse_origins("https://mock.example.com/path", "x").is_err());
        assert!(parse_origins("ftp://mock.example.com", "x").is_err());
    }

    struct Files {
        dir: PathBuf,
    }

    impl Files {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("fm-cfg-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&dir).unwrap();
            let key = rcgen::KeyPair::generate().unwrap();
            let cert = rcgen::CertificateParams::new(vec!["db.example.com".into()])
                .unwrap()
                .self_signed(&key)
                .unwrap();
            std::fs::write(dir.join("ca.pem"), cert.pem()).unwrap();
            std::fs::write(dir.join("cert.pem"), cert.pem()).unwrap();
            std::fs::write(dir.join("key.pem"), key.serialize_pem()).unwrap();
            std::fs::write(dir.join("junk.pem"), "not a certificate").unwrap();
            Self { dir }
        }

        fn path(&self, name: &str) -> String {
            self.dir.join(name).display().to_string()
        }
    }

    impl Drop for Files {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn lookup(pairs: &[(&str, String)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn db(pairs: &[(&str, String)]) -> anyhow::Result<DatabaseSettings> {
        database_settings(&lookup(pairs))
    }

    fn valkey(pairs: &[(&str, String)]) -> anyhow::Result<ValkeySettings> {
        valkey_settings(&lookup(pairs))
    }

    fn err<T: std::fmt::Debug>(r: anyhow::Result<T>) -> String {
        format!("{:#}", r.unwrap_err())
    }

    const DB: &str = "postgres://fm@db.example.com:5432/fastmock";

    #[test]
    fn database_defaults_keep_password_merge() {
        let d = db(&[
            ("DATABASE_URL", DB.into()),
            ("DATABASE_PASSWORD", "p@ss#1".into()),
        ])
        .unwrap();
        assert_eq!(
            d.url,
            "postgres://fm:p%40ss%231@db.example.com:5432/fastmock"
        );
        assert_eq!(d.tls, PgTls::default());
        assert_eq!(d.migrate, MigrateMode::Auto);
        assert_eq!(d.migration_timeout, Duration::from_secs(120));
        assert!(err(db(&[])).contains("FASTMOCK_DATABASE_URL is required"));
    }

    #[test]
    fn database_tls_settings_are_validated() {
        let f = Files::new();
        let d = db(&[
            ("DATABASE_URL", DB.into()),
            ("DATABASE_SSL_MODE", "verify-full".into()),
            ("DATABASE_SSL_ROOT_CERT", f.path("ca.pem")),
            ("DATABASE_SSL_CERT", f.path("cert.pem")),
            ("DATABASE_SSL_KEY", f.path("key.pem")),
        ])
        .unwrap();
        assert_eq!(d.tls.mode, Some(fastmock_store::SslMode::VerifyFull));
        assert_eq!(d.tls.root_cert, Some(PathBuf::from(f.path("ca.pem"))));
        let plan = fastmock_store::pg_connect_options(&d.url, &d.tls).unwrap();
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);

        let e = err(db(&[
            ("DATABASE_URL", DB.into()),
            ("DATABASE_SSL_MODE", "verify-everything".into()),
        ]));
        assert!(e.contains("FASTMOCK_DATABASE_SSL_MODE"), "{e}");
        assert!(e.contains("verify-full"), "{e}");

        let e = err(db(&[
            ("DATABASE_URL", DB.into()),
            ("DATABASE_SSL_CERT", f.path("cert.pem")),
        ]));
        assert!(e.contains("FASTMOCK_DATABASE_SSL_KEY"), "{e}");

        let e = err(db(&[
            ("DATABASE_URL", DB.into()),
            ("DATABASE_SSL_MODE", "verify-full".into()),
            ("DATABASE_SSL_ROOT_CERT", f.path("missing.pem")),
        ]));
        assert!(e.contains("cannot read"), "{e}");
        assert!(e.contains("missing.pem"), "{e}");

        let e = err(db(&[
            ("DATABASE_URL", DB.into()),
            ("DATABASE_SSL_ROOT_CERT", f.path("junk.pem")),
        ]));
        assert!(e.contains("no PEM certificate"), "{e}");

        let e = err(db(&[
            ("DATABASE_URL", DB.into()),
            ("DATABASE_SSL_CERT", f.path("cert.pem")),
            ("DATABASE_SSL_KEY", f.path("junk.pem")),
        ]));
        assert!(e.contains("no usable private key"), "{e}");
    }

    #[test]
    fn database_url_tls_parameters_still_work() {
        let f = Files::new();
        let url = format!(
            "{DB}?sslmode=verify-ca&sslrootcert={}&sslcert={}&sslkey={}",
            f.path("ca.pem"),
            f.path("cert.pem"),
            f.path("key.pem")
        );
        let d = db(&[("DATABASE_URL", url.clone())]).unwrap();
        let plan = fastmock_store::pg_connect_options(&d.url, &d.tls).unwrap();
        assert_eq!(plan.mode, fastmock_store::SslMode::VerifyCa);
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);

        let d = db(&[
            ("DATABASE_URL", url),
            ("DATABASE_SSL_MODE", "verify-full".into()),
        ])
        .unwrap();
        let plan = fastmock_store::pg_connect_options(&d.url, &d.tls).unwrap();
        assert_eq!(plan.mode, fastmock_store::SslMode::VerifyFull);

        let e = err(db(&[(
            "DATABASE_URL",
            format!("{DB}?sslcert={}", f.path("cert.pem")),
        )]));
        assert!(e.contains("without a key"), "{e}");

        let e = err(db(&[(
            "DATABASE_URL",
            format!(
                "{DB}?sslmode=verify-full&sslrootcert={}",
                f.path("nope.pem")
            ),
        )]));
        assert!(e.contains("cannot read"), "{e}");

        assert!(err(db(&[("DATABASE_URL", format!("{DB}?sslmode=strict"))])).contains("ssl"));
    }

    #[test]
    fn database_tls_warnings_explain_trust() {
        let f = Files::new();
        let plan = fastmock_store::pg_connect_options(
            DB,
            &PgTls {
                mode: Some(fastmock_store::SslMode::VerifyFull),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(plan.warnings.len(), 1);
        assert!(plan.warnings[0].contains("FASTMOCK_DATABASE_SSL_ROOT_CERT"));
        assert!(plan.warnings[0].contains("public"));
        let plan = fastmock_store::pg_connect_options(
            DB,
            &PgTls {
                mode: Some(fastmock_store::SslMode::Require),
                root_cert: Some(f.path("ca.pem").into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            plan.warnings[0].contains("does not verify"),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn migration_settings() {
        let d = db(&[
            ("DATABASE_URL", DB.into()),
            ("DATABASE_MIGRATE", "skip".into()),
            ("DATABASE_MIGRATION_TIMEOUT", "5m".into()),
        ])
        .unwrap();
        assert_eq!(d.migrate, MigrateMode::Skip);
        assert_eq!(d.migration_timeout, Duration::from_secs(300));
        assert!(
            err(db(&[
                ("DATABASE_URL", DB.into()),
                ("DATABASE_MIGRATE", "later".into())
            ]))
            .contains("auto or skip")
        );
        for bad in ["0s", "soon"] {
            assert!(
                err(db(&[
                    ("DATABASE_URL", DB.into()),
                    ("DATABASE_MIGRATION_TIMEOUT", bad.into())
                ]))
                .contains("FASTMOCK_DATABASE_MIGRATION_TIMEOUT")
            );
        }
    }

    #[test]
    fn valkey_tls_and_acl_settings() {
        let f = Files::new();
        let v = valkey(&[
            ("VALKEY_URL", "rediss://valkey.example.com:6380/".into()),
            ("VALKEY_USERNAME", "fastmock".into()),
            ("VALKEY_PASSWORD", "s3cret".into()),
            ("VALKEY_TLS_CA_FILE", f.path("ca.pem")),
            ("VALKEY_TLS_CERT_FILE", f.path("cert.pem")),
            ("VALKEY_TLS_KEY_FILE", f.path("key.pem")),
        ])
        .unwrap();
        assert_eq!(v.username.as_deref(), Some("fastmock"));
        assert_eq!(v.url, "rediss://:s3cret@valkey.example.com:6380/");
        assert!(
            valkey(&[("VALKEY_URL", "valkeys://valkey.example.com/".into())]).is_ok(),
            "TLS without extra settings uses the public roots"
        );

        let e = err(valkey(&[
            ("VALKEY_URL", "redis://valkey.example.com:6379/".into()),
            ("VALKEY_TLS_CA_FILE", f.path("ca.pem")),
        ]));
        assert!(e.contains("rediss://"), "{e}");
        let e = err(valkey(&[
            ("VALKEY_URL", "rediss://valkey.example.com/".into()),
            ("VALKEY_TLS_CERT_FILE", f.path("cert.pem")),
        ]));
        assert!(e.contains("FASTMOCK_VALKEY_TLS_KEY_FILE"), "{e}");
        let e = err(valkey(&[
            ("VALKEY_URL", "rediss://valkey.example.com/".into()),
            ("VALKEY_TLS_CA_FILE", f.path("missing.pem")),
        ]));
        assert!(e.contains("cannot read"), "{e}");
        let e = err(valkey(&[
            ("VALKEY_URL", "redis://valkey.example.com/".into()),
            ("VALKEY_USERNAME", "fastmock".into()),
        ]));
        assert!(e.contains("FASTMOCK_VALKEY_PASSWORD"), "{e}");
        let e = err(valkey(&[(
            "VALKEY_URL",
            "rediss://valkey.example.com/#insecure".into(),
        )]));
        assert!(e.contains("insecure"), "{e}");
        assert!(err(valkey(&[])).contains("FASTMOCK_VALKEY_URL is required"));
    }

    #[test]
    fn urls_are_redacted() {
        let r = redact_url("postgres://fm:hunter2@db.example.com/fm?sslmode=require&password=x9");
        assert!(!r.contains("hunter2"), "{r}");
        assert!(!r.contains("x9"), "{r}");
        assert!(r.contains("sslmode=require"), "{r}");
        assert!(r.contains("db.example.com"), "{r}");
        assert_eq!(
            redact_url("redis://valkey.example.com:6379/"),
            "redis://valkey.example.com:6379/"
        );
        assert_eq!(redact_url("::not a url::"), "<unparsable URL>");
        let kind = StorageKind::Postgres {
            database_url: "postgres://fm:hunter2@db.example.com/fm".into(),
            valkey_url: "rediss://:t0psecret@valkey.example.com/".into(),
            valkey_prefix: "fm:".into(),
            max_connections: 5,
            options: Box::default(),
        };
        let dbg = format!("{kind:?}");
        assert!(
            !dbg.contains("hunter2") && !dbg.contains("t0psecret"),
            "{dbg}"
        );
        assert!(dbg.contains("REDACTED"), "{dbg}");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_duration("2").unwrap(), Duration::from_secs(2));
        assert!(parse_duration("5y").is_err());
        assert!(parse_duration("abc").is_err());
    }
}
