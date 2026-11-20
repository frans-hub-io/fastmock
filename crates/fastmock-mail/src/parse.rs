//! MIME parsing with mail-parser: the summary stored with a message and the detailed view of one
//! message. Parsing is bounded: messages above [`ParseLimits::max_bytes`] are kept raw, and nesting and
//! part counts are lower than mail-parser's defaults.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Utc};
use fastmock_core::mail::{MailAddress, MailSummary};
use mail_parser::{AddressList, HeaderForm, Message, MessageParser, MessagePart};
use serde::Serialize;

/// Characters of [`MailSummary::preview`].
pub const PREVIEW_CHARS: usize = 200;
/// Bytes of subject and text kept for free-text search.
pub const SEARCH_TEXT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseLimits {
    /// Larger messages are stored without parsing (`summary.unparsed`).
    pub max_bytes: usize,
    pub max_depth: usize,
    pub max_parts: usize,
}

impl Default for ParseLimits {
    fn default() -> Self {
        Self {
            max_bytes: 10 * 1024 * 1024,
            max_depth: 16,
            max_parts: 500,
        }
    }
}

impl ParseLimits {
    pub fn parser(&self) -> MessageParser {
        MessageParser::new()
            .max_depth(self.max_depth)
            .max_parts(self.max_parts)
    }
}

/// A header field as written in the message, its value decoded (RFC 2047) and unfolded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct MailHeader {
    #[schema(example = "Subject")]
    pub name: String,
    #[schema(example = "Your invoice")]
    pub value: String,
}

/// An attachment or inline part.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MailAttachment {
    /// For `/__admin/mail/messages/{id}/parts/{partId}`.
    #[schema(example = "3")]
    pub part_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "invoice.pdf")]
    pub name: Option<String>,
    #[schema(example = "application/pdf")]
    pub content_type: String,
    /// Decoded size in bytes.
    pub size: u64,
    /// `Content-ID` without angle brackets, for `cid:` references.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
    /// Marked to be shown inside the message.
    pub inline: bool,
}

/// The parsed parts of `GET /__admin/mail/messages/{id}`; the envelope, labels and source come from the
/// stored message.
#[derive(Clone, Debug, Default, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MailDetail {
    /// Header fields of the message in order.
    pub headers: Vec<MailHeader>,
    pub from: Vec<MailAddress>,
    pub to: Vec<MailAddress>,
    pub cc: Vec<MailAddress>,
    pub bcc: Vec<MailAddress>,
    pub reply_to: Vec<MailAddress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// The text body (HTML converted when the message has no text part).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The message has an HTML body, served by `/html`.
    pub has_html: bool,
    pub attachments: Vec<MailAttachment>,
    /// The message could not be parsed; only the raw form is available.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unparsed: bool,
}

/// The decoded content of one part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartContent {
    pub content_type: String,
    pub name: Option<String>,
    pub bytes: Vec<u8>,
}

/// A message parsed within [`ParseLimits`]; it borrows the raw bytes.
pub struct ParsedMail<'a> {
    msg: Option<Message<'a>>,
    size: usize,
}

fn addresses(list: Option<AddressList<'_>>) -> Vec<MailAddress> {
    list.map(|l| {
        l.mailboxes()
            .filter_map(|m| {
                m.address().map(|a| MailAddress {
                    name: m.name().filter(|n| !n.is_empty()).map(String::from),
                    address: a.to_string(),
                })
            })
            .collect()
    })
    .unwrap_or_default()
}

fn content_type(p: &MessagePart<'_>) -> String {
    match p.content_type() {
        Some(ct) => match ct.subtype() {
            Some(sub) => format!("{}/{}", ct.ctype(), sub),
            None => ct.ctype().to_string(),
        },
        None if p.is_message() => "message/rfc822".into(),
        None => "text/plain".into(),
    }
}

fn truncate_bytes(s: &mut String, max: usize) {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
}

fn collapse_spaces(s: &str, max_chars: usize) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

