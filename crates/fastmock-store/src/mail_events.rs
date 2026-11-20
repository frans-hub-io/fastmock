//! Mail events between replicas over Valkey pub/sub: [`ValkeyMailEvents`] implements [`MailEvents`].
//!
//! `publish` sends the event as JSON to the channel of its area, `<prefix>mail:events:<space>` (`_global`
//! for the global area; space names cannot contain `_`). One listener task per process holds a
//! `PSUBSCRIBE <prefix>mail:events:*` connection and hands every event it receives, this replica's own
//! included, to a [`MailEventHub`] that `subscribe` reads from.
//!
//! Pub/sub does not keep messages: events published while the listener is disconnected never reach this
//! replica. The listener pings Valkey, reconnects with a growing pause (up to [`MAX_BACKOFF`]) and logs the
//! gap; [`MailEvents::poll_interval`] tells waiters to re-read the store meanwhile. When publishing fails,
//! the event still reaches this replica's subscribers.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use fastmock_core::SpaceFilter;
use fastmock_core::StoreResult;
use fastmock_core::mail::{MailEvent, MailEventHub, MailEvents, MailSubscription};
use futures_util::StreamExt;
use redis::aio::{ConnectionManager, PubSubSink, PubSubStream};
use redis::{AsyncCommands, Msg};

use crate::valkey::escape_glob;

/// Channel name of the global area.
pub const GLOBAL_CHANNEL: &str = "_global";
/// How often waiters re-read the store by default (see [`MailEvents::poll_interval`]).
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Largest event accepted from Valkey; larger messages are dropped. Larger events are published without
/// their `entry`.
pub const MAX_EVENT_BYTES: usize = 1 << 20;
/// Longest pause between reconnection attempts.
pub const MAX_BACKOFF: Duration = Duration::from_secs(10);
const MIN_BACKOFF: Duration = Duration::from_millis(100);
const PING_EVERY: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(2);

/// [`MailEvents`] shared by every replica through Valkey.
pub struct ValkeyMailEvents {
    conn: ConnectionManager,
    channel_prefix: String,
    hub: MailEventHub,
    connected: Arc<AtomicBool>,
    poll: Duration,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ValkeyMailEvents {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn channel_prefix(prefix: &str) -> String {
    format!("{prefix}mail:events:")
}

fn channel_area(space: Option<&str>) -> &str {
    space.unwrap_or(GLOBAL_CHANNEL)
}

async fn subscribe(
    client: &redis::Client,
    pattern: &str,
) -> anyhow::Result<(PubSubSink, PubSubStream)> {
    let pubsub = tokio::time::timeout(IO_TIMEOUT, client.get_async_pubsub())
        .await
        .map_err(|_| anyhow::anyhow!("Valkey did not answer within {IO_TIMEOUT:?}"))??;
    let (mut sink, stream) = pubsub.split();
    tokio::time::timeout(IO_TIMEOUT, sink.psubscribe(pattern))
        .await
        .map_err(|_| anyhow::anyhow!("PSUBSCRIBE got no answer within {IO_TIMEOUT:?}"))??;
    Ok((sink, stream))
}

/// The event of a pub/sub message, or `None` for messages to ignore (foreign channel, too large, not an
/// event, or an event of another area than its channel).
pub fn decode_event(channel_prefix: &str, channel: &str, payload: &[u8]) -> Option<MailEvent> {
    let area = channel.strip_prefix(channel_prefix)?;
    if payload.len() > MAX_EVENT_BYTES {
        tracing::warn!(
            bytes = payload.len(),
            "dropping an oversized mail event from Valkey"
        );
        return None;
    }
    let event: MailEvent = match serde_json::from_slice(payload) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "dropping a mail event from Valkey that is not valid JSON");
            return None;
        }
    };
    if channel_area(event.space.as_deref()) != area {
        tracing::warn!("dropping a mail event published to another area's channel");
        return None;
    }
    Some(event)
}

/// The JSON sent for an event: without `entry` when it would exceed [`MAX_EVENT_BYTES`].
pub fn encode_event(event: &MailEvent) -> StoreResult<Vec<u8>> {
    let payload = serde_json::to_vec(event)?;
    if payload.len() <= MAX_EVENT_BYTES {
        return Ok(payload);
    }
    let mut slim = event.clone();
    slim.entry = None;
    Ok(serde_json::to_vec(&slim)?)
}

/// Delivers the events of one subscription until it breaks.
async fn pump(
    mut sink: PubSubSink,
    mut stream: PubSubStream,
    prefix: &str,
    hub: &MailEventHub,
) -> &'static str {
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await;
    loop {
        tokio::select! {
            m = stream.next() => match m {
                Some(msg) => deliver(&msg, prefix, hub),
                None => return "the connection closed",
            },
            _ = ping.tick() => {
                match tokio::time::timeout(IO_TIMEOUT, sink.ping::<redis::Value>()).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(_)) => return "PING failed",
                    Err(_) => return "PING got no answer",
                }
            }
        }
    }
}

fn deliver(msg: &Msg, prefix: &str, hub: &MailEventHub) {
    let channel = msg.get_channel_name();
    if let Some(e) = decode_event(prefix, channel, msg.get_payload_bytes()) {
        hub.deliver(e);
    }
}

fn pause(backoff: Duration) -> Duration {
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() % 100)
        .unwrap_or(0);
    backoff + Duration::from_millis(u64::from(jitter))
}

