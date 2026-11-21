//! Builds the mail module of a server: settings, storage, events, space routing.

use std::collections::BTreeSet;
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::Context;
use fastmock_core::mail::{LocalMailEvents, LoginFailures, MailEvents, MailStore, MemoryMailStore};
use fastmock_mail::MailService;
use fastmock_store::{PgMailStore, PgStore, ValkeyMailEvents, valkey_client};

use crate::access::AccessConfig;
use crate::config::{Config, StorageKind};

/// Events a slow subscriber may fall behind before it is told to re-read.
const EVENT_BUFFER: usize = 4096;

/// How often the mail storage gauges are refreshed.
const USAGE_EVERY: Duration = Duration::from_secs(60);

/// The `space` label of the global area in mail metrics.
const GLOBAL_LABEL: &str = "_global";

/// The mail storage of a server: messages, events and (with several replicas) shared failed-login
/// counters.
pub struct MailBackends {
    pub store: Arc<dyn MailStore>,
    pub events: Arc<dyn MailEvents>,
    /// `None`: counters in this process.
    pub login_failures: Option<Arc<dyn LoginFailures>>,
}

/// The mail store and events for the configured storage: memory and a local broadcast, or PostgreSQL
/// (`pg`, the server's pool), Valkey pub/sub and failed-login counters in Valkey.
pub async fn backends(cfg: &Config, pg: Option<&PgStore>) -> anyhow::Result<MailBackends> {
    let limits = cfg.mail.as_ref().map(|m| m.retention).unwrap_or_default();
    Ok(match &cfg.storage {
        StorageKind::Memory => MailBackends {
            store: Arc::new(MemoryMailStore::with_limits(limits)),
            events: Arc::new(LocalMailEvents::new(EVENT_BUFFER)),
            login_failures: None,
        },
        StorageKind::Postgres {
            valkey_url,
            valkey_prefix,
            options,
            ..
        } => {
            let pg = pg.context("mail storage needs the PostgreSQL connection")?;
            let store = Arc::new(
                PgMailStore::new(pg.pool().clone()).with_max_mailboxes(limits.max_mailboxes),
            );
            tokio::spawn(report_usage(Arc::downgrade(&store)));
            let client = valkey_client(
                valkey_url,
                options.valkey_username.as_deref(),
                &options.valkey_tls,
            )
            .context("Valkey settings")?;
            let events = ValkeyMailEvents::connect(client, valkey_prefix, EVENT_BUFFER)
                .await
                .context("subscribing to mail events in Valkey")?;
            MailBackends {
                store,
                login_failures: Some(Arc::new(events.login_failures(valkey_prefix))),
                events: Arc::new(events),
            }
        }
    })
}

/// Sets `fastmock_mail_messages` and `fastmock_mail_storage_bytes` per area every minute while the store
/// lives. Every replica reports the same cluster-wide values.
async fn report_usage(store: Weak<PgMailStore>) {
    let mut labels: BTreeSet<String> = BTreeSet::new();
    loop {
        let Some(s) = store.upgrade() else {
            return;
        };
        match s.usage().await {
            Ok(usage) => {
                let mut now = BTreeSet::new();
                for u in usage {
                    let label = u.space.unwrap_or_else(|| GLOBAL_LABEL.to_string());
                    metrics::gauge!("fastmock_mail_messages", "space" => label.clone())
                        .set(u.messages as f64);
                    metrics::gauge!("fastmock_mail_storage_bytes", "space" => label.clone())
                        .set(u.bytes as f64);
                    now.insert(label);
                }
                for gone in labels.difference(&now) {
                    metrics::gauge!("fastmock_mail_messages", "space" => gone.clone()).set(0.0);
                    metrics::gauge!("fastmock_mail_storage_bytes", "space" => gone.clone())
                        .set(0.0);
                }
                labels = now;
            }
            Err(e) => {
                metrics::counter!("fastmock_store_errors_total", "store" => "mail").increment(1);
                tracing::warn!(error = %e, "reading mail storage usage failed");
            }
        }
        drop(s);
        tokio::time::sleep(USAGE_EVERY).await;
    }
}

/// The mail module, or `None` when `FASTMOCK_MAIL_ENABLED` is off.
pub async fn build(
    cfg: &Config,
    access: &AccessConfig,
    pg: Option<&PgStore>,
) -> anyhow::Result<Option<Arc<MailService>>> {
    let Some(mail) = &cfg.mail else {
        return Ok(None);
    };
    let b = backends(cfg, pg).await?;
    let mut svc = MailService::new(
        Arc::new(mail.clone()),
        b.store,
        b.events,
        access.mail_domains(),
        access.spaces().map(|s| s.name.clone()).collect(),
    );
    if let Some(f) = b.login_failures {
        svc = svc.with_login_failures(f);
    }
    Ok(Some(Arc::new(svc)))
}
