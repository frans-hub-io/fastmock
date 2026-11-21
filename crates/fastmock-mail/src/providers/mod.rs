//! Provider HTTP APIs (M7): each provider's send requests turned into MIME messages with provider
//! extras, and each provider's success and error answers. Nothing here knows HTTP types; the server
//! (`fastmock-server/src/mail_providers.rs`) reads the request, calls [`route`], [`credential`],
//! [`parse`], stores the messages and answers with [`success`] or [`failure`].

pub mod brevo;
pub mod build;
pub mod mailgun;
pub mod postmark;
pub mod resend;
pub mod sendgrid;
pub mod ses;

use bytes::Bytes;
use fastmock_core::mail::ProviderExtras;
use serde_json::Value;

/// A provider whose send API FastMock answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Provider {
    Ses,
    Sendgrid,
    Mailgun,
    Postmark,
    Resend,
    Brevo,
}

impl Provider {
    pub const ALL: [Provider; 6] = [
        Provider::Ses,
        Provider::Sendgrid,
        Provider::Mailgun,
        Provider::Postmark,
        Provider::Resend,
        Provider::Brevo,
    ];

    /// The name in `/__mail/<provider>/` and in stored messages.
    pub fn name(self) -> &'static str {
        match self {
            Provider::Ses => "ses",
            Provider::Sendgrid => "sendgrid",
            Provider::Mailgun => "mailgun",
            Provider::Postmark => "postmark",
            Provider::Resend => "resend",
            Provider::Brevo => "brevo",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|p| p.name().eq_ignore_ascii_case(s.trim()))
    }

    /// The `providerError` names of mail chaos rules this provider answers (besides provider-native
    /// codes, see each module).
    pub fn error_names(self) -> &'static [&'static str] {
        match self {
            Provider::Ses => ses::ERROR_NAMES,
            Provider::Sendgrid => sendgrid::ERROR_NAMES,
            Provider::Mailgun => mailgun::ERROR_NAMES,
            Provider::Postmark => postmark::ERROR_NAMES,
            Provider::Resend => resend::ERROR_NAMES,
            Provider::Brevo => brevo::ERROR_NAMES,
        }
    }
}

/// An endpoint of a provider API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// `POST /v2/email/outbound-emails`.
    SesV2Send,
    /// `POST /v2/email/outbound-bulk-emails`.
    SesV2Bulk,
    /// `POST /` with `Action=SendEmail` or `SendRawEmail`.
    SesV1,
    /// `POST /v3/mail/send`.
    SendgridSend,
    /// `POST /v3/{domain}/messages` and `/messages.mime`.
    MailgunMessages { domain: String, mime: bool },
    /// `POST /email` and `/email/withTemplate`.
    PostmarkEmail { template: bool },
    /// `POST /email/batch` and `/email/batchWithTemplates`.
    PostmarkBatch { template: bool },
    /// `POST /emails`.
    ResendSend,
    /// `POST /v3/smtp/email`.
    BrevoSend,
}

impl Op {
    pub fn provider(&self) -> Provider {
        match self {
            Op::SesV2Send | Op::SesV2Bulk | Op::SesV1 => Provider::Ses,
            Op::SendgridSend => Provider::Sendgrid,
            Op::MailgunMessages { .. } => Provider::Mailgun,
            Op::PostmarkEmail { .. } | Op::PostmarkBatch { .. } => Provider::Postmark,
            Op::ResendSend => Provider::Resend,
            Op::BrevoSend => Provider::Brevo,
        }
    }

    /// The body is a form (multipart or urlencoded) the server parses into [`FormPart`]s.
    pub fn takes_form(&self) -> bool {
        matches!(self, Op::SesV1 | Op::MailgunMessages { .. })
    }
}

/// A provider answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    /// Includes `content-type` when there is a body.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json(status: u16, body: &Value) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: serde_json::to_vec(body).unwrap_or_default(),
        }
    }

    pub fn empty(status: u16) -> Self {
        Self {
            status,
            headers: vec![],
            body: vec![],
        }
    }

    pub fn with_header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
    }
}

/// Why a call (or one message of a batch) fails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fail {
    /// No credentials at all.
    MissingAuth,
    /// Credentials that are malformed or not among the expected keys of the space.
    BadKey,
    /// The request is invalid; `field` names the offending field when known.
    Invalid {
        field: Option<String>,
        message: String,
    },
    /// The body is not valid JSON (or not the expected shape at the top).
    BadJson(String),
    /// The message or request exceeds a size limit.
    TooLarge,
    /// The content type (or encoding) is not accepted.
    Media(String),
    /// No such endpoint (or space).
    NotFound(String),
    /// The endpoint exists for another method.
    Method,
    /// A mail chaos rule asks for this provider error.
    Named(String),
    /// Storing failed.
    Internal(String),
}

