//! The search syntax of `GET /__admin/mail/messages?query=` and the UI search box:
//! `from:`, `to:`, `cc:`, `subject:`, `has:attachment`, `label:`, `after:`, `before:` and free text, with
//! values in double quotes when they hold spaces (`subject:"monthly invoice"`). All terms must hold.
//! A `key:value` with another key is free text, so URLs can be searched for.

use chrono::{DateTime, NaiveDate, Utc};
use fastmock_core::mail::{MailFilter, MailTerm};

use crate::labels::normalize_label;

pub const MAX_QUERY_BYTES: usize = 1024;
pub const MAX_TERMS: usize = 32;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct QueryError(pub String);

fn err<T>(msg: impl Into<String>) -> Result<T, QueryError> {
    Err(QueryError(msg.into()))
}

/// `YYYY-MM-DD` (midnight UTC) or an RFC 3339 time.
fn parse_time(v: &str) -> Option<DateTime<Utc>> {
    if let Ok(d) = NaiveDate::parse_from_str(v, "%Y-%m-%d") {
        return d.and_hms_opt(0, 0, 0).map(|t| t.and_utc());
    }
    DateTime::parse_from_rfc3339(v)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Splits into tokens at unquoted white space; quotes are removed. Each token is `(key, value)` where
/// `key` is set for `key:value` tokens whose key is written before any quote.
fn tokens(q: &str) -> Result<Vec<(Option<String>, String)>, QueryError> {
    let mut out = Vec::new();
    let mut chars = q.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            return Ok(out);
        }
        let mut key: Option<String> = None;
        let mut cur = String::new();
        let mut quoted_any = false;
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() {
                break;
            }
            chars.next();
            match c {
                '"' => {
                    quoted_any = true;
                    let mut closed = false;
                    for c in chars.by_ref() {
                        if c == '"' {
                            closed = true;
                            break;
                        }
                        cur.push(c);
                    }
                    if !closed {
                        return err("unterminated quote in query");
                    }
                }
                ':' if key.is_none() && !quoted_any && !cur.is_empty() => {
                    key = Some(std::mem::take(&mut cur).to_ascii_lowercase());
                }
                _ => cur.push(c),
            }
        }
        out.push((key, cur));
    }
}

/// Parses a search; an empty or blank query selects every message.
pub fn parse_query(q: &str) -> Result<MailFilter, QueryError> {
    if q.len() > MAX_QUERY_BYTES {
        return err(format!("the query is limited to {MAX_QUERY_BYTES} bytes"));
    }
    let mut terms = Vec::new();
    for (key, value) in tokens(q)? {
        let lower = value.to_lowercase();
        let need = |what: &str| -> Result<String, QueryError> {
            if lower.trim().is_empty() {
                err(format!("{what}: needs a value"))
            } else {
                Ok(lower.clone())
            }
        };
        let term = match key.as_deref() {
            Some("from") => MailTerm::From(need("from")?),
            Some("to") => MailTerm::To(need("to")?),
            Some("cc") => MailTerm::Cc(need("cc")?),
            Some("subject") => MailTerm::Subject(need("subject")?),
            Some("has") => match lower.as_str() {
                "attachment" | "attachments" => MailTerm::HasAttachment,
                other => return err(format!("has:{other} is not supported; use has:attachment")),
            },
            Some("label") => MailTerm::Label(
                normalize_label(&value)
                    .ok_or_else(|| QueryError(format!("label:{value} is not a valid label")))?,
            ),
            Some(k @ ("after" | "before")) => {
                let at = parse_time(value.trim()).ok_or_else(|| {
                    QueryError(format!(
                        "{k}:{value} is not a date: use YYYY-MM-DD or an RFC 3339 time"
                    ))
                })?;
                if k == "after" {
                    MailTerm::After(at)
                } else {
                    MailTerm::Before(at)
                }
            }
            Some(other) => MailTerm::Text(format!("{other}:{lower}")),
            None if lower.is_empty() => continue,
            None => MailTerm::Text(lower),
        };
        terms.push(term);
        if terms.len() > MAX_TERMS {
            return err(format!("at most {MAX_TERMS} terms"));
        }
    }
    Ok(MailFilter { terms })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_syntax() {
        let f = parse_query(
            r#"from:Billing to:"Alice Smith" cc:carol subject:"Monthly Invoice" has:attachment label:OTP after:2026-11-01 before:2026-11-20T12:00:00+02:00 Pay  "two words" https://example.com/x"#,
        )
        .unwrap();
        assert_eq!(
            f.terms,
            vec![
                MailTerm::From("billing".into()),
                MailTerm::To("alice smith".into()),
                MailTerm::Cc("carol".into()),
                MailTerm::Subject("monthly invoice".into()),
                MailTerm::HasAttachment,
                MailTerm::Label("otp".into()),
                MailTerm::After(parse_time("2026-11-01T00:00:00Z").unwrap()),
                MailTerm::Before(parse_time("2026-11-20T10:00:00Z").unwrap()),
                MailTerm::Text("pay".into()),
                MailTerm::Text("two words".into()),
                MailTerm::Text("https://example.com/x".into()),
            ]
        );
        assert!(parse_query("").unwrap().is_empty());
        assert!(parse_query("   \"\"  ").unwrap().is_empty());
        assert_eq!(
            parse_query("\"a:b\"").unwrap().terms,
            vec![MailTerm::Text("a:b".into())]
        );
        for (bad, why) in [
            ("from:", "needs a value"),
            ("subject:\"open", "unterminated"),
            ("has:pictures", "has:attachment"),
            ("label:\"a b\"", "not a valid label"),
            ("after:yesterday", "YYYY-MM-DD"),
        ] {
            let e = parse_query(bad).unwrap_err().to_string();
            assert!(e.contains(why), "{bad}: {e}");
        }
        assert!(parse_query(&"x ".repeat(40)).is_err());
        assert!(parse_query(&"x".repeat(2000)).is_err());
    }
}
