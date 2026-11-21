//! The capture pipeline shared by SMTP, the admin API (inject) and the provider APIs: parse once, route
//! to spaces, label, store with mailbox deliveries and publish `message.received`.

use std::collections::BTreeSet;
use std::sync::Arc;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use fastmock_core::mail::{
    Envelope, LoginFailures, MailEntry, MailEvent, MailEvents, MailMessage, MailRef, MailSource,
    MailStore, MailboxDelivery, ProviderExtras, TranscriptLine, micros,
};
use uuid::Uuid;

use crate::address::mailbox_of;
use crate::auth_guard::AuthGuard;
use crate::config::MailConfig;
use crate::labels::{LABELS_HEADER, auto_labels};
use crate::parse::ParsedMail;
use crate::routing::{MailDomains, Route, route};

/// Where an accepted message goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Destination {
    /// Route by recipient domains and the AUTH user (SMTP, provider APIs without `@space`).
    Route,
    /// This area only (`?space=` of the admin API, `/__mail/<provider>/@<space>/`); `None` is the global
    /// area.
    Area(Option<String>),
}

/// A message to capture.
#[derive(Clone, Debug)]
pub struct Incoming {
    pub source: MailSource,
    pub provider: Option<String>,
    /// For each stored copy, `rcpt_to` is replaced by the copy's recipients.
    pub envelope: Envelope,
    pub raw: Bytes,
    pub transcript: Vec<TranscriptLine>,
    pub provider_extras: Option<ProviderExtras>,
    /// Labels on top of the automatic ones; invalid ones are skipped.
    pub labels: Vec<String>,
    pub destination: Destination,
    /// Folders to file the message into instead of the INBOX of each recipient's mailbox.
    pub mailboxes: Option<Vec<MailboxDelivery>>,
}