impl Fail {
    pub fn invalid(m: impl Into<String>) -> Self {
        Fail::Invalid {
            field: None,
            message: m.into(),
        }
    }

    pub fn field(f: &str, m: impl Into<String>) -> Self {
        Fail::Invalid {
            field: Some(f.to_string()),
            message: m.into(),
        }
    }

    /// The human-readable part.
    pub fn message(&self) -> String {
        match self {
            Fail::MissingAuth => "authorization required".into(),
            Fail::BadKey => "the API key is not valid".into(),
            Fail::Invalid { message, .. } => message.clone(),
            Fail::BadJson(m) => m.clone(),
            Fail::TooLarge => "the message is too large".into(),
            Fail::Media(m) => m.clone(),
            Fail::NotFound(m) => m.clone(),
            Fail::Method => "method not allowed".into(),
            Fail::Named(n) => format!("FastMock chaos: {n}"),
            Fail::Internal(m) => m.clone(),
        }
    }
}

/// A field of a form body (multipart or urlencoded).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FormPart {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub data: Bytes,
}

impl FormPart {
    pub fn text(name: &str, value: &str) -> Self {
        Self {
            name: name.into(),
            data: Bytes::copy_from_slice(value.as_bytes()),
            ..Default::default()
        }
    }

    pub fn value(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }
}

/// A provider call as the server read it.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    /// Lowercase names.
    pub headers: &'a [(String, String)],
    pub query: &'a str,
    /// Decompressed.
    pub body: &'a [u8],
    /// The form fields when [`Op::takes_form`].
    pub form: &'a [FormPart],
    /// The mail module's message size cap.
    pub max_bytes: usize,
}

/// One message to store.
#[derive(Clone, Debug, PartialEq)]
pub struct OutMessage {
    pub mail_from: String,
    pub rcpt_to: Vec<String>,
    pub raw: Vec<u8>,
    /// `provider_message_id` is set.
    pub extras: ProviderExtras,
    /// The recipients as Postmark answers them (`To`).
    pub display_to: String,
}

/// A parsed call: the messages (or per-message failures of a batch) to store.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Parsed {
    pub items: Vec<Result<OutMessage, Fail>>,
    /// Sandbox or test mode (stored as `testMode`; no webhooks).
    pub test_mode: bool,
    /// Brevo `scheduledAt`: answered with 202.
    pub scheduled: bool,
    /// SES v1 `Action` (`SendEmail`, `SendRawEmail`); `messageVersions` for Brevo calls with versions.
    pub action: Option<String>,
}

impl Parsed {
    pub fn one(m: OutMessage) -> Self {
        Self {
            test_mode: m.extras.test_mode,
            items: vec![Ok(m)],
            ..Default::default()
        }
    }
}

/// The value of a header (case-insensitive).
pub fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// The media type of `content-type`, lowercase, without parameters.
pub fn media_type(headers: &[(String, String)]) -> Option<String> {
    header(headers, "content-type").map(|c| {
        c.split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
    })
}

/// `content-type` is absent or JSON.
pub fn json_body(headers: &[(String, String)]) -> bool {
    media_type(headers).is_none_or(|m| m == "application/json" || m.ends_with("+json"))
}

/// A bearer token (`Authorization: Bearer <token>`).
pub fn bearer(headers: &[(String, String)]) -> Option<String> {
    let v = header(headers, "authorization")?.trim();
    let (scheme, token) = v.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty())
        .then(|| token.trim().to_string())
}

/// User and password of `Authorization: Basic`.
pub fn basic(headers: &[(String, String)]) -> Option<(String, String)> {
    use base64::Engine as _;
    let v = header(headers, "authorization")?.trim();
    let (scheme, b64) = v.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    let s = String::from_utf8(raw).ok()?;
    let (u, p) = s.split_once(':')?;
    Some((u.to_string(), p.to_string()))
}

/// The endpoint of a provider path (after `/__mail/<provider>[/@<space>]`).
pub fn route(provider: Provider, method: &str, path: &str) -> Result<Op, Fail> {
    let path = if path.is_empty() { "/" } else { path };
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let op = match (provider, segs.as_slice()) {
        (Provider::Ses, ["v2", "email", "outbound-emails"]) => Op::SesV2Send,
        (Provider::Ses, ["v2", "email", "outbound-bulk-emails"]) => Op::SesV2Bulk,
        (Provider::Ses, [""]) => Op::SesV1,
        (Provider::Sendgrid, ["v3", "mail", "send"]) => Op::SendgridSend,
        (Provider::Mailgun, ["v3", domain, last @ ("messages" | "messages.mime")])
            if mailgun::domain_ok(domain) =>
        {
            Op::MailgunMessages {
                domain: domain.to_ascii_lowercase(),
                mime: *last == "messages.mime",
            }
        }
        (Provider::Postmark, ["email"]) => Op::PostmarkEmail { template: false },
        (Provider::Postmark, ["email", "withTemplate"]) => Op::PostmarkEmail { template: true },
        (Provider::Postmark, ["email", "batch"]) => Op::PostmarkBatch { template: false },
        (Provider::Postmark, ["email", "batchWithTemplates"]) => {
            Op::PostmarkBatch { template: true }
        }
        (Provider::Resend, ["emails"]) => Op::ResendSend,
        (Provider::Brevo, ["v3", "smtp", "email"]) => Op::BrevoSend,
        _ => {
            return Err(Fail::NotFound(format!(
                "FastMock does not implement {method} {path} for {}",
                provider.name()
            )));
        }
    };
    if !method.eq_ignore_ascii_case("POST") {
        return Err(Fail::Method);
    }
    Ok(op)
}

