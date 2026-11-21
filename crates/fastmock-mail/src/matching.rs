//! `MailMatch`: which messages `wait` and `verify` look for, written with the stub string matchers
//! (`equalTo`, `contains`, `matches`, `caseInsensitive`, `and`/`or`/`not`, `includes`, ...).

use std::collections::BTreeMap;

use fastmock_core::mail::{MailAddress, MailMessage};
use fastmock_core::matcher::{CompileError, CompiledString};
use fastmock_core::model::StringMatcher;
use serde::{Deserialize, Serialize};

use crate::parse::ParsedMail;

pub const MAX_HEADER_MATCHERS: usize = 32;

/// Conditions on a message; all given ones must hold. Address fields hold when one address matches
/// (its address alone or `Name <address>`); multi-value matchers (`includes`, `hasExactly`) see the
/// bare addresses. `to` covers the `To` header and the envelope recipients, so Bcc recipients match.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MailMatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<StringMatcher>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<StringMatcher>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cc: Option<StringMatcher>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<StringMatcher>,
    /// The text body (HTML converted when there is no text part).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<StringMatcher>,
    /// The HTML body as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html: Option<StringMatcher>,
    /// Header fields by name (case-insensitive), as for stub request headers; a field present several
    /// times matches when one occurrence matches.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, StringMatcher>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_attachment: Option<bool>,
    /// One attachment's file name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment_name: Option<StringMatcher>,
    /// One label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<StringMatcher>,
    /// `smtp`, `provider` or `inject`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<StringMatcher>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<StringMatcher>,
}

/// What a [`MailMatch`] looks at, taken from a stored message and its parsed form once.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MatchView {
    pub from: Vec<MailAddress>,
    pub to: Vec<MailAddress>,
    pub cc: Vec<MailAddress>,
    pub subject: Option<String>,
    pub text: Option<String>,
    pub html: Option<String>,
    /// Lowercase names with decoded values, in order.
    pub headers: Vec<(String, String)>,
    pub has_attachments: bool,
    pub attachment_names: Vec<String>,
    pub labels: Vec<String>,
    pub source: String,
    pub provider: Option<String>,
}

impl MatchView {
    pub fn new(msg: &MailMessage, parsed: &ParsedMail<'_>) -> Self {
        let mut to = parsed.to();
        for r in &msg.envelope.rcpt_to {
            if !to.iter().any(|a| a.address.eq_ignore_ascii_case(r)) {
                to.push(MailAddress {
                    name: None,
                    address: r.clone(),
                });
            }
        }
        Self {
            from: parsed.from(),
            to,
            cc: parsed.cc(),
            subject: parsed.subject(),
            text: parsed.text(),
            html: parsed.html(),
            headers: parsed
                .headers()
                .into_iter()
                .map(|h| (h.name.to_ascii_lowercase(), h.value))
                .collect(),
            has_attachments: msg.summary.has_attachments,
            attachment_names: msg.summary.attachment_names.clone(),
            labels: msg.labels.clone(),
            source: msg.source.as_str().to_string(),
            provider: msg.provider.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    From,
    To,
    Cc,
    Subject,
    Text,
    Html,
    AttachmentName,
    Label,
    Source,
    Provider,
}

impl Field {
    fn name(self) -> &'static str {
        match self {
            Field::From => "from",
            Field::To => "to",
            Field::Cc => "cc",
            Field::Subject => "subject",
            Field::Text => "text",
            Field::Html => "html",
            Field::AttachmentName => "attachmentName",
            Field::Label => "label",
            Field::Source => "source",
            Field::Provider => "provider",
        }
    }
}

#[derive(Clone, Debug)]
struct Check {
    matcher: CompiledString,
    multi: bool,
}

impl Check {
    fn new(m: &StringMatcher) -> Result<Self, CompileError> {
        Ok(Self {
            matcher: CompiledString::compile(m)?,
            multi: m.is_multi_value(),
        })
    }

    fn values<S: AsRef<str>>(&self, values: &[S]) -> bool {
        if self.multi {
            return self.matcher.matches_values(values);
        }
        if values.is_empty() {
            return self.matcher.matches(None);
        }
        values
            .iter()
            .any(|v| self.matcher.matches(Some(v.as_ref())))
    }

    fn addresses(&self, list: &[MailAddress]) -> bool {
        if self.multi || list.is_empty() {
            let bare: Vec<&str> = list.iter().map(|a| a.address.as_str()).collect();
            return self.values(&bare);
        }
        list.iter().any(|a| {
            self.matcher.matches(Some(&a.address)) || self.matcher.matches(Some(&a.to_string()))
        })
    }
}

/// A [`MailMatch`] with its regular expressions compiled.
#[derive(Clone, Debug, Default)]
pub struct CompiledMailMatch {
    fields: Vec<(Field, Check)>,
    headers: Vec<(String, Check)>,
    has_attachment: Option<bool>,
}

impl CompiledMailMatch {
    pub fn compile(m: &MailMatch) -> Result<Self, CompileError> {
        if m.headers.len() > MAX_HEADER_MATCHERS {
            return Err(CompileError::Invalid(format!(
                "at most {MAX_HEADER_MATCHERS} header matchers"
            )));
        }
        let mut fields = Vec::new();
        for (f, spec) in [
            (Field::From, &m.from),
            (Field::To, &m.to),
            (Field::Cc, &m.cc),
            (Field::Subject, &m.subject),
            (Field::Text, &m.text),
            (Field::Html, &m.html),
            (Field::AttachmentName, &m.attachment_name),
            (Field::Label, &m.label),
            (Field::Source, &m.source),
            (Field::Provider, &m.provider),
        ] {
            if let Some(s) = spec {
                fields.push((
                    f,
                    Check::new(s)
                        .map_err(|e| CompileError::Invalid(format!("{}: {e}", f.name())))?,
                ));
            }
        }
        let mut headers = Vec::new();
        for (name, spec) in &m.headers {
            if name.is_empty()
                || name.len() > 76
                || !name.bytes().all(|b| b.is_ascii_graphic() && b != b':')
            {
                return Err(CompileError::Invalid(format!(
                    "invalid header name `{name}`"
                )));
            }
            headers.push((
                name.to_ascii_lowercase(),
                Check::new(spec)
                    .map_err(|e| CompileError::Invalid(format!("headers.{name}: {e}")))?,
            ));
        }
        Ok(Self {
            fields,
            headers,
            has_attachment: m.has_attachment,
        })
    }

