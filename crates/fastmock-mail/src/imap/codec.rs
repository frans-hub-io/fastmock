//! The only module that touches `imap-codec`/`imap-types` (pinned alphas): it decodes one complete
//! command message (lines and literals already framed by the session) into [`Request`]. Responses are
//! written by [`super::response`] without the crate, so a later swap only changes this file.

use imap_codec::CommandCodec;
use imap_codec::decode::{CommandDecodeError, Decoder};
use imap_types::command::CommandBody;
use imap_types::core::AString;
use imap_types::extensions::binary::LiteralOrLiteral8;
use imap_types::extensions::enable::CapabilityEnable;
use imap_types::fetch::{Macro, MacroOrMessageDataItemNames, MessageDataItemName, Part, Section};
use imap_types::flag::{Flag, StoreResponse, StoreType};
use imap_types::mailbox::{ListMailbox, Mailbox};
use imap_types::search::SearchKey as Key;
use imap_types::sequence::{SeqOrUid, Sequence, SequenceSet};
use imap_types::status::StatusDataItemName;

use super::command::{
    Cmd, FetchItem, MAX_SEQUENCE_RANGES, Request, SearchKey, Secret, SectionText, SeqNum, SeqSet,
    StatusItem, StoreOp,
};
use crate::imap::command::Section as OurSection;

/// Why a command could not be used; answered with `BAD`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// The tag, when one could be read.
    pub tag: Option<String>,
    pub reason: &'static str,
}

/// The tag at the start of a message: the bytes before the first space, if they form a valid tag.
pub fn guess_tag(msg: &[u8]) -> Option<String> {
    let end = msg.iter().position(|b| *b == b' ')?;
    let t = &msg[..end];
    let ok = !t.is_empty()
        && t.len() <= 64
        && t.iter().all(|b| {
            b.is_ascii_graphic()
                && !matches!(b, b'(' | b')' | b'{' | b'%' | b'*' | b'"' | b'\\' | b'+')
        });
    ok.then(|| String::from_utf8_lossy(t).into_owned())
}

fn err(tag: &Option<String>, reason: &'static str) -> ParseError {
    ParseError {
        tag: tag.clone(),
        reason,
    }
}

/// Decodes one framed command message (ending in CRLF, literals inline).
pub fn parse(msg: &[u8]) -> Result<Request, ParseError> {
    let tag = guess_tag(msg);
    let cmd = match CommandCodec::default().decode(msg) {
        Ok((b"", cmd)) => cmd,
        Ok(_) => return Err(err(&tag, "Unexpected data after the command")),
        Err(CommandDecodeError::LiteralFound { .. } | CommandDecodeError::Incomplete) => {
            return Err(err(&tag, "Incomplete command"));
        }
        Err(CommandDecodeError::Failed) => return Err(err(&tag, "Command syntax error")),
    };
    let tag = cmd.tag.as_ref().to_string();
    let body = convert(cmd.body).map_err(|reason| err(&Some(tag.clone()), reason))?;
    Ok(Request { tag, cmd: body })
}

type R<T> = Result<T, &'static str>;

fn utf8(bytes: &[u8]) -> R<String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| "Names must be UTF-8")
}

fn mailbox(m: &Mailbox<'_>) -> R<String> {
    match m {
        Mailbox::Inbox => Ok(fastmock_core::mail::INBOX.to_string()),
        Mailbox::Other(o) => utf8(o.as_ref()),
    }
}

fn list_mailbox(m: &ListMailbox<'_>) -> R<String> {
    match m {
        ListMailbox::Token(t) => utf8(t.as_ref()),
        ListMailbox::String(s) => utf8(s.as_ref()),
    }
}

fn astring(s: &AString<'_>) -> R<String> {
    utf8(s.as_ref())
}

fn seq_num(s: &SeqOrUid) -> SeqNum {
    match s {
        SeqOrUid::Value(n) => SeqNum::Num(n.get()),
        SeqOrUid::Asterisk => SeqNum::Star,
    }
}

