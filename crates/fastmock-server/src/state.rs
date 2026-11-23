use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use fastmock_core::{Engine, SecretProvider, Stores};
use fastmock_store::PgStore;
use serde::Serialize;

use crate::auth::Authenticator;
use crate::config::Config;
use crate::redact::Redactor;

#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct PluginInfo {
    pub name: String,
    pub description: String,
    pub mount: String,
    pub admin: bool,
}

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub stores: Stores,
    pub engine: Arc<Engine>,
    pub auth: Arc<Authenticator>,
    pub redactor: Arc<Redactor>,
    pub plugins: Arc<Vec<PluginInfo>>,
    pub pg: Option<PgStore>,
    pub started: Instant,
    /// Random id of this process, new on every start.
    pub instance_id: Arc<str>,
    pub ready: Arc<AtomicBool>,
    pub http: reqwest::Client,
    pub policy: Arc<crate::outbox::EgressPolicy>,
    pub chaos: Arc<crate::extras::ChaosCache>,
    pub kafka: Arc<crate::extras::KafkaHub>,
    /// Publishers of stub messages and async mocks: Kafka, MQTT and AMQP.
    pub brokers: Arc<crate::brokers::Brokers>,
    /// Message capture; `None` unless `FASTMOCK_CAPTURE_ENABLED=true`.
    pub capture: Option<Arc<crate::capture::CaptureService>>,
    pub grpc: Arc<crate::grpc::Registry>,
    /// Upstream TLS settings and the call budget of gRPC proxying.
    pub grpc_proxy: Arc<crate::grpc_proxy::GrpcProxy>,
    pub body_budget: Arc<tokio::sync::Semaphore>,
    pub journal_tx: tokio::sync::mpsc::Sender<fastmock_core::JournalEntry>,
    pub scans: Arc<tokio::sync::Semaphore>,
    pub tokens: Arc<crate::tokens::TokenService>,
    pub oidc: Option<Arc<crate::oidc::Oidc>>,
    /// The mail module; `None` unless `FASTMOCK_MAIL_ENABLED=true`.
    pub mail: Option<Arc<fastmock_mail::MailService>>,
    /// Mail chaos rules and counters; present with the mail module.
    pub mail_chaos: Option<Arc<fastmock_mail::chaos::MailChaos>>,
    /// Seals mail webhook secrets at rest; present with the mail module.
    pub mail_sealer: Option<Arc<crate::mail_webhook_secrets::Sealer>>,
}

pub struct StubSecrets {
    pub inner: Arc<dyn SecretProvider>,
    pub allowed: Vec<String>,
}

impl SecretProvider for StubSecrets {
    fn get(&self, name: &str) -> Option<String> {
        if self.allowed.iter().any(|a| a == name) {
            self.inner.get(name)
        } else {
            None
        }
    }
}

impl AppState {
    /// Neither an access file nor OIDC is configured: denials keep the bodies of earlier versions.
    pub fn legacy_setup(&self) -> bool {
        self.cfg.access_file.is_none() && self.oidc.is_none()
    }

    pub fn source_for(&self, path: &str) -> String {
        if self.mail.is_some()
            && (path == crate::mail_providers::PREFIX
                || path.starts_with(&format!("{}/", crate::mail_providers::PREFIX)))
        {
            return "mail".to_string();
        }
        for p in self.plugins.iter() {
            if path == p.mount || path.starts_with(&format!("{}/", p.mount)) {
                return p.name.clone();
            }
        }
        fastmock_core::engine::STUB_SOURCE.to_string()
    }
}

pub struct Secrets {
    dir: Option<PathBuf>,
}

impl Secrets {
    pub fn new(dir: Option<PathBuf>) -> Self {
        Self { dir }
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.')
}

impl SecretProvider for Secrets {
    fn get(&self, name: &str) -> Option<String> {
        if !valid_name(name) {
            return None;
        }
        let env_name = format!(
            "FASTMOCK_SECRET_{}",
            name.to_ascii_uppercase().replace(['-', '.'], "_")
        );
        if let Ok(v) = std::env::var(env_name) {
            return Some(v);
        }
        let dir = self.dir.as_ref()?;
        std::fs::read_to_string(dir.join(name))
            .ok()
            .map(|s| s.trim_end_matches(['\n', '\r']).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_from_dir_reject_traversal() {
        let dir = std::env::temp_dir().join(format!("fm-secrets-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hook-key"), "abc\n").unwrap();
        let s = Secrets::new(Some(dir.clone()));
        assert_eq!(s.get("hook-key").as_deref(), Some("abc"));
        assert_eq!(s.get("../hook-key"), None);
        assert_eq!(s.get(".hidden"), None);
        assert_eq!(s.get("missing"), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
