//! The IMAP view of a stored message: ENVELOPE and BODYSTRUCTURE from mail-parser's tree, and the
//! bytes of `BODY[...]` sections.

use std::borrow::Cow;

use mail_parser::{Address, AddressList, Headers, Message, MessagePart, MessageRef};

use super::command::{Section, SectionText};
use super::response::{header_text, nstring, string, text_value, unfold};
use crate::parse::ParseLimits;

/// Where the header block of a message ends: after the first empty line, or at the end.
pub fn header_end(raw: &[u8]) -> usize {
    let mut i = 0;
    while i < raw.len() {
        let line_end = raw[i..]
            .iter()
            .position(|b| *b == b'\n')
            .map_or(raw.len(), |p| i + p + 1);
        let line = &raw[i..line_end];
        if line == b"\r\n" || line == b"\n" {
            return line_end;
        }
        i = line_end;
    }
    raw.len()
}

/// The fields of a header block: `(name, the field's bytes with its line breaks)`.
pub fn header_fields(block: &[u8]) -> Vec<(&[u8], &[u8])> {
    let mut out: Vec<(&[u8], &[u8])> = Vec::new();
    let mut i = 0;
    let mut start: Option<usize> = None;
    while i < block.len() {
        let line_end = block[i..]
            .iter()
            .position(|b| *b == b'\n')
            .map_or(block.len(), |p| i + p + 1);
        let line = &block[i..line_end];
        if line == b"\r\n" || line == b"\n" {
            break;
        }
        let continuation = matches!(line.first(), Some(b' ' | b'\t'));
        if !continuation {
            if let Some(s) = start {
                out.push(field(&block[s..i]));
            }
            start = Some(i);
        }
        i = line_end;
    }
    if let Some(s) = start {
        out.push(field(&block[s..i]));
    }
    out
}

fn field(bytes: &[u8]) -> (&[u8], &[u8]) {
    let name = bytes
        .iter()
        .position(|b| *b == b':')
        .map_or(&bytes[..0], |p| &bytes[..p]);
    (name.trim_ascii(), bytes)
}

/// The fields named (or, with `not`, not named) in `names` (upper case), then an empty line.
pub fn select_fields(block: &[u8], names: &[String], not: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, bytes) in header_fields(block) {
        let listed = names
            .iter()
            .any(|n| n.as_bytes().eq_ignore_ascii_case(name));
        if listed != not && !name.is_empty() {
            out.extend_from_slice(bytes);
            if !bytes.ends_with(b"\n") {
                out.extend_from_slice(b"\r\n");
            }
        }
    }
    out.extend_from_slice(b"\r\n");
    out
}

/// A stored message parsed for IMAP within the parse limits.
pub struct MessageView<'a> {
    raw: &'a [u8],
    msg: Option<Message<'a>>,
}

/// What a part path points at.
enum Target<'m> {
    /// The top-level message (empty path).
    Root,
    Part(MessagePart<'m>),
}

impl<'a> MessageView<'a> {
    pub fn parse(raw: &'a [u8], limits: &ParseLimits) -> Self {
        let msg = if raw.len() > limits.max_bytes {
            None
        } else {
            limits.parser().parse(raw)
        };
        Self { raw, msg }
    }