fn seq_set(s: &SequenceSet) -> R<SeqSet> {
    let items = s.0.as_ref();
    if items.len() > MAX_SEQUENCE_RANGES {
        return Err("Sequence set too long");
    }
    Ok(SeqSet(
        items
            .iter()
            .map(|x| match x {
                Sequence::Single(a) => (seq_num(a), seq_num(a)),
                Sequence::Range(a, b) => (seq_num(a), seq_num(b)),
            })
            .collect(),
    ))
}

fn flag(f: &Flag<'_>) -> String {
    fastmock_core::mail::flag_name(&f.to_string())
}

fn part(p: &Option<Part>) -> Vec<u32> {
    p.as_ref()
        .map(|p| p.0.as_ref().iter().map(|n| n.get()).collect())
        .unwrap_or_default()
}

fn fields(v: &imap_types::core::Vec1<AString<'_>>) -> R<Vec<String>> {
    v.as_ref()
        .iter()
        .map(|f| astring(f).map(|s| s.to_ascii_uppercase()))
        .collect()
}

fn section(s: &Option<Section<'_>>) -> R<OurSection> {
    let Some(s) = s else {
        return Ok(OurSection::full());
    };
    Ok(match s {
        Section::Part(p) => OurSection {
            part: part(&Some(p.clone())),
            text: SectionText::Full,
        },
        Section::Header(p) => OurSection {
            part: part(p),
            text: SectionText::Header,
        },
        Section::HeaderFields(p, f) => OurSection {
            part: part(p),
            text: SectionText::HeaderFields(fields(f)?),
        },
        Section::HeaderFieldsNot(p, f) => OurSection {
            part: part(p),
            text: SectionText::HeaderFieldsNot(fields(f)?),
        },
        Section::Text(p) => OurSection {
            part: part(p),
            text: SectionText::Text,
        },
        Section::Mime(p) => OurSection {
            part: part(&Some(p.clone())),
            text: SectionText::Mime,
        },
    })
}

fn fetch_item(i: &MessageDataItemName<'_>) -> R<FetchItem> {
    Ok(match i {
        MessageDataItemName::Body => FetchItem::Body,
        MessageDataItemName::BodyExt {
            section: s,
            partial,
            peek,
        } => FetchItem::BodySection {
            section: section(s)?,
            partial: partial.map(|(o, l)| (o, l.get())),
            peek: *peek,
        },
        MessageDataItemName::BodyStructure => FetchItem::BodyStructure,
        MessageDataItemName::Envelope => FetchItem::Envelope,
        MessageDataItemName::Flags => FetchItem::Flags,
        MessageDataItemName::InternalDate => FetchItem::InternalDate,
        MessageDataItemName::Rfc822 => FetchItem::Rfc822,
        MessageDataItemName::Rfc822Header => FetchItem::Rfc822Header,
        MessageDataItemName::Rfc822Size => FetchItem::Rfc822Size,
        MessageDataItemName::Rfc822Text => FetchItem::Rfc822Text,
        MessageDataItemName::Uid => FetchItem::Uid,
        MessageDataItemName::Binary { .. } | MessageDataItemName::BinarySize { .. } => {
            return Err("BINARY is not supported");
        }
    })
}

fn fetch_items(m: &MacroOrMessageDataItemNames<'_>) -> R<Vec<FetchItem>> {
    match m {
        MacroOrMessageDataItemNames::Macro(m) => {
            let items = match m {
                Macro::Fast => vec![
                    FetchItem::Flags,
                    FetchItem::InternalDate,
                    FetchItem::Rfc822Size,
                ],
                Macro::All => vec![
                    FetchItem::Flags,
                    FetchItem::InternalDate,
                    FetchItem::Rfc822Size,
                    FetchItem::Envelope,
                ],
                _ => vec![
                    FetchItem::Flags,
                    FetchItem::InternalDate,
                    FetchItem::Rfc822Size,
                    FetchItem::Envelope,
                    FetchItem::Body,
                ],
            };
            Ok(items)
        }
        MacroOrMessageDataItemNames::MessageDataItemNames(v) => v.iter().map(fetch_item).collect(),
    }
}

/// Deepest nesting of NOT/OR/AND accepted (imap-codec stops at 8 already).
const MAX_SEARCH_DEPTH: usize = 16;

