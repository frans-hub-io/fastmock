//! Links and codes found in a message (`/links`, `/codes`). Inputs and results are capped, and custom
//! code patterns are size-limited; the `regex` crate matches in linear time, so the work is bounded.

use std::sync::LazyLock;

use regex::{Regex, RegexBuilder};
use serde::Serialize;

/// Bytes of text or HTML scanned.
pub const MAX_SCAN_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_LINKS: usize = 1000;
pub const MAX_CODES: usize = 100;
/// Bytes of a custom code pattern.
pub const MAX_PATTERN_BYTES: usize = 256;
const PATTERN_SIZE_LIMIT: usize = 64 * 1024;
const MAX_ANCHOR_CHARS: usize = 200;
/// Codes of 4 to 8 digits.
pub const DEFAULT_CODE_PATTERN: &str = r"\b\d{4,8}\b";

static ANCHOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<a\b[^>]*?\bhref\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))[^>]*>(.*?)</a\s*>"#)
        .expect("anchor pattern")
});
static URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\bhttps?://[^\s<>"'`{}|\\^\[\]]+"#).expect("url pattern"));
static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]*>").expect("tag pattern"));

#[derive(Clone, Debug, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct MailLink {
    #[schema(example = "https://example.com/verify?t=1")]
    pub url: String,
    /// Text of the HTML anchor, when the link comes from one.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "Verify your address")]
    pub text: Option<String>,
}

fn capped(s: &str) -> &str {
    if s.len() <= MAX_SCAN_BYTES {
        return s;
    }
    let mut cut = MAX_SCAN_BYTES;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

/// Decodes the HTML entities that commonly appear in URLs and anchor texts.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&#x2F;", "/")
        .replace("&#47;", "/")
        .replace("&amp;", "&")
}

fn web_url(raw: &str) -> Option<String> {
    let u = decode_entities(raw.trim());
    let lower = u.to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://")).then_some(u)
}

fn trim_url(u: &str) -> &str {
    let mut u = u.trim_end_matches(['.', ',', ';', ':', '!', '?', '\'', '"']);
    while u.ends_with(')') && u.matches('(').count() < u.matches(')').count() {
        u = u[..u.len() - 1].trim_end_matches(['.', ',', ';', ':', '!', '?']);
    }
    u
}

fn push(out: &mut Vec<MailLink>, url: String, text: Option<String>) {
    if let Some(existing) = out.iter_mut().find(|l| l.url == url) {
        if existing.text.is_none() {
            existing.text = text;
        }
    } else if out.len() < MAX_LINKS {
        out.push(MailLink { url, text });
    }
}

/// Every http(s) URL of the text and the HTML body, de-duplicated in order of appearance: HTML anchors
/// (with their text) first, then URLs written in the text body or in the visible HTML text.
pub fn links(text: Option<&str>, html: Option<&str>) -> Vec<MailLink> {
    let mut out = Vec::new();
    if let Some(h) = html.map(capped) {
        for c in ANCHOR.captures_iter(h) {
            let href = c
                .get(1)
                .or(c.get(2))
                .or(c.get(3))
                .map_or("", |m| m.as_str());
            let Some(url) = web_url(href) else {
                continue;
            };
            let label = c.get(4).map_or("", |m| m.as_str());
            let label: String = decode_entities(&TAG.replace_all(label, " "))
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(MAX_ANCHOR_CHARS)
                .collect();
            push(&mut out, url, (!label.is_empty()).then_some(label));
        }
    }
    let visible_html = html.map(|h| mail_parser::html_to_text(capped(h)));
    for t in [text.map(capped), visible_html.as_deref()]
        .into_iter()
        .flatten()
    {
        for m in URL.find_iter(t) {
            push(&mut out, trim_url(m.as_str()).to_string(), None);
        }
    }
    out
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PatternError {
    #[error("the code pattern is limited to {MAX_PATTERN_BYTES} bytes")]
    TooLong,
    #[error("invalid code pattern: {0}")]
    Invalid(String),
}

/// A compiled code pattern; with a capture group, the first group is the code.
#[derive(Clone, Debug)]
pub struct CodePattern(Regex);

impl CodePattern {
    /// The given pattern, or [`DEFAULT_CODE_PATTERN`].
    pub fn new(pattern: Option<&str>) -> Result<Self, PatternError> {
        let p = pattern.unwrap_or(DEFAULT_CODE_PATTERN);
        if p.len() > MAX_PATTERN_BYTES {
            return Err(PatternError::TooLong);
        }
        RegexBuilder::new(p)
            .size_limit(PATTERN_SIZE_LIMIT)
            .dfa_size_limit(PATTERN_SIZE_LIMIT)
            .nest_limit(16)
            .build()
            .map(Self)
            .map_err(|e| PatternError::Invalid(e.to_string()))
    }

    /// Distinct codes in order of appearance, at most [`MAX_CODES`].
    pub fn codes(&self, text: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for c in self.0.captures_iter(capped(text)) {
            let Some(m) = c.get(1).or(c.get(0)) else {
                continue;
            };
            if m.as_str().is_empty() || out.iter().any(|x| x == m.as_str()) {
                continue;
            }
            out.push(m.as_str().to_string());
            if out.len() >= MAX_CODES {
                break;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_from_anchors_text_and_visible_html() {
        let html = r#"<p>Hi <a class="x" href="https://example.com/verify?t=1&amp;x=2"><b>Verify</b>
            now</a> <a href='mailto:a@example.com'>mail</a> <a href=http://example.com/u>u</a>
            <img src="https://example.com/pixel.gif"> Also https://example.com/plain.</p>"#;
        let text = "See https://example.com/verify?t=1&x=2, (https://example.com/a_(b)) and HTTPS://EXAMPLE.COM/X).";
        let l = links(Some(text), Some(html));
        let urls: Vec<&str> = l.iter().map(|x| x.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://example.com/verify?t=1&x=2",
                "http://example.com/u",
                "https://example.com/a_(b)",
                "HTTPS://EXAMPLE.COM/X",
                "https://example.com/plain",
            ]
        );
        assert_eq!(l[0].text.as_deref(), Some("Verify now"));
        assert_eq!(l[2].text, None);
        assert!(links(None, None).is_empty());
    }

    #[test]
    fn codes_with_default_and_custom_patterns() {
        let d = CodePattern::new(None).unwrap();
        assert_eq!(
            d.codes("Code 123456, again 123456, order 12, ref 123456789, pin 0042"),
            vec!["123456", "0042"]
        );
        let c = CodePattern::new(Some(r"code:\s*([A-Z0-9]{6})")).unwrap();
        assert_eq!(c.codes("your code: AB12CD."), vec!["AB12CD"]);
        assert_eq!(
            CodePattern::new(Some(&"a".repeat(300))).unwrap_err(),
            PatternError::TooLong
        );
        assert!(matches!(
            CodePattern::new(Some("(")),
            Err(PatternError::Invalid(_))
        ));
        assert!(matches!(
            CodePattern::new(Some(r"\w{1000}{1000}")),
            Err(PatternError::Invalid(_))
        ));
        let many: String = (1000..1200).map(|n| format!("{n} ")).collect();
        assert_eq!(d.codes(&many).len(), MAX_CODES);
    }
}
