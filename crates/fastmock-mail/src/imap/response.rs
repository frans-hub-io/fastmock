//! Writing IMAP response syntax: strings (quoted or literal), `NIL`, lists, dates and RFC 2047 words.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Utc};

/// Longest value sent as a quoted string; longer ones become literals.
const MAX_QUOTED: usize = 1024;

fn quotable(bytes: &[u8], utf8: bool) -> bool {
    bytes.len() <= MAX_QUOTED
        && !bytes.iter().any(|b| matches!(b, 0 | b'\r' | b'\n'))
        && (bytes.is_ascii() || (utf8 && std::str::from_utf8(bytes).is_ok()))
}

/// An IMAP `string`: quoted when possible (UTF-8 only after `ENABLE UTF8=ACCEPT`), else a literal.
pub fn string(out: &mut Vec<u8>, bytes: &[u8], utf8: bool) {
    if quotable(bytes, utf8) {
        out.push(b'"');
        for b in bytes {
            if matches!(b, b'"' | b'\\') {
                out.push(b'\\');
            }
            out.push(*b);
        }
        out.push(b'"');
    } else {
        literal(out, bytes);
    }
}

/// A literal `{n}CRLF` followed by the bytes.
pub fn literal(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(format!("{{{}}}\r\n", bytes.len()).as_bytes());
    out.extend_from_slice(bytes);
}

/// An `nstring`: `NIL` for `None`.
pub fn nstring(out: &mut Vec<u8>, bytes: Option<&[u8]>, utf8: bool) {
    match bytes {
        Some(b) => string(out, b, utf8),
        None => out.extend_from_slice(b"NIL"),
    }
}

/// Text for clients: as is when ASCII (or UTF-8 is enabled), else RFC 2047 encoded words.
pub fn text_value(s: &str, utf8: bool) -> Vec<u8> {
    if s.is_ascii() || utf8 {
        return s.as_bytes().to_vec();
    }
    let mut words = Vec::new();
    let mut chunk = String::new();
    for c in s.chars() {
        if chunk.len() + c.len_utf8() > 45 {
            words.push(format!("=?utf-8?b?{}?=", B64.encode(chunk.as_bytes())));
            chunk.clear();
        }
        chunk.push(c);
    }
    if !chunk.is_empty() {
        words.push(format!("=?utf-8?b?{}?=", B64.encode(chunk.as_bytes())));
    }
    words.join(" ").into_bytes()
}

/// A header value as written, unfolded and trimmed.
pub fn unfold(raw: &[u8]) -> Vec<u8> {
    let v: Vec<u8> = raw
        .iter()
        .copied()
        .filter(|b| *b != b'\r' && *b != b'\n')
        .collect();
    let start = v
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(v.len());
    let end = v
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |e| e + 1);
    v[start..end].to_vec()
}

/// A header value for ENVELOPE: unfolded; 8-bit UTF-8 becomes encoded words without UTF8=ACCEPT.
pub fn header_text(raw: &[u8], utf8: bool) -> Vec<u8> {
    let v = unfold(raw);
    match std::str::from_utf8(&v) {
        Ok(s) => text_value(s, utf8),
        Err(_) => v,
    }
}

/// INTERNALDATE: `"21-Nov-2026 09:05:00 +0000"`.
pub fn internal_date(at: DateTime<Utc>) -> String {
    format!("\"{}\"", at.format("%d-%b-%Y %H:%M:%S +0000"))
}

/// A parenthesised list of flags.
pub fn flag_list(flags: &[String]) -> String {
    format!("({})", flags.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(b: &[u8], utf8: bool) -> String {
        let mut o = Vec::new();
        string(&mut o, b, utf8);
        String::from_utf8_lossy(&o).into_owned()
    }

    #[test]
    fn strings_are_quoted_or_literal() {
        assert_eq!(s(b"hello", false), "\"hello\"");
        assert_eq!(s(b"a\"b\\c", false), "\"a\\\"b\\\\c\"");
        assert_eq!(s(b"a\r\nb", false), "{4}\r\na\r\nb");
        assert_eq!(s("é".as_bytes(), false), "{2}\r\né");
        assert_eq!(s("é".as_bytes(), true), "\"é\"");
        assert!(s(&[b'x'; 2000], false).starts_with("{2000}\r\n"));
        let mut o = Vec::new();
        nstring(&mut o, None, false);
        assert_eq!(o, b"NIL");
    }

    #[test]
    fn non_ascii_text_is_encoded_without_utf8() {
        assert_eq!(text_value("Plain", false), b"Plain");
        assert_eq!(
            String::from_utf8(text_value("Grüße", false)).unwrap(),
            "=?utf-8?b?R3LDvMOfZQ==?="
        );
        assert_eq!(text_value("Grüße", true), "Grüße".as_bytes());
        let long = "ä".repeat(60);
        let enc = String::from_utf8(text_value(&long, false)).unwrap();
        assert_eq!(enc.matches("=?utf-8?b?").count(), 3);
        assert!(enc.split(' ').all(|w| w.len() <= 75));
    }

    #[test]
    fn unfolding_and_dates() {
        assert_eq!(unfold(b" a\r\n b \r\n"), b"a b");
        assert_eq!(unfold(b"  \r\n"), b"");
        let at = DateTime::parse_from_rfc3339("2026-02-03T04:05:06Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(internal_date(at), "\"03-Feb-2026 04:05:06 +0000\"");
        assert_eq!(flag_list(&["\\Seen".into(), "$x".into()]), "(\\Seen $x)");
    }
}
