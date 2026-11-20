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

/// `FASTMOCK_LOG_FORMAT`: `json` (the default) or `text`.
pub fn log_format_from_env() -> anyhow::Result<LogFormat> {
    match env("LOG_FORMAT").as_deref().unwrap_or("json") {
        "json" => Ok(LogFormat::Json),
        "text" => Ok(LogFormat::Text),
        other => bail!("FASTMOCK_LOG_FORMAT must be json or text, got {other}"),
    }
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
    pub access_file: Option<PathBuf>,
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
    pub kafka_security: fastmock_kafka::KafkaSecurity,
    pub kafka_schema_registry_options: fastmock_kafka::RegistryOptions,
    pub plugin_overrides:
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
    pub max_inflight_body_bytes: usize,
    pub journal_queue: usize,
    pub stub_secrets: Vec<String>,
    pub recording_max_stubs: i64,
    pub kafka_allowed_topics: Option<Vec<String>>,
    pub kafka_create_topics: Option<fastmock_kafka::TopicCreation>,
    pub shutdown_delay: Duration,
    pub header_read_timeout: Duration,
    pub contract_secrets: Vec<ContractSecret>,
    pub kafka_contract_topics: Vec<String>,
    pub ui_data_plane_origins: Vec<String>,
    pub journal_scan_timeout: Duration,
    pub journal_scan_max_work: u64,
    pub journal_scan_concurrency: usize,
    /// Longest lifetime of a managed token, in days (`FASTMOCK_TOKENS_MAX_DAYS`).
    pub tokens_max_days: u32,
    /// How long a replica trusts its cached view of a managed token; revocation takes effect within it.
    pub token_cache_ttl: Duration,
    /// OIDC sign-in and bearer JWTs (`FASTMOCK_OIDC_*`); off when `None`.
    pub oidc: Option<crate::oidc::OidcSettings>,
    /// The mail module (`FASTMOCK_MAIL_*`); `None` unless `FASTMOCK_MAIL_ENABLED=true`.
    pub mail: Option<fastmock_mail::MailConfig>,
}

pub const DEFAULT_TOKEN_DAYS: u32 = 90;
pub const TOKEN_CACHE_TTL: Duration = Duration::from_secs(10);

