//! IMAP SEARCH: matching mailbox entries (flags, dates, sizes, numbers) and, when a key needs it, the
//! message's headers and text. String keys match case-insensitively as substrings of decoded values.

use chrono::NaiveDate;
use fastmock_core::mail::MailboxEntry;
use mail_parser::{AddressList, HeaderValue};

use super::command::SearchKey;
use super::mime::{MessageView, header_end};
use super::response::unfold;
use crate::parse::ParseLimits;

/// Bytes of body text kept per message for BODY and TEXT.
pub const MAX_SEARCH_BODY: usize = 1024 * 1024;

/// What string keys of SEARCH look at, lowercased.
#[derive(Clone, Debug, Default)]
pub struct SearchDoc {
    from: String,
    to: String,
    cc: String,
    bcc: String,
    subject: String,
    /// `(lowercase name, lowercase decoded value)`.
    headers: Vec<(String, String)>,
    header_block: String,
    body: String,
    sent: Option<NaiveDate>,
}

fn addr_text(list: Option<AddressList<'_>>) -> String {
    list.map(|l| {
        l.mailboxes()
            .map(|m| match (m.name(), m.address()) {
                (Some(n), Some(a)) => format!("{n} <{a}>"),
                (None, Some(a)) => a.to_string(),
                (Some(n), None) => n.to_string(),
                (None, None) => String::new(),
            })
            .collect::<Vec<_>>()
            .join(", ")
            .to_lowercase()
    })
    .unwrap_or_default()
}

fn cap(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

impl SearchDoc {
    pub fn build(raw: &[u8], limits: &ParseLimits) -> Self {
        let view = MessageView::parse(raw, limits);
        let header_block = String::from_utf8_lossy(&raw[..header_end(raw)]).to_lowercase();
        let Some(root) = view.root() else {
            return Self {
                header_block,
                body: cap(
                    String::from_utf8_lossy(&raw[header_end(raw)..]).to_lowercase(),
                    MAX_SEARCH_BODY,
                ),
                ..Default::default()
            };
        };
        let h = root.headers();
        let mut headers = Vec::new();
        for f in h.iter() {
            let raw_v = String::from_utf8_lossy(&unfold(f.raw_value())).to_lowercase();
            headers.push((f.raw_name().to_ascii_lowercase(), raw_v));
            if let HeaderValue::Text(t) = f.value() {
                headers.push((f.raw_name().to_ascii_lowercase(), t.to_lowercase()));
            }
        }
        let mut body = String::new();
        for t in root.text_bodies().chain(root.html_bodies()) {
            if body.len() >= MAX_SEARCH_BODY {
                break;
            }
            body.push_str(&t.to_lowercase());
            body.push('\n');
        }
        let header_of = |name: &str| {
            headers
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, v)| v.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let with_raw = |parsed: String, name: &str| format!("{parsed}\n{}", header_of(name));
        Self {
            from: with_raw(addr_text(h.from()), "from"),
            to: with_raw(addr_text(h.to()), "to"),
            cc: with_raw(addr_text(h.cc()), "cc"),
            bcc: with_raw(addr_text(h.bcc()), "bcc"),
            subject: with_raw(
                h.subject().map(str::to_lowercase).unwrap_or_default(),
                "subject",
            ),
            sent: h
                .date()
                .and_then(|d| NaiveDate::from_ymd_opt(d.year.into(), d.month.into(), d.day.into())),
            headers,
            header_block,
            body: cap(body, MAX_SEARCH_BODY),
        }
    }
}

/// A message being searched.
pub struct Candidate<'a> {
    pub seq: u32,
    pub entry: &'a MailboxEntry,
    /// Present when the key needs the message; missing documents match no string key.
    pub doc: Option<&'a SearchDoc>,
}

fn has(hay: &str, needle: &str) -> bool {
    hay.contains(&needle.to_lowercase())
}