impl<'a> ParsedMail<'a> {
    pub fn parse(raw: &'a [u8], limits: &ParseLimits) -> Self {
        let msg = if raw.len() > limits.max_bytes {
            None
        } else {
            limits.parser().parse(raw)
        };
        Self {
            msg,
            size: raw.len(),
        }
    }

    pub fn is_parsed(&self) -> bool {
        self.msg.is_some()
    }

    pub fn subject(&self) -> Option<String> {
        self.msg.as_ref()?.subject().map(String::from)
    }

    pub fn from(&self) -> Vec<MailAddress> {
        self.msg
            .as_ref()
            .map(|m| addresses(m.from()))
            .unwrap_or_default()
    }

    pub fn to(&self) -> Vec<MailAddress> {
        self.msg
            .as_ref()
            .map(|m| addresses(m.to()))
            .unwrap_or_default()
    }

    pub fn cc(&self) -> Vec<MailAddress> {
        self.msg
            .as_ref()
            .map(|m| addresses(m.cc()))
            .unwrap_or_default()
    }

    /// Addresses of To, Cc and Bcc, lowercase, for messages that come without an envelope.
    pub fn recipients(&self) -> Vec<String> {
        let Some(m) = &self.msg else {
            return vec![];
        };
        let mut v: Vec<String> = [m.to(), m.cc(), m.bcc()]
            .into_iter()
            .flat_map(addresses)
            .map(|a| a.address.to_lowercase())
            .collect();
        v.dedup();
        v
    }

    pub fn date(&self) -> Option<DateTime<Utc>> {
        let d = self.msg.as_ref()?.date()?;
        d.is_valid()
            .then(|| DateTime::from_timestamp(d.to_timestamp(), 0))
            .flatten()
    }

    /// Header fields in order, values decoded.
    pub fn headers(&self) -> Vec<MailHeader> {
        let Some(m) = &self.msg else {
            return vec![];
        };
        m.headers()
            .iter()
            .map(|h| {
                let parsed = h.parse_as(HeaderForm::Text);
                let value = parsed
                    .value()
                    .as_text()
                    .map(str::to_string)
                    .unwrap_or_else(|| String::from_utf8_lossy(h.raw_value()).trim().to_string());
                MailHeader {
                    name: h.raw_name().to_string(),
                    value,
                }
            })
            .collect()
    }

    /// Decoded values of the header fields with this name, in order.
    pub fn header_values(&self, name: &str) -> Vec<String> {
        let Some(m) = &self.msg else {
            return vec![];
        };
        m.headers()
            .all(name)
            .map(|h| {
                let parsed = h.parse_as(HeaderForm::Text);
                parsed.value().as_text().unwrap_or_default().to_string()
            })
            .collect()
    }

    /// The text body, HTML converted when the message has no text part.
    pub fn text(&self) -> Option<String> {
        let m = self.msg.as_ref()?;
        let parts: Vec<String> = m.text_bodies().map(|t| t.into_owned()).collect();
        (!parts.is_empty()).then(|| parts.join("\n"))
    }

    /// The HTML parts of the HTML body, as written (no conversion of text parts).
    pub fn html(&self) -> Option<String> {
        let m = self.msg.as_ref()?;
        let parts: Vec<String> = m
            .html_body()
            .filter(|p| p.is_content_type("text", "html"))
            .filter_map(|p| p.text().map(|t| t.into_owned()))
            .collect();
        (!parts.is_empty()).then(|| parts.join("\n"))
    }

    pub fn attachments(&self) -> Vec<MailAttachment> {
        let Some(m) = &self.msg else {
            return vec![];
        };
        m.attachments()
            .map(|p| MailAttachment {
                part_id: p.id().to_string(),
                name: p.attachment_name().map(String::from),
                content_type: content_type(&p),
                size: p.decoded_len() as u64,
                content_id: p.content_id().map(String::from),
                inline: p.is_inline(),
            })
            .collect()
    }

    /// The decoded content of a leaf part (`partId` of [`MailAttachment`]).
    pub fn part(&self, part_id: &str) -> Option<PartContent> {
        let id: u32 = part_id.parse().ok()?;
        let p = self.msg.as_ref()?.part(id)?;
        if p.is_multipart() {
            return None;
        }
        Some(PartContent {
            content_type: content_type(&p),
            name: p.attachment_name().map(String::from),
            bytes: p.decoded().into_owned(),
        })
    }

