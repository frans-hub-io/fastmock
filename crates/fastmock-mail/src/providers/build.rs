//! MIME building, address lists and template rendering shared by the provider APIs.

use std::collections::BTreeSet;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use fastmock_core::mail::ProviderExtras;
use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address;
use mail_builder::headers::raw::Raw;
use mail_builder::headers::text::Text;
use serde_json::Value;

use super::{Fail, OutMessage};
use crate::compose::parse_address;

/// Most header fields a provider message may set.
pub const MAX_HEADERS: usize = 64;
/// Most attachments of one provider message.
pub const MAX_ATTACHMENTS: usize = 100;
/// Most bytes of one template part (subject, text or HTML) rendered from the request.
pub const MAX_TEMPLATE_BYTES: usize = 64 * 1024;
/// Most bytes of template data rendered into a template.
pub const MAX_TEMPLATE_DATA_BYTES: usize = 64 * 1024;
/// Most bytes the messages of one call may hold together when content is repeated per recipient
/// (SendGrid personalizations, Mailgun batch sending, Brevo message versions, SES bulk entries).
pub const MAX_FANOUT_BYTES: usize = 128 * 1024 * 1024;
/// Most loop iterations times template bytes a render may need.
const MAX_RENDER_WORK: u128 = 64 * 1024 * 1024;

/// Header fields that the message fields set or that describe the MIME structure.
const RESERVED: [&str; 10] = [
    "from",
    "to",
    "cc",
    "bcc",
    "subject",
    "reply-to",
    "mime-version",
    "content-type",
    "content-transfer-encoding",
    "content-disposition",
];

/// An attachment of a provider message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attachment {
    pub name: String,
    pub content_type: Option<String>,
    pub data: Vec<u8>,
    /// Makes the part inline (`cid:`).
    pub content_id: Option<String>,
}

/// A message to build from a provider payload.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Spec {
    /// `addr` or `Name <addr>`.
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    /// Envelope only; never written as a header.
    pub bcc: Vec<String>,
    pub reply_to: Vec<String>,
    pub subject: Option<String>,
    pub text: Option<String>,
    pub html: Option<String>,
    pub headers: Vec<(String, String)>,
    pub attachments: Vec<Attachment>,
    /// `Message-ID` without angle brackets; generated when absent.
    pub message_id: Option<String>,
    pub extras: ProviderExtras,
    /// What Postmark answers as `To`.
    pub display_to: String,
}

fn printable(s: &str) -> bool {
    !s.chars().any(|c| c.is_control() && c != '\t')
}

fn header_name_ok(n: &str) -> bool {
    !n.is_empty() && n.len() <= 76 && n.bytes().all(|b| (33..=126).contains(&b) && b != b':')
}

/// The address of `addr` or `Name <addr>`.
pub fn address_of(raw: &str) -> Result<String, Fail> {
    parse_address(raw)
        .map(|(_, a)| a)
        .map_err(|e| Fail::invalid(e.to_string()))
}