impl Incoming {
    pub fn new(source: MailSource, envelope: Envelope, raw: impl Into<Bytes>) -> Self {
        Self {
            source,
            provider: None,
            envelope,
            raw: raw.into(),
            transcript: vec![],
            provider_extras: None,
            labels: vec![],
            destination: Destination::Route,
            mailboxes: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AcceptError {
    #[error("the message exceeds {0} bytes")]
    TooLarge(usize),
    #[error("storing the message failed: {0:#}")]
    Store(anyhow::Error),
}

/// Mail storage, events and settings of a running server.
pub struct MailService {
    pub config: Arc<MailConfig>,
    pub store: Arc<dyn MailStore>,
    pub events: Arc<dyn MailEvents>,
    pub domains: MailDomains,
    /// Names of the configured spaces (for AUTH-user routing).
    pub spaces: BTreeSet<String>,
    /// Failed-login counters of SMTP AUTH, IMAP and POP3.
    pub auth: AuthGuard,
}

impl MailService {
    pub fn new(
        config: Arc<MailConfig>,
        store: Arc<dyn MailStore>,
        events: Arc<dyn MailEvents>,
        domains: MailDomains,
        spaces: BTreeSet<String>,
    ) -> Self {
        let auth = AuthGuard::memory(config.login_throttle());
        Self {
            config,
            store,
            events,
            domains,
            spaces,
            auth,
        }
    }

    /// Shares the failed-login counters through `failures` (Valkey with several replicas).
    pub fn with_login_failures(mut self, failures: Arc<dyn LoginFailures>) -> Self {
        self.auth = self.auth.with_failures(failures);
        self
    }

    /// The stored copies a message would make (see [`route`]).
    pub fn routes(&self, rcpt_to: &[String], auth_user: Option<&str>) -> Vec<Route> {
        route(rcpt_to, auth_user, &self.domains, &|s| {
            self.spaces.contains(s)
        })
    }

    /// Parses, routes, labels and stores a message, then publishes `message.received` for each copy.
    /// Returns the stored copies. Runs in a `fastmock.mail.message` span (source, provider, the spaces
    /// and ids of the copies).
    pub async fn accept(&self, inc: Incoming) -> Result<Vec<MailEntry>, AcceptError> {
        use tracing::Instrument as _;
        let span = tracing::info_span!(
            "fastmock.mail.message",
            otel.name = "mail message",
            fastmock.mail.source = inc.source.as_str(),
            fastmock.mail.provider = inc.provider.as_deref().unwrap_or(""),
            fastmock.mail.size = inc.raw.len(),
            fastmock.space = tracing::field::Empty,
            fastmock.mail.ids = tracing::field::Empty,
            fastmock.mail.result = tracing::field::Empty,
        );
        let r = self.accept_inner(inc).instrument(span.clone()).await;
        match &r {
            Ok(entries) => {
                let spaces: Vec<&str> = entries
                    .iter()
                    .map(|e| e.space.as_deref().unwrap_or("_global"))
                    .collect();
                let ids: Vec<String> = entries.iter().map(|e| e.id.to_string()).collect();
                span.record("fastmock.space", spaces.join(",").as_str());
                span.record("fastmock.mail.ids", ids.join(",").as_str());
                span.record("fastmock.mail.result", "stored");
            }
            Err(AcceptError::TooLarge(_)) => {
                span.record("fastmock.mail.result", "too_large");
            }
            Err(AcceptError::Store(_)) => {
                span.record("fastmock.mail.result", "error");
            }
        }
        r
    }

    async fn accept_inner(&self, inc: Incoming) -> Result<Vec<MailEntry>, AcceptError> {
        let max = self.config.max_message_bytes;
        if inc.raw.len() > max {
            return Err(AcceptError::TooLarge(max));
        }
        let (summary, search_text, header_labels) = {
            let parsed = ParsedMail::parse(&inc.raw, &self.config.parse_limits());
            (
                parsed.summary(),
                parsed.search_text(),
                parsed.header_values(LABELS_HEADER),
            )
        };
        let routes = match &inc.destination {
            Destination::Route => {
                self.routes(&inc.envelope.rcpt_to, inc.envelope.auth_user.as_deref())
            }
            Destination::Area(a) => vec![Route {
                space: a.clone(),
                rcpt_to: inc.envelope.rcpt_to.clone(),
            }],
        };
        let received_at = micros(Utc::now());
        let mut out = Vec::with_capacity(routes.len());
        for r in routes {
            let deliveries = inc.mailboxes.clone().unwrap_or_else(|| {
                let mut seen = BTreeSet::new();
                r.rcpt_to
                    .iter()
                    .map(|a| mailbox_of(a))
                    .filter(|m| seen.insert(m.clone()))
                    .map(|m| MailboxDelivery::inbox(&m))
                    .collect()
            });
            let msg = MailMessage {
                id: Uuid::now_v7(),
                space: r.space,
                received_at,
                source: inc.source,
                provider: inc.provider.clone(),
                labels: auto_labels(&r.rcpt_to, &header_labels, &inc.labels),
                envelope: Envelope {
                    rcpt_to: r.rcpt_to,
                    ..inc.envelope.clone()
                },
                summary: summary.clone(),
                provider_extras: inc.provider_extras.clone(),
                transcript: inc.transcript.clone(),
                search_text: search_text.clone(),
                raw: inc.raw.clone(),
            };
            self.store
                .insert(&msg, &deliveries)
                .await
                .map_err(AcceptError::Store)?;
            metrics::counter!(
                "fastmock_mail_messages_total",
                "source" => msg.source.as_str(),
                "provider" => msg.provider.clone().unwrap_or_default()
            )
            .increment(1);
            let inboxes = deliveries
                .iter()
                .filter(|d| d.folder == fastmock_core::mail::INBOX)
                .map(|d| d.address.clone())
                .collect();
            if let Err(e) = self
                .events
                .publish(&MailEvent::received(&msg, inboxes))
                .await
            {
                tracing::warn!(error = %e, id = %msg.id, "publishing message.received failed");
            }
            out.push(msg.entry());
        }
        Ok(out)
    }

    /// Publishes `message.deleted`, one event per area.
    pub async fn publish_deleted(&self, refs: &[MailRef]) {
        let mut by_area: std::collections::BTreeMap<Option<String>, Vec<Uuid>> = Default::default();
        for r in refs {
            by_area.entry(r.space.clone()).or_default().push(r.id);
        }
        for (space, ids) in by_area {
            if let Err(e) = self.events.publish(&MailEvent::deleted(space, ids)).await {
                tracing::warn!(error = %e, "publishing message.deleted failed");
            }
        }
    }

    /// Applies retention and publishes the deletions; returns how many messages were dropped.
    pub async fn sweep(&self, now: DateTime<Utc>) -> anyhow::Result<usize> {
        let refs = self.store.sweep(&self.config.retention, now).await?;
        self.publish_deleted(&refs).await;
        Ok(refs.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastmock_core::mail::{
        INBOX, LocalMailEvents, MailEventKind, MailQuery, MailboxKey, MemoryMailStore,
    };
    use fastmock_core::model::SpaceFilter;

    fn service() -> MailService {
        let mut domains = MailDomains::default();
        domains.add("billing", "billing.example.test");
        domains.add("crm", "crm.example.test");
        MailService::new(
            Arc::new(MailConfig::default()),
            Arc::new(MemoryMailStore::new(100)),
            Arc::new(LocalMailEvents::new(16)),
            domains,
            ["billing", "crm", "ops"].map(String::from).into(),
        )
    }

    const RAW: &str = "From: shop@example.com\r\nTo: a@billing.example.test\r\nSubject: Hi\r\nX-FastMock-Labels: otp\r\n\r\nCode 1234\r\n";

    #[tokio::test]
    async fn accept_routes_labels_stores_and_notifies() {
        let svc = service();
        let mut sub = svc.events.subscribe(SpaceFilter::Any);
        let mut inc = Incoming::new(
            MailSource::Smtp,
            Envelope {
                mail_from: "shop@example.com".into(),
                rcpt_to: vec![
                    "A+Signup@billing.example.test".into(),
                    "a@billing.example.test".into(),
                    "x@crm.example.test".into(),
                ],
                auth_user: Some("ops".into()),
                ..Default::default()
            },
            RAW.as_bytes().to_vec(),
        );
        inc.labels = vec!["extra".into(), "bad label".into()];
        let stored = svc.accept(inc).await.unwrap();
        assert_eq!(stored.len(), 2);
        let billing = &stored[0];
        assert_eq!(billing.space.as_deref(), Some("billing"));
        assert_eq!(
            billing.envelope.rcpt_to,
            vec!["A+Signup@billing.example.test", "a@billing.example.test"]
        );
        assert_eq!(billing.labels, vec!["extra", "otp", "signup"]);
        assert_eq!(billing.summary.subject.as_deref(), Some("Hi"));
        assert_eq!(stored[1].space.as_deref(), Some("crm"));
        assert_eq!(stored[1].labels, vec!["extra", "otp"]);
        assert_ne!(billing.id, stored[1].id);

        let e = sub.recv().await.unwrap();
        assert_eq!(e.kind, MailEventKind::Received);
        assert_eq!(e.space.as_deref(), Some("billing"));
        assert_eq!(e.mailboxes, vec!["a@billing.example.test"]);
        assert_eq!(e.entry.as_ref().unwrap().id, billing.id);
        assert_eq!(sub.recv().await.unwrap().space.as_deref(), Some("crm"));

        let inbox = MailboxKey::inbox(Some("billing"), "a@billing.example.test");
        let entries = svc.store.mailbox_entries(&inbox).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].message_id, billing.id);
        let full = svc
            .store
            .get(billing.id, &SpaceFilter::Any)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(full.raw, RAW.as_bytes());
        assert!(full.search_text.contains("code 1234"));
    }

    #[tokio::test]
    async fn accept_into_an_area_and_mailbox_overrides() {
        let svc = service();
        let mut inc = Incoming::new(
            MailSource::Inject,
            Envelope {
                rcpt_to: vec!["a@billing.example.test".into()],
                ..Default::default()
            },
            RAW.as_bytes().to_vec(),
        );
        inc.destination = Destination::Area(None);
        inc.mailboxes = Some(vec![MailboxDelivery {
            address: "archive@example.test".into(),
            folder: "Sent".into(),
            flags: vec!["\\Seen".into()],
        }]);
        let stored = svc.accept(inc).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].space, None);
        let folders = svc
            .store
            .mailbox_folders(None, "archive@example.test")
            .await
            .unwrap();
        assert_eq!(folders[0].folder, "Sent");
        assert_eq!(folders[0].unseen, 0);
        assert!(
            svc.store
                .mailbox_info(&MailboxKey::inbox(None, "a@billing.example.test"))
                .await
                .unwrap()
                .is_none()
        );
        assert_ne!(folders[0].folder, INBOX);

        let big = Incoming::new(
            MailSource::Inject,
            Envelope::default(),
            vec![b'x'; svc.config.max_message_bytes + 1],
        );
        assert!(matches!(
            svc.accept(big).await,
            Err(AcceptError::TooLarge(_))
        ));
    }

    #[tokio::test]
    async fn sweep_publishes_deletions() {
        let svc = service();
        let mut sub = svc.events.subscribe(SpaceFilter::Any);
        svc.accept(Incoming::new(
            MailSource::Smtp,
            Envelope {
                rcpt_to: vec!["a@crm.example.test".into()],
                ..Default::default()
            },
            RAW.as_bytes().to_vec(),
        ))
        .await
        .unwrap();
        sub.recv().await.unwrap();
        let later = Utc::now() + chrono::Duration::days(8);
        assert_eq!(svc.sweep(later).await.unwrap(), 1);
        let e = sub.recv().await.unwrap();
        assert_eq!(e.kind, MailEventKind::Deleted);
        assert_eq!(e.space.as_deref(), Some("crm"));
        let page = svc.store.list(&MailQuery::default()).await.unwrap();
        assert!(page.items.is_empty());
    }
}
