use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::model::{MailEntry, MailMessage};
use super::query::{MailFilter, MailPage, MailQuery};
use crate::model::SpaceFilter;
use crate::storage::StoreResult;

/// The folder every mailbox has.
pub const INBOX: &str = "INBOX";

/// The canonical spelling of a folder name: `INBOX` in any case is `INBOX` (RFC 3501 5.1), other names are
/// kept as given.
pub fn folder_name(name: &str) -> String {
    if name.eq_ignore_ascii_case(INBOX) {
        INBOX.to_string()
    } else {
        name.to_string()
    }
}

/// The canonical spelling of an IMAP flag: system flags (`\Seen`, `\Answered`, `\Flagged`, `\Deleted`,
/// `\Draft`) in their RFC 3501 case, keywords as given.
pub fn flag_name(flag: &str) -> String {
    const SYSTEM: [&str; 5] = ["\\Seen", "\\Answered", "\\Flagged", "\\Deleted", "\\Draft"];
    SYSTEM
        .iter()
        .find(|s| s.eq_ignore_ascii_case(flag))
        .map_or_else(|| flag.to_string(), |s| s.to_string())
}

/// A folder of the mailbox of one address in one area (a space or the global area). Addresses are
/// lowercase mailbox addresses (`fastmock_mail::address::mailbox_of`); `INBOX` is spelled in capitals.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MailboxKey {
    pub space: Option<String>,
    pub address: String,
    pub folder: String,
}

impl MailboxKey {
    pub fn inbox(space: Option<&str>, address: &str) -> Self {
        Self {
            space: space.map(String::from),
            address: address.to_string(),
            folder: INBOX.to_string(),
        }
    }
}

/// Where [`MailStore::insert`] files a message, in the message's own area.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxDelivery {
    pub address: String,
    pub folder: String,
    /// Initial IMAP flags (`\Seen`, keywords); usually none.
    pub flags: Vec<String>,
}

impl MailboxDelivery {
    pub fn inbox(address: &str) -> Self {
        Self {
            address: address.to_string(),
            folder: INBOX.to_string(),
            flags: vec![],
        }
    }
}

/// IMAP mailbox state (RFC 3501 SELECT/STATUS).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxInfo {
    pub folder: String,
    /// Fixed for the life of the folder; a folder deleted and created again gets a larger value.
    pub uid_validity: u32,
    /// The UID the next message filed into the folder gets.
    pub uid_next: u32,
    /// Messages in the folder.
    pub exists: u32,
    /// Messages without `\Seen`.
    pub unseen: u32,
}

/// A message in a folder. Sequence numbers are the 1-based positions in UID order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxEntry {
    pub uid: u32,
    pub message_id: Uuid,
    /// Sorted, without duplicates; system flags keep their backslash (`\Seen`).
    pub flags: Vec<String>,
    /// IMAP INTERNALDATE.
    pub received_at: DateTime<Utc>,
    /// RFC822.SIZE.
    pub size: u64,
}

impl MailboxEntry {
    pub fn has_flag(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f.eq_ignore_ascii_case(flag))
    }
}

/// An address that has a mailbox, as the admin API lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxOwner {
    pub space: Option<String>,
    pub address: String,
    pub folders: Vec<String>,
    pub has_password: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlagOp {
    Add,
    Remove,
    Replace,
}

/// A message removed from the store, for `message.deleted` events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailRef {
    pub space: Option<String>,
    pub id: Uuid,
}

/// Retention and growth limits (`FASTMOCK_MAIL_MAX_MESSAGES`, `FASTMOCK_MAIL_MAX_AGE`,
/// `FASTMOCK_MAIL_MAX_BYTES`, `FASTMOCK_MAIL_MAX_MAILBOXES`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MailRetention {
    /// Per area (each space and the global area); the oldest messages beyond it are dropped.
    pub max_messages: usize,
    pub max_age: Duration,
    /// Raw bytes per area; the oldest messages beyond it are dropped (the newest one is kept).
    pub max_bytes: u64,
    /// Mailboxes per area. Deliveries to further addresses are stored as messages without a mailbox
    /// entry; empty mailboxes without a password are removed by the sweep.
    pub max_mailboxes: usize,
}

impl Default for MailRetention {
    fn default() -> Self {
        Self {
            max_messages: 1000,
            max_age: Duration::from_secs(7 * 86_400),
            max_bytes: 256 * 1024 * 1024,
            max_mailboxes: 10_000,
        }
    }
}

/// Keywords (flags without a backslash) one entry may carry.
pub const MAX_KEYWORDS: usize = 32;
/// Longest keyword in bytes.
pub const MAX_KEYWORD_BYTES: usize = 64;

/// Where a page of [`MailStore::mailboxes`] continues: after this area and address.
pub type MailboxAfter = (Option<String>, String);