    /// The HTML body with `cid:` references to parts of the message replaced by `data:` URIs, as long as
    /// the inlined parts stay within `max_inline_bytes` in total; other references are left as they are.
    pub fn html_with_inline_parts(&self, max_inline_bytes: usize) -> Option<String> {
        let html = self.html()?;
        let m = self.msg.as_ref()?;
        let mut budget = max_inline_bytes;
        let mut out = html;
        for p in m.parts() {
            let Some(cid) = p.content_id() else {
                continue;
            };
            let needle = format!("cid:{cid}");
            if !out.contains(&needle) || p.is_multipart() {
                continue;
            }
            let bytes = p.decoded();
            let encoded = B64.encode(&bytes);
            if encoded.len() > budget {
                continue;
            }
            budget -= encoded.len();
            let uri = format!("data:{};base64,{encoded}", content_type(&p));
            out = out.replace(&needle, &uri);
        }
        Some(out)
    }

    /// Lowercase subject and text for free-text search, at most [`SEARCH_TEXT_BYTES`].
    pub fn search_text(&self) -> String {
        let mut s = self.subject().unwrap_or_default();
        if let Some(t) = self.text() {
            s.push('\n');
            s.push_str(&t);
        }
        truncate_bytes(&mut s, SEARCH_TEXT_BYTES);
        s.to_lowercase()
    }

    pub fn summary(&self) -> MailSummary {
        let Some(m) = &self.msg else {
            return MailSummary {
                size: self.size as u64,
                unparsed: true,
                ..Default::default()
            };
        };
        let attachments = self.attachments();
        MailSummary {
            subject: self.subject(),
            from: self.from(),
            to: self.to(),
            cc: self.cc(),
            date: self.date(),
            message_id: m.message_id().map(String::from),
            size: self.size as u64,
            has_attachments: m.has_attachments(),
            attachment_names: attachments.iter().filter_map(|a| a.name.clone()).collect(),
            has_text: m.text_body().any(|p| p.is_content_type("text", "plain")),
            has_html: m.html_body().any(|p| p.is_content_type("text", "html")),
            preview: m
                .body_preview(PREVIEW_CHARS * 4)
                .map(|p| collapse_spaces(&p, PREVIEW_CHARS))
                .unwrap_or_default(),
            unparsed: false,
        }
    }

