//! MIME building with mail-builder, for messages injected as JSON and for provider APIs.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address;
use mail_builder::headers::raw::Raw;
use mail_builder::headers::text::Text;
use serde::Deserialize;

const MAX_HEADERS: usize = 64;
const MAX_ATTACHMENTS: usize = 100;
/// Headers that the fields set or that describe the MIME structure.
const RESERVED: [&str; 8] = [
    "from",
    "to",
    "cc",
    "subject",
    "mime-version",
    "content-type",
    "content-transfer-encoding",
    "content-disposition",
];

/// A message to build (`POST /__admin/mail/messages` with JSON).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ComposeRequest {
    /// `addr@example.com` or `Name <addr@example.com>`.
    #[schema(example = "Shop <shop@example.com>")]
    pub from: String,
    #[schema(example = json!(["alice@example.test"]))]
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub html: Option<String>,
    /// Further header fields; `Message-ID` and `Date` replace the generated ones.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub attachments: Vec<ComposeAttachment>,
    /// Mailbox (address) to file the message into instead of the recipients' INBOXes; read by the admin
    /// API, not part of the MIME message.
    #[serde(default)]
    pub mailbox: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ComposeAttachment {
    #[schema(example = "invoice.pdf")]
    pub name: String,
    /// Default `application/octet-stream`.
    #[serde(default)]
    #[schema(example = "application/pdf")]
    pub content_type: Option<String>,
    /// The content, base64.
    pub base64: String,
    /// Makes the part inline, referenced from the HTML as `cid:<contentId>`.
    #[serde(default)]
    pub content_id: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ComposeError {
    #[error("invalid address `{0}`")]
    Address(String),
    #[error("{0}")]
    Invalid(String),
    #[error("the message exceeds {0} bytes")]
    TooLarge(usize),
}

fn printable(s: &str) -> bool {
    !s.chars().any(|c| c.is_control() && c != '\t')
}

/// Parses `addr` or `Name <addr>`.
pub fn parse_address(raw: &str) -> Result<(Option<String>, String), ComposeError> {
    let bad = || ComposeError::Address(raw.chars().take(200).collect());
    let s = raw.trim();
    let (name, addr) = match (s.rfind('<'), s.ends_with('>')) {
        (Some(i), true) => {
            let name = s[..i].trim().trim_matches('"').trim();
            (
                (!name.is_empty()).then(|| name.to_string()),
                s[i + 1..s.len() - 1].trim(),
            )
        }
        _ => (None, s),
    };
    if !printable(s)
        || name.as_deref().is_some_and(|n| n.len() > 200)
        || addr.len() > 320
        || crate::address::split(addr).is_none()
        || addr
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '<' | '>' | ','))
    {
        return Err(bad());
    }
    Ok((name, addr.to_string()))
}

fn address_list(list: &[String]) -> Result<Address<'static>, ComposeError> {
    let items = list
        .iter()
        .map(|a| parse_address(a).map(|(n, e)| Address::new_address(n, e)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Address::new_list(items))
}

fn header_name_ok(n: &str) -> bool {
    !n.is_empty() && n.len() <= 76 && n.bytes().all(|b| (33..=126).contains(&b) && b != b':')
}

