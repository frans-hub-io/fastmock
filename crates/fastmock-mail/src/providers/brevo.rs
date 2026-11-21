//! Brevo `POST /v3/smtp/email`: `201 {"messageId"}` (`{"messageIds"}` with `messageVersions`, `202`
//! when `scheduledAt` is set); errors `{"code", "message"}`.

use fastmock_core::mail::ProviderExtras;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::build::{Attachment, Fanout, Spec, address_of, decode_base64};
use super::{Fail, OutMessage, Parsed, Reply, Request, json_body, norm};

pub const MAX_RECIPIENTS: usize = 99;
pub const MAX_VERSIONS: usize = 1000;

/// `providerError` names: the common ones and `credits` (HTTP 402).
pub const ERROR_NAMES: &[&str] = &[
    "invalidKey",
    "forbidden",
    "validation",
    "messageRejected",
    "rateLimit",
    "quota",
    "credits",
    "templateNotFound",
    "tooLarge",
    "serverError",
    "unavailable",
];

/// A new message id `<YYYYMMDDhhmm.<digits>@smtp-relay.fastmock.test>`.
pub fn message_id() -> String {
    let n = uuid::Uuid::new_v4().as_u128() % 100_000_000_000;
    format!(
        "<{}.{n:011}@smtp-relay.fastmock.test>",
        chrono::Utc::now().format("%Y%m%d%H%M")
    )
}

