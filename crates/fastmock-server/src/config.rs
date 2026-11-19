use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use ipnet::IpNet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StorageKind {
    Memory,
    Postgres {
        database_url: String,
        valkey_url: String,
        valkey_prefix: String,
        max_connections: u32,
    },
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
    pub kafka_security: fastmock_kafka::KafkaSecurity,
    pub kafka_schema_registry_options: fastmock_kafka::RegistryOptions,
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
            "postgres" => StorageKind::Postgres {
                database_url: with_password(
                    &env("DATABASE_URL")
                        .context("FASTMOCK_DATABASE_URL is required for postgres storage")?,
                    env("DATABASE_PASSWORD"),
                    "FASTMOCK_DATABASE_URL",
                )?,
                valkey_url: with_password(
                    &env("VALKEY_URL")
                        .context("FASTMOCK_VALKEY_URL is required for postgres storage")?,
                    env("VALKEY_PASSWORD"),
                    "FASTMOCK_VALKEY_URL",
                )?,
                valkey_prefix: env("VALKEY_PREFIX").unwrap_or_else(|| "fastmock:".into()),
                max_connections: parse("DB_MAX_CONNECTIONS", 10u32)?,
            },
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
            kafka_schema_registry,
            kafka_security,
            kafka_schema_registry_options,
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

    #[test]
    fn durations() {
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_duration("2").unwrap(), Duration::from_secs(2));
        assert!(parse_duration("5y").is_err());
        assert!(parse_duration("abc").is_err());
    }
}