/// The API key of a call, to compare with the expected keys of the space: the SES access key id, the
/// SendGrid, Resend or Brevo key, the Mailgun password (user `api`), the Postmark server token.
/// Calls without credentials fail with [`Fail::MissingAuth`], malformed ones with [`Fail::BadKey`]
/// (SES: `IncompleteSignature`).
pub fn credential(op: &Op, req: &Request<'_>) -> Result<String, Fail> {
    let h = req.headers;
    match op.provider() {
        Provider::Ses => ses::access_key(h, req.query),
        Provider::Sendgrid | Provider::Resend => match header(h, "authorization") {
            None => Err(Fail::MissingAuth),
            Some(_) => bearer(h).ok_or(Fail::BadKey),
        },
        Provider::Mailgun => match header(h, "authorization") {
            None => Err(Fail::MissingAuth),
            Some(_) => match basic(h) {
                Some((u, p)) if u == "api" && !p.is_empty() => Ok(p),
                _ => Err(Fail::BadKey),
            },
        },
        Provider::Postmark => match header(h, "x-postmark-server-token") {
            Some(t) if !t.trim().is_empty() => Ok(t.trim().to_string()),
            _ => Err(Fail::MissingAuth),
        },
        Provider::Brevo => match header(h, "api-key") {
            Some(k) if !k.trim().is_empty() => Ok(k.trim().to_string()),
            _ => bearer(h).ok_or(Fail::MissingAuth),
        },
    }
}

/// Keys that are always accepted for a provider, whatever the space expects (Postmark's
/// `POSTMARK_API_TEST`).
pub fn always_accepted(provider: Provider, key: &str) -> bool {
    provider == Provider::Postmark && key == postmark::TEST_TOKEN
}

/// Reads the messages of a call.
pub fn parse(op: &Op, req: &Request<'_>) -> Result<Parsed, Fail> {
    match op {
        Op::SesV2Send => ses::parse_v2_send(req),
        Op::SesV2Bulk => ses::parse_v2_bulk(req),
        Op::SesV1 => ses::parse_v1(req),
        Op::SendgridSend => sendgrid::parse(req),
        Op::MailgunMessages { domain, mime } => mailgun::parse(req, domain, *mime),
        Op::PostmarkEmail { template } => postmark::parse_one(req, *template),
        Op::PostmarkBatch { template } => postmark::parse_batch(req, *template),
        Op::ResendSend => resend::parse(req),
        Op::BrevoSend => brevo::parse(req),
    }
}

/// The success answer once the messages were stored (failed items stay `Err`).
pub fn success(op: &Op, parsed: &Parsed) -> Reply {
    match op {
        Op::SesV2Send => ses::success_v2_send(parsed),
        Op::SesV2Bulk => ses::success_v2_bulk(parsed),
        Op::SesV1 => ses::success_v1(parsed),
        Op::SendgridSend => sendgrid::success(parsed),
        Op::MailgunMessages { .. } => mailgun::success(parsed),
        Op::PostmarkEmail { .. } => postmark::success_one(parsed),
        Op::PostmarkBatch { .. } => postmark::success_batch(parsed),
        Op::ResendSend => resend::success(parsed),
        Op::BrevoSend => brevo::success(parsed),
    }
}

/// The error answer, in the provider's format.
pub fn failure(op: &Op, fail: &Fail) -> Reply {
    match op {
        Op::SesV2Send | Op::SesV2Bulk => ses::failure_v2(fail),
        Op::SesV1 => ses::failure_v1(fail),
        Op::SendgridSend => sendgrid::failure(fail),
        Op::MailgunMessages { .. } => mailgun::failure(fail),
        Op::PostmarkEmail { .. } | Op::PostmarkBatch { .. } => postmark::failure(fail),
        Op::ResendSend => resend::failure(fail),
        Op::BrevoSend => brevo::failure(fail),
    }
}