    pub fn raw(&self) -> &'a [u8] {
        self.raw
    }

    pub fn header(&self) -> &'a [u8] {
        &self.raw[..header_end(self.raw)]
    }

    pub fn text(&self) -> &'a [u8] {
        &self.raw[header_end(self.raw)..]
    }

    /// The parsed root message.
    pub fn root(&self) -> Option<MessageRef<'_>> {
        self.msg.as_ref().map(|m| m.root())
    }

    /// The `ENVELOPE` of the message.
    pub fn envelope(&self, out: &mut Vec<u8>, utf8: bool) {
        match self.root() {
            Some(m) => envelope(out, m.headers(), utf8),
            None => out.extend_from_slice(b"(NIL NIL NIL NIL NIL NIL NIL NIL NIL NIL)"),
        }
    }

    /// `BODYSTRUCTURE` (`ext`) or `BODY`.
    pub fn body_structure(&self, out: &mut Vec<u8>, ext: bool, utf8: bool) {
        match self.root() {
            Some(m) => body(out, &m.root_part(), ext, utf8, 0),
            None => {
                let text = self.text();
                out.extend_from_slice(
                    format!(
                        "(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"US-ASCII\") NIL NIL \"7BIT\" {} {}",
                        text.len(),
                        lines(text)
                    )
                    .as_bytes(),
                );
                if ext {
                    out.extend_from_slice(b" NIL NIL NIL NIL");
                }
                out.push(b')');
            }
        }
    }

    fn target(&self, path: &[u32]) -> Option<Target<'_>> {
        if path.is_empty() {
            return Some(Target::Root);
        }
        let root = self.root()?;
        let mut cur: Option<MessagePart<'_>> = None;
        for n in path {
            let n = *n as usize;
            let container = match &cur {
                None => root.root_part(),
                Some(p) if p.is_message() => p.nested()?.root_part(),
                Some(p) if p.is_multipart() => *p,
                Some(_) => return None,
            };
            let entered_message = cur.as_ref().is_none_or(|p| p.is_message());
            cur = Some(if container.is_multipart() {
                container.children().nth(n.checked_sub(1)?)?
            } else if n == 1 && entered_message {
                container
            } else {
                return None;
            });
        }
        cur.map(Target::Part)
    }

    /// The bytes of a section, `None` when the part does not exist; borrowed where no copy is needed.
    pub fn section(&self, s: &Section) -> Option<Cow<'_, [u8]>> {
        let target = self.target(&s.part)?;
        let whole: &[u8] = match &target {
            Target::Root => self.raw,
            Target::Part(p) if p.is_message() => p.nested().map_or(p.raw_body(), |m| m.raw()),
            Target::Part(p) => p.raw_body(),
        };
        let message_like = match &target {
            Target::Root => true,
            Target::Part(p) => p.is_message(),
        };
        Some(match &s.text {
            SectionText::Full => match &target {
                Target::Root => Cow::Borrowed(self.raw),
                Target::Part(p) => Cow::Owned(p.raw_body().to_vec()),
            },
            SectionText::Header if message_like => Cow::Owned(whole[..header_end(whole)].to_vec()),
            SectionText::HeaderFields(f) if message_like => {
                Cow::Owned(select_fields(&whole[..header_end(whole)], f, false))
            }
            SectionText::HeaderFieldsNot(f) if message_like => {
                Cow::Owned(select_fields(&whole[..header_end(whole)], f, true))
            }
            SectionText::Text if message_like => Cow::Owned(whole[header_end(whole)..].to_vec()),
            SectionText::Mime => match &target {
                Target::Part(p) => {
                    let mut h = p.raw_headers().to_vec();
                    if !(h.ends_with(b"\r\n\r\n") || h.ends_with(b"\n\n")) {
                        if !h.is_empty() && !h.ends_with(b"\n") {
                            h.extend_from_slice(b"\r\n");
                        }
                        h.extend_from_slice(b"\r\n");
                    }
                    Cow::Owned(h)
                }
                Target::Root => return None,
            },
            _ => return None,
        })
    }
}

fn lines(b: &[u8]) -> usize {
    b.iter().filter(|c| **c == b'\n').count()
}

fn addresses(out: &mut Vec<u8>, list: Option<AddressList<'_>>, utf8: bool) {
    let Some(list) = list.filter(|l| !l.is_empty()) else {
        out.extend_from_slice(b"NIL");
        return;
    };
    out.push(b'(');
    for a in list.iter() {
        match a {
            Address::Mailbox(m) => address(out, m.name(), m.address(), utf8),
            Address::Group(g) => {
                out.extend_from_slice(b"(NIL NIL ");
                nstring(out, Some(&text_value(g.name().unwrap_or(""), utf8)), utf8);
                out.extend_from_slice(b" NIL)");
                for m in g.mailboxes() {
                    address(out, m.name(), m.address(), utf8);
                }
                out.extend_from_slice(b"(NIL NIL NIL NIL)");
            }
        }
    }
    out.push(b')');
}

fn address(out: &mut Vec<u8>, name: Option<&str>, addr: Option<&str>, utf8: bool) {
    out.push(b'(');
    let name = name.filter(|n| !n.is_empty()).map(|n| text_value(n, utf8));
    nstring(out, name.as_deref(), utf8);
    out.extend_from_slice(b" NIL ");
    let (local, host) = match addr.and_then(|a| a.rsplit_once('@')) {
        Some((l, h)) => (Some(l), Some(h)),
        None => (addr, None),
    };
    nstring(out, local.map(str::as_bytes), utf8);
    out.push(b' ');
    nstring(out, host.map(str::as_bytes), utf8);
    out.push(b')');
}

