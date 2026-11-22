//! The `FASTMOCK_MQTT_*`, `FASTMOCK_AMQP_*` and `FASTMOCK_CAPTURE_*` settings. Every function reads through
//! `get`, which takes the name without `FASTMOCK_` and returns `None` for unset or blank variables.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use fastmock_brokers::amqp::{AmqpSasl, AmqpSettings};
use fastmock_brokers::mqtt::{MqttSettings, MqttVersion};
use fastmock_brokers::{ClientCertFiles, Credentials, TlsFiles};
use fastmock_core::capture::CaptureSource;
use sha2::Digest;
use uuid::Uuid;

pub type Get<'a> = &'a dyn Fn(&str) -> Option<String>;

fn list(v: &str) -> Vec<String> {
    v.split([',', '\n'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn flag(get: Get, name: &str, default: bool) -> anyhow::Result<bool> {
    match get(name).map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        None => Ok(default),
        Some("1" | "true" | "yes" | "on") => Ok(true),
        Some("0" | "false" | "no" | "off") => Ok(false),
        Some(other) => bail!("FASTMOCK_{name}: not a boolean: {other}"),
    }
}

fn number<T: std::str::FromStr>(get: Get, name: &str, default: T) -> anyhow::Result<T>
where
    T::Err: std::fmt::Display,
{
    match get(name) {
        None => Ok(default),
        Some(v) => v
            .trim()
            .parse()
            .map_err(|e| anyhow::anyhow!("FASTMOCK_{name}: {e}")),
    }
}

fn duration(get: Get, name: &str, default: &str) -> anyhow::Result<Duration> {
    crate::config::parse_duration(&get(name).unwrap_or_else(|| default.to_string()))
        .with_context(|| format!("FASTMOCK_{name}"))
}

/// User name and password, both or neither.
fn credentials(get: Get, prefix: &str) -> anyhow::Result<Option<Credentials>> {
    let u = format!("{prefix}_USERNAME");
    let p = format!("{prefix}_PASSWORD");
    match (get(&u), get(&p)) {
        (Some(username), Some(password)) => Ok(Some(Credentials {
            username: username.trim().to_string(),
            password,
        })),
        (None, None) => Ok(None),
        (Some(username), None) if prefix == "MQTT" => Ok(Some(Credentials {
            username: username.trim().to_string(),
            password: String::new(),
        })),
        _ => bail!("set both FASTMOCK_{u} and FASTMOCK_{p}, or neither"),
    }
}

fn tls_files(get: Get, prefix: &str) -> anyhow::Result<TlsFiles> {
    let ca = format!("{prefix}_TLS_CA_FILE");
    let cert = format!("{prefix}_TLS_CERT_FILE");
    let key = format!("{prefix}_TLS_KEY_FILE");
    let client_cert = match (get(&cert), get(&key)) {
        (Some(c), Some(k)) => Some(ClientCertFiles {
            cert_file: c.trim().into(),
            key_file: k.trim().into(),
        }),
        (None, None) => None,
        _ => bail!("set both FASTMOCK_{cert} and FASTMOCK_{key}, or neither"),
    };
    Ok(TlsFiles {
        ca_file: get(&ca).map(|v| PathBuf::from(v.trim())),
        client_cert,
    })
}

fn others_without_url(get: Get, prefix: &str, names: &[&str]) -> anyhow::Result<()> {
    let set: Vec<String> = names
        .iter()
        .map(|n| format!("{prefix}_{n}"))
        .filter(|n| get(n).is_some())
        .map(|n| format!("FASTMOCK_{n}"))
        .collect();
    if !set.is_empty() {
        bail!(
            "{} {} set but FASTMOCK_{prefix}_URL is not",
            set.join(", "),
            if set.len() == 1 { "is" } else { "are" }
        );
    }
    Ok(())
}

/// `FASTMOCK_MQTT_*`: `None` without `FASTMOCK_MQTT_URL`.
pub fn mqtt_settings(get: Get) -> anyhow::Result<Option<MqttSettings>> {
    let Some(url) = get("MQTT_URL") else {
        others_without_url(
            get,
            "MQTT",
            &[
                "VERSION",
                "USERNAME",
                "PASSWORD",
                "TLS_CA_FILE",
                "TLS_CERT_FILE",
                "TLS_KEY_FILE",
                "CLIENT_ID_PREFIX",
            ],
        )?;
        return Ok(None);
    };
    let mut s = MqttSettings::new(&url).context("FASTMOCK_MQTT_URL")?;
    if let Some(v) = get("MQTT_VERSION") {
        s.version = v.parse::<MqttVersion>().context("FASTMOCK_MQTT_VERSION")?;
    }
    if let Some(p) = get("MQTT_CLIENT_ID_PREFIX") {
        s.client_id_prefix = p.trim().to_string();
    }
    s.credentials = credentials(get, "MQTT")?;
    s.tls_files = tls_files(get, "MQTT")?;
    s.keep_alive = duration(get, "MQTT_KEEP_ALIVE", "30s")?;
    s.validate()
        .context("invalid MQTT settings (FASTMOCK_MQTT_*)")?;
    Ok(Some(s))
}

/// `FASTMOCK_AMQP_*`: `None` without `FASTMOCK_AMQP_URL`.
pub fn amqp_settings(get: Get) -> anyhow::Result<Option<AmqpSettings>> {
    let Some(url) = get("AMQP_URL") else {
        others_without_url(
            get,
            "AMQP",
            &[
                "USERNAME",
                "PASSWORD",
                "SASL_MECHANISM",
                "TLS_CA_FILE",
                "TLS_CERT_FILE",
                "TLS_KEY_FILE",
                "DECLARE_EXCHANGES",
            ],
        )?;
        return Ok(None);
    };
    let mut s = AmqpSettings::new(&url).context("FASTMOCK_AMQP_URL")?;
    if let Some(m) = get("AMQP_SASL_MECHANISM") {
        s.mechanism = m
            .parse::<AmqpSasl>()
            .context("FASTMOCK_AMQP_SASL_MECHANISM")?;
    }
    s.credentials = credentials(get, "AMQP")?;
    s.tls_files = tls_files(get, "AMQP")?;
    s.declare_exchanges = flag(get, "AMQP_DECLARE_EXCHANGES", false)?;
    s.validate()
        .context("invalid AMQP settings (FASTMOCK_AMQP_*)")?;
    Ok(Some(s))
}

/// Message capture (`FASTMOCK_CAPTURE_*`).
#[derive(Clone, Debug, PartialEq)]
pub struct CaptureConfig {
    pub enabled: bool,
    pub max_sources: usize,
    /// Messages kept per source.
    pub max_messages: usize,
    /// Body bytes kept per message; longer bodies are cut and marked `truncated`.
    pub max_message_bytes: usize,
    /// Bytes of stored messages in all (`FASTMOCK_CAPTURE_MAX_BYTES`); the oldest go first.
    pub max_bytes: u64,
    /// Bytes of stored messages per space, and of the global area (`FASTMOCK_CAPTURE_MAX_SPACE_BYTES`).
    pub max_space_bytes: u64,
    pub max_age: Duration,
    /// Allow-lists of what sources may read (`name` or `prefix*`, `*` for all). Without one, only sources
    /// from the configuration can read that protocol; sources created through the API need it.
    pub kafka_topics: Option<Vec<String>>,
    pub mqtt_topics: Option<Vec<String>>,
    pub amqp_exchanges: Option<Vec<String>>,
    /// Sources from `FASTMOCK_CAPTURE_SOURCES` and `FASTMOCK_CAPTURE_SOURCES_FILE`.
    pub sources: Vec<CaptureSource>,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_sources: 50,
            max_messages: 1000,
            max_message_bytes: 1024 * 1024,
            max_bytes: DEFAULT_MAX_BYTES,
            max_space_bytes: DEFAULT_MAX_BYTES / 4,
            max_age: Duration::from_secs(86_400),
            kafka_topics: None,
            mqtt_topics: None,
            amqp_exchanges: None,
            sources: vec![],
        }
    }
}