fn search_key(k: &Key<'_>, depth: usize) -> R<SearchKey> {
    if depth > MAX_SEARCH_DEPTH {
        return Err("Search criteria nested too deeply");
    }
    let s = |a: &AString<'_>| astring(a);
    let fl = |name: &str| SearchKey::Flag(name.to_string());
    let nofl = |name: &str| SearchKey::NoFlag(name.to_string());
    Ok(match k {
        Key::And(v) => SearchKey::And(
            v.as_ref()
                .iter()
                .map(|k| search_key(k, depth + 1))
                .collect::<R<_>>()?,
        ),
        Key::SequenceSet(set) => SearchKey::SeqSet(seq_set(set)?),
        Key::All => SearchKey::All,
        Key::Answered => fl("\\Answered"),
        Key::Bcc(a) => SearchKey::Bcc(s(a)?),
        Key::Before(d) => SearchKey::Before(*d.as_ref()),
        Key::Body(a) => SearchKey::Body(s(a)?),
        Key::Cc(a) => SearchKey::Cc(s(a)?),
        Key::Deleted => fl("\\Deleted"),
        Key::Draft => fl("\\Draft"),
        Key::Flagged => fl("\\Flagged"),
        Key::From(a) => SearchKey::From(s(a)?),
        Key::Header(n, v) => SearchKey::Header(s(n)?, s(v)?),
        Key::Keyword(a) => fl(a.as_ref()),
        Key::Larger(n) => SearchKey::Larger(*n),
        Key::New => SearchKey::New,
        Key::Not(k) => SearchKey::Not(Box::new(search_key(k, depth + 1)?)),
        Key::Old => SearchKey::Old,
        Key::On(d) => SearchKey::On(*d.as_ref()),
        Key::Or(a, b) => SearchKey::Or(
            Box::new(search_key(a, depth + 1)?),
            Box::new(search_key(b, depth + 1)?),
        ),
        Key::Recent => SearchKey::Recent,
        Key::Seen => fl("\\Seen"),
        Key::SentBefore(d) => SearchKey::SentBefore(*d.as_ref()),
        Key::SentOn(d) => SearchKey::SentOn(*d.as_ref()),
        Key::SentSince(d) => SearchKey::SentSince(*d.as_ref()),
        Key::Since(d) => SearchKey::Since(*d.as_ref()),
        Key::Smaller(n) => SearchKey::Smaller(*n),
        Key::Subject(a) => SearchKey::Subject(s(a)?),
        Key::Text(a) => SearchKey::Text(s(a)?),
        Key::To(a) => SearchKey::To(s(a)?),
        Key::Uid(set) => SearchKey::Uid(seq_set(set)?),
        Key::Unanswered => nofl("\\Answered"),
        Key::Undeleted => nofl("\\Deleted"),
        Key::Undraft => nofl("\\Draft"),
        Key::Unflagged => nofl("\\Flagged"),
        Key::Unkeyword(a) => nofl(a.as_ref()),
        Key::Unseen => nofl("\\Seen"),
    })
}

fn status_items(items: &[StatusDataItemName]) -> Vec<StatusItem> {
    items
        .iter()
        .map(|i| match i {
            StatusDataItemName::Messages => StatusItem::Messages,
            StatusDataItemName::Recent => StatusItem::Recent,
            StatusDataItemName::UidNext => StatusItem::UidNext,
            StatusDataItemName::UidValidity => StatusItem::UidValidity,
            StatusDataItemName::Unseen => StatusItem::Unseen,
            StatusDataItemName::Deleted => StatusItem::Deleted,
            StatusDataItemName::DeletedStorage => StatusItem::DeletedStorage,
        })
        .collect()
}