fn raw_header<'m>(h: &Headers<'m>, name: &str) -> Option<&'m [u8]> {
    h.get(name).map(|x| x.raw_value())
}

/// `ENVELOPE` of a header block.
pub fn envelope(out: &mut Vec<u8>, h: Headers<'_>, utf8: bool) {
    out.push(b'(');
    let text = |out: &mut Vec<u8>, name: &str| {
        let v = raw_header(&h, name).map(|v| header_text(v, utf8));
        nstring(out, v.as_deref(), utf8);
    };
    text(out, "Date");
    out.push(b' ');
    text(out, "Subject");
    let from = h.from();
    out.push(b' ');
    addresses(out, from, utf8);
    out.push(b' ');
    addresses(out, h.sender().or(from), utf8);
    out.push(b' ');
    addresses(out, h.reply_to().or(from), utf8);
    out.push(b' ');
    addresses(out, h.to(), utf8);
    out.push(b' ');
    addresses(out, h.cc(), utf8);
    out.push(b' ');
    addresses(out, h.bcc(), utf8);
    out.push(b' ');
    let raw = |out: &mut Vec<u8>, name: &str| {
        let v = raw_header(&h, name).map(unfold);
        nstring(out, v.as_deref(), utf8);
    };
    raw(out, "In-Reply-To");
    out.push(b' ');
    raw(out, "Message-ID");
    out.push(b')');
}

fn params(out: &mut Vec<u8>, attrs: Vec<(&str, &str)>, utf8: bool) {
    if attrs.is_empty() {
        out.extend_from_slice(b"NIL");
        return;
    }
    out.push(b'(');
    for (i, (k, v)) in attrs.iter().enumerate() {
        if i > 0 {
            out.push(b' ');
        }
        string(out, k.to_ascii_uppercase().as_bytes(), utf8);
        out.push(b' ');
        string(out, &text_value(v, utf8), utf8);
    }
    out.push(b')');
}

fn extension(out: &mut Vec<u8>, p: &MessagePart<'_>, utf8: bool) {
    out.push(b' ');
    match p.content_disposition() {
        Some(d) => {
            out.push(b'(');
            string(out, d.ctype().as_bytes(), utf8);
            out.push(b' ');
            params(out, d.attributes().collect(), utf8);
            out.push(b')');
        }
        None => out.extend_from_slice(b"NIL"),
    }
    out.extend_from_slice(b" NIL ");
    let loc = p.content_location().map(|l| text_value(l, utf8));
    nstring(out, loc.as_deref(), utf8);
}

/// Deepest BODYSTRUCTURE nesting written; deeper parts are shown as opaque.
const MAX_BODY_DEPTH: usize = 32;

