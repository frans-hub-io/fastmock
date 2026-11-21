//! Captured mail: the stored message model, the [`MailStore`] and [`MailEvents`] interfaces and their
//! in-memory implementations. Parsing, routing and the protocol servers live in the `fastmock-mail`
//! crate; the PostgreSQL store and the Valkey notifications live in `fastmock-store`.

#[doc(hidden)]
pub mod conformance;
mod events;
mod memory;
mod model;
mod query;
mod store;
mod throttle;

pub use events::{
    LocalMailEvents, MailEvent, MailEventHub, MailEventKind, MailEvents, MailSubscription,
    MailSubscriptionError,
};
pub use memory::MemoryMailStore;
pub use model::{
    Envelope, MailAddress, MailEntry, MailMessage, MailSource, MailSummary, ProviderExtras,
    TranscriptDirection, TranscriptLine, micros,
};
pub use query::{MAX_PAGE, MailCursor, MailFilter, MailPage, MailQuery, MailTerm};
pub use store::{
    FlagOp, INBOX, MAX_KEYWORD_BYTES, MAX_KEYWORDS, MailRef, MailRetention, MailStore,
    MailboxAfter, MailboxDelivery, MailboxEntry, MailboxInfo, MailboxKey, MailboxOwner, flag_name,
    folder_name,
};
pub use throttle::{LoginFailures, MemoryLoginFailures, ThrottlePolicy};
