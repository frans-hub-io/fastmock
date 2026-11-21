//! The IMAP commands the server understands, independent of the parser crate. [`super::codec`] turns
//! parsed commands into these types; the session only sees these.

use chrono::NaiveDate;

/// Most ranges in one sequence set.
pub const MAX_SEQUENCE_RANGES: usize = 1000;

/// A number of a sequence set: a value or `*` (the largest number in use).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeqNum {
    Num(u32),
    Star,
}

impl SeqNum {
    fn resolve(self, largest: u32) -> u32 {
        match self {
            SeqNum::Num(n) => n,
            SeqNum::Star => largest,
        }
    }
}

/// A sequence set of message sequence numbers or UIDs (RFC 3501 `sequence-set`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeqSet(pub Vec<(SeqNum, SeqNum)>);

impl SeqSet {
    /// Whether `n` is in the set when `*` stands for `largest` (0 for an empty mailbox, where `*` matches
    /// nothing). Ranges hold the numbers between their ends in either order.
    pub fn contains(&self, n: u32, largest: u32) -> bool {
        self.0.iter().any(|(a, b)| {
            let (a, b) = (a.resolve(largest), b.resolve(largest));
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            lo != 0 && (lo..=hi).contains(&n)
        })
    }
}

/// Password bytes; never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(pub Vec<u8>);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusItem {
    Messages,
    Recent,
    UidNext,
    UidValidity,
    Unseen,
    Deleted,
    DeletedStorage,
}

/// What a section of `BODY[...]` names inside its part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SectionText {
    /// The whole message (empty part path) or the body of the part.
    Full,
    Header,
    /// Upper-cased field names.
    HeaderFields(Vec<String>),
    HeaderFieldsNot(Vec<String>),
    Text,
    Mime,
}

/// A `BODY[...]` section: a part path (`1.2`) and what to take from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub part: Vec<u32>,
    pub text: SectionText,
}

impl Section {
    pub fn full() -> Self {
        Self {
            part: vec![],
            text: SectionText::Full,
        }
    }