fn body(out: &mut Vec<u8>, p: &MessagePart<'_>, ext: bool, utf8: bool, depth: usize) {
    if p.is_multipart() && depth < MAX_BODY_DEPTH {
        out.push(b'(');
        let mut any = false;
        for c in p.children() {
            body(out, &c, ext, utf8, depth + 1);
            any = true;
        }
        if !any {
            out.extend_from_slice(
                b"(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"US-ASCII\") NIL NIL \"7BIT\" 0 0)",
            );
        }
        out.push(b' ');
        let sub = p
            .content_type()
            .and_then(|c| c.subtype())
            .unwrap_or("mixed")
            .to_ascii_uppercase();
        string(out, sub.as_bytes(), utf8);
        if ext {
            out.push(b' ');
            params(
                out,
                p.content_type()
                    .map(|c| c.attributes().collect())
                    .unwrap_or_default(),
                utf8,
            );
            extension(out, p, utf8);
        }
        out.push(b')');
        return;
    }
    let (mut ctype, mut sub) = match p.content_type() {
        Some(c) => (
            c.ctype().to_ascii_uppercase(),
            c.subtype().unwrap_or("").to_ascii_uppercase(),
        ),
        None if p.is_message() => ("MESSAGE".into(), "RFC822".into()),
        None => ("TEXT".into(), "PLAIN".into()),
    };
    let nested = p.nested().filter(|_| depth < MAX_BODY_DEPTH);
    let is_message = nested.is_some();
    if !is_message && ctype == "MESSAGE" && (sub == "RFC822" || sub == "GLOBAL") {
        ctype = "APPLICATION".into();
        sub = "OCTET-STREAM".into();
    }
    if sub.is_empty() || p.is_multipart() {
        ctype = "APPLICATION".into();
        sub = "OCTET-STREAM".into();
    }
    out.push(b'(');
    string(out, ctype.as_bytes(), utf8);
    out.push(b' ');
    string(out, sub.as_bytes(), utf8);
    out.push(b' ');
    let mut attrs: Vec<(&str, &str)> = p
        .content_type()
        .map(|c| c.attributes().collect())
        .unwrap_or_default();
    if attrs.is_empty() && ctype == "TEXT" {
        attrs.push(("charset", "us-ascii"));
    }
    params(out, attrs, utf8);
    out.push(b' ');
    let id = p.headers().get("Content-ID").map(|h| unfold(h.raw_value()));
    nstring(out, id.as_deref(), utf8);
    out.push(b' ');
    let desc = p
        .headers()
        .get("Content-Description")
        .map(|h| header_text(h.raw_value(), utf8));
    nstring(out, desc.as_deref(), utf8);
    out.push(b' ');
    let enc = p
        .content_transfer_encoding()
        .map(|e| e.trim().to_ascii_uppercase())
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| "7BIT".into());
    string(out, enc.as_bytes(), utf8);
    let raw_body = p.raw_body();
    out.extend_from_slice(format!(" {}", raw_body.len()).as_bytes());
    if let Some(m) = nested {
        out.push(b' ');
        envelope(out, m.headers(), utf8);
        out.push(b' ');
        body(out, &m.root_part(), ext, utf8, depth + 1);
        out.extend_from_slice(format!(" {}", lines(raw_body)).as_bytes());
    } else if ctype == "TEXT" {
        out.extend_from_slice(format!(" {}", lines(raw_body)).as_bytes());
    }
    if ext {
        out.extend_from_slice(b" NIL");
        extension(out, p, utf8);
    }
    out.push(b')');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(raw: &[u8]) -> MessageView<'_> {
        MessageView::parse(raw, &ParseLimits::default())
    }

    fn env(raw: &[u8], utf8: bool) -> String {
        let mut o = Vec::new();
        view(raw).envelope(&mut o, utf8);
        String::from_utf8(o).unwrap()
    }

    fn bs(raw: &[u8], ext: bool) -> String {
        let mut o = Vec::new();
        view(raw).body_structure(&mut o, ext, false);
        String::from_utf8(o).unwrap()
    }

    fn sec(raw: &[u8], part: &[u32], text: SectionText) -> Option<String> {
        view(raw)
            .section(&Section {
                part: part.to_vec(),
                text,
            })
            .map(|b| String::from_utf8(b.into_owned()).unwrap())
    }

    const SIMPLE: &[u8] = b"Date: Mon, 2 Feb 2026 10:00:00 +0000\r\nFrom: Shop <shop@example.com>\r\nTo: a@example.test, \"B, Jr\" <b@example.test>\r\nSubject: =?utf-8?q?Gr=C3=BC=C3=9Fe?=\r\nMessage-ID: <m1@example.com>\r\n\r\nHello\r\nworld\r\n";

    #[test]
    fn envelope_of_a_simple_message() {
        assert_eq!(
            env(SIMPLE, false),
            "(\"Mon, 2 Feb 2026 10:00:00 +0000\" \"=?utf-8?q?Gr=C3=BC=C3=9Fe?=\" ((\"Shop\" NIL \"shop\" \"example.com\")) ((\"Shop\" NIL \"shop\" \"example.com\")) ((\"Shop\" NIL \"shop\" \"example.com\")) ((NIL NIL \"a\" \"example.test\")(\"B, Jr\" NIL \"b\" \"example.test\")) NIL NIL NIL \"<m1@example.com>\")"
        );
        assert_eq!(
            bs(SIMPLE, true),
            "(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"us-ascii\") NIL NIL \"7BIT\" 14 2 NIL NIL NIL NIL)"
        );
        assert_eq!(
            bs(SIMPLE, false),
            "(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"us-ascii\") NIL NIL \"7BIT\" 14 2)"
        );
    }

    #[test]
    fn encoded_names_and_raw_utf8_headers() {
        let raw = "From: =?utf-8?b?SsO8cmdlbg==?= <j@example.com>\r\nSubject: Grüße\r\nSender: Group: x@example.com, y@example.com;\r\n\r\nb\r\n".as_bytes();
        let e = env(raw, false);
        assert!(
            e.contains("((\"=?utf-8?b?SsO8cmdlbg==?=\" NIL \"j\" \"example.com\"))"),
            "{e}"
        );
        assert!(e.contains("\"=?utf-8?b?R3LDvMOfZQ==?=\""), "{e}");
        assert!(
            e.contains("((NIL NIL \"Group\" NIL)(NIL NIL \"x\" \"example.com\")(NIL NIL \"y\" \"example.com\")(NIL NIL NIL NIL))"),
            "{e}"
        );
        let e = env(raw, true);
        assert!(e.contains("\"Jürgen\""), "{e}");
        assert!(e.contains("\"Grüße\""), "{e}");
    }

    const ALT: &[u8] = b"From: a@example.com\r\nSubject: alt\r\nContent-Type: multipart/mixed; boundary=\"outer\"\r\n\r\npreamble\r\n--outer\r\nContent-Type: multipart/alternative; boundary=inner\r\n\r\n--inner\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nHi =C3=A4\r\n--inner\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Hi</p>\r\n--inner--\r\n--outer\r\nContent-Type: application/pdf; name=\"inv.pdf\"\r\nContent-Disposition: attachment; filename=\"inv.pdf\"\r\nContent-Transfer-Encoding: base64\r\nContent-ID: <pdf1>\r\n\r\nJVBERi0x\r\n--outer--\r\n";

    #[test]
    fn multipart_alternative_with_an_attachment() {
        let s = bs(ALT, true);
        assert_eq!(
            s,
            "(((\"TEXT\" \"PLAIN\" (\"CHARSET\" \"utf-8\") NIL NIL \"QUOTED-PRINTABLE\" 9 0 NIL NIL NIL NIL)(\"TEXT\" \"HTML\" (\"CHARSET\" \"utf-8\") NIL NIL \"7BIT\" 9 0 NIL NIL NIL NIL) \"ALTERNATIVE\" (\"BOUNDARY\" \"inner\") NIL NIL NIL)(\"APPLICATION\" \"PDF\" (\"NAME\" \"inv.pdf\") \"<pdf1>\" NIL \"BASE64\" 8 NIL (\"attachment\" (\"FILENAME\" \"inv.pdf\")) NIL NIL) \"MIXED\" (\"BOUNDARY\" \"outer\") NIL NIL NIL)"
        );
        let b = bs(ALT, false);
        assert!(
            b.starts_with(
                "(((\"TEXT\" \"PLAIN\" (\"CHARSET\" \"utf-8\") NIL NIL \"QUOTED-PRINTABLE\" 9 0)"
            ),
            "{b}"
        );
        assert!(b.ends_with(" \"MIXED\")"), "{b}");
        assert_eq!(sec(ALT, &[1, 1], SectionText::Full).unwrap(), "Hi =C3=A4");
        assert_eq!(sec(ALT, &[1, 2], SectionText::Full).unwrap(), "<p>Hi</p>");
        assert_eq!(sec(ALT, &[2], SectionText::Full).unwrap(), "JVBERi0x");
        let mime = sec(ALT, &[2], SectionText::Mime).unwrap();
        assert!(mime.starts_with("Content-Type: application/pdf"), "{mime}");
        assert!(mime.ends_with("<pdf1>\r\n\r\n"), "{mime}");
        assert_eq!(sec(ALT, &[3], SectionText::Full), None);
        assert_eq!(sec(ALT, &[1, 1, 1], SectionText::Full), None);
        assert_eq!(
            sec(ALT, &[2], SectionText::Header),
            None,
            "not a message part"
        );
        let h = sec(ALT, &[], SectionText::Header).unwrap();
        assert!(h.ends_with("outer\"\r\n\r\n"), "{h}");
        let t = sec(ALT, &[], SectionText::Text).unwrap();
        assert!(t.starts_with("preamble"), "{t}");
        assert_eq!(
            sec(
                ALT,
                &[],
                SectionText::HeaderFields(vec!["SUBJECT".into(), "FROM".into()])
            )
            .unwrap(),
            "From: a@example.com\r\nSubject: alt\r\n\r\n"
        );
        assert_eq!(
            sec(
                ALT,
                &[],
                SectionText::HeaderFieldsNot(vec!["CONTENT-TYPE".into()])
            )
            .unwrap(),
            "From: a@example.com\r\nSubject: alt\r\n\r\n"
        );
    }

    const NESTED: &[u8] = b"From: outer@example.com\r\nSubject: fwd\r\nContent-Type: multipart/mixed; boundary=b1\r\n\r\n--b1\r\nContent-Type: text/plain\r\n\r\nsee below\r\n--b1\r\nContent-Type: message/rfc822\r\n\r\nFrom: inner@example.com\r\nSubject: original\r\nContent-Type: multipart/alternative; boundary=b2\r\n\r\n--b2\r\nContent-Type: text/plain\r\n\r\ninner text\r\n--b2\r\nContent-Type: text/html\r\n\r\n<b>inner</b>\r\n--b2--\r\n--b1--\r\n";

    #[test]
    fn nested_message_rfc822() {
        let s = bs(NESTED, false);
        assert!(
            s.contains("(\"MESSAGE\" \"RFC822\" NIL NIL NIL \"7BIT\" "),
            "{s}"
        );
        assert!(
            s.contains("\"original\" ((NIL NIL \"inner\" \"example.com\"))"),
            "{s}"
        );
        assert!(s.contains(" \"ALTERNATIVE\") "), "{s}");
        let h = sec(NESTED, &[2], SectionText::Header).unwrap();
        assert!(h.starts_with("From: inner@example.com\r\n"), "{h}");
        assert!(h.ends_with("b2\r\n\r\n"), "{h}");
        let fields = sec(
            NESTED,
            &[2],
            SectionText::HeaderFields(vec!["SUBJECT".into()]),
        )
        .unwrap();
        assert_eq!(fields, "Subject: original\r\n\r\n");
        let t = sec(NESTED, &[2], SectionText::Text).unwrap();
        assert!(t.starts_with("--b2\r\n"), "{t}");
        assert_eq!(
            sec(NESTED, &[2, 1], SectionText::Full).unwrap(),
            "inner text"
        );
        assert_eq!(
            sec(NESTED, &[2, 2], SectionText::Full).unwrap(),
            "<b>inner</b>"
        );
        let whole = sec(NESTED, &[2], SectionText::Full).unwrap();
        assert!(whole.starts_with("From: inner@example.com"), "{whole}");
        assert_eq!(sec(NESTED, &[1], SectionText::Full).unwrap(), "see below");
    }

    #[test]
    fn single_part_numbering_and_unparsed_messages() {
        assert_eq!(
            sec(SIMPLE, &[1], SectionText::Full).unwrap(),
            "Hello\r\nworld\r\n"
        );
        assert_eq!(sec(SIMPLE, &[2], SectionText::Full), None);
        assert_eq!(sec(SIMPLE, &[1, 1], SectionText::Full), None);
        let raw = b"no header block at all";
        let v = MessageView::parse(
            raw,
            &ParseLimits {
                max_bytes: 4,
                ..Default::default()
            },
        );
        let mut o = Vec::new();
        v.envelope(&mut o, false);
        assert_eq!(o, b"(NIL NIL NIL NIL NIL NIL NIL NIL NIL NIL)");
        let mut o = Vec::new();
        v.body_structure(&mut o, true, false);
        assert!(
            String::from_utf8(o)
                .unwrap()
                .starts_with("(\"TEXT\" \"PLAIN\"")
        );
        assert_eq!(&*v.section(&Section::full()).unwrap(), &raw[..]);
    }

    #[test]
    fn header_helpers() {
        assert_eq!(header_end(b"A: 1\r\nB: 2\r\n\r\nbody"), 14);
        assert_eq!(header_end(b"A: 1\nB: 2\n\nbody"), 11);
        assert_eq!(header_end(b"A: 1\r\n"), 6);
        let f = header_fields(b"A: 1\r\nLong: x\r\n  y\r\nC: 3\r\n\r\nignored: 1\r\n");
        let names: Vec<&[u8]> = f.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec![&b"A"[..], b"Long", b"C"]);
        assert_eq!(f[1].1, b"Long: x\r\n  y\r\n");
        assert_eq!(
            select_fields(b"A: 1\r\nLong: x\r\n  y\r\n\r\n", &["LONG".into()], false),
            b"Long: x\r\n  y\r\n\r\n"
        );
    }
}
