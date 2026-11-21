use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::model::{MailAddress, MailEntry};
use crate::model::SpaceFilter;

/// Largest page a list returns.
pub const MAX_PAGE: usize = 500;

/// One condition of a search. Text values are lowercase and compared as substrings, case-insensitively.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MailTerm {
    /// `from:` – the `From` header or the envelope sender.
    From(String),
    /// `to:` – the `To` header or the envelope recipients (so Bcc recipients are found too).
    To(String),
    /// `cc:` – the `Cc` header.
    Cc(String),
    /// `subject:`
    Subject(String),
    /// `has:attachment`
    HasAttachment,
    /// `label:` – one label, compared exactly.
    Label(String),
    /// `after:` – received at or after.
    After(DateTime<Utc>),
    /// `before:` – received before.
    Before(DateTime<Utc>),
    /// Free text: the subject or the text of the message.
    Text(String),
}

/// All terms must hold; no term selects everything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MailFilter {
    pub terms: Vec<MailTerm>,
}

fn addresses_contain(list: &[MailAddress], needle: &str) -> bool {
    list.iter().any(|a| {
        a.address.to_lowercase().contains(needle)
            || a.name
                .as_deref()
                .is_some_and(|n| n.to_lowercase().contains(needle))
    })
}

impl MailFilter {
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Whether a message matches; `search_text` is [`super::MailMessage::search_text`].
    pub fn accepts(&self, e: &MailEntry, search_text: &str) -> bool {
        let s = &e.summary;
        self.terms.iter().all(|t| match t {
            MailTerm::From(v) => {
                addresses_contain(&s.from, v) || e.envelope.mail_from.to_lowercase().contains(v)
            }
            MailTerm::To(v) => {
                addresses_contain(&s.to, v)
                    || e.envelope
                        .rcpt_to
                        .iter()
                        .any(|r| r.to_lowercase().contains(v))
            }
            MailTerm::Cc(v) => addresses_contain(&s.cc, v),
            MailTerm::Subject(v) => s
                .subject
                .as_deref()
                .is_some_and(|x| x.to_lowercase().contains(v)),
            MailTerm::HasAttachment => s.has_attachments,
            MailTerm::Label(v) => e.labels.iter().any(|l| l == v),
            MailTerm::After(at) => e.received_at >= *at,
            MailTerm::Before(at) => e.received_at < *at,
            MailTerm::Text(v) => {
                search_text.contains(v.as_str())
                    || s.subject
                        .as_deref()
                        .is_some_and(|x| x.to_lowercase().contains(v))
            }
        })
    }
}

/// Position in a list: the last message of the previous page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MailCursor {
    pub received_at: DateTime<Utc>,
    pub id: Uuid,
}

impl MailCursor {
    pub fn of(e: &MailEntry) -> Self {
        Self {
            received_at: e.received_at,
            id: e.id,
        }
    }

    /// Opaque text for the `cursor` parameter.
    pub fn encode(&self) -> String {
        format!(
            "{}.{}",
            self.received_at.timestamp_micros(),
            self.id.simple()
        )
    }

    pub fn decode(s: &str) -> Option<Self> {
        let (t, id) = s.split_once('.')?;
        let micros: i64 = t.parse().ok()?;
        Some(Self {
            received_at: DateTime::from_timestamp_micros(micros)?,
            id: Uuid::try_parse(id).ok()?,
        })
    }
}

/// A list or search. Messages are ordered by `(received_at, id)`, newest first unless `oldest_first`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailQuery {
    pub spaces: SpaceFilter,
    pub filter: MailFilter,
    /// 1 to [`MAX_PAGE`]; other values are clamped.
    pub limit: usize,
    /// Continue after this message (exclusive).
    pub cursor: Option<MailCursor>,
    pub oldest_first: bool,
}

impl Default for MailQuery {
    fn default() -> Self {
        Self {
            spaces: SpaceFilter::Any,
            filter: MailFilter::default(),
            limit: 50,
            cursor: None,
            oldest_first: false,
        }
    }
}

impl MailQuery {
    pub fn page_size(&self) -> usize {
        self.limit.clamp(1, MAX_PAGE)
    }

    /// Whether a message at `c` lies after the cursor in the query's order.
    pub fn after_cursor(&self, c: MailCursor) -> bool {
        match self.cursor {
            None => true,
            Some(cur) if self.oldest_first => c > cur,
            Some(cur) => c < cur,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MailPage {
    pub items: Vec<MailEntry>,
    /// Set when more messages follow; pass it back as the query's cursor.
    pub next: Option<MailCursor>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::model::{Envelope, MailSource, MailSummary};

    fn entry() -> MailEntry {
        MailEntry {
            id: Uuid::now_v7(),
            space: None,
            received_at: DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
            source: MailSource::Smtp,
            provider: None,
            envelope: Envelope {
                mail_from: "Bounce@Example.com".into(),
                rcpt_to: vec!["hidden@example.test".into()],
                ..Default::default()
            },
            summary: MailSummary {
                subject: Some("Your Invoice 42".into()),
                from: vec![MailAddress {
                    name: Some("Billing Team".into()),
                    address: "billing@example.com".into(),
                }],
                to: vec![MailAddress {
                    name: None,
                    address: "alice@example.test".into(),
                }],
                has_attachments: true,
                ..Default::default()
            },
            labels: vec!["welcome".into()],
            provider_extras: None,
        }
    }

    #[test]
    fn filter_terms() {
        let e = entry();
        let ok = |terms: Vec<MailTerm>| MailFilter { terms }.accepts(&e, "pay now");
        assert!(ok(vec![]));
        assert!(ok(vec![MailTerm::From("billing team".into())]));
        assert!(ok(vec![MailTerm::From("bounce@".into())]));
        assert!(ok(vec![MailTerm::To("hidden".into())]));
        assert!(ok(vec![MailTerm::To("alice".into())]));
        assert!(!ok(vec![MailTerm::Cc("alice".into())]));
        assert!(ok(vec![MailTerm::Subject("invoice 42".into())]));
        assert!(ok(vec![
            MailTerm::HasAttachment,
            MailTerm::Label("welcome".into())
        ]));
        assert!(!ok(vec![MailTerm::Label("welc".into())]));
        assert!(ok(vec![MailTerm::Text("pay".into())]));
        assert!(ok(vec![MailTerm::Text("invoice".into())]));
        assert!(!ok(vec![MailTerm::Text("refund".into())]));
        assert!(ok(vec![MailTerm::After(e.received_at)]));
        assert!(!ok(vec![MailTerm::Before(e.received_at)]));
    }

    #[test]
    fn cursors_round_trip() {
        let e = entry();
        let c = MailCursor::of(&e);
        assert_eq!(MailCursor::decode(&c.encode()), Some(c));
        assert_eq!(MailCursor::decode("x.y"), None);
        assert_eq!(MailCursor::decode("12"), None);
        let q = MailQuery {
            cursor: Some(c),
            ..Default::default()
        };
        let older = MailCursor {
            received_at: c.received_at,
            id: Uuid::nil(),
        };
        assert!(q.after_cursor(older));
        assert!(!q.after_cursor(c));
        assert_eq!(MailQuery::default().page_size(), 50);
        assert_eq!(
            MailQuery {
                limit: 0,
                ..Default::default()
            }
            .page_size(),
            1
        );
    }
}