    pub fn detail(&self) -> MailDetail {
        let Some(m) = &self.msg else {
            return MailDetail {
                unparsed: true,
                ..Default::default()
            };
        };
        MailDetail {
            headers: self.headers(),
            from: self.from(),
            to: self.to(),
            cc: self.cc(),
            bcc: addresses(m.bcc()),
            reply_to: addresses(m.reply_to()),
            subject: self.subject(),
            date: self.date(),
            message_id: m.message_id().map(String::from),
            text: self.text(),
            has_html: m.html_body().any(|p| p.is_content_type("text", "html")),
            attachments: self.attachments(),
            unparsed: false,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const MULTIPART: &str = "From: \"Billing Team\" <billing@example.com>\r\n\
To: Alice <alice@example.test>, bob@example.test\r\n\
Cc: carol@example.test\r\n\
Subject: =?UTF-8?B?w4RyZ2VyIGludm9pY2U=?=\r\n\
Date: Fri, 20 Nov 2026 10:00:00 +0000\r\n\
Message-ID: <abc@example.com>\r\n\
X-FastMock-Labels: otp\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"outer\"\r\n\
\r\n\
--outer\r\n\
Content-Type: multipart/related; boundary=\"rel\"\r\n\
\r\n\
--rel\r\n\
Content-Type: multipart/alternative; boundary=\"alt\"\r\n\
\r\n\
--alt\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Your code is 123456. Visit https://example.com/verify?t=1.\r\n\
--alt\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<p>Your code is <b>123456</b>. <a href=\"https://example.com/verify?t=1&amp;x=2\">Verify</a><img src=\"cid:logo@x\"></p>\r\n\
--alt--\r\n\
--rel\r\n\
Content-Type: image/png\r\n\
Content-ID: <logo@x>\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
iVBORw0K\r\n\
--rel--\r\n\
--outer\r\n\
Content-Type: application/pdf; name=\"invoice.pdf\"\r\n\
Content-Disposition: attachment; filename=\"invoice.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
JVBERi0x\r\n\
--outer--\r\n";

    #[test]
    fn summary_and_detail_of_a_multipart_message() {
        let p = ParsedMail::parse(MULTIPART.as_bytes(), &ParseLimits::default());
        assert!(p.is_parsed());
        let s = p.summary();
        assert_eq!(s.subject.as_deref(), Some("Ärger invoice"));
        assert_eq!(s.from[0].name.as_deref(), Some("Billing Team"));
        assert_eq!(s.from[0].address, "billing@example.com");
        assert_eq!(s.to.len(), 2);
        assert_eq!(s.cc[0].address, "carol@example.test");
        assert_eq!(
            s.date,
            DateTime::parse_from_rfc3339("2026-11-20T10:00:00Z")
                .ok()
                .map(|d| d.with_timezone(&Utc))
        );
        assert_eq!(s.message_id.as_deref(), Some("abc@example.com"));
        assert_eq!(s.size, MULTIPART.len() as u64);
        assert!(s.has_attachments && s.has_text && s.has_html && !s.unparsed);
        assert_eq!(s.attachment_names, vec!["invoice.pdf"]);
        assert!(
            s.preview.starts_with("Your code is 123456."),
            "{}",
            s.preview
        );
        assert!(
            p.search_text()
                .contains("ärger invoice\nyour code is 123456")
        );
        assert_eq!(p.header_values("x-fastmock-labels"), vec!["otp"]);
        assert_eq!(
            p.recipients(),
            vec![
                "alice@example.test",
                "bob@example.test",
                "carol@example.test"
            ]
        );

        let d = p.detail();
        assert_eq!(d.headers[0].name, "From");
        assert_eq!(d.headers[3].value, "Ärger invoice");
        assert!(d.has_html);
        assert!(d.text.as_deref().unwrap().contains("123456"));
        let pdf = d
            .attachments
            .iter()
            .find(|a| a.name.as_deref() == Some("invoice.pdf"))
            .unwrap();
        assert_eq!(pdf.content_type, "application/pdf");
        assert!(!pdf.inline);
        let part = p.part(&pdf.part_id).unwrap();
        assert_eq!(part.bytes, b"%PDF-1");
        assert!(p.part("999").is_none());
        assert!(p.part("0").is_none());
        let html = p.html_with_inline_parts(1024).unwrap();
        assert!(
            html.contains("src=\"data:image/png;base64,iVBORw0K\""),
            "{html}"
        );
        assert!(p.html_with_inline_parts(4).unwrap().contains("cid:logo@x"));
    }

    #[test]
    fn oversized_or_headerless_input_stays_raw() {
        let limits = ParseLimits {
            max_bytes: 10,
            ..Default::default()
        };
        let p = ParsedMail::parse(MULTIPART.as_bytes(), &limits);
        assert!(!p.is_parsed());
        let s = p.summary();
        assert!(s.unparsed && s.subject.is_none());
        assert_eq!(s.size, MULTIPART.len() as u64);
        assert!(p.detail().unparsed);
        assert!(p.text().is_none() && p.search_text().is_empty());
    }

    #[test]
    fn deep_nesting_is_bounded() {
        let mut raw = String::new();
        for i in 0..40 {
            raw.push_str(&format!(
                "Content-Type: multipart/mixed; boundary=\"b{i}\"\r\n\r\n--b{i}\r\n"
            ));
        }
        raw.push_str("Content-Type: text/plain\r\n\r\ndeep\r\n");
        let p = ParsedMail::parse(raw.as_bytes(), &ParseLimits::default());
        assert!(p.is_parsed());
        let s = p.summary();
        assert!(!s.unparsed);
    }
}