    /// The section as written in the response, e.g. `1.HEADER.FIELDS (FROM SUBJECT)`.
    pub fn spec(&self) -> String {
        let mut s = self
            .part
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(".");
        let text = match &self.text {
            SectionText::Full => String::new(),
            SectionText::Header => "HEADER".into(),
            SectionText::HeaderFields(f) => format!("HEADER.FIELDS ({})", f.join(" ")),
            SectionText::HeaderFieldsNot(f) => format!("HEADER.FIELDS.NOT ({})", f.join(" ")),
            SectionText::Text => "TEXT".into(),
            SectionText::Mime => "MIME".into(),
        };
        if !text.is_empty() {
            if !s.is_empty() {
                s.push('.');
            }
            s.push_str(&text);
        }
        s
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchItem {
    Flags,
    Uid,
    InternalDate,
    Rfc822Size,
    Envelope,
    BodyStructure,
    /// `BODY` without a section: the non-extensible structure.
    Body,
    BodySection {
        section: Section,
        /// `<origin.length>`.
        partial: Option<(u32, u32)>,
        peek: bool,
    },
    Rfc822,
    Rfc822Header,
    Rfc822Text,
}

impl FetchItem {
    /// Returns message bytes: `BODY[...]`, `RFC822`, `RFC822.HEADER` or `RFC822.TEXT`.
    pub fn is_section(&self) -> bool {
        matches!(
            self,
            FetchItem::BodySection { .. }
                | FetchItem::Rfc822
                | FetchItem::Rfc822Header
                | FetchItem::Rfc822Text
        )
    }

    /// Reading it sets `\Seen` (unless the mailbox is read-only).
    pub fn sets_seen(&self) -> bool {
        match self {
            FetchItem::BodySection { peek, .. } => !peek,
            FetchItem::Rfc822 | FetchItem::Rfc822Text => true,
            _ => false,
        }
    }

    /// Needs the stored message, not only the mailbox entry.
    pub fn needs_message(&self) -> bool {
        !matches!(
            self,
            FetchItem::Flags | FetchItem::Uid | FetchItem::InternalDate | FetchItem::Rfc822Size
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchKey {
    All,
    And(Vec<SearchKey>),
    Or(Box<SearchKey>, Box<SearchKey>),
    Not(Box<SearchKey>),
    SeqSet(SeqSet),
    Uid(SeqSet),
    /// A flag is set (`SEEN`, `FLAGGED`, `KEYWORD x`, ...): the canonical flag name.
    Flag(String),
    /// A flag is not set (`UNSEEN`, `UNKEYWORD x`, ...).
    NoFlag(String),
    New,
    Old,
    Recent,
    From(String),
    To(String),
    Cc(String),
    Bcc(String),
    Subject(String),
    Text(String),
    Body(String),
    Header(String, String),
    Since(NaiveDate),
    Before(NaiveDate),
    On(NaiveDate),
    SentSince(NaiveDate),
    SentBefore(NaiveDate),
    SentOn(NaiveDate),
    Larger(u32),
    Smaller(u32),
}

impl SearchKey {
    /// Needs the stored message (headers or text), not only the mailbox entry.
    pub fn needs_message(&self) -> bool {
        match self {
            SearchKey::And(v) => v.iter().any(Self::needs_message),
            SearchKey::Or(a, b) => a.needs_message() || b.needs_message(),
            SearchKey::Not(k) => k.needs_message(),
            SearchKey::From(_)
            | SearchKey::To(_)
            | SearchKey::Cc(_)
            | SearchKey::Bcc(_)
            | SearchKey::Subject(_)
            | SearchKey::Text(_)
            | SearchKey::Body(_)
            | SearchKey::Header(..)
            | SearchKey::SentSince(_)
            | SearchKey::SentBefore(_)
            | SearchKey::SentOn(_) => true,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreOp {
    Add,
    Remove,
    Replace,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cmd {
    Capability,
    Noop,
    Logout,
    StartTls,
    Authenticate {
        mechanism: String,
        /// SASL-IR, already base64-decoded.
        initial: Option<Secret>,
    },
    Login {
        user: Vec<u8>,
        password: Secret,
    },
    Select {
        mailbox: String,
        read_only: bool,
    },
    Create(String),
    Delete(String),
    Rename,
    Subscribe(String),
    Unsubscribe(String),
    List {
        reference: String,
        pattern: String,
        lsub: bool,
    },
    Status {
        mailbox: String,
        items: Vec<StatusItem>,
    },
    Append {
        mailbox: String,
        flags: Vec<String>,
        message: Vec<u8>,
    },
    Check,
    Close,
    Unselect,
    /// `EXPUNGE`, or `UID EXPUNGE` with its UID set.
    Expunge(Option<SeqSet>),
    Search {
        charset: Option<String>,
        key: SearchKey,
        uid: bool,
    },
    Fetch {
        set: SeqSet,
        items: Vec<FetchItem>,
        uid: bool,
    },
    Store {
        set: SeqSet,
        op: StoreOp,
        silent: bool,
        flags: Vec<String>,
        uid: bool,
    },
    Copy {
        set: SeqSet,
        mailbox: String,
        uid: bool,
        /// MOVE instead of COPY.
        remove: bool,
    },
    Idle,
    Enable(Vec<String>),
    Id,
    Namespace,
    /// A command the parser knows that this server does not implement.
    Unsupported(&'static str),
}

impl Cmd {
    /// The command name for logs and errors.
    pub fn name(&self) -> &'static str {
        match self {
            Cmd::Capability => "CAPABILITY",
            Cmd::Noop => "NOOP",
            Cmd::Logout => "LOGOUT",
            Cmd::StartTls => "STARTTLS",
            Cmd::Authenticate { .. } => "AUTHENTICATE",
            Cmd::Login { .. } => "LOGIN",
            Cmd::Select {
                read_only: false, ..
            } => "SELECT",
            Cmd::Select { .. } => "EXAMINE",
            Cmd::Create(_) => "CREATE",
            Cmd::Delete(_) => "DELETE",
            Cmd::Rename => "RENAME",
            Cmd::Subscribe(_) => "SUBSCRIBE",
            Cmd::Unsubscribe(_) => "UNSUBSCRIBE",
            Cmd::List { lsub: false, .. } => "LIST",
            Cmd::List { .. } => "LSUB",
            Cmd::Status { .. } => "STATUS",
            Cmd::Append { .. } => "APPEND",
            Cmd::Check => "CHECK",
            Cmd::Close => "CLOSE",
            Cmd::Unselect => "UNSELECT",
            Cmd::Expunge(_) => "EXPUNGE",
            Cmd::Search { .. } => "SEARCH",
            Cmd::Fetch { .. } => "FETCH",
            Cmd::Store { .. } => "STORE",
            Cmd::Copy { remove: false, .. } => "COPY",
            Cmd::Copy { .. } => "MOVE",
            Cmd::Idle => "IDLE",
            Cmd::Enable(_) => "ENABLE",
            Cmd::Id => "ID",
            Cmd::Namespace => "NAMESPACE",
            Cmd::Unsupported(n) => n,
        }
    }
}

/// A tagged command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub tag: String,
    pub cmd: Cmd,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_sets_resolve_star_and_reversed_ranges() {
        let s = SeqSet(vec![
            (SeqNum::Num(2), SeqNum::Num(4)),
            (SeqNum::Num(9), SeqNum::Star),
        ]);
        assert!(!s.contains(1, 7));
        assert!(s.contains(3, 7));
        assert!(s.contains(7, 7));
        assert!(s.contains(8, 7), "9:* with * = 7 is 7:9");
        assert!(!s.contains(10, 7));
        let star = SeqSet(vec![(SeqNum::Star, SeqNum::Star)]);
        assert!(
            !star.contains(1, 0),
            "* matches nothing in an empty mailbox"
        );
        assert!(star.contains(5, 5));
    }

    #[test]
    fn section_specs() {
        assert_eq!(Section::full().spec(), "");
        let s = Section {
            part: vec![1, 2],
            text: SectionText::HeaderFields(vec!["FROM".into(), "SUBJECT".into()]),
        };
        assert_eq!(s.spec(), "1.2.HEADER.FIELDS (FROM SUBJECT)");
        let s = Section {
            part: vec![3],
            text: SectionText::Full,
        };
        assert_eq!(s.spec(), "3");
        let s = Section {
            part: vec![],
            text: SectionText::Text,
        };
        assert_eq!(s.spec(), "TEXT");
    }

    #[test]
    fn secrets_are_not_printed() {
        let c = Cmd::Login {
            user: b"a".to_vec(),
            password: Secret(b"hunter2".to_vec()),
        };
        assert!(!format!("{c:?}").contains("hunter2"));
    }
}