/// `FASTMOCK_TOKENS_MAX_DAYS`: 1 to 3650, default 365.
pub fn parse_tokens_max_days(raw: Option<&str>) -> anyhow::Result<u32> {
    let Some(v) = raw.map(str::trim) else {
        return Ok(365);
    };
    let n: u32 = v.parse().map_err(|_| {
        anyhow::anyhow!("FASTMOCK_TOKENS_MAX_DAYS must be a whole number of days, got `{v}`")
    })?;
    if !(1..=3650).contains(&n) {
        bail!("FASTMOCK_TOKENS_MAX_DAYS must be between 1 and 3650 days, got {n}");
    }
    Ok(n)
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

/// Like [`env`], except that `FASTMOCK_OIDC_API_AUDIENCE` keeps a blank value, so that a set but
/// empty audience stops the start instead of silently turning bearer JWTs off.
fn oidc_env(name: &str) -> Option<String> {
    if name == "OIDC_API_AUDIENCE" {
        std::env::var(format!("FASTMOCK_{name}")).ok()
    } else {
        env(name)
    }
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

fn kafka_topic_creation() -> anyhow::Result<Option<fastmock_kafka::TopicCreation>> {
    let partitions: i32 = parse("KAFKA_TOPIC_PARTITIONS", 1)?;
    let replication: i16 = parse("KAFKA_TOPIC_REPLICATION", -1)?;
    if !bool_env("KAFKA_CREATE_TOPICS", false)? {
        if env("KAFKA_TOPIC_PARTITIONS").is_some() || env("KAFKA_TOPIC_REPLICATION").is_some() {
            bail!(
                "FASTMOCK_KAFKA_TOPIC_PARTITIONS and FASTMOCK_KAFKA_TOPIC_REPLICATION need FASTMOCK_KAFKA_CREATE_TOPICS=true"
            );
        }
        return Ok(None);
    }
    if partitions < 1 {
        bail!("FASTMOCK_KAFKA_TOPIC_PARTITIONS must be at least 1");
    }
    if replication == 0 || replication < -1 {
        bail!("FASTMOCK_KAFKA_TOPIC_REPLICATION must be -1 (broker default) or at least 1");
    }
    Ok(Some(fastmock_kafka::TopicCreation {
        partitions,
        replication,
    }))
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

/// Reads the `FASTMOCK_KAFKA_*` security variables through `get` (which takes the name
/// without the `FASTMOCK_` prefix) and checks them, including the PEM files.
pub fn parse_kafka_security(
    get: &dyn Fn(&str) -> Option<String>,
    registry_url: Option<&str>,
) -> anyhow::Result<(
    fastmock_kafka::KafkaSecurity,
    fastmock_kafka::RegistryOptions,
)> {
    use fastmock_kafka::{
        BasicAuth, ClientCertFiles, KafkaSecurity, RegistryOptions, SaslSettings, SecurityProtocol,
        TlsFiles,
    };
    let protocol: SecurityProtocol = match get("KAFKA_SECURITY_PROTOCOL") {
        None => SecurityProtocol::Plaintext,
        Some(v) => v
            .parse()
            .map_err(|e| anyhow::anyhow!("FASTMOCK_KAFKA_SECURITY_PROTOCOL: {e}"))?,
    };
    let sasl_vars = [
        "KAFKA_SASL_MECHANISM",
        "KAFKA_SASL_USERNAME",
        "KAFKA_SASL_PASSWORD",
    ];
    let sasl = if protocol.uses_sasl() {
        let need = |name: &str| {
            get(name).with_context(|| {
                format!(
                    "FASTMOCK_{name} is required for FASTMOCK_KAFKA_SECURITY_PROTOCOL={protocol}"
                )
            })
        };
        let mechanism = need("KAFKA_SASL_MECHANISM")?
            .parse()
            .map_err(|e| anyhow::anyhow!("FASTMOCK_KAFKA_SASL_MECHANISM: {e}"))?;
        Some(SaslSettings {
            mechanism,
            username: need("KAFKA_SASL_USERNAME")?.trim().to_string(),
            password: need("KAFKA_SASL_PASSWORD")?,
        })
    } else {
        let set: Vec<String> = sasl_vars
            .iter()
            .filter(|n| get(n).is_some())
            .map(|n| format!("FASTMOCK_{n}"))
            .collect();
        if !set.is_empty() {
            bail!(
                "{} {} set but FASTMOCK_KAFKA_SECURITY_PROTOCOL is {protocol}; use sasl_plaintext or sasl_ssl",
                set.join(", "),
                if set.len() == 1 { "is" } else { "are" }
            );
        }
        None
    };
    let ca_file = get("KAFKA_TLS_CA_FILE").map(PathBuf::from);
    let client_cert = match (get("KAFKA_TLS_CERT_FILE"), get("KAFKA_TLS_KEY_FILE")) {
        (Some(c), Some(k)) => Some(ClientCertFiles {
            cert_file: c.into(),
            key_file: k.into(),
        }),
        (None, None) => None,
        _ => bail!(
            "set both FASTMOCK_KAFKA_TLS_CERT_FILE and FASTMOCK_KAFKA_TLS_KEY_FILE, or neither"
        ),
    };
    let registry_https = registry_url.is_some_and(|u| u.trim().starts_with("https://"));
    if !protocol.uses_tls() {
        if client_cert.is_some() {
            bail!(
                "FASTMOCK_KAFKA_TLS_CERT_FILE and FASTMOCK_KAFKA_TLS_KEY_FILE are set but FASTMOCK_KAFKA_SECURITY_PROTOCOL is {protocol}; use ssl or sasl_ssl"
            );
        }
        if ca_file.is_some() && !registry_https {
            bail!(
                "FASTMOCK_KAFKA_TLS_CA_FILE is set but FASTMOCK_KAFKA_SECURITY_PROTOCOL is {protocol} and FASTMOCK_KAFKA_SCHEMA_REGISTRY_URL is not https://; use ssl or sasl_ssl"
            );
        }
    }
    if let Some(ca) = &ca_file {
        fastmock_kafka::read_ca_bundle(ca).context("FASTMOCK_KAFKA_TLS_CA_FILE")?;
    }
    if let Some(files) = &client_cert {
        fastmock_kafka::read_client_cert(files)
            .context("FASTMOCK_KAFKA_TLS_CERT_FILE / FASTMOCK_KAFKA_TLS_KEY_FILE")?;
    }
    let security = KafkaSecurity {
        protocol,
        sasl,
        tls: if protocol.uses_tls() {
            TlsFiles {
                ca_file: ca_file.clone(),
                client_cert,
            }
        } else {
            TlsFiles::default()
        },
    };
    security
        .validate()
        .context("invalid Kafka security settings")?;
    let auth = match (
        get("KAFKA_SCHEMA_REGISTRY_USERNAME"),
        get("KAFKA_SCHEMA_REGISTRY_PASSWORD"),
    ) {
        (Some(u), Some(p)) => Some(BasicAuth {
            username: u.trim().to_string(),
            password: p,
        }),
        (None, None) => None,
        _ => bail!(
            "set both FASTMOCK_KAFKA_SCHEMA_REGISTRY_USERNAME and FASTMOCK_KAFKA_SCHEMA_REGISTRY_PASSWORD, or neither"
        ),
    };
    if auth.is_some() && registry_url.is_none() {
        bail!(
            "FASTMOCK_KAFKA_SCHEMA_REGISTRY_USERNAME and FASTMOCK_KAFKA_SCHEMA_REGISTRY_PASSWORD are set but FASTMOCK_KAFKA_SCHEMA_REGISTRY_URL is not"
        );
    }
    Ok((
        security,
        RegistryOptions {
            ca_file: ca_file.filter(|_| registry_https),
            auth,
        },
    ))
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
            access_file: None,
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
            kafka_security: Default::default(),
            kafka_schema_registry_options: Default::default(),
            plugin_overrides: Default::default(),
            max_inflight_body_bytes: 64 * 1024 * 1024,
            journal_queue: 1024,
            stub_secrets: vec![],
            recording_max_stubs: 1000,
            kafka_allowed_topics: None,
            kafka_create_topics: None,
            shutdown_delay: Duration::ZERO,
            header_read_timeout: Duration::from_secs(30),
            contract_secrets: vec![],
            kafka_contract_topics: vec![],
            ui_data_plane_origins: vec![],
            journal_scan_timeout: Duration::from_secs(10),
            journal_scan_max_work: DEFAULT_JOURNAL_SCAN_MAX_WORK,
            journal_scan_concurrency: 2,
            tokens_max_days: 365,
            token_cache_ttl: TOKEN_CACHE_TTL,
            oidc: None,
            mail: None,
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
        let kafka_schema_registry = env("KAFKA_SCHEMA_REGISTRY_URL");
        let (kafka_security, kafka_schema_registry_options) =
            parse_kafka_security(&env, kafka_schema_registry.as_deref())?;
        let cfg = Self {
            listen: parse("LISTEN", "0.0.0.0:8080".parse()?)?,
            admin_listen: parse("ADMIN_LISTEN", "0.0.0.0:9090".parse()?)?,
            storage,
            auth_disabled: bool_env("AUTH_DISABLED", false)?,
            admin_tokens: env("ADMIN_TOKENS").map(|v| list(&v)).unwrap_or_default(),
            admin_tokens_file: env("ADMIN_TOKENS_FILE").map(PathBuf::from),
            access_file: env("ACCESS_FILE").map(PathBuf::from),
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
            log_format: log_format_from_env()?,
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
            kafka_schema_registry,
            kafka_security,
            kafka_schema_registry_options,
            plugin_overrides: Default::default(),
            max_inflight_body_bytes: parse("MAX_INFLIGHT_BODY_BYTES", 256 * 1024 * 1024usize)?,
            journal_queue: parse("JOURNAL_QUEUE", 2048usize)?.max(1),
            stub_secrets: env("STUB_SECRETS").map(|v| list(&v)).unwrap_or_default(),
            recording_max_stubs: parse("RECORDING_MAX_STUBS", 1000i64)?.max(1),
            kafka_allowed_topics: env("KAFKA_ALLOWED_TOPICS").map(|v| list(&v)),
            kafka_create_topics: kafka_topic_creation()?,
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
            tokens_max_days: parse_tokens_max_days(env("TOKENS_MAX_DAYS").as_deref())?,
            token_cache_ttl: TOKEN_CACHE_TTL,
            oidc: crate::oidc::settings::from_env(&oidc_env)?,
            mail: None,
        };
        if cfg.max_inflight_body_bytes < cfg.max_body_bytes {
            bail!("FASTMOCK_MAX_INFLIGHT_BODY_BYTES must be at least FASTMOCK_MAX_BODY_BYTES");
        }
        if cfg.listen == cfg.admin_listen {
            bail!(
                "FASTMOCK_LISTEN and FASTMOCK_ADMIN_LISTEN must differ: the control plane is never served on the mock port"
            );
        }
        let mail = mail_settings(
            &|n| std::env::var(format!("FASTMOCK_{n}")).ok(),
            cfg.listen,
            cfg.admin_listen,
            cfg.tls.as_ref(),
        )?;
        Ok(Config { mail, ..cfg })
    }
}

/// Reads the `FASTMOCK_MAIL_*` settings through `get` (the name without `FASTMOCK_`, the raw value, so an
/// empty listener variable turns that listener off) and checks them: limits, listeners against each other
/// and against the HTTP listeners `listen` and `admin_listen`, and the TLS files. `None` unless
/// `MAIL_ENABLED` is true.
pub fn mail_settings(
    get: &dyn Fn(&str) -> Option<String>,
    listen: SocketAddr,
    admin_listen: SocketAddr,
    server_tls: Option<&TlsSettings>,
) -> anyhow::Result<Option<fastmock_mail::MailConfig>> {
    use fastmock_mail::config::{MailTlsFiles, addr_conflict, parse_smtp_users};
    let val = |n: &str| get(n).filter(|v| !v.trim().is_empty());
    let flag = |n: &str, default: bool| -> anyhow::Result<bool> {
        match val(n)
            .as_deref()
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref()
        {
            None => Ok(default),
            Some("1" | "true" | "yes" | "on") => Ok(true),
            Some("0" | "false" | "no" | "off") => Ok(false),
            Some(other) => bail!("FASTMOCK_{n}: not a boolean: {other}"),
        }
    };
    if !flag("MAIL_ENABLED", false)? {
        return Ok(None);
    }
    let d = fastmock_mail::MailConfig::default();
    let listener = |n: &str, default: Option<SocketAddr>| -> anyhow::Result<Option<SocketAddr>> {
        match get(n) {
            None => Ok(default),
            Some(v) if v.trim().is_empty() => Ok(None),
            Some(v) => v
                .trim()
                .parse()
                .map(Some)
                .map_err(|e| anyhow::anyhow!("FASTMOCK_{n}: {e} (`{}`)", v.trim())),
        }
    };
    fn number<T: std::str::FromStr>(
        val: &dyn Fn(&str) -> Option<String>,
        n: &str,
        default: T,
    ) -> anyhow::Result<T>
    where
        T::Err: std::fmt::Display,
    {
        match val(n) {
            None => Ok(default),
            Some(v) => v
                .trim()
                .parse()
                .map_err(|e| anyhow::anyhow!("FASTMOCK_{n}: {e} (`{}`)", v.trim())),
        }
    }
    let duration = |n: &str, default: Duration| -> anyhow::Result<Duration> {
        match val(n) {
            None => Ok(default),
            Some(v) => parse_duration(&v).with_context(|| format!("FASTMOCK_{n}")),
        }
    };
    let tls = match (val("MAIL_TLS_CERT_FILE"), val("MAIL_TLS_KEY_FILE")) {
        (Some(c), Some(k)) => Some(MailTlsFiles {
            cert_file: c.trim().into(),
            key_file: k.trim().into(),
            from_server: false,
        }),
        (None, None) => server_tls.map(|t| MailTlsFiles {
            cert_file: t.cert_file.clone(),
            key_file: t.key_file.clone(),
            from_server: true,
        }),
        _ => {
            bail!("set both FASTMOCK_MAIL_TLS_CERT_FILE and FASTMOCK_MAIL_TLS_KEY_FILE, or neither")
        }
    };
    let cfg = fastmock_mail::MailConfig {
        listeners: fastmock_mail::config::MailListeners {
            smtp: listener("MAIL_SMTP_LISTEN", d.listeners.smtp)?,
            smtps: listener("MAIL_SMTPS_LISTEN", None)?,
            imap: listener("MAIL_IMAP_LISTEN", None)?,
            imaps: listener("MAIL_IMAPS_LISTEN", None)?,
            pop3: listener("MAIL_POP3_LISTEN", None)?,
            pop3s: listener("MAIL_POP3S_LISTEN", None)?,
        },
        tls,
        hostname: val("MAIL_HOSTNAME")
            .map(|h| h.trim().to_string())
            .unwrap_or(d.hostname),
        allow_plaintext_auth: flag("MAIL_ALLOW_PLAINTEXT_AUTH", false)?,
        smtp_require_auth: flag("MAIL_SMTP_REQUIRE_AUTH", false)?,
        smtp_users: parse_smtp_users(&val("MAIL_SMTP_USERS").unwrap_or_default())
            .map_err(anyhow::Error::msg)?,
        max_message_bytes: number(&val, "MAIL_MAX_MESSAGE_BYTES", d.max_message_bytes)?,
        max_recipients: number(&val, "MAIL_MAX_RECIPIENTS", d.max_recipients)?,
        max_line_bytes: number(&val, "MAIL_MAX_LINE_BYTES", d.max_line_bytes)?,
        idle_timeout: duration("MAIL_IDLE_TIMEOUT", d.idle_timeout)?,
        data_timeout: duration("MAIL_DATA_TIMEOUT", d.data_timeout)?,
        session_timeout: duration("MAIL_SESSION_TIMEOUT", d.session_timeout)?,
        max_connections: number(&val, "MAIL_MAX_CONNECTIONS", d.max_connections)?,
        max_connections_per_ip: number(
            &val,
            "MAIL_MAX_CONNECTIONS_PER_IP",
            d.max_connections_per_ip,
        )?,
        max_auth_failures: number(&val, "MAIL_MAX_AUTH_FAILURES", d.max_auth_failures)?,
        imap_idle_timeout: duration("MAIL_IMAP_IDLE_TIMEOUT", d.imap_idle_timeout)?,
        retention: fastmock_core::mail::MailRetention {
            max_messages: number(&val, "MAIL_MAX_MESSAGES", d.retention.max_messages)?,
            max_age: duration("MAIL_MAX_AGE", d.retention.max_age)?,
        },
    };
    cfg.validate().map_err(anyhow::Error::msg)?;
    for (name, addr, _) in cfg.listeners.enabled() {
        for (other, at) in [
            ("FASTMOCK_LISTEN", listen),
            ("FASTMOCK_ADMIN_LISTEN", admin_listen),
        ] {
            if addr_conflict(addr, at) {
                bail!(
                    "{name} ({addr}) and {other} ({at}) use the same port; mail listeners need ports of their own"
                );
            }
        }
    }
    if let Some(t) = &cfg.tls {
        let what = if t.from_server {
            "FASTMOCK_TLS_CERT_FILE / FASTMOCK_TLS_KEY_FILE (used for mail)"
        } else {
            "FASTMOCK_MAIL_TLS_CERT_FILE / FASTMOCK_MAIL_TLS_KEY_FILE"
        };
        fastmock_mail::tls::server_config(t).with_context(|| what.to_string())?;
    }
    Ok(Some(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_max_days_is_validated() {
        assert_eq!(parse_tokens_max_days(None).unwrap(), 365);
        assert_eq!(parse_tokens_max_days(Some(" 30 ")).unwrap(), 30);
        assert_eq!(parse_tokens_max_days(Some("3650")).unwrap(), 3650);
        for (v, want) in [
            ("0", "between 1 and 3650"),
            ("3651", "between 1 and 3650"),
            ("-1", "whole number"),
            ("90d", "whole number"),
        ] {
            let e = parse_tokens_max_days(Some(v)).unwrap_err().to_string();
            assert!(
                e.contains("FASTMOCK_TOKENS_MAX_DAYS") && e.contains(want),
                "{v}: {e}"
            );
        }
    }

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

    fn kafka_env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name: &str| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    fn kafka_pems() -> (PathBuf, String, String, String) {
        let dir = std::env::temp_dir().join(format!("fm-kafka-cfg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca = rcgen::CertificateParams::new(vec![]).unwrap();
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_pem = ca.self_signed(&ca_key).unwrap().pem();
        let issuer = rcgen::Issuer::new(ca, ca_key);
        let ck = rcgen::KeyPair::generate().unwrap();
        let client = rcgen::CertificateParams::new(vec!["client".into()])
            .unwrap()
            .signed_by(&ck, &issuer)
            .unwrap();
        std::fs::write(dir.join("ca.pem"), ca_pem).unwrap();
        std::fs::write(dir.join("client.pem"), client.pem()).unwrap();
        std::fs::write(dir.join("client.key"), ck.serialize_pem()).unwrap();
        std::fs::write(dir.join("empty.pem"), "\n").unwrap();
        let p = |n: &str| dir.join(n).display().to_string();
        let (ca, cert, key) = (p("ca.pem"), p("client.pem"), p("client.key"));
        (dir, ca, cert, key)
    }

    fn kafka_err(vars: &[(&str, &str)], registry: Option<&str>) -> String {
        let e = parse_kafka_security(&kafka_env(vars), registry).unwrap_err();
        let msg = format!("{e:#} {e:?}");
        assert!(!msg.contains("pa55w0rd-value"), "password leaked: {msg}");
        msg
    }

    #[test]
    fn kafka_security_defaults_to_plaintext() {
        let (s, r) = parse_kafka_security(&kafka_env(&[]), None).unwrap();
        assert_eq!(s, fastmock_kafka::KafkaSecurity::default());
        assert_eq!(r, fastmock_kafka::RegistryOptions::default());
        let (s, _) = parse_kafka_security(
            &kafka_env(&[("KAFKA_SECURITY_PROTOCOL", "PLAINTEXT")]),
            None,
        )
        .unwrap();
        assert_eq!(s.protocol, fastmock_kafka::SecurityProtocol::Plaintext);
    }

    #[test]
    fn kafka_sasl_ssl_with_mtls_and_registry_auth() {
        let (dir, ca, cert, key) = kafka_pems();
        let vars = [
            ("KAFKA_SECURITY_PROTOCOL", "SASL_SSL"),
            ("KAFKA_SASL_MECHANISM", "scram-sha-512"),
            ("KAFKA_SASL_USERNAME", "fastmock"),
            ("KAFKA_SASL_PASSWORD", "pa55w0rd-value"),
            ("KAFKA_TLS_CA_FILE", ca.as_str()),
            ("KAFKA_TLS_CERT_FILE", cert.as_str()),
            ("KAFKA_TLS_KEY_FILE", key.as_str()),
            ("KAFKA_SCHEMA_REGISTRY_USERNAME", "sr"),
            ("KAFKA_SCHEMA_REGISTRY_PASSWORD", "pa55w0rd-value"),
        ];
        let (s, r) =
            parse_kafka_security(&kafka_env(&vars), Some("https://registry.example.com")).unwrap();
        assert_eq!(s.protocol, fastmock_kafka::SecurityProtocol::SaslSsl);
        let sasl = s.sasl.as_ref().unwrap();
        assert_eq!(sasl.mechanism, fastmock_kafka::SaslMechanism::ScramSha512);
        assert_eq!(sasl.username, "fastmock");
        assert_eq!(s.tls.ca_file.as_deref(), Some(std::path::Path::new(&ca)));
        assert!(s.tls.client_cert.is_some());
        assert_eq!(r.ca_file, s.tls.ca_file);
        assert_eq!(r.auth.as_ref().unwrap().username, "sr");
        let mut cfg = Config::for_tests(StorageKind::Memory, vec![]);
        cfg.kafka_security = s;
        cfg.kafka_schema_registry_options = r;
        let d = format!("{cfg:?}");
        assert!(!d.contains("pa55w0rd-value"), "{d}");
        assert!(d.contains("<redacted>"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn kafka_security_errors_are_clear() {
        let (dir, ca, cert, key) = kafka_pems();
        let empty = dir.join("empty.pem").display().to_string();
        let missing = dir.join("missing.pem").display().to_string();
        let pw = ("KAFKA_SASL_PASSWORD", "pa55w0rd-value");
        let user = ("KAFKA_SASL_USERNAME", "fastmock");
        let mech = ("KAFKA_SASL_MECHANISM", "PLAIN");
        let sasl_pt = ("KAFKA_SECURITY_PROTOCOL", "sasl_plaintext");

        let e = kafka_err(&[("KAFKA_SECURITY_PROTOCOL", "tls")], None);
        assert!(e.contains("FASTMOCK_KAFKA_SECURITY_PROTOCOL"), "{e}");
        let e = kafka_err(
            &[sasl_pt, ("KAFKA_SASL_MECHANISM", "GSSAPI"), user, pw],
            None,
        );
        assert!(e.contains("FASTMOCK_KAFKA_SASL_MECHANISM"), "{e}");
        let e = kafka_err(&[sasl_pt, user, pw], None);
        assert!(
            e.contains("FASTMOCK_KAFKA_SASL_MECHANISM is required"),
            "{e}"
        );
        let e = kafka_err(&[sasl_pt, mech, pw], None);
        assert!(
            e.contains("FASTMOCK_KAFKA_SASL_USERNAME is required"),
            "{e}"
        );
        let e = kafka_err(&[sasl_pt, mech, user], None);
        assert!(
            e.contains("FASTMOCK_KAFKA_SASL_PASSWORD is required"),
            "{e}"
        );
        let e = kafka_err(&[user, pw], None);
        assert!(
            e.contains("FASTMOCK_KAFKA_SASL_USERNAME, FASTMOCK_KAFKA_SASL_PASSWORD are set")
                && e.contains("plaintext"),
            "{e}"
        );
        let e = kafka_err(&[("KAFKA_SECURITY_PROTOCOL", "ssl"), pw], None);
        assert!(e.contains("is ssl"), "{e}");
        let e = kafka_err(&[("KAFKA_TLS_CA_FILE", ca.as_str())], None);
        assert!(e.contains("FASTMOCK_KAFKA_TLS_CA_FILE is set"), "{e}");
        let e = kafka_err(
            &[sasl_pt, mech, user, pw, ("KAFKA_TLS_CA_FILE", ca.as_str())],
            Some("http://registry.example.com"),
        );
        assert!(e.contains("FASTMOCK_KAFKA_TLS_CA_FILE is set"), "{e}");
        let e = kafka_err(
            &[
                ("KAFKA_TLS_CERT_FILE", cert.as_str()),
                ("KAFKA_TLS_KEY_FILE", key.as_str()),
            ],
            None,
        );
        assert!(e.contains("use ssl or sasl_ssl"), "{e}");
        let ssl = ("KAFKA_SECURITY_PROTOCOL", "ssl");
        let e = kafka_err(&[ssl, ("KAFKA_TLS_CERT_FILE", cert.as_str())], None);
        assert!(e.contains("set both FASTMOCK_KAFKA_TLS_CERT_FILE"), "{e}");
        let e = kafka_err(&[ssl, ("KAFKA_TLS_CA_FILE", empty.as_str())], None);
        assert!(
            e.contains("FASTMOCK_KAFKA_TLS_CA_FILE") && e.contains("no PEM certificates"),
            "{e}"
        );
        let e = kafka_err(&[ssl, ("KAFKA_TLS_CA_FILE", missing.as_str())], None);
        assert!(e.contains("missing.pem"), "{e}");
        let e = kafka_err(
            &[
                ssl,
                ("KAFKA_TLS_CERT_FILE", cert.as_str()),
                ("KAFKA_TLS_KEY_FILE", empty.as_str()),
            ],
            None,
        );
        assert!(e.contains("FASTMOCK_KAFKA_TLS_KEY_FILE"), "{e}");
        let e = kafka_err(
            &[("KAFKA_SCHEMA_REGISTRY_PASSWORD", "pa55w0rd-value")],
            Some("https://registry.example.com"),
        );
        assert!(
            e.contains("set both FASTMOCK_KAFKA_SCHEMA_REGISTRY_USERNAME"),
            "{e}"
        );
        let e = kafka_err(
            &[
                ("KAFKA_SCHEMA_REGISTRY_USERNAME", "sr"),
                ("KAFKA_SCHEMA_REGISTRY_PASSWORD", "pa55w0rd-value"),
            ],
            None,
        );
        assert!(
            e.contains("FASTMOCK_KAFKA_SCHEMA_REGISTRY_URL is not"),
            "{e}"
        );

        let (s, r) = parse_kafka_security(
            &kafka_env(&[("KAFKA_TLS_CA_FILE", ca.as_str())]),
            Some("https://registry.example.com"),
        )
        .unwrap();
        assert_eq!(s, fastmock_kafka::KafkaSecurity::default());
        assert!(r.ca_file.is_some(), "the CA is used by the registry only");
        let (s, _) = parse_kafka_security(&kafka_env(&[sasl_pt, mech, user, pw]), None).unwrap();
        assert_eq!(
            s.sasl.unwrap().mechanism,
            fastmock_kafka::SaslMechanism::Plain
        );
        let _ = std::fs::remove_dir_all(dir);
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

    fn mail(
        vars: &[(&str, &str)],
        tls: Option<&TlsSettings>,
    ) -> anyhow::Result<Option<fastmock_mail::MailConfig>> {
        let get = |n: &str| {
            vars.iter()
                .find(|(k, _)| *k == n)
                .map(|(_, v)| v.to_string())
        };
        mail_settings(
            &get,
            "0.0.0.0:8080".parse().unwrap(),
            "127.0.0.1:9090".parse().unwrap(),
            tls,
        )
    }

    fn mail_err(vars: &[(&str, &str)]) -> String {
        format!("{:#}", mail(vars, None).unwrap_err())
    }

    #[test]
    fn mail_is_off_by_default_and_has_design_defaults() {
        assert!(mail(&[], None).unwrap().is_none());
        assert!(
            mail(
                &[("MAIL_ENABLED", "false"), ("MAIL_SMTP_LISTEN", "x")],
                None
            )
            .unwrap()
            .is_none()
        );
        let c = mail(&[("MAIL_ENABLED", "true")], None).unwrap().unwrap();
        assert_eq!(c, fastmock_mail::MailConfig::default());
        let c = mail(
            &[
                ("MAIL_ENABLED", "yes"),
                ("MAIL_SMTP_LISTEN", ""),
                ("MAIL_IMAP_LISTEN", "127.0.0.1:1143"),
                ("MAIL_POP3_LISTEN", " 127.0.0.1:1110 "),
                ("MAIL_MAX_MESSAGE_BYTES", "2048"),
                ("MAIL_MAX_RECIPIENTS", "5"),
                ("MAIL_IDLE_TIMEOUT", "5s"),
                ("MAIL_SESSION_TIMEOUT", "2m"),
                ("MAIL_MAX_MESSAGES", "10"),
                ("MAIL_MAX_AGE", "1d"),
                ("MAIL_HOSTNAME", "mail.example.test"),
                ("MAIL_ALLOW_PLAINTEXT_AUTH", "true"),
                ("MAIL_SMTP_REQUIRE_AUTH", "on"),
                ("MAIL_SMTP_USERS", &format!("app:{}", "0".repeat(64))),
            ],
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(c.listeners.smtp, None);
        assert_eq!(c.listeners.imap, Some("127.0.0.1:1143".parse().unwrap()));
        assert_eq!(c.listeners.pop3, Some("127.0.0.1:1110".parse().unwrap()));
        assert_eq!((c.max_message_bytes, c.max_recipients), (2048, 5));
        assert_eq!(c.idle_timeout, Duration::from_secs(5));
        assert_eq!(c.session_timeout, Duration::from_secs(120));
        assert_eq!(c.retention.max_messages, 10);
        assert_eq!(c.retention.max_age, Duration::from_secs(86_400));
        assert_eq!(c.hostname, "mail.example.test");
        assert!(c.allow_plaintext_auth && c.smtp_require_auth);
        assert_eq!(c.smtp_users[0].name, "app");
        assert_eq!(
            c.features(),
            vec!["mail", "mail.imap", "mail.pop3", "mail.providers"]
        );
    }

    #[test]
    fn mail_settings_are_validated() {
        let on = ("MAIL_ENABLED", "true");
        for (vars, want) in [
            (vec![("MAIL_ENABLED", "maybe")], "not a boolean"),
            (
                vec![on, ("MAIL_SMTP_LISTEN", "0.0.0.0:8080")],
                "FASTMOCK_MAIL_SMTP_LISTEN (0.0.0.0:8080) and FASTMOCK_LISTEN (0.0.0.0:8080) use the same port",
            ),
            (
                vec![on, ("MAIL_SMTP_LISTEN", "127.0.0.1:9090")],
                "and FASTMOCK_ADMIN_LISTEN (127.0.0.1:9090)",
            ),
            (
                vec![on, ("MAIL_POP3_LISTEN", "127.0.0.1:2525")],
                "FASTMOCK_MAIL_SMTP_LISTEN (0.0.0.0:2525) and FASTMOCK_MAIL_POP3_LISTEN",
            ),
            (
                vec![on, ("MAIL_IMAP_LISTEN", "localhost")],
                "FASTMOCK_MAIL_IMAP_LISTEN",
            ),
            (
                vec![on, ("MAIL_MAX_RECIPIENTS", "0")],
                "FASTMOCK_MAIL_MAX_RECIPIENTS must be greater than zero",
            ),
            (
                vec![on, ("MAIL_MAX_MESSAGES", "-1")],
                "FASTMOCK_MAIL_MAX_MESSAGES",
            ),
            (
                vec![on, ("MAIL_MAX_AGE", "0s")],
                "FASTMOCK_MAIL_MAX_AGE must be greater than zero",
            ),
            (
                vec![on, ("MAIL_DATA_TIMEOUT", "soon")],
                "FASTMOCK_MAIL_DATA_TIMEOUT",
            ),
            (
                vec![on, ("MAIL_SMTPS_LISTEN", "0.0.0.0:4650")],
                "FASTMOCK_MAIL_SMTPS_LISTEN needs a certificate",
            ),
            (
                vec![on, ("MAIL_TLS_CERT_FILE", "/nonexistent/c.pem")],
                "set both FASTMOCK_MAIL_TLS_CERT_FILE and FASTMOCK_MAIL_TLS_KEY_FILE",
            ),
            (
                vec![on, ("MAIL_SMTP_USERS", "app:nothex")],
                "FASTMOCK_MAIL_SMTP_USERS",
            ),
        ] {
            let e = mail_err(&vars);
            assert!(e.contains(want), "{vars:?}: {e}");
        }
    }

    #[test]
    fn mail_tls_files_are_read_or_taken_from_the_server() {
        let (dir, _ca, cert, key) = kafka_pems();
        let e = mail_err(&[
            ("MAIL_ENABLED", "true"),
            ("MAIL_TLS_CERT_FILE", "/nonexistent/c.pem"),
            ("MAIL_TLS_KEY_FILE", &key),
        ]);
        assert!(
            e.contains("FASTMOCK_MAIL_TLS_CERT_FILE") && e.contains("/nonexistent/c.pem"),
            "{e}"
        );
        let c = mail(
            &[
                ("MAIL_ENABLED", "true"),
                ("MAIL_SMTPS_LISTEN", "127.0.0.1:4650"),
                ("MAIL_TLS_CERT_FILE", &cert),
                ("MAIL_TLS_KEY_FILE", &key),
            ],
            None,
        )
        .unwrap()
        .unwrap();
        let t = c.tls.unwrap();
        assert!(!t.from_server);
        assert_eq!(t.cert_file, PathBuf::from(&cert));
        let server = TlsSettings {
            cert_file: cert.clone().into(),
            key_file: key.clone().into(),
            client_ca_file: None,
            client_auth_required: false,
        };
        let c = mail(&[("MAIL_ENABLED", "true")], Some(&server))
            .unwrap()
            .unwrap();
        assert!(c.tls.unwrap().from_server);
        let broken = TlsSettings {
            key_file: dir.join("empty.pem"),
            ..server
        };
        let e = format!(
            "{:#}",
            mail(&[("MAIL_ENABLED", "true")], Some(&broken)).unwrap_err()
        );
        assert!(
            e.contains("FASTMOCK_TLS_CERT_FILE / FASTMOCK_TLS_KEY_FILE (used for mail)"),
            "{e}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