/// Builds the message; fails on invalid addresses or headers, bad base64 or a result above `max_bytes`.
pub fn compose(req: &ComposeRequest, max_bytes: usize) -> Result<Vec<u8>, ComposeError> {
    if req.to.is_empty() {
        return Err(ComposeError::Invalid(
            "to must name at least one recipient".into(),
        ));
    }
    if req.headers.len() > MAX_HEADERS {
        return Err(ComposeError::Invalid(format!(
            "at most {MAX_HEADERS} headers"
        )));
    }
    if req.attachments.len() > MAX_ATTACHMENTS {
        return Err(ComposeError::Invalid(format!(
            "at most {MAX_ATTACHMENTS} attachments"
        )));
    }
    let (from_name, from) = parse_address(&req.from)?;
    let mut b = MessageBuilder::new()
        .from(Address::new_address(from_name, from))
        .to(address_list(&req.to)?);
    if !req.cc.is_empty() {
        b = b.cc(address_list(&req.cc)?);
    }
    if let Some(s) = &req.subject {
        if !printable(s) {
            return Err(ComposeError::Invalid("subject must be one line".into()));
        }
        b = b.subject(Text::new(s.clone()));
    }
    let mut has_id = false;
    for (name, value) in &req.headers {
        let lower = name.to_ascii_lowercase();
        if !header_name_ok(name) || RESERVED.contains(&lower.as_str()) {
            return Err(ComposeError::Invalid(format!(
                "header `{name}` cannot be set: use a printable name other than {}",
                RESERVED.join(", ")
            )));
        }
        if !printable(value) || value.len() > 998 {
            return Err(ComposeError::Invalid(format!(
                "header `{name}` must be one line of at most 998 bytes"
            )));
        }
        let canonical = match lower.as_str() {
            "message-id" => {
                has_id = true;
                "Message-ID".to_string()
            }
            "date" => "Date".to_string(),
            _ => name.clone(),
        };
        b = b.header(canonical, Raw::new(value.clone()));
    }
    if !has_id {
        b = b.message_id(format!("{}@fastmock", uuid::Uuid::now_v7().simple()));
    }
    if let Some(t) = &req.text {
        b = b.text_body(t.clone());
    }
    if let Some(h) = &req.html {
        b = b.html_body(h.clone());
    }
    if req.text.is_none() && req.html.is_none() {
        b = b.text_body(String::new());
    }
    let mut total = 0usize;
    for a in &req.attachments {
        let bytes = B64
            .decode(a.base64.trim())
            .map_err(|e| ComposeError::Invalid(format!("attachment {}: {e}", a.name)))?;
        total += bytes.len();
        if total > max_bytes {
            return Err(ComposeError::TooLarge(max_bytes));
        }
        let ct = a
            .content_type
            .clone()
            .unwrap_or_else(|| "application/octet-stream".into());
        if !printable(&ct) || !ct.contains('/') || !printable(&a.name) {
            return Err(ComposeError::Invalid(format!(
                "attachment {}: invalid name or content type",
                a.name
            )));
        }
        b = match &a.content_id {
            Some(cid) => {
                if !printable(cid) || cid.contains(['<', '>', ' ']) {
                    return Err(ComposeError::Invalid(format!(
                        "attachment {}: invalid contentId",
                        a.name
                    )));
                }
                b.inline(ct, cid.clone(), bytes)
            }
            None => b.attachment(ct, a.name.clone(), bytes),
        };
    }
    let out = b
        .write_to_vec()
        .map_err(|e| ComposeError::Invalid(e.to_string()))?;
    if out.len() > max_bytes {
        return Err(ComposeError::TooLarge(max_bytes));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{ParseLimits, ParsedMail};

    fn req() -> ComposeRequest {
        ComposeRequest {
            from: "Shop <shop@example.com>".into(),
            to: vec![
                "alice@example.test".into(),
                "\"Bob B\" <bob@example.test>".into(),
            ],
            cc: vec!["carol@example.test".into()],
            subject: Some("Grüße".into()),
            text: Some("Code 4242".into()),
            html: Some("<p>Code <b>4242</b><img src=\"cid:logo\"></p>".into()),
            headers: BTreeMap::from([
                ("X-FastMock-Labels".into(), "welcome".into()),
                ("message-id".into(), "<fixed@example.com>".into()),
            ]),
            attachments: vec![
                ComposeAttachment {
                    name: "a.txt".into(),
                    content_type: Some("text/plain".into()),
                    base64: B64.encode("hello"),
                    content_id: None,
                },
                ComposeAttachment {
                    name: "logo.png".into(),
                    content_type: Some("image/png".into()),
                    base64: B64.encode([1u8, 2, 3]),
                    content_id: Some("logo".into()),
                },
            ],
            mailbox: None,
        }
    }

    #[test]
    fn builds_a_message_that_parses_back() {
        let raw = compose(&req(), 1 << 20).unwrap();
        let p = ParsedMail::parse(&raw, &ParseLimits::default());
        let s = p.summary();
        assert_eq!(s.subject.as_deref(), Some("Grüße"));
        assert_eq!(s.from[0].name.as_deref(), Some("Shop"));
        assert_eq!(s.to[1].name.as_deref(), Some("Bob B"));
        assert_eq!(s.cc[0].address, "carol@example.test");
        assert_eq!(s.message_id.as_deref(), Some("fixed@example.com"));
        assert!(s.has_text && s.has_html && s.has_attachments);
        assert_eq!(p.header_values("X-FastMock-Labels"), vec!["welcome"]);
        let a = p
            .attachments()
            .into_iter()
            .find(|a| a.name.as_deref() == Some("a.txt"))
            .unwrap();
        assert_eq!(p.part(&a.part_id).unwrap().bytes, b"hello");
        assert!(
            p.html_with_inline_parts(100)
                .unwrap()
                .contains("data:image/png;base64,AQID")
        );
        let generated = compose(
            &ComposeRequest {
                headers: BTreeMap::new(),
                attachments: vec![],
                html: None,
                text: None,
                ..req()
            },
            1 << 20,
        )
        .unwrap();
        let p = ParsedMail::parse(&generated, &ParseLimits::default());
        assert!(p.summary().message_id.unwrap().ends_with("@fastmock"));
    }

    #[test]
    fn refuses_injection_and_bad_input() {
        let with = |f: &dyn Fn(&mut ComposeRequest)| {
            let mut r = req();
            f(&mut r);
            compose(&r, 1 << 20).unwrap_err()
        };
        assert!(matches!(
            with(&|r| r.to = vec!["a@example.test\r\nBcc: x@example.com".into()]),
            ComposeError::Address(_)
        ));
        assert!(matches!(
            with(&|r| r.from = "nobody".into()),
            ComposeError::Address(_)
        ));
        assert!(matches!(with(&|r| r.to.clear()), ComposeError::Invalid(_)));
        assert!(matches!(
            with(&|r| r.subject = Some("a\r\nBcc: x@example.com".into())),
            ComposeError::Invalid(_)
        ));
        assert!(matches!(
            with(&|r| {
                r.headers
                    .insert("X-A".into(), "v\r\nBcc: x@example.com".into());
            }),
            ComposeError::Invalid(_)
        ));
        assert!(matches!(
            with(&|r| {
                r.headers.insert("Content-Type".into(), "text/html".into());
            }),
            ComposeError::Invalid(_)
        ));
        assert!(matches!(
            with(&|r| {
                r.headers.insert("Bad Name".into(), "v".into());
            }),
            ComposeError::Invalid(_)
        ));
        assert!(matches!(
            with(&|r| r.attachments[0].base64 = "***".into()),
            ComposeError::Invalid(_)
        ));
        assert_eq!(compose(&req(), 64).unwrap_err(), ComposeError::TooLarge(64));
        assert_eq!(
            parse_address(" Ann  <ann@example.com> ").unwrap(),
            (Some("Ann".into()), "ann@example.com".into())
        );
    }
}