async fn listen(
    client: redis::Client,
    pattern: String,
    prefix: String,
    hub: MailEventHub,
    connected: Arc<AtomicBool>,
    first: (PubSubSink, PubSubStream),
) {
    let mut current = Some(first);
    let mut backoff = MIN_BACKOFF;
    loop {
        if let Some((sink, stream)) = current.take() {
            connected.store(true, Ordering::Relaxed);
            let why = pump(sink, stream, &prefix, &hub).await;
            connected.store(false, Ordering::Relaxed);
            tracing::warn!(
                reason = why,
                "mail events: lost the Valkey subscription; reconnecting, events published meanwhile are not delivered to this replica"
            );
            metrics::counter!("fastmock_mail_events_reconnects_total").increment(1);
            backoff = MIN_BACKOFF;
        }
        tokio::time::sleep(pause(backoff)).await;
        match subscribe(&client, &pattern).await {
            Ok(s) => {
                tracing::info!("mail events: subscribed to Valkey again");
                current = Some(s);
            }
            Err(e) => {
                backoff = (backoff * 2).min(MAX_BACKOFF);
                tracing::warn!(error = %e, retry_in_ms = backoff.as_millis() as u64, "mail events: subscribing to Valkey failed");
            }
        }
    }
}

impl ValkeyMailEvents {
    /// Connects, subscribes and starts the listener task. Fails when Valkey cannot be reached now; later
    /// outages are retried. `capacity`: events a slow subscriber may fall behind (see [`MailEventHub`]).
    pub async fn connect(
        client: redis::Client,
        prefix: &str,
        capacity: usize,
    ) -> anyhow::Result<Self> {
        let config = redis::aio::ConnectionManagerConfig::new()
            .set_connection_timeout(IO_TIMEOUT)
            .set_response_timeout(IO_TIMEOUT)
            .set_number_of_retries(1);
        let conn = tokio::time::timeout(
            Duration::from_secs(30),
            ConnectionManager::new_with_config(client.clone(), config),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Valkey did not answer within 30s"))??;
        let prefix = channel_prefix(prefix);
        let pattern = format!("{}*", escape_glob(&prefix));
        let first = subscribe(&client, &pattern).await?;
        let hub = MailEventHub::new(capacity);
        let connected = Arc::new(AtomicBool::new(true));
        let task = tokio::spawn(listen(
            client,
            pattern,
            prefix.clone(),
            hub.clone(),
            connected.clone(),
            first,
        ));
        Ok(Self {
            conn,
            channel_prefix: prefix,
            hub,
            connected,
            poll: DEFAULT_POLL_INTERVAL,
            task,
        })
    }

    /// Changes the hint of [`MailEvents::poll_interval`].
    pub fn with_poll_interval(mut self, poll: Duration) -> Self {
        self.poll = poll;
        self
    }

    /// Whether the listener holds a subscription now.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// The channel of an area.
    pub fn channel(&self, space: Option<&str>) -> String {
        format!("{}{}", self.channel_prefix, channel_area(space))
    }
}

#[async_trait]
impl MailEvents for ValkeyMailEvents {
    async fn publish(&self, event: &MailEvent) -> StoreResult<()> {
        let payload = encode_event(event)?;
        let mut conn = self.conn.clone();
        let channel = self.channel(event.space.as_deref());
        let sent = tokio::time::timeout(
            PUBLISH_TIMEOUT,
            conn.publish::<_, _, i64>(&channel, payload),
        )
        .await;
        match sent {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => {
                self.hub.deliver(event.clone());
                Err(anyhow::anyhow!(
                    "publishing to Valkey failed, only this replica's subscribers got the event: {e}"
                ))
            }
            Err(_) => {
                self.hub.deliver(event.clone());
                Err(anyhow::anyhow!(
                    "Valkey did not answer PUBLISH within {PUBLISH_TIMEOUT:?}, only this replica's subscribers got the event"
                ))
            }
        }
    }

    fn subscribe(&self, filter: SpaceFilter) -> MailSubscription {
        self.hub.subscribe(filter)
    }

    fn poll_interval(&self) -> Option<Duration> {
        Some(self.poll)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn events_decode_only_from_their_own_channel() {
        let p = channel_prefix("fm:");
        let e = MailEvent::deleted(Some("billing".into()), vec![Uuid::nil()]);
        let json = encode_event(&e).unwrap();
        assert_eq!(
            decode_event(&p, "fm:mail:events:billing", &json),
            Some(e.clone())
        );
        assert_eq!(decode_event(&p, "fm:mail:events:crm", &json), None);
        assert_eq!(decode_event(&p, "other:mail:events:billing", &json), None);
        assert_eq!(decode_event(&p, "fm:mail:events:billing", b"{"), None);
        let g = MailEvent::deleted(None, vec![]);
        assert_eq!(
            decode_event(&p, "fm:mail:events:_global", &encode_event(&g).unwrap()),
            Some(g)
        );
        let big = vec![b' '; MAX_EVENT_BYTES + 1];
        assert_eq!(decode_event(&p, "fm:mail:events:billing", &big), None);
    }

    #[test]
    fn oversized_events_travel_without_their_entry() {
        let mut msg = fastmock_core::mail::conformance::message(Some("s"), chrono::Utc::now(), "x");
        msg.summary.attachment_names = vec!["a".repeat(1024); 1100];
        let e = MailEvent::received(&msg, vec!["a@example.test".into()]);
        let json = encode_event(&e).unwrap();
        assert!(json.len() < MAX_EVENT_BYTES);
        let back: MailEvent = serde_json::from_slice(&json).unwrap();
        assert!(back.entry.is_none());
        assert_eq!(back.ids, e.ids);
        assert_eq!(back.mailboxes, e.mailboxes);
    }

    #[test]
    fn reconnect_pauses_stay_bounded() {
        assert!(pause(MAX_BACKOFF) < MAX_BACKOFF + Duration::from_millis(100));
        assert!(pause(MIN_BACKOFF) >= MIN_BACKOFF);
    }
}
