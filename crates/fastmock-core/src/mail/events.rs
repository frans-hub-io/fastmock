use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

use super::model::{MailEntry, MailMessage};
use crate::model::SpaceFilter;
use crate::storage::StoreResult;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum MailEventKind {
    #[serde(rename = "message.received")]
    Received,
    #[serde(rename = "message.deleted")]
    Deleted,
}

impl MailEventKind {
    /// The SSE event name.
    pub fn as_str(self) -> &'static str {
        match self {
            MailEventKind::Received => "message.received",
            MailEventKind::Deleted => "message.deleted",
        }
    }
}

/// A change to the stored mail of one area (a space or the global area). It travels between replicas as
/// JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MailEvent {
    #[serde(rename = "type")]
    pub kind: MailEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    /// The message received, or the messages deleted.
    pub ids: Vec<Uuid>,
    /// `message.received`: mailbox addresses whose INBOX got the message (IMAP IDLE).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mailboxes: Vec<String>,
    /// `message.received`: the message's summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<MailEntry>,
}

impl MailEvent {
    pub fn received(msg: &MailMessage, mailboxes: Vec<String>) -> Self {
        Self {
            kind: MailEventKind::Received,
            space: msg.space.clone(),
            ids: vec![msg.id],
            mailboxes,
            entry: Some(msg.entry()),
        }
    }

    pub fn deleted(space: Option<String>, ids: Vec<Uuid>) -> Self {
        Self {
            kind: MailEventKind::Deleted,
            space,
            ids,
            mailboxes: vec![],
            entry: None,
        }
    }
}

/// Publishes mail events to every replica and lets this one subscribe.
///
/// [`LocalMailEvents`] serves one process. A multi-replica implementation publishes to a shared bus
/// (Valkey pub/sub, a channel per area) and runs one listener task per process that hands what it
/// receives, its own events included, to a [`MailEventHub`]; `subscribe` then reads from that hub.
#[async_trait]
pub trait MailEvents: Send + Sync {
    /// Sends an event to the subscribers of every replica, this one included.
    async fn publish(&self, event: &MailEvent) -> StoreResult<()>;

    /// Events of the areas `filter` selects, from now on.
    fn subscribe(&self, filter: SpaceFilter) -> MailSubscription;

    /// How often a waiter should also re-read the store, because this implementation can lose events (a
    /// shared bus that was briefly unreachable). `None`: events are never lost while the process runs.
    fn poll_interval(&self) -> Option<std::time::Duration> {
        None
    }
}

/// The in-process fan-out of mail events.
#[derive(Clone)]
pub struct MailEventHub {
    tx: broadcast::Sender<Arc<MailEvent>>,
}

impl MailEventHub {
    /// `capacity`: events a slow subscriber may fall behind before it sees
    /// [`MailSubscriptionError::Lagged`].
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        Self { tx }
    }

    /// Hands an event to this process's subscribers.
    pub fn deliver(&self, event: MailEvent) {
        let _ = self.tx.send(Arc::new(event));
    }

    pub fn subscribe(&self, filter: SpaceFilter) -> MailSubscription {
        MailSubscription {
            rx: self.tx.subscribe(),
            filter,
        }
    }

    pub fn subscribers(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for MailEventHub {
    fn default() -> Self {
        Self::new(1024)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MailSubscriptionError {
    /// The subscriber fell behind and missed this many events; read the current state from the store.
    #[error("missed {0} mail events")]
    Lagged(u64),
    #[error("mail events closed")]
    Closed,
}

/// The events of some areas, in publication order.
pub struct MailSubscription {
    rx: broadcast::Receiver<Arc<MailEvent>>,
    filter: SpaceFilter,
}

impl MailSubscription {
    pub async fn recv(&mut self) -> Result<Arc<MailEvent>, MailSubscriptionError> {
        loop {
            match self.rx.recv().await {
                Ok(e) if self.filter.matches(e.space.as_deref()) => return Ok(e),
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    return Err(MailSubscriptionError::Lagged(n));
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(MailSubscriptionError::Closed);
                }
            }
        }
    }
}

/// Mail events of a single process (memory storage).
#[derive(Clone, Default)]
pub struct LocalMailEvents {
    hub: MailEventHub,
}

impl LocalMailEvents {
    pub fn new(capacity: usize) -> Self {
        Self {
            hub: MailEventHub::new(capacity),
        }
    }
}

#[async_trait]
impl MailEvents for LocalMailEvents {
    async fn publish(&self, event: &MailEvent) -> StoreResult<()> {
        self.hub.deliver(event.clone());
        Ok(())
    }

    fn subscribe(&self, filter: SpaceFilter) -> MailSubscription {
        self.hub.subscribe(filter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscribers_see_their_areas_only() {
        let events = LocalMailEvents::new(4);
        let mut billing = events.subscribe(SpaceFilter::In(vec!["billing".into()]));
        let mut global = events.subscribe(SpaceFilter::Global);
        let mut all = events.subscribe(SpaceFilter::Any);
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        events
            .publish(&MailEvent::deleted(Some("other".into()), vec![a]))
            .await
            .unwrap();
        events
            .publish(&MailEvent::deleted(Some("billing".into()), vec![b]))
            .await
            .unwrap();
        events
            .publish(&MailEvent::deleted(None, vec![a, b]))
            .await
            .unwrap();
        assert_eq!(billing.recv().await.unwrap().ids, vec![b]);
        assert_eq!(global.recv().await.unwrap().ids, vec![a, b]);
        assert_eq!(all.recv().await.unwrap().ids, vec![a]);
        assert_eq!(all.recv().await.unwrap().ids, vec![b]);
    }

    #[tokio::test]
    async fn slow_subscribers_learn_they_lagged() {
        let events = LocalMailEvents::new(2);
        let mut s = events.subscribe(SpaceFilter::Any);
        for _ in 0..5 {
            events
                .publish(&MailEvent::deleted(None, vec![]))
                .await
                .unwrap();
        }
        assert_eq!(
            s.recv().await.unwrap_err(),
            MailSubscriptionError::Lagged(3)
        );
        assert!(s.recv().await.is_ok());
    }

    #[test]
    fn events_serialize_with_their_sse_names() {
        let e = MailEvent::deleted(Some("billing".into()), vec![Uuid::nil()]);
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], "message.deleted");
        assert_eq!(v["space"], "billing");
        assert!(v.get("mailboxes").is_none());
        let back: MailEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back, e);
        assert_eq!(MailEventKind::Received.as_str(), "message.received");
    }
}