/// Default of `FASTMOCK_CAPTURE_MAX_BYTES`: 256 MiB.
pub const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// A stable id for a source defined in the configuration: derived from its space and name.
pub fn config_source_id(space: Option<&str>, name: &str) -> Uuid {
    let h = sha2::Sha256::digest(format!("capture-source\0{}\0{name}", space.unwrap_or("")));
    let mut b = [0u8; 16];
    b.copy_from_slice(&h[..16]);
    b[6] = (b[6] & 0x0f) | 0x80;
    b[8] = (b[8] & 0x3f) | 0x80;
    Uuid::from_bytes(b)
}

fn parse_sources(text: &str, what: &str) -> anyhow::Result<Vec<CaptureSource>> {
    let v: serde_json::Value = fastmock_core::openapi::parse_document(text)
        .map_err(|e| anyhow::anyhow!("{what}: neither JSON nor YAML: {e}"))?;
    let list = match v {
        serde_json::Value::Array(a) => a,
        serde_json::Value::Object(mut o) => match o.remove("sources") {
            Some(serde_json::Value::Array(a)) => a,
            _ => bail!("{what}: expected a list of sources or {{\"sources\": [...]}}"),
        },
        _ => bail!("{what}: expected a list of sources"),
    };
    let mut out = Vec::new();
    for (i, raw) in list.into_iter().enumerate() {
        let mut s: CaptureSource =
            serde_json::from_value(raw).with_context(|| format!("{what}: source {i}"))?;
        s.validate()
            .map_err(|e| anyhow::anyhow!("{what}: source {} ({i}): {e}", s.name))?;
        s.id = config_source_id(s.space.as_deref(), &s.name);
        s.from_config = true;
        out.push(s);
    }
    Ok(out)
}