/// The error answer for a path that is no endpoint (unknown path, wrong method, unknown space).
pub fn path_failure(provider: Provider, path: &str, fail: &Fail) -> Reply {
    let op = match provider {
        Provider::Ses if path.starts_with("/v2/") => Op::SesV2Send,
        Provider::Ses => Op::SesV1,
        Provider::Sendgrid => Op::SendgridSend,
        Provider::Mailgun => Op::MailgunMessages {
            domain: String::new(),
            mime: false,
        },
        Provider::Postmark => Op::PostmarkEmail { template: false },
        Provider::Resend => Op::ResendSend,
        Provider::Brevo => Op::BrevoSend,
    };
    failure(&op, fail)
}

/// Lowercase comparison for chaos names: `rateLimit`, `ratelimit` and `rate-limit` are the same name.
pub(crate) fn norm(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// The generic chaos names every provider answers.
pub const COMMON_ERRORS: &[&str] = &[
    "invalidKey",
    "forbidden",
    "validation",
    "messageRejected",
    "rateLimit",
    "quota",
    "templateNotFound",
    "tooLarge",
    "serverError",
    "unavailable",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_paths_of_every_provider() {
        let ok = |p, path| route(p, "POST", path).unwrap();
        assert_eq!(ok(Provider::Ses, "/"), Op::SesV1);
        assert_eq!(ok(Provider::Ses, ""), Op::SesV1);
        assert_eq!(
            ok(Provider::Ses, "/v2/email/outbound-emails"),
            Op::SesV2Send
        );
        assert_eq!(
            ok(Provider::Ses, "/v2/email/outbound-bulk-emails"),
            Op::SesV2Bulk
        );
        assert_eq!(ok(Provider::Sendgrid, "/v3/mail/send"), Op::SendgridSend);
        assert_eq!(
            ok(Provider::Mailgun, "/v3/MG.Example.com/messages.mime"),
            Op::MailgunMessages {
                domain: "mg.example.com".into(),
                mime: true
            }
        );
        assert_eq!(
            ok(Provider::Postmark, "/email/batchWithTemplates"),
            Op::PostmarkBatch { template: true }
        );
        assert_eq!(ok(Provider::Resend, "/emails"), Op::ResendSend);
        assert_eq!(ok(Provider::Brevo, "/v3/smtp/email"), Op::BrevoSend);
        assert!(matches!(
            route(Provider::Sendgrid, "GET", "/v3/mail/send"),
            Err(Fail::Method)
        ));
        assert!(matches!(
            route(Provider::Resend, "POST", "/emails/batch"),
            Err(Fail::NotFound(_))
        ));
        assert!(matches!(
            route(Provider::Mailgun, "POST", "/v3/bad domain/messages"),
            Err(Fail::NotFound(_))
        ));
        assert_eq!(Provider::parse("SendGrid"), Some(Provider::Sendgrid));
        assert_eq!(Provider::parse("smtp"), None);
    }

    #[test]
    fn reads_credentials() {
        let req = |h: Vec<(&str, &str)>| {
            h.into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<Vec<_>>()
        };
        let call = |op: &Op, h: &[(String, String)]| {
            credential(
                op,
                &Request {
                    headers: h,
                    query: "",
                    body: b"",
                    form: &[],
                    max_bytes: 1 << 20,
                },
            )
        };
        assert_eq!(
            call(
                &Op::SendgridSend,
                &req(vec![("authorization", "Bearer SG.key")])
            ),
            Ok("SG.key".into())
        );
        assert_eq!(
            call(&Op::SendgridSend, &req(vec![])),
            Err(Fail::MissingAuth)
        );
        assert_eq!(
            call(&Op::SendgridSend, &req(vec![("authorization", "Basic x")])),
            Err(Fail::BadKey)
        );
        let mg = Op::MailgunMessages {
            domain: "mg.example.com".into(),
            mime: false,
        };
        assert_eq!(
            call(&mg, &req(vec![("authorization", "Basic YXBpOmtleS0x")])),
            Ok("key-1".into())
        );
        assert_eq!(
            call(&mg, &req(vec![("authorization", "Basic dXNlcjprZXk=")])),
            Err(Fail::BadKey)
        );
        assert_eq!(
            call(
                &Op::PostmarkEmail { template: false },
                &req(vec![("x-postmark-server-token", "tok")])
            ),
            Ok("tok".into())
        );
        assert_eq!(
            call(&Op::BrevoSend, &req(vec![("api-key", "xkeysib-1")])),
            Ok("xkeysib-1".into())
        );
        assert_eq!(
            call(&Op::BrevoSend, &req(vec![("authorization", "Bearer o")])),
            Ok("o".into())
        );
        assert!(always_accepted(Provider::Postmark, "POSTMARK_API_TEST"));
        assert_eq!(norm("rate-Limit"), "ratelimit");
    }
}