/// Whether a message matches; `max_seq`/`max_uid` resolve `*`.
pub fn matches(key: &SearchKey, c: &Candidate<'_>, max_seq: u32, max_uid: u32) -> bool {
    let doc = |f: &dyn Fn(&SearchDoc) -> bool| c.doc.is_some_and(f);
    let date = c.entry.received_at.date_naive();
    match key {
        SearchKey::All | SearchKey::Old => true,
        SearchKey::New | SearchKey::Recent => false,
        SearchKey::And(v) => v.iter().all(|k| matches(k, c, max_seq, max_uid)),
        SearchKey::Or(a, b) => matches(a, c, max_seq, max_uid) || matches(b, c, max_seq, max_uid),
        SearchKey::Not(k) => !matches(k, c, max_seq, max_uid),
        SearchKey::SeqSet(s) => s.contains(c.seq, max_seq),
        SearchKey::Uid(s) => s.contains(c.entry.uid, max_uid),
        SearchKey::Flag(f) => c.entry.has_flag(f),
        SearchKey::NoFlag(f) => !c.entry.has_flag(f),
        SearchKey::From(s) => doc(&|d| has(&d.from, s)),
        SearchKey::To(s) => doc(&|d| has(&d.to, s)),
        SearchKey::Cc(s) => doc(&|d| has(&d.cc, s)),
        SearchKey::Bcc(s) => doc(&|d| has(&d.bcc, s)),
        SearchKey::Subject(s) => doc(&|d| has(&d.subject, s)),
        SearchKey::Body(s) => doc(&|d| has(&d.body, s)),
        SearchKey::Text(s) => doc(&|d| has(&d.header_block, s) || has(&d.body, s)),
        SearchKey::Header(name, value) => doc(&|d| {
            let name = name.to_ascii_lowercase();
            d.headers.iter().any(|(n, v)| *n == name && has(v, value))
        }),
        SearchKey::Since(d) => date >= *d,
        SearchKey::Before(d) => date < *d,
        SearchKey::On(d) => date == *d,
        SearchKey::SentSince(d) => doc(&|x| x.sent.is_some_and(|s| s >= *d)),
        SearchKey::SentBefore(d) => doc(&|x| x.sent.is_some_and(|s| s < *d)),
        SearchKey::SentOn(d) => doc(&|x| x.sent == Some(*d)),
        SearchKey::Larger(n) => c.entry.size > u64::from(*n),
        SearchKey::Smaller(n) => c.entry.size < u64::from(*n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imap::command::{SeqNum, SeqSet};
    use chrono::{TimeZone, Utc};

    const RAW: &[u8] = b"From: Alice Example <alice@example.com>\r\nTo: bob@billing.example.test\r\nCc: carol@example.com\r\nSubject: =?utf-8?q?Your_invoice_=E2=82=AC?=\r\nDate: Tue, 3 Feb 2026 23:30:00 -0500\r\nX-Order: A-17\r\nContent-Type: multipart/alternative; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nTotal: 12 =E2=82=AC, code 4711\r\n--b\r\nContent-Type: text/html\r\n\r\n<p>Pay now</p>\r\n--b--\r\n";

    fn entry(uid: u32, flags: &[&str], size: u64) -> MailboxEntry {
        MailboxEntry {
            uid,
            message_id: uuid::Uuid::nil(),
            flags: flags.iter().map(|s| s.to_string()).collect(),
            received_at: Utc.with_ymd_and_hms(2026, 2, 4, 8, 0, 0).unwrap(),
            size,
        }
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn string_keys_use_decoded_values() {
        let doc = SearchDoc::build(RAW, &ParseLimits::default());
        let e = entry(7, &["\\Seen"], 400);
        let c = Candidate {
            seq: 2,
            entry: &e,
            doc: Some(&doc),
        };
        let m = |k: SearchKey| matches(&k, &c, 3, 9);
        assert!(m(SearchKey::From("ALICE".into())));
        assert!(m(SearchKey::From("alice@example.com".into())));
        assert!(!m(SearchKey::From("bob".into())));
        assert!(m(SearchKey::To("billing.example".into())));
        assert!(m(SearchKey::Cc("carol".into())));
        assert!(!m(SearchKey::Bcc("carol".into())));
        assert!(m(SearchKey::Subject("invoice €".into())));
        assert!(m(SearchKey::Body("12 €".into())));
        assert!(m(SearchKey::Body("pay now".into())));
        assert!(!m(SearchKey::Body("x-order".into())));
        assert!(m(SearchKey::Text("x-order".into())));
        assert!(m(SearchKey::Text("4711".into())));
        assert!(m(SearchKey::Header("x-order".into(), "a-17".into())));
        assert!(m(SearchKey::Header("X-Order".into(), "".into())));
        assert!(!m(SearchKey::Header("X-Missing".into(), "".into())));
        assert!(
            m(SearchKey::SentOn(date(2026, 2, 3))),
            "the sender's local date"
        );
        assert!(m(SearchKey::SentBefore(date(2026, 2, 4))));
        assert!(!m(SearchKey::SentSince(date(2026, 2, 4))));
    }

    #[test]
    fn entry_keys() {
        let e = entry(7, &["\\Seen", "\\Flagged", "$Work"], 400);
        let c = Candidate {
            seq: 2,
            entry: &e,
            doc: None,
        };
        let m = |k: SearchKey| matches(&k, &c, 3, 9);
        assert!(m(SearchKey::All));
        assert!(m(SearchKey::Flag("\\Seen".into())));
        assert!(!m(SearchKey::NoFlag("\\Seen".into())));
        assert!(m(SearchKey::NoFlag("\\Deleted".into())));
        assert!(m(SearchKey::Flag("$work".into())));
        assert!(!m(SearchKey::New));
        assert!(m(SearchKey::Old));
        assert!(m(SearchKey::Since(date(2026, 2, 4))));
        assert!(m(SearchKey::On(date(2026, 2, 4))));
        assert!(!m(SearchKey::Before(date(2026, 2, 4))));
        assert!(m(SearchKey::Larger(399)));
        assert!(!m(SearchKey::Larger(400)));
        assert!(m(SearchKey::Smaller(401)));
        let set = |a, b| SeqSet(vec![(a, b)]);
        assert!(m(SearchKey::SeqSet(set(SeqNum::Num(2), SeqNum::Star))));
        assert!(!m(SearchKey::SeqSet(set(SeqNum::Star, SeqNum::Star))));
        assert!(m(SearchKey::Uid(set(SeqNum::Num(7), SeqNum::Num(7)))));
        assert!(m(SearchKey::Or(
            Box::new(SearchKey::Flag("\\Draft".into())),
            Box::new(SearchKey::Flag("\\Flagged".into()))
        )));
        assert!(!m(SearchKey::And(vec![
            SearchKey::Flag("\\Seen".into()),
            SearchKey::Not(Box::new(SearchKey::Flag("\\Flagged".into())))
        ])));
        assert!(!m(SearchKey::From("alice".into())), "no document, no match");
    }

    #[test]
    fn unparsed_messages_still_match_text() {
        let doc = SearchDoc::build(
            b"Subject: big\r\n\r\nneedle",
            &ParseLimits {
                max_bytes: 4,
                ..Default::default()
            },
        );
        let e = entry(1, &[], 10);
        let c = Candidate {
            seq: 1,
            entry: &e,
            doc: Some(&doc),
        };
        assert!(matches(&SearchKey::Body("NEEDLE".into()), &c, 1, 1));
        assert!(matches(&SearchKey::Text("subject: big".into()), &c, 1, 1));
    }
}
