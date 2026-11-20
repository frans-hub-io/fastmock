//! Builds the mail module of a server: settings, storage, events, space routing.

use std::sync::Arc;

use fastmock_core::mail::{LocalMailEvents, MailEvents, MailStore, MemoryMailStore};
use fastmock_mail::MailService;

use crate::access::AccessConfig;
use crate::config::{Config, StorageKind};

/// Events a slow subscriber may fall behind before it is told to re-read.
const EVENT_BUFFER: usize = 4096;

/// The mail store and events for the configured storage.
pub async fn backends(cfg: &Config) -> anyhow::Result<(Arc<dyn MailStore>, Arc<dyn MailEvents>)> {
    let max = cfg.mail.as_ref().map_or(1000, |m| m.retention.max_messages);
    Ok(match &cfg.storage {
        StorageKind::Memory | StorageKind::Postgres { .. } => (
            Arc::new(MemoryMailStore::new(max)),
            Arc::new(LocalMailEvents::new(EVENT_BUFFER)),
        ),
    })
}

/// The mail module, or `None` when `FASTMOCK_MAIL_ENABLED` is off.
pub async fn build(
    cfg: &Config,
    access: &AccessConfig,
) -> anyhow::Result<Option<Arc<MailService>>> {
    let Some(mail) = &cfg.mail else {
        return Ok(None);
    };
    let (store, events) = backends(cfg).await?;
    Ok(Some(Arc::new(MailService::new(
        Arc::new(mail.clone()),
        store,
        events,
        access.mail_domains(),
        access.spaces().map(|s| s.name.clone()).collect(),
    ))))
}