fn convert(body: CommandBody<'_>) -> R<Cmd> {
    Ok(match body {
        CommandBody::Capability => Cmd::Capability,
        CommandBody::Noop => Cmd::Noop,
        CommandBody::Logout => Cmd::Logout,
        CommandBody::StartTLS => Cmd::StartTls,
        CommandBody::Authenticate {
            mechanism,
            initial_response,
        } => Cmd::Authenticate {
            mechanism: mechanism.as_ref().to_ascii_uppercase(),
            initial: initial_response.map(|r| Secret(r.declassify().to_vec())),
        },
        CommandBody::Login { username, password } => Cmd::Login {
            user: username.as_ref().to_vec(),
            password: Secret(password.declassify().as_ref().to_vec()),
        },
        CommandBody::Select { mailbox: m } => Cmd::Select {
            mailbox: mailbox(&m)?,
            read_only: false,
        },
        CommandBody::Examine { mailbox: m } => Cmd::Select {
            mailbox: mailbox(&m)?,
            read_only: true,
        },
        CommandBody::Unselect => Cmd::Unselect,
        CommandBody::Create { mailbox: m } => Cmd::Create(mailbox(&m)?),
        CommandBody::Delete { mailbox: m } => Cmd::Delete(mailbox(&m)?),
        CommandBody::Rename { .. } => Cmd::Rename,
        CommandBody::Subscribe { mailbox: m } => Cmd::Subscribe(mailbox(&m)?),
        CommandBody::Unsubscribe { mailbox: m } => Cmd::Unsubscribe(mailbox(&m)?),
        CommandBody::List {
            reference,
            mailbox_wildcard,
        } => Cmd::List {
            reference: mailbox(&reference)?,
            pattern: list_mailbox(&mailbox_wildcard)?,
            lsub: false,
        },
        CommandBody::Lsub {
            reference,
            mailbox_wildcard,
        } => Cmd::List {
            reference: mailbox(&reference)?,
            pattern: list_mailbox(&mailbox_wildcard)?,
            lsub: true,
        },
        CommandBody::Status {
            mailbox: m,
            item_names,
        } => Cmd::Status {
            mailbox: mailbox(&m)?,
            items: status_items(&item_names),
        },
        CommandBody::Append {
            mailbox: m,
            flags,
            message,
            ..
        } => {
            let message = match message {
                LiteralOrLiteral8::Literal(l) => l.data().to_vec(),
                LiteralOrLiteral8::Literal8(l) => l.data.to_vec(),
            };
            Cmd::Append {
                mailbox: mailbox(&m)?,
                flags: flags.iter().map(flag).collect(),
                message,
            }
        }
        CommandBody::Check => Cmd::Check,
        CommandBody::Close => Cmd::Close,
        CommandBody::Expunge => Cmd::Expunge(None),
        CommandBody::ExpungeUid { sequence_set } => Cmd::Expunge(Some(seq_set(&sequence_set)?)),
        CommandBody::Search {
            charset,
            criteria,
            uid,
        } => {
            let keys: Vec<SearchKey> = criteria
                .as_ref()
                .iter()
                .map(|k| search_key(k, 0))
                .collect::<R<_>>()?;
            let key = if keys.len() == 1 {
                keys.into_iter().next().unwrap_or(SearchKey::All)
            } else {
                SearchKey::And(keys)
            };
            Cmd::Search {
                charset: charset.map(|c| c.as_ref().to_string()),
                key,
                uid,
            }
        }
        CommandBody::Fetch {
            sequence_set,
            macro_or_item_names,
            uid,
        } => Cmd::Fetch {
            set: seq_set(&sequence_set)?,
            items: fetch_items(&macro_or_item_names)?,
            uid,
        },
        CommandBody::Store {
            sequence_set,
            kind,
            response,
            flags,
            uid,
        } => Cmd::Store {
            set: seq_set(&sequence_set)?,
            op: match kind {
                StoreType::Add => StoreOp::Add,
                StoreType::Remove => StoreOp::Remove,
                StoreType::Replace => StoreOp::Replace,
            },
            silent: response == StoreResponse::Silent,
            flags: flags.iter().map(flag).collect(),
            uid,
        },
        CommandBody::Copy {
            sequence_set,
            mailbox: m,
            uid,
        } => Cmd::Copy {
            set: seq_set(&sequence_set)?,
            mailbox: mailbox(&m)?,
            uid,
            remove: false,
        },
        CommandBody::Move {
            sequence_set,
            mailbox: m,
            uid,
        } => Cmd::Copy {
            set: seq_set(&sequence_set)?,
            mailbox: mailbox(&m)?,
            uid,
            remove: true,
        },
        CommandBody::Idle => Cmd::Idle,
        CommandBody::Enable { capabilities } => Cmd::Enable(
            capabilities
                .as_ref()
                .iter()
                .map(|c: &CapabilityEnable<'_>| c.to_string().to_ascii_uppercase())
                .collect(),
        ),
        CommandBody::Id { .. } => Cmd::Id,
        CommandBody::Namespace => Cmd::Namespace,
        CommandBody::Sort { .. } => Cmd::Unsupported("SORT"),
        CommandBody::Thread { .. } => Cmd::Unsupported("THREAD"),
        CommandBody::Compress { .. } => Cmd::Unsupported("COMPRESS"),
        CommandBody::GetQuota { .. } => Cmd::Unsupported("GETQUOTA"),
        CommandBody::GetQuotaRoot { .. } => Cmd::Unsupported("GETQUOTAROOT"),
        CommandBody::SetQuota { .. } => Cmd::Unsupported("SETQUOTA"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Cmd {
        parse(s.as_bytes()).unwrap().cmd
    }

    #[test]
    fn tags_and_errors() {
        let r = parse(b"a1 NOOP\r\n").unwrap();
        assert_eq!(r.tag, "a1");
        assert_eq!(r.cmd, Cmd::Noop);
        let e = parse(b"a2 FROB\r\n").unwrap_err();
        assert_eq!(e.tag.as_deref(), Some("a2"));
        let e = parse(b"* NOOP\r\n").unwrap_err();
        assert_eq!(e.tag, None);
        assert_eq!(
            parse(b"a3 NOOP\r\nb NOOP\r\n").unwrap_err().reason,
            "Unexpected data after the command"
        );
        assert_eq!(guess_tag(b"x{1} NOOP"), None);
    }

    #[test]
    fn login_and_authenticate() {
        match p("a LOGIN {5}\r\nalice \"p w\"\r\n") {
            Cmd::Login { user, password } => {
                assert_eq!(user, b"alice");
                assert_eq!(password.0, b"p w");
            }
            c => panic!("{c:?}"),
        }
        match p("a AUTHENTICATE plain AGEAYg==\r\n") {
            Cmd::Authenticate { mechanism, initial } => {
                assert_eq!(mechanism, "PLAIN");
                assert_eq!(initial.unwrap().0, b"\0a\0b");
            }
            c => panic!("{c:?}"),
        }
        match p("a AUTHENTICATE PLAIN\r\n") {
            Cmd::Authenticate { initial, .. } => assert!(initial.is_none()),
            c => panic!("{c:?}"),
        }
    }

    #[test]
    fn mailboxes_and_lists() {
        assert_eq!(
            p("a SELECT inbox\r\n"),
            Cmd::Select {
                mailbox: "INBOX".into(),
                read_only: false
            }
        );
        assert_eq!(
            p("a EXAMINE \"Sent\"\r\n"),
            Cmd::Select {
                mailbox: "Sent".into(),
                read_only: true
            }
        );
        assert_eq!(
            p("a LIST \"\" *\r\n"),
            Cmd::List {
                reference: "".into(),
                pattern: "*".into(),
                lsub: false
            }
        );
        assert_eq!(
            p("a STATUS INBOX (MESSAGES UIDNEXT UNSEEN)\r\n"),
            Cmd::Status {
                mailbox: "INBOX".into(),
                items: vec![
                    StatusItem::Messages,
                    StatusItem::UidNext,
                    StatusItem::Unseen
                ]
            }
        );
    }

    #[test]
    fn fetch_items_and_sections() {
        match p(
            "a UID FETCH 1:* (FLAGS BODY.PEEK[1.2.HEADER.FIELDS (from Subject)]<0.10> BODY[] RFC822.SIZE)\r\n",
        ) {
            Cmd::Fetch { set, items, uid } => {
                assert!(uid);
                assert_eq!(set.0, vec![(SeqNum::Num(1), SeqNum::Star)]);
                assert_eq!(items.len(), 4);
                assert_eq!(
                    items[1],
                    FetchItem::BodySection {
                        section: OurSection {
                            part: vec![1, 2],
                            text: SectionText::HeaderFields(vec!["FROM".into(), "SUBJECT".into()])
                        },
                        partial: Some((0, 10)),
                        peek: true
                    }
                );
                assert!(items[2].sets_seen());
            }
            c => panic!("{c:?}"),
        }
        match p("a FETCH 2 ALL\r\n") {
            Cmd::Fetch { items, .. } => assert!(items.contains(&FetchItem::Envelope)),
            c => panic!("{c:?}"),
        }
        match p("a FETCH 2 (BODY[2.MIME] BODY[TEXT])\r\n") {
            Cmd::Fetch { items, .. } => {
                assert_eq!(
                    items[0],
                    FetchItem::BodySection {
                        section: OurSection {
                            part: vec![2],
                            text: SectionText::Mime
                        },
                        partial: None,
                        peek: false
                    }
                );
            }
            c => panic!("{c:?}"),
        }
        assert_eq!(
            parse(b"a FETCH 1 BINARY[1]\r\n").unwrap_err().reason,
            "BINARY is not supported"
        );
    }

    #[test]
    fn search_store_copy_move() {
        match p(
            "a UID SEARCH CHARSET UTF-8 UNSEEN OR FROM alice SUBJECT \"hi there\" NOT DELETED SINCE 1-Feb-2026 UID 3:5\r\n",
        ) {
            Cmd::Search { charset, key, uid } => {
                assert!(uid);
                assert_eq!(charset.as_deref(), Some("UTF-8"));
                let SearchKey::And(v) = key else { panic!() };
                assert_eq!(v[0], SearchKey::NoFlag("\\Seen".into()));
                assert_eq!(
                    v[1],
                    SearchKey::Or(
                        Box::new(SearchKey::From("alice".into())),
                        Box::new(SearchKey::Subject("hi there".into()))
                    )
                );
                assert_eq!(
                    v[2],
                    SearchKey::Not(Box::new(SearchKey::Flag("\\Deleted".into())))
                );
                assert_eq!(
                    v[3],
                    SearchKey::Since(chrono::NaiveDate::from_ymd_opt(2026, 2, 1).unwrap())
                );
                assert!(matches!(v[4], SearchKey::Uid(_)));
            }
            c => panic!("{c:?}"),
        }
        assert_eq!(
            p("a STORE 1,3 +FLAGS.SILENT (\\seen $Label)\r\n"),
            Cmd::Store {
                set: SeqSet(vec![
                    (SeqNum::Num(1), SeqNum::Num(1)),
                    (SeqNum::Num(3), SeqNum::Num(3))
                ]),
                op: StoreOp::Add,
                silent: true,
                flags: vec!["\\Seen".into(), "$Label".into()],
                uid: false
            }
        );
        assert!(matches!(
            p("a UID MOVE 4 Archive\r\n"),
            Cmd::Copy {
                remove: true,
                uid: true,
                ..
            }
        ));
        assert!(matches!(
            p("a COPY 4 Archive\r\n"),
            Cmd::Copy {
                remove: false,
                uid: false,
                ..
            }
        ));
        assert!(matches!(p("a UID EXPUNGE 4:7\r\n"), Cmd::Expunge(Some(_))));
    }

    #[test]
    fn append_and_extensions() {
        match p("a APPEND Sent (\\Seen) {12}\r\nSubject: x\r\n\r\n") {
            Cmd::Append {
                mailbox,
                flags,
                message,
            } => {
                assert_eq!(mailbox, "Sent");
                assert_eq!(flags, vec!["\\Seen"]);
                assert_eq!(message, b"Subject: x\r\n");
            }
            c => panic!("{c:?}"),
        }
        assert_eq!(
            p("a ENABLE UTF8=ACCEPT\r\n"),
            Cmd::Enable(vec!["UTF8=ACCEPT".into()])
        );
        assert_eq!(p("a ID NIL\r\n"), Cmd::Id);
        assert_eq!(p("a ID (\"name\" \"x\")\r\n"), Cmd::Id);
        assert_eq!(p("a NAMESPACE\r\n"), Cmd::Namespace);
        assert_eq!(p("a IDLE\r\n"), Cmd::Idle);
        assert_eq!(p("a UNSELECT\r\n"), Cmd::Unselect);
        assert_eq!(p("a STARTTLS\r\n"), Cmd::StartTls);
        assert_eq!(
            p("a GETQUOTAROOT INBOX\r\n"),
            Cmd::Unsupported("GETQUOTAROOT")
        );
    }
}