    /// The matchers that fail, by name (`subject`, `headers.x-mailer`, `hasAttachment`); empty when the
    /// message matches. For near misses in `verify`.
    pub fn failures(&self, v: &MatchView) -> Vec<String> {
        let mut out = Vec::new();
        for (f, c) in &self.fields {
            let ok = match f {
                Field::From => c.addresses(&v.from),
                Field::To => c.addresses(&v.to),
                Field::Cc => c.addresses(&v.cc),
                Field::Subject => c.matcher.matches(v.subject.as_deref()),
                Field::Text => c.matcher.matches(v.text.as_deref()),
                Field::Html => c.matcher.matches(v.html.as_deref()),
                Field::AttachmentName => c.values(&v.attachment_names),
                Field::Label => c.values(&v.labels),
                Field::Source => c.matcher.matches(Some(&v.source)),
                Field::Provider => c.matcher.matches(v.provider.as_deref()),
            };
            if !ok {
                out.push(f.name().to_string());
            }
        }
        for (name, c) in &self.headers {
            let values: Vec<&str> = v
                .headers
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, x)| x.as_str())
                .collect();
            if !c.values(&values) {
                out.push(format!("headers.{name}"));
            }
        }
        if self
            .has_attachment
            .is_some_and(|want| want != v.has_attachments)
        {
            out.push("hasAttachment".into());
        }
        out
    }

    pub fn matches(&self, v: &MatchView) -> bool {
        self.failures(v).is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ParseLimits;

    fn view() -> MatchView {
        let raw = crate::parse::tests::MULTIPART.as_bytes();
        let parsed = ParsedMail::parse(raw, &ParseLimits::default());
        let mut msg = fastmock_core::mail::conformance::message(None, chrono::Utc::now(), "x");
        msg.envelope.rcpt_to = vec!["alice@example.test".into(), "hidden@example.test".into()];
        msg.summary = parsed.summary();
        msg.labels = vec!["otp".into()];
        MatchView::new(&msg, &parsed)
    }

    fn check(json: serde_json::Value) -> Vec<String> {
        let m: MailMatch = serde_json::from_value(json).unwrap();
        CompiledMailMatch::compile(&m).unwrap().failures(&view())
    }

    #[test]
    fn mail_matchers() {
        assert!(check(serde_json::json!({})).is_empty());
        assert!(
            check(serde_json::json!({
                "from": {"equalTo": "billing@example.com"},
                "to": {"equalTo": "hidden@example.test"},
                "cc": {"contains": "carol"},
                "subject": {"matches": ".*invoice", "caseInsensitive": true},
                "text": {"matches": "(?s).*code is \\d{6}.*"},
                "html": {"contains": "<b>123456</b>"},
                "headers": {"X-FastMock-Labels": {"equalTo": "otp"}, "Message-ID": {"contains": "abc@"}},
                "hasAttachment": true,
                "attachmentName": {"equalTo": "invoice.pdf"},
                "label": {"equalTo": "otp"},
                "source": {"equalTo": "smtp"},
                "provider": {"absent": true}
            }))
            .is_empty()
        );
        assert!(check(serde_json::json!({"from": {"contains": "Billing Team <"}})).is_empty());
        assert!(
            check(serde_json::json!({"to": {"includes": [{"equalTo": "bob@example.test"}, {"equalTo": "hidden@example.test"}]}}))
                .is_empty()
        );
        assert_eq!(
            check(serde_json::json!({
                "subject": {"equalTo": "nope"},
                "headers": {"X-Missing": {"equalTo": "1"}},
                "hasAttachment": false,
                "label": {"equalTo": "welcome"},
                "provider": {"equalTo": "sendgrid"}
            })),
            vec![
                "subject",
                "label",
                "provider",
                "headers.x-missing",
                "hasAttachment"
            ]
        );
        assert!(check(serde_json::json!({"headers": {"X-Missing": {"absent": true}}})).is_empty());
        let bad: MailMatch =
            serde_json::from_value(serde_json::json!({"subject": {"matches": "("}})).unwrap();
        assert!(
            CompiledMailMatch::compile(&bad)
                .unwrap_err()
                .to_string()
                .contains("subject")
        );
        assert!(serde_json::from_value::<MailMatch>(serde_json::json!({"nope": {}})).is_err());
    }
}