pub fn capture_settings(get: Get) -> anyhow::Result<CaptureConfig> {
    let d = CaptureConfig::default();
    let enabled = flag(get, "CAPTURE_ENABLED", false)?;
    let mut sources = Vec::new();
    if let Some(inline) = get("CAPTURE_SOURCES") {
        sources.extend(parse_sources(&inline, "FASTMOCK_CAPTURE_SOURCES")?);
    }
    if let Some(path) = get("CAPTURE_SOURCES_FILE") {
        let text = std::fs::read_to_string(path.trim())
            .with_context(|| format!("FASTMOCK_CAPTURE_SOURCES_FILE: reading {}", path.trim()))?;
        sources.extend(parse_sources(&text, "FASTMOCK_CAPTURE_SOURCES_FILE")?);
    }
    let mut seen = std::collections::HashSet::new();
    for s in &sources {
        if !seen.insert((s.space.clone(), s.name.clone())) {
            bail!(
                "capture source {} is defined twice{}",
                s.name,
                s.space
                    .as_deref()
                    .map(|x| format!(" in space {x}"))
                    .unwrap_or_default()
            );
        }
    }
    let max_bytes: u64 = number(get, "CAPTURE_MAX_BYTES", d.max_bytes)?;
    let cfg = CaptureConfig {
        enabled,
        max_sources: number(get, "CAPTURE_MAX_SOURCES", d.max_sources)?,
        max_messages: number(get, "CAPTURE_MAX_MESSAGES", d.max_messages)?,
        max_message_bytes: number(get, "CAPTURE_MAX_MESSAGE_BYTES", d.max_message_bytes)?,
        max_bytes,
        max_space_bytes: number(get, "CAPTURE_MAX_SPACE_BYTES", max_bytes / 4)?,
        max_age: duration(get, "CAPTURE_MAX_AGE", "24h")?,
        kafka_topics: get("CAPTURE_KAFKA_TOPICS").map(|v| list(&v)),
        mqtt_topics: get("CAPTURE_MQTT_TOPICS").map(|v| list(&v)),
        amqp_exchanges: get("CAPTURE_AMQP_EXCHANGES").map(|v| list(&v)),
        sources,
    };
    if !(1..=1000).contains(&cfg.max_sources) {
        bail!("FASTMOCK_CAPTURE_MAX_SOURCES must be between 1 and 1000");
    }
    if !(1..=100_000).contains(&cfg.max_messages) {
        bail!("FASTMOCK_CAPTURE_MAX_MESSAGES must be between 1 and 100000");
    }
    if !(1..=16 * 1024 * 1024).contains(&cfg.max_message_bytes) {
        bail!("FASTMOCK_CAPTURE_MAX_MESSAGE_BYTES must be between 1 and 16777216");
    }
    if !(1024 * 1024..=1024 * 1024 * 1024 * 1024).contains(&cfg.max_bytes) {
        bail!(
            "FASTMOCK_CAPTURE_MAX_BYTES must be between 1048576 (1 MiB) and 1099511627776 (1 TiB)"
        );
    }
    if cfg.max_space_bytes < 64 * 1024 || cfg.max_space_bytes > cfg.max_bytes {
        bail!(
            "FASTMOCK_CAPTURE_MAX_SPACE_BYTES must be between 65536 and FASTMOCK_CAPTURE_MAX_BYTES ({})",
            cfg.max_bytes
        );
    }
    if cfg.max_age.is_zero() {
        bail!("FASTMOCK_CAPTURE_MAX_AGE must be positive");
    }
    if cfg.sources.len() > cfg.max_sources {
        bail!(
            "{} capture sources are configured but FASTMOCK_CAPTURE_MAX_SOURCES is {}",
            cfg.sources.len(),
            cfg.max_sources
        );
    }
    if !cfg.enabled && !cfg.sources.is_empty() {
        bail!("capture sources are configured but FASTMOCK_CAPTURE_ENABLED is not true");
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars<'a>(v: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |n| {
            v.iter()
                .find(|(k, _)| *k == n)
                .map(|(_, x)| x.to_string())
                .filter(|x| !x.trim().is_empty())
        }
    }

    #[test]
    fn mqtt_settings_are_parsed_and_checked() {
        assert!(mqtt_settings(&vars(&[])).unwrap().is_none());
        let s = mqtt_settings(&vars(&[
            ("MQTT_URL", "mqtt://broker.example.com"),
            ("MQTT_VERSION", "3.1.1"),
            ("MQTT_USERNAME", "fastmock"),
            ("MQTT_PASSWORD", "secret"),
            ("MQTT_CLIENT_ID_PREFIX", "mock"),
            ("MQTT_KEEP_ALIVE", "10s"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(s.port, 1883);
        assert_eq!(s.version, MqttVersion::V311);
        assert_eq!(s.client_id_prefix, "mock");
        assert_eq!(s.keep_alive, Duration::from_secs(10));
        assert!(!format!("{s:?}").contains("secret"));
        let e = mqtt_settings(&vars(&[("MQTT_USERNAME", "x")])).unwrap_err();
        assert!(e.to_string().contains("FASTMOCK_MQTT_URL is not"), "{e}");
        let e = mqtt_settings(&vars(&[
            ("MQTT_URL", "mqtt://h"),
            ("MQTT_TLS_CA_FILE", "/x.pem"),
        ]))
        .unwrap_err();
        assert!(format!("{e:#}").contains("mqtts://"), "{e:#}");
        let e =
            mqtt_settings(&vars(&[("MQTT_URL", "mqtt://h"), ("MQTT_VERSION", "6")])).unwrap_err();
        assert!(format!("{e:#}").contains("FASTMOCK_MQTT_VERSION"));
        let e = mqtt_settings(&vars(&[
            ("MQTT_URL", "mqtts://h"),
            ("MQTT_TLS_CERT_FILE", "/c.pem"),
        ]))
        .unwrap_err();
        assert!(e.to_string().contains("TLS_KEY_FILE"));
    }

    #[test]
    fn amqp_settings_are_parsed_and_checked() {
        assert!(amqp_settings(&vars(&[])).unwrap().is_none());
        let s = amqp_settings(&vars(&[
            ("AMQP_URL", "amqp://rabbit.example.com/orders"),
            ("AMQP_USERNAME", "fastmock"),
            ("AMQP_PASSWORD", "secret"),
            ("AMQP_DECLARE_EXCHANGES", "true"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(s.vhost, "orders");
        assert!(s.declare_exchanges);
        assert!(!format!("{s:?}").contains("secret"));
        let e = amqp_settings(&vars(&[("AMQP_URL", "amqp://u:p@h")])).unwrap_err();
        assert!(format!("{e:#}").contains("credentials"));
        let e =
            amqp_settings(&vars(&[("AMQP_URL", "amqp://h"), ("AMQP_USERNAME", "u")])).unwrap_err();
        assert!(e.to_string().contains("AMQP_PASSWORD"));
        let e = amqp_settings(&vars(&[
            ("AMQP_URL", "amqp://h"),
            ("AMQP_SASL_MECHANISM", "EXTERNAL"),
        ]))
        .unwrap_err();
        assert!(format!("{e:#}").contains("EXTERNAL"));
        let e = amqp_settings(&vars(&[("AMQP_DECLARE_EXCHANGES", "true")])).unwrap_err();
        assert!(e.to_string().contains("FASTMOCK_AMQP_URL is not"));
    }

    #[test]
    fn capture_settings_and_static_sources() {
        let c = capture_settings(&vars(&[])).unwrap();
        assert_eq!(c, CaptureConfig::default());
        let src = r#"[{"name": "orders", "protocol": "kafka", "topics": ["orders.events"]},
                     {"name": "devices", "space": "iot", "protocol": "mqtt", "topics": ["devices/+/state"]}]"#;
        let c = capture_settings(&vars(&[
            ("CAPTURE_ENABLED", "true"),
            ("CAPTURE_SOURCES", src),
            ("CAPTURE_MAX_MESSAGES", "50"),
            ("CAPTURE_MQTT_TOPICS", "devices/*"),
        ]))
        .unwrap();
        assert_eq!(c.sources.len(), 2);
        assert!(c.sources.iter().all(|s| s.from_config));
        assert_eq!(c.sources[0].id, config_source_id(None, "orders"));
        assert_ne!(c.sources[1].id, config_source_id(None, "devices"));
        assert_eq!(c.max_messages, 50);
        assert_eq!(c.mqtt_topics, Some(vec!["devices/*".to_string()]));
        let e = capture_settings(&vars(&[("CAPTURE_SOURCES", src)])).unwrap_err();
        assert!(e.to_string().contains("FASTMOCK_CAPTURE_ENABLED"));
        let e = capture_settings(&vars(&[
            ("CAPTURE_ENABLED", "true"),
            (
                "CAPTURE_SOURCES",
                "[{\"name\": \"x\", \"protocol\": \"kafka\"}]",
            ),
        ]))
        .unwrap_err();
        assert!(e.to_string().contains("needs topics"), "{e}");
        let e = capture_settings(&vars(&[
            ("CAPTURE_ENABLED", "true"),
            ("CAPTURE_SOURCES", "sources:\n  - {name: a, protocol: kafka, topics: [t]}\n  - {name: a, protocol: kafka, topics: [u]}"),
        ]))
        .unwrap_err();
        assert!(e.to_string().contains("twice"));
        assert!(capture_settings(&vars(&[("CAPTURE_MAX_MESSAGES", "0")])).is_err());
    }

    #[test]
    fn capture_storage_has_byte_budgets() {
        let c = capture_settings(&vars(&[])).unwrap();
        assert_eq!(c.max_bytes, 256 * 1024 * 1024);
        assert_eq!(c.max_space_bytes, 64 * 1024 * 1024);
        let c = capture_settings(&vars(&[("CAPTURE_MAX_BYTES", "8388608")])).unwrap();
        assert_eq!(c.max_space_bytes, 2 * 1024 * 1024);
        let c = capture_settings(&vars(&[
            ("CAPTURE_MAX_BYTES", "8388608"),
            ("CAPTURE_MAX_SPACE_BYTES", "8388608"),
        ]))
        .unwrap();
        assert_eq!(c.max_space_bytes, 8 * 1024 * 1024);
        assert!(capture_settings(&vars(&[("CAPTURE_MAX_BYTES", "1000")])).is_err());
        let e = capture_settings(&vars(&[
            ("CAPTURE_MAX_BYTES", "8388608"),
            ("CAPTURE_MAX_SPACE_BYTES", "9000000"),
        ]))
        .unwrap_err();
        assert!(e.to_string().contains("FASTMOCK_CAPTURE_MAX_SPACE_BYTES"));
    }
}