fn address_list(list: &[String]) -> Result<Address<'static>, Fail> {
    let items = list
        .iter()
        .map(|a| {
            parse_address(a)
                .map(|(n, e)| Address::new_address(n, e))
                .map_err(|e| Fail::invalid(e.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Address::new_list(items))
}

/// Splits `a@example.com, "Doe, J" <j@example.com>` at commas outside quotes and angle brackets.
pub fn split_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut quoted, mut angle) = (false, false);
    for c in s.chars() {
        match c {
            '"' => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' | ';' if !quoted && !angle => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Refuses a call whose `count` messages would repeat `shared` bytes beyond [`MAX_FANOUT_BYTES`].
pub fn check_fanout(shared: usize, count: usize) -> Result<(), Fail> {
    if shared.saturating_mul(count) > MAX_FANOUT_BYTES {
        return Err(Fail::TooLarge);
    }
    Ok(())
}

/// Decodes base64 content (standard alphabet, whitespace ignored).
pub fn decode_base64(what: &str, s: &str) -> Result<Vec<u8>, Fail> {
    let clean: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    B64.decode(clean.as_bytes())
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(clean.as_bytes()))
        .map_err(|_| Fail::invalid(format!("{what} is not valid base64")))
}

impl Spec {
    /// Envelope recipients: to, cc and bcc addresses without duplicates, in order.
    pub fn recipients(&self) -> Result<Vec<String>, Fail> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for a in self.to.iter().chain(&self.cc).chain(&self.bcc) {
            let addr = address_of(a)?;
            if seen.insert(addr.to_ascii_lowercase()) {
                out.push(addr);
            }
        }
        Ok(out)
    }

    /// Builds the MIME message; fails on invalid addresses or headers or a result above `max_bytes`.
    pub fn build(self, max_bytes: usize) -> Result<OutMessage, Fail> {
        if self.to.is_empty() && self.cc.is_empty() && self.bcc.is_empty() {
            return Err(Fail::invalid("the message has no recipients"));
        }
        if self.headers.len() > MAX_HEADERS {
            return Err(Fail::invalid(format!("at most {MAX_HEADERS} headers")));
        }
        if self.attachments.len() > MAX_ATTACHMENTS {
            return Err(Fail::invalid(format!(
                "at most {MAX_ATTACHMENTS} attachments"
            )));
        }
        let rcpt_to = self.recipients()?;
        let (from_name, from) =
            parse_address(&self.from).map_err(|e| Fail::invalid(e.to_string()))?;
        let mut b = MessageBuilder::new().from(Address::new_address(from_name, from.clone()));
        if !self.to.is_empty() {
            b = b.to(address_list(&self.to)?);
        }
        if !self.cc.is_empty() {
            b = b.cc(address_list(&self.cc)?);
        }
        if !self.reply_to.is_empty() {
            b = b.reply_to(address_list(&self.reply_to)?);
        }
        if let Some(s) = &self.subject {
            if !printable(s) || s.len() > 998 * 4 {
                return Err(Fail::invalid("the subject must be one line"));
            }
            b = b.subject(Text::new(s.clone()));
        }
        let mut message_id = self.message_id.clone();
        for (name, value) in &self.headers {
            let lower = name.to_ascii_lowercase();
            if !header_name_ok(name) || RESERVED.contains(&lower.as_str()) {
                return Err(Fail::invalid(format!(
                    "header `{}` cannot be set",
                    name.chars().take(80).collect::<String>()
                )));
            }
            if !printable(value) || value.len() > 998 {
                return Err(Fail::invalid(format!(
                    "header `{name}` must be one line of at most 998 bytes"
                )));
            }
            if lower == "message-id" {
                message_id = Some(value.trim().trim_matches(['<', '>']).to_string());
                continue;
            }
            let canonical = if lower == "date" { "Date" } else { name };
            b = b.header(canonical.to_string(), Raw::new(value.clone()));
        }
        let id = message_id
            .filter(|m| !m.is_empty() && printable(m) && !m.contains(['<', '>', ' ']))
            .unwrap_or_else(|| format!("{}@fastmock", uuid::Uuid::now_v7().simple()));
        b = b.message_id(id);
        if let Some(t) = &self.text {
            b = b.text_body(t.clone());
        }
        if let Some(h) = &self.html {
            b = b.html_body(h.clone());
        }
        if self.text.is_none() && self.html.is_none() {
            b = b.text_body(String::new());
        }
        let mut total = 0usize;
        for a in self.attachments {
            total += a.data.len();
            if total > max_bytes {
                return Err(Fail::TooLarge);
            }
            let ct = a
                .content_type
                .filter(|c| !c.trim().is_empty())
                .unwrap_or_else(|| "application/octet-stream".into());
            if !printable(&ct) || !ct.contains('/') || !printable(&a.name) || a.name.len() > 255 {
                return Err(Fail::invalid(format!(
                    "attachment `{}`: invalid name or content type",
                    a.name.chars().take(80).collect::<String>()
                )));
            }
            b = match a.content_id {
                Some(cid) => {
                    let cid = cid.trim().trim_start_matches("cid:");
                    let cid = cid.trim_matches(['<', '>']).to_string();
                    if cid.is_empty() || !printable(&cid) || cid.contains(' ') {
                        return Err(Fail::invalid(format!(
                            "attachment `{}`: invalid content id",
                            a.name
                        )));
                    }
                    b.inline(ct, cid, a.data)
                }
                None => b.attachment(ct, a.name, a.data),
            };
        }
        let raw = b.write_to_vec().map_err(|e| Fail::invalid(e.to_string()))?;
        if raw.len() > max_bytes {
            return Err(Fail::TooLarge);
        }
        Ok(OutMessage {
            mail_from: from,
            rcpt_to,
            raw,
            extras: self.extras,
            display_to: self.display_to,
        })
    }
}

/// Raw MIME from a provider payload: recipients from `rcpt` when given, else from the To, Cc and Bcc
/// headers; the envelope sender from `from` when given, else from the From header.
pub fn raw_message(
    raw: Vec<u8>,
    from: Option<&str>,
    rcpt: &[String],
    extras: ProviderExtras,
    max_bytes: usize,
) -> Result<OutMessage, Fail> {
    if raw.len() > max_bytes {
        return Err(Fail::TooLarge);
    }
    let parsed = crate::parse::ParsedMail::parse(
        &raw,
        &crate::parse::ParseLimits {
            max_bytes,
            ..Default::default()
        },
    );
    if !parsed.is_parsed() {
        return Err(Fail::invalid("the raw message is not a MIME message"));
    }
    let rcpt_to = if rcpt.is_empty() {
        parsed.recipients()
    } else {
        let mut seen = BTreeSet::new();
        let mut v = Vec::new();
        for r in rcpt {
            let a = address_of(r)?;
            if seen.insert(a.to_ascii_lowercase()) {
                v.push(a);
            }
        }
        v
    };
    if rcpt_to.is_empty() {
        return Err(Fail::invalid("the message has no recipients"));
    }
    let mail_from = match from {
        Some(f) if !f.trim().is_empty() => address_of(f)?,
        _ => parsed
            .from()
            .first()
            .map(|a| a.address.clone())
            .unwrap_or_default(),
    };
    let display_to = parsed
        .to()
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Ok(OutMessage {
        mail_from,
        rcpt_to,
        raw,
        extras,
        display_to,
    })
}

fn max_collection(v: &Value) -> u128 {
    match v {
        Value::Array(a) => a
            .iter()
            .map(max_collection)
            .max()
            .unwrap_or(0)
            .max(a.len() as u128),
        Value::Object(m) => m
            .values()
            .map(max_collection)
            .max()
            .unwrap_or(0)
            .max(m.len() as u128),
        _ => 0,
    }
}

/// Deepest nesting of `{{#each}}` blocks.
fn each_depth(template: &str) -> usize {
    let mut stack: Vec<bool> = Vec::new();
    let mut max = 0;
    let mut rest = template;
    while let Some(i) = rest.find("{{") {
        rest = &rest[i + 2..];
        let tag = rest.trim_start_matches(['~', '{']).trim_start();
        if let Some(name) = tag.strip_prefix('#') {
            let each = name.trim_start().starts_with("each");
            stack.push(each);
            max = max.max(stack.iter().filter(|e| **e).count());
        } else if tag.starts_with('/') {
            stack.pop();
        }
    }
    max
}

/// Renders a Handlebars template part with the data (SES `TemplateContent`); HTML-escapes values when
/// `html`. Templates and data are size-capped and loops bounded, partials are not available.
pub fn render(template: &str, data: &Value, html: bool) -> Result<String, Fail> {
    if template.len() > MAX_TEMPLATE_BYTES {
        return Err(Fail::invalid(format!(
            "a template part is larger than {MAX_TEMPLATE_BYTES} bytes"
        )));
    }
    let depth = each_depth(template);
    let width = max_collection(data).max(1);
    let work = width
        .checked_pow(depth as u32)
        .unwrap_or(u128::MAX)
        .saturating_mul(template.len().max(1) as u128);
    if work > MAX_RENDER_WORK {
        return Err(Fail::invalid(
            "the template loops over too much data to render",
        ));
    }
    let mut hb = handlebars::Handlebars::new();
    hb.set_strict_mode(false);
    hb.set_prevent_indent(true);
    hb.register_helper("log", Box::new(noop_helper));
    if !html {
        hb.register_escape_fn(handlebars::no_escape);
    }
    hb.render_template(template, data)
        .map_err(|e| Fail::invalid(format!("the template cannot be rendered: {}", e.reason())))
}

fn noop_helper(
    _: &handlebars::Helper<'_>,
    _: &handlebars::Handlebars<'_>,
    _: &handlebars::Context,
    _: &mut handlebars::RenderContext<'_, '_>,
    _: &mut dyn handlebars::Output,
) -> handlebars::HelperResult {
    Ok(())
}

/// Parses template data given as a JSON string (SES `TemplateData`).
pub fn template_data(what: &str, s: &str) -> Result<Value, Fail> {
    if s.len() > MAX_TEMPLATE_DATA_BYTES {
        return Err(Fail::invalid(format!(
            "{what} is larger than {MAX_TEMPLATE_DATA_BYTES} bytes"
        )));
    }
    serde_json::from_str(s).map_err(|_| Fail::invalid(format!("{what} is not valid JSON")))
}

/// Merges `over` into `base` (objects, one level).
pub fn merge_data(base: &Value, over: &Value) -> Value {
    match (base, over) {
        (Value::Object(a), Value::Object(b)) => {
            let mut m = a.clone();
            for (k, v) in b {
                m.insert(k.clone(), v.clone());
            }
            Value::Object(m)
        }
        (_, Value::Null) => base.clone(),
        _ => over.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn splits_address_lists() {
        assert_eq!(
            split_list(r#"a@example.com, "Doe, J" <j@example.com>;b@example.com"#),
            vec![
                "a@example.com",
                r#""Doe, J" <j@example.com>"#,
                "b@example.com"
            ]
        );
        assert!(split_list(" , ").is_empty());
    }

    #[test]
    fn builds_and_keeps_bcc_out_of_headers() {
        let m = Spec {
            from: "Shop <shop@example.com>".into(),
            to: vec!["a@example.test".into()],
            bcc: vec!["hidden@example.test".into(), "A@example.test".into()],
            reply_to: vec!["help@example.com".into()],
            subject: Some("Hi".into()),
            text: Some("x".into()),
            headers: vec![("Message-ID".into(), "<id-1@example.com>".into())],
            attachments: vec![Attachment {
                name: "logo.png".into(),
                content_type: Some("image/png".into()),
                data: vec![1, 2],
                content_id: Some("cid:logo".into()),
            }],
            ..Default::default()
        }
        .build(1 << 20)
        .unwrap();
        assert_eq!(m.rcpt_to, vec!["a@example.test", "hidden@example.test"]);
        assert_eq!(m.mail_from, "shop@example.com");
        let text = String::from_utf8_lossy(&m.raw);
        assert!(!text.contains("hidden@"));
        assert!(text.contains("Message-ID: <id-1@example.com>"));
        assert!(text.contains("Reply-To: <help@example.com>"));
        assert!(text.contains("Content-ID: <logo>"));
    }

    #[test]
    fn refuses_bad_input() {
        let base = Spec {
            from: "shop@example.com".into(),
            to: vec!["a@example.test".into()],
            ..Default::default()
        };
        let mut s = base.clone();
        s.headers = vec![("Bcc".into(), "x@example.com".into())];
        assert!(s.build(1 << 20).is_err());
        let mut s = base.clone();
        s.headers = vec![("X-A".into(), "a\r\nBcc: x@example.com".into())];
        assert!(s.build(1 << 20).is_err());
        let mut s = base.clone();
        s.to = vec![];
        assert!(s.build(1 << 20).is_err());
        let mut s = base.clone();
        s.text = Some("x".repeat(100));
        assert_eq!(s.build(50).unwrap_err(), Fail::TooLarge);
        assert!(decode_base64("content", "***").is_err());
        assert_eq!(decode_base64("c", "aGk=\n").unwrap(), b"hi");
    }

    #[test]
    fn renders_templates_with_bounds() {
        let data = json!({"name": "<Ann>", "items": [1, 2, 3]});
        assert_eq!(
            render("Hi {{name}}{{#each items}} {{this}}{{/each}}", &data, false).unwrap(),
            "Hi <Ann> 1 2 3"
        );
        assert_eq!(
            render("<p>{{name}}</p>", &data, true).unwrap(),
            "<p>&lt;Ann&gt;</p>"
        );
        assert_eq!(render("{{missing}}!", &data, false).unwrap(), "!");
        let big = json!({"a": (0..2000).collect::<Vec<_>>()});
        assert!(
            render(
                "{{#each a}}{{#each ../a}}{{#each ../../a}}x{{/each}}{{/each}}{{/each}}",
                &big,
                false
            )
            .is_err()
        );
        assert!(render("{{> partial}}", &data, false).is_err());
        assert_eq!(each_depth("{{#if a}}{{#each b}}{{/each}}{{/if}}"), 1);
        assert_eq!(
            merge_data(&json!({"a": 1, "b": 2}), &json!({"b": 3})),
            json!({"a": 1, "b": 3})
        );
    }
}