#[derive(Deserialize, Clone, Default)]
struct Contact {
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

impl Contact {
    fn display(&self, field: &str) -> Result<String, Fail> {
        let email = self.email.clone().unwrap_or_default();
        if address_of(&email).is_err() {
            return Err(code(
                "invalid_parameter",
                format!("email is not valid in {field}"),
            ));
        }
        Ok(match &self.name {
            Some(n) if !n.trim().is_empty() => {
                format!("\"{}\" <{email}>", n.replace(['"', '\\'], ""))
            }
            _ => email,
        })
    }
}

#[derive(Deserialize)]
struct Att {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Version {
    #[serde(default)]
    to: Vec<Contact>,
    #[serde(default)]
    cc: Vec<Contact>,
    #[serde(default)]
    bcc: Vec<Contact>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    reply_to: Option<Contact>,
    #[serde(default)]
    html_content: Option<String>,
    #[serde(default)]
    text_content: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendEmail {
    #[serde(default)]
    sender: Option<Contact>,
    #[serde(default)]
    to: Vec<Contact>,
    #[serde(default)]
    cc: Vec<Contact>,
    #[serde(default)]
    bcc: Vec<Contact>,
    #[serde(default)]
    reply_to: Option<Contact>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    html_content: Option<String>,
    #[serde(default)]
    text_content: Option<String>,
    #[serde(default)]
    attachment: Vec<Att>,
    #[serde(default)]
    headers: Map<String, Value>,
    #[serde(default)]
    template_id: Option<Value>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    message_versions: Vec<Version>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    scheduled_at: Option<String>,
}

fn code(c: &str, m: impl Into<String>) -> Fail {
    Fail::Invalid {
        field: Some(c.to_string()),
        message: m.into(),
    }
}

fn contacts(list: &[Contact], field: &str) -> Result<Vec<String>, Fail> {
    list.iter().map(|c| c.display(field)).collect()
}

/// `POST /v3/smtp/email`.
pub fn parse(req: &Request<'_>) -> Result<Parsed, Fail> {
    if !json_body(req.headers) {
        return Err(Fail::Media("Content-Type must be application/json".into()));
    }
    let body: SendEmail = serde_json::from_slice(req.body).map_err(|e| {
        code(
            "bad_request",
            format!("Input must be a valid JSON object: {e}"),
        )
    })?;
    let template_id = match &body.template_id {
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    };
    let from = match &body.sender {
        Some(s) if s.email.is_some() => s.display("sender")?,
        _ if template_id.is_some() => "template-sender@fastmock.invalid".into(),
        _ => return Err(code("missing_parameter", "sender is missing")),
    };
    let has_content = body.html_content.as_deref().is_some_and(|h| !h.is_empty())
        || body.text_content.as_deref().is_some_and(|t| !t.is_empty())
        || body
            .message_versions
            .iter()
            .any(|v| v.html_content.is_some() || v.text_content.is_some());
    if template_id.is_none() && !has_content {
        return Err(code(
            "missing_parameter",
            "Either of htmlContent, textContent or templateId is required",
        ));
    }
    if template_id.is_none()
        && body.subject.as_deref().is_none_or(str::is_empty)
        && body.message_versions.iter().all(|v| v.subject.is_none())
    {
        return Err(code("missing_parameter", "subject is missing"));
    }
    if body.message_versions.len() > MAX_VERSIONS {
        return Err(code(
            "invalid_parameter",
            format!("messageVersions can hold at most {MAX_VERSIONS} versions"),
        ));
    }
    let mut attachments = Vec::with_capacity(body.attachment.len());
    for a in &body.attachment {
        let name = a.name.clone().unwrap_or_default();
        if name.is_empty() {
            return Err(code("missing_parameter", "attachment name is missing"));
        }
        let data = match (&a.content, &a.url) {
            (Some(c), _) => decode_base64("attachment content", c).map_err(|_| {
                code(
                    "invalid_parameter",
                    "attachment content is not valid base64",
                )
            })?,
            (None, Some(_)) => Vec::new(),
            (None, None) => {
                return Err(code(
                    "missing_parameter",
                    "attachment url or content is missing",
                ));
            }
        };
        attachments.push(Attachment {
            name,
            content_type: None,
            data,
            content_id: None,
        });
    }
    let headers: Vec<(String, String)> = body
        .headers
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                v.as_str()
                    .map(String::from)
                    .unwrap_or_else(|| v.to_string()),
            )
        })
        .collect();
    let base_versions = if body.message_versions.is_empty() {
        vec![Version::default()]
    } else {
        body.message_versions
    };
    let shared = body.html_content.as_ref().map_or(0, String::len)
        + body.text_content.as_ref().map_or(0, String::len)
        + attachments.iter().map(|a| a.data.len()).sum::<usize>();
    let mut fan = Fanout::new(shared, base_versions.len())?;
    let scheduled = body.scheduled_at.is_some();
    let mut items = Vec::with_capacity(base_versions.len());
    for v in base_versions {
        let to = if v.to.is_empty() { &body.to } else { &v.to };
        if to.is_empty() {
            return Err(code("missing_parameter", "to is missing"));
        }
        let cc = if v.cc.is_empty() { &body.cc } else { &v.cc };
        let bcc = if v.bcc.is_empty() { &body.bcc } else { &v.bcc };
        if to.len() + cc.len() + bcc.len() > MAX_RECIPIENTS {
            return Err(code(
                "invalid_parameter",
                format!("at most {MAX_RECIPIENTS} recipients per message"),
            ));
        }
        let to = contacts(to, "to")?;
        let reply_to = v
            .reply_to
            .as_ref()
            .or(body.reply_to.as_ref())
            .map(|r| r.display("replyTo"))
            .transpose()?;
        let params = match (&body.params, &v.params) {
            (Some(a), Some(b)) => Some(super::build::merge_data(a, b)),
            (a, b) => b.clone().or_else(|| a.clone()),
        };
        let spec = Spec {
            from: from.clone(),
            display_to: to.join(", "),
            to,
            cc: contacts(cc, "cc")?,
            bcc: contacts(bcc, "bcc")?,
            reply_to: reply_to.into_iter().collect(),
            subject: v.subject.or_else(|| body.subject.clone()),
            text: v.text_content.or_else(|| body.text_content.clone()),
            html: v.html_content.or_else(|| body.html_content.clone()),
            headers: headers.clone(),
            attachments: attachments.clone(),
            extras: ProviderExtras {
                provider_message_id: Some(message_id()),
                template_id: template_id.clone(),
                template_data: params,
                tags: body.tags.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        let m: OutMessage = spec.build(req.max_bytes).map_err(|e| match e {
            Fail::Invalid { message, .. } => code("invalid_parameter", message),
            other => other,
        })?;
        items.push(Ok(fan.message(m)?));
    }
    let versions = items.len() > 1;
    Ok(Parsed {
        items,
        scheduled,
        action: versions.then(|| "messageVersions".to_string()),
        ..Default::default()
    })
}

/// `201 {"messageId"}`, or `{"messageIds"}` with `messageVersions`; `202` when scheduled.
pub fn success(parsed: &Parsed) -> Reply {
    if let Some(Err(f)) = parsed.items.iter().find(|i| i.is_err()) {
        return failure(f);
    }
    let ids: Vec<String> = parsed
        .items
        .iter()
        .filter_map(|i| i.as_ref().ok())
        .filter_map(|m| m.extras.provider_message_id.clone())
        .collect();
    let status = if parsed.scheduled { 202 } else { 201 };
    if parsed.action.as_deref() == Some("messageVersions") {
        Reply::json(status, &json!({"messageIds": ids}))
    } else {
        Reply::json(
            status,
            &json!({"messageId": ids.first().cloned().unwrap_or_default()}),
        )
    }
}

fn reply(status: u16, c: &str, m: &str) -> Reply {
    let r = Reply::json(status, &json!({"code": c, "message": m}));
    if status == 429 {
        r.with_header("x-sib-ratelimit-limit", "1000")
            .with_header("x-sib-ratelimit-remaining", "0")
            .with_header("x-sib-ratelimit-reset", "1")
    } else {
        r
    }
}

fn named(name: &str) -> Reply {
    match norm(name).as_str() {
        "invalidkey" | "unauthorized" => reply(401, "unauthorized", "Key not found"),
        "forbidden" | "permissiondenied" => reply(
            403,
            "permission_denied",
            "Unable to send email. Your SMTP account is not yet activated.",
        ),
        "validation" | "invalidparameter" => {
            reply(400, "invalid_parameter", "The request is invalid")
        }
        "messagerejected" => reply(400, "invalid_parameter", "Sender is not valid"),
        "ratelimit" | "toomanyrequests" => reply(429, "too_many_requests", "Too many requests"),
        "quota" | "credits" | "notenoughcredits" => reply(
            402,
            "not_enough_credits",
            "You do not have enough credits to send emails.",
        ),
        "templatenotfound" | "documentnotfound" => {
            reply(404, "document_not_found", "Template ID does not exist")
        }
        "toolarge" => reply(
            400,
            "invalid_parameter",
            "The message size exceeds the limit",
        ),
        "servererror" => reply(500, "internal_error", "Internal server error"),
        "unavailable" => reply(503, "service_unavailable", "Service unavailable"),
        _ => reply(
            500,
            "internal_error",
            &format!("FastMock: unknown providerError `{name}` for brevo"),
        ),
    }
}

/// `{"code", "message"}`.
pub fn failure(fail: &Fail) -> Reply {
    match fail {
        Fail::Invalid {
            field: Some(c),
            message,
        } => reply(400, c, message),
        Fail::MissingAuth | Fail::BadKey => named("invalidKey"),
        Fail::Invalid { message, .. } | Fail::BadJson(message) => {
            reply(400, "invalid_parameter", message)
        }
        Fail::TooLarge => named("tooLarge"),
        Fail::Media(m) => reply(406, "not_acceptable", m),
        Fail::NotFound(m) => reply(404, "not_found", m),
        Fail::Method => reply(405, "method_not_allowed", "Method not allowed"),
        Fail::Named(n) => named(n),
        Fail::Internal(_) => named("serverError"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(body: Value) -> Result<Parsed, Fail> {
        let bytes = serde_json::to_vec(&body).unwrap();
        let h = vec![("content-type".to_string(), "application/json".to_string())];
        parse(&Request {
            headers: &h,
            query: "",
            body: &bytes,
            form: &[],
            max_bytes: 1 << 20,
        })
    }

    #[test]
    fn sends_and_answers() {
        let p = call(json!({
            "sender": {"name": "Shop", "email": "shop@example.com"},
            "to": [{"email": "a@example.test", "name": "Ann"}],
            "bcc": [{"email": "b@example.test"}],
            "replyTo": {"email": "help@example.com"},
            "subject": "Hi", "htmlContent": "<p>Hi</p>",
            "attachment": [{"content": "aGk=", "name": "a.txt"}],
            "headers": {"X-Mailin-custom": "v"}, "tags": ["otp"]
        }))
        .unwrap();
        let m = p.items[0].as_ref().unwrap();
        assert_eq!(m.rcpt_to, vec!["a@example.test", "b@example.test"]);
        assert_eq!(m.extras.tags, vec!["otp"]);
        let r = success(&p);
        assert_eq!(r.status, 201);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v, json!({"messageId": m.extras.provider_message_id}));

        let p = call(json!({
            "templateId": 7, "params": {"a": 1, "b": 1},
            "to": [{"email": "x@example.test"}],
            "messageVersions": [{"to": [{"email": "a@example.test"}], "params": {"b": 2}}, {"to": [{"email": "b@example.test"}]}],
            "scheduledAt": "2026-12-01T10:00:00Z"
        }))
        .unwrap();
        assert_eq!(p.items.len(), 2);
        let a = p.items[0].as_ref().unwrap();
        assert_eq!(a.extras.template_id.as_deref(), Some("7"));
        assert_eq!(a.extras.template_data, Some(json!({"a": 1, "b": 2})));
        let r = success(&p);
        assert_eq!(r.status, 202);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["messageIds"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn errors() {
        let check = |body: Value, c: &str| {
            let r = failure(&call(body).unwrap_err());
            let v: Value = serde_json::from_slice(&r.body).unwrap();
            assert_eq!((r.status, v["code"].as_str().unwrap()), (400, c), "{v}");
        };
        check(
            json!({"to": [{"email": "a@example.test"}], "subject": "s", "textContent": "x"}),
            "missing_parameter",
        );
        check(
            json!({"sender": {"email": "s@example.com"}, "subject": "s", "textContent": "x"}),
            "missing_parameter",
        );
        check(
            json!({"sender": {"email": "s@example.com"}, "to": [{"email": "bad"}], "subject": "s", "textContent": "x"}),
            "invalid_parameter",
        );
        check(
            json!({"sender": {"email": "s@example.com"}, "to": [{"email": "a@example.test"}], "subject": "s"}),
            "missing_parameter",
        );
        let r = failure(&Fail::MissingAuth);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(
            (r.status, v),
            (
                401,
                json!({"code": "unauthorized", "message": "Key not found"})
            )
        );
        assert_eq!(failure(&Fail::Named("credits".into())).status, 402);
        assert_eq!(failure(&Fail::Media("x".into())).status, 406);
    }
}