/// Storage of captured mail and of the IMAP/POP3 mailbox state.
///
/// Messages are copies per area: routing to several spaces stores several messages. Mailboxes belong to
/// an area and an address and hold folders; a folder entry points at a stored message. Deleting a
/// message (admin API, retention) removes its folder entries; expunging a folder entry (IMAP, POP3)
/// leaves the message in the store for the admin API.
#[async_trait]
pub trait MailStore: Send + Sync {
    /// Stores a message and files it into the folders of `deliveries` in its area, creating mailboxes
    /// and folders as needed. The id must be new.
    async fn insert(&self, msg: &MailMessage, deliveries: &[MailboxDelivery]) -> StoreResult<()>;

    /// A message in one of the areas `spaces` selects.
    async fn get(&self, id: Uuid, spaces: &SpaceFilter) -> StoreResult<Option<MailMessage>>;

    /// A page of summaries in the query's order; see [`MailQuery`].
    async fn list(&self, query: &MailQuery) -> StoreResult<MailPage>;

    async fn delete(&self, id: Uuid, spaces: &SpaceFilter) -> StoreResult<Option<MailRef>>;

    /// Deletes the messages of `spaces` that match `filter` (all of them for an empty filter).
    async fn delete_matching(
        &self,
        spaces: &SpaceFilter,
        filter: &MailFilter,
    ) -> StoreResult<Vec<MailRef>>;

    /// Replaces the labels; `labels` is already normalised. `None` when the message does not exist there.
    async fn set_labels(
        &self,
        id: Uuid,
        spaces: &SpaceFilter,
        labels: &[String],
    ) -> StoreResult<Option<MailEntry>>;

    /// Drops messages older than `now - max_age` and, per area, the oldest beyond `max_messages` and
    /// `max_bytes`, then removes mailboxes without entries and without a password. Several replicas may
    /// call it; implementations make concurrent sweeps harmless.
    async fn sweep(&self, policy: &MailRetention, now: DateTime<Utc>) -> StoreResult<Vec<MailRef>>;

    /// Folders of an address, sorted by name (`INBOX` first); empty when it has no mailbox.
    async fn mailbox_folders(
        &self,
        space: Option<&str>,
        address: &str,
    ) -> StoreResult<Vec<MailboxInfo>>;

    /// At most `limit` addresses with a mailbox in `spaces` after `after`, sorted by area (the global
    /// area first) and address, bytewise.
    async fn mailboxes(
        &self,
        spaces: &SpaceFilter,
        after: Option<&MailboxAfter>,
        limit: usize,
    ) -> StoreResult<Vec<MailboxOwner>>;

    /// Creates a folder (and the mailbox); false when it exists. Fails when a new mailbox would exceed
    /// the store's mailbox limit for the area.
    async fn mailbox_create(&self, key: &MailboxKey) -> StoreResult<bool>;

    /// Deletes a folder other than `INBOX` with its entries; false when it does not exist.
    async fn mailbox_delete(&self, key: &MailboxKey) -> StoreResult<bool>;

    async fn mailbox_info(&self, key: &MailboxKey) -> StoreResult<Option<MailboxInfo>>;

    /// Entries in UID order; empty when the folder does not exist.
    async fn mailbox_entries(&self, key: &MailboxKey) -> StoreResult<Vec<MailboxEntry>>;

    /// Changes flags of the entries with these UIDs and returns them, changed, in UID order.
    async fn mailbox_store_flags(
        &self,
        key: &MailboxKey,
        uids: &[u32],
        op: FlagOp,
        flags: &[String],
    ) -> StoreResult<Vec<MailboxEntry>>;

    /// Files the entries with these UIDs into the folder `target` of the same mailbox, keeping their
    /// flags. Returns `(source uid, new uid)` pairs (UIDPLUS COPYUID), or `None` when `target` does not
    /// exist (IMAP `[TRYCREATE]`).
    async fn mailbox_copy(
        &self,
        key: &MailboxKey,
        uids: &[u32],
        target: &str,
    ) -> StoreResult<Option<Vec<(u32, u32)>>>;

    /// Removes the entries flagged `\Deleted` (only those among `only` when given) and returns their UIDs
    /// in ascending order.
    async fn mailbox_expunge(
        &self,
        key: &MailboxKey,
        only: Option<&[u32]>,
    ) -> StoreResult<Vec<u32>>;

    /// Removes the entries with these UIDs whatever their flags (IMAP MOVE, POP3 DELE at QUIT) and
    /// returns the UIDs removed in ascending order.
    async fn mailbox_remove(&self, key: &MailboxKey, uids: &[u32]) -> StoreResult<Vec<u32>>;

    /// The stored password hash of an address's mailbox (see C2 in the crate README).
    async fn mailbox_password(
        &self,
        space: Option<&str>,
        address: &str,
    ) -> StoreResult<Option<String>>;

    /// Sets (`Some`) or removes (`None`) the password hash; creates the mailbox when setting one.
    /// Returns false when removing a password that was not set.
    async fn set_mailbox_password(
        &self,
        space: Option<&str>,
        address: &str,
        hash: Option<&str>,
    ) -> StoreResult<bool>;
}
