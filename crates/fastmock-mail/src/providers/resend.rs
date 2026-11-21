//! Resend `POST /emails`: `200 {"id": "<uuid>"}`; errors `{"statusCode", "name", "message"}`.

use fastmock_core::mail::ProviderExtras;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::build::{Attachment, Spec, address_of, decode_base64};
use super::{Fail, Parsed, Reply, Request, header, json_body, norm};

pub const MAX_RECIPIENTS: usize = 50;
/// Longest `Idempotency-Key`.
pub const MAX_IDEMPOTENCY_KEY: usize = 256;

/// `providerError` names: the common ones and the Resend error names (`rate_limit_exceeded`,
/// `daily_quota_exceeded`, `validation_error`, ...).
pub const ERROR_NAMES: &[&str] = &[
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
    "missing_api_key",
    "restricted_api_key",
    "invalid_api_key",
    "validation_error",
    "invalid_idempotency_key",
    "invalid_idempotent_request",
    "concurrent_idempotent_requests",
    "invalid_attachment",
    "invalid_from_address",
    "invalid_parameter",
    "missing_required_field",
    "monthly_quota_exceeded",
    "daily_quota_exceeded",
    "rate_limit_exceeded",
    "security_error",
    "not_found",
    "application_error",
    "internal_server_error",
];

/// Resend error names with their status.
const NAMES: &[(&str, u16, &str)] = &[
    (
        "missing_api_key",
        401,
        "Missing API key in the authorization header. Include the following header 'Authorization: Bearer YOUR_API_KEY' in the request.",
    ),
    (
        "restricted_api_key",
        401,
        "This API key is restricted to only send emails.",
    ),
    ("invalid_api_key", 403, "API key is invalid"),
    ("validation_error", 422, "The request is invalid."),
    (
        "invalid_idempotency_key",
        400,
        "The key must be between 1-256 chars.",
    ),
    (
        "invalid_idempotent_request",
        409,
        "Same idempotency key used with a different request payload. Change your idempotency key or payload.",
    ),
    (
        "concurrent_idempotent_requests",
        409,
        "Same idempotency key used while original request is still in progress. Try the request again later.",
    ),
    (
        "invalid_attachment",
        422,
        "Attachment must have either a `content` or `path`.",
    ),
    (
        "invalid_from_address",
        422,
        "Invalid `from` field. The email address needs to follow the `email@example.com` or `Name <email@example.com>` format.",
    ),
    (
        "invalid_access",
        422,
        "Access must be \"full_access\" | \"sending_access\".",
    ),
    ("invalid_parameter", 422, "The parameter is invalid."),
    (
        "invalid_region",
        422,
        "Region must be \"us-east-1\" | \"eu-west-1\" | \"sa-east-1\".",
    ),
    ("missing_required_field", 422, "Missing required field."),
    (
        "monthly_quota_exceeded",
        429,
        "You have reached your monthly email sending quota. Consider upgrading your plan.",
    ),
    (
        "daily_quota_exceeded",
        429,
        "You have reached your daily email sending quota. Consider upgrading your plan.",
    ),
    (
        "rate_limit_exceeded",
        429,
        "Too many requests. You can only make 2 requests per second. See rate limit response headers for more information.",
    ),
    (
        "security_error",
        451,
        "We may have found a security issue with the request.",
    ),
    ("not_found", 404, "The requested endpoint does not exist."),
    (
        "method_not_allowed",
        405,
        "Method is not allowed for the requested path.",
    ),
    ("application_error", 500, "An unexpected error occurred."),
    (
        "internal_server_error",
        500,
        "An unexpected error occurred.",
    ),
];

fn err(name: &str, message: impl Into<String>) -> Fail {
    let status = NAMES
        .iter()
        .find(|(n, ..)| *n == name)
        .map_or(422, |(_, s, _)| *s);
    Fail::Invalid {
        field: Some(format!("{status}:{name}")),
        message: message.into(),
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    fn list(self) -> Vec<String> {
        match self {
            OneOrMany::One(s) if s.trim().is_empty() => vec![],
            OneOrMany::One(s) => vec![s],
            OneOrMany::Many(v) => v,
        }
    }
}

#[derive(Deserialize)]
struct Att {
    #[serde(default)]
    content: Option<Value>,
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default, alias = "contentType")]
    content_type: Option<String>,
    #[serde(default, alias = "contentId", alias = "inline_content_id")]
    content_id: Option<String>,
}

#[derive(Deserialize)]
struct Tag {
    name: String,
    #[serde(default)]
    value: Option<String>,
}

#[derive(Deserialize)]
struct TemplateRef {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    variables: Option<Value>,
}

#[derive(Deserialize)]
struct SendEmail {
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<OneOrMany>,
    #[serde(default)]
    cc: Option<OneOrMany>,
    #[serde(default)]
    bcc: Option<OneOrMany>,
    #[serde(default, alias = "replyTo")]
    reply_to: Option<OneOrMany>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    html: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    headers: Map<String, Value>,
    #[serde(default)]
    attachments: Vec<Att>,
    #[serde(default)]
    tags: Vec<Tag>,
    #[serde(default)]
    template: Option<TemplateRef>,
}

fn content_bytes(v: &Value) -> Result<Vec<u8>, Fail> {
    let bytes = |a: &Vec<Value>| -> Option<Vec<u8>> {
        a.iter()
            .map(|x| x.as_u64().filter(|n| *n <= 255).map(|n| n as u8))
            .collect()
    };
    match v {
        Value::String(s) => decode_base64("attachment content", s)
            .map_err(|_| err("invalid_attachment", "Attachment content must be base64.")),
        Value::Array(a) => {
            bytes(a).ok_or_else(|| err("invalid_attachment", "Invalid attachment content."))
        }
        Value::Object(o) => o
            .get("data")
            .and_then(Value::as_array)
            .and_then(bytes)
            .ok_or_else(|| err("invalid_attachment", "Invalid attachment content.")),
        _ => Err(err("invalid_attachment", "Invalid attachment content.")),
    }
}

fn addresses(field: &str, v: Option<OneOrMany>) -> Result<Vec<String>, Fail> {
    let list = v.map(OneOrMany::list).unwrap_or_default();
    for a in &list {
        if address_of(a).is_err() {
            return Err(err(
                "validation_error",
                format!(
                    "Invalid `{field}` field. The email address needs to follow the `email@example.com` or `Name <email@example.com>` format."
                ),
            ));
        }
    }
    Ok(list)
}

/// `POST /emails`.
pub fn parse(req: &Request<'_>) -> Result<Parsed, Fail> {
    if header(req.headers, "user-agent").is_none_or(|u| u.trim().is_empty()) {
        return Err(Fail::Invalid {
            field: Some("403:validation_error".into()),
            message: "The User-Agent header is required.".into(),
        });
    }
    if !json_body(req.headers) {
        return Err(err(
            "validation_error",
            "Content-Type must be application/json.",
        ));
    }
    if let Some(k) = header(req.headers, "idempotency-key")
        && (k.is_empty() || k.len() > MAX_IDEMPOTENCY_KEY)
    {
        return Err(err(
            "invalid_idempotency_key",
            "The key must be between 1-256 chars.",
        ));
    }
    let body: SendEmail = serde_json::from_slice(req.body).map_err(|e| Fail::Invalid {
        field: Some("400:validation_error".into()),
        message: format!("Invalid JSON body: {e}"),
    })?;
    let from = body.from.clone().unwrap_or_default();
    if from.trim().is_empty() && body.template.is_none() {
        return Err(err("missing_required_field", "Missing `from` field."));
    }
    if !from.trim().is_empty() && address_of(&from).is_err() {
        return Err(err(
            "validation_error",
            "Invalid `from` field. The email address needs to follow the `email@example.com` or `Name <email@example.com>` format.",
        ));
    }
    let to = addresses("to", body.to)?;
    if to.is_empty() {
        return Err(err("missing_required_field", "Missing `to` field."));
    }
    if to.len() > MAX_RECIPIENTS {
        return Err(err(
            "validation_error",
            format!("Too many recipients in `to`. The maximum is {MAX_RECIPIENTS}."),
        ));
    }
    let (template_id, template_data) = match &body.template {
        Some(t) => {
            let id =
                t.id.clone()
                    .filter(|i| !i.is_empty())
                    .ok_or_else(|| err("missing_required_field", "Missing `template.id` field."))?;
            if body.html.is_some() || body.text.is_some() {
                return Err(err(
                    "validation_error",
                    "`template` cannot be used together with `html`, `text` or `react`.",
                ));
            }
            (Some(id), t.variables.clone())
        }
        None => (None, None),
    };
    if template_id.is_none() && body.subject.as_deref().is_none_or(|s| s.trim().is_empty()) {
        return Err(err("missing_required_field", "Missing `subject` field."));
    }
    let mut attachments = Vec::with_capacity(body.attachments.len());
    for a in &body.attachments {
        let data = match (&a.content, &a.path) {
            (Some(c), _) => content_bytes(c)?,
            (None, Some(_)) => Vec::new(),
            (None, None) => {
                return Err(err(
                    "invalid_attachment",
                    "Attachment must have either a `content` or `path`.",
                ));
            }
        };
        let name = a
            .filename
            .clone()
            .or_else(|| {
                a.path
                    .as_deref()
                    .and_then(|p| p.rsplit('/').next())
                    .map(String::from)
            })
            .unwrap_or_else(|| "attachment".into());
        attachments.push(Attachment {
            name,
            content_type: a.content_type.clone(),
            data,
            content_id: a.content_id.clone(),
        });
    }
    let metadata = body
        .tags
        .iter()
        .map(|t| {
            (
                t.name.clone(),
                Value::String(t.value.clone().unwrap_or_default()),
            )
        })
        .collect();
    let spec = Spec {
        from: if from.trim().is_empty() {
            "template-sender@fastmock.invalid".into()
        } else {
            from
        },
        display_to: to.join(", "),
        to,
        cc: addresses("cc", body.cc)?,
        bcc: addresses("bcc", body.bcc)?,
        reply_to: addresses("reply_to", body.reply_to)?,
        subject: body.subject,
        text: body.text,
        html: body.html,
        headers: body
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
            .collect(),
        attachments,
        extras: ProviderExtras {
            provider_message_id: Some(uuid::Uuid::new_v4().to_string()),
            template_id,
            template_data,
            metadata,
            ..Default::default()
        },
        ..Default::default()
    };
    let m = spec.build(req.max_bytes).map_err(|e| match e {
        Fail::Invalid { message, .. } => err("validation_error", message),
        other => other,
    })?;
    Ok(Parsed::one(m))
}

/// `200 {"id": ...}`.
pub fn success(parsed: &Parsed) -> Reply {
    match parsed.items.first() {
        Some(Ok(m)) => Reply::json(200, &json!({"id": m.extras.provider_message_id})),
        Some(Err(f)) => failure(f),
        None => failure(&Fail::invalid("no message")),
    }
}

fn reply(status: u16, name: &str, message: &str) -> Reply {
    let r = Reply::json(
        status,
        &json!({"statusCode": status, "name": name, "message": message}),
    );
    if status == 429 {
        r.with_header("ratelimit-limit", "2")
            .with_header("ratelimit-remaining", "0")
            .with_header("ratelimit-reset", "1")
            .with_header("retry-after", "1")
    } else {
        r
    }
}

fn named(name: &str) -> Reply {
    let key = match norm(name).as_str() {
        "invalidkey" => "invalid_api_key",
        "forbidden" | "messagerejected" => {
            return reply(
                403,
                "validation_error",
                "The example.com domain is not verified. Please, add and verify your domain.",
            );
        }
        "validation" => "validation_error",
        "ratelimit" => "rate_limit_exceeded",
        "quota" => "daily_quota_exceeded",
        "templatenotfound" => {
            return reply(404, "not_found", "Template not found");
        }
        "toolarge" => {
            return reply(
                422,
                "invalid_attachment",
                "Attachments exceed the 40 MB limit.",
            );
        }
        "servererror" => "application_error",
        "unavailable" => {
            return reply(503, "internal_server_error", "Service unavailable.");
        }
        _ => name,
    };
    match NAMES.iter().find(|(n, ..)| *n == key) {
        Some((n, s, m)) => reply(*s, n, m),
        None => reply(
            500,
            "application_error",
            &format!("FastMock: unknown providerError `{name}` for resend"),
        ),
    }
}

/// `{"statusCode", "name", "message"}`.
pub fn failure(fail: &Fail) -> Reply {
    match fail {
        Fail::Invalid {
            field: Some(f),
            message,
        } if f.contains(':') => {
            let (s, n) = f.split_once(':').unwrap_or_default();
            reply(s.parse().unwrap_or(422), n, message)
        }
        Fail::MissingAuth => named("missing_api_key"),
        Fail::BadKey => named("invalid_api_key"),
        Fail::Invalid { message, .. } | Fail::BadJson(message) | Fail::Media(message) => {
            reply(422, "validation_error", message)
        }
        Fail::TooLarge => named("tooLarge"),
        Fail::NotFound(_) => named("not_found"),
        Fail::Method => named("method_not_allowed"),
        Fail::Named(n) => named(n),
        Fail::Internal(_) => named("application_error"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(body: Value, ua: bool) -> Result<Parsed, Fail> {
        let bytes = serde_json::to_vec(&body).unwrap();
        let mut h = vec![("content-type".to_string(), "application/json".to_string())];
        if ua {
            h.push(("user-agent".into(), "resend-node:6.0.0".into()));
        }
        parse(&Request {
            headers: &h,
            query: "",
            body: &bytes,
            form: &[],
            max_bytes: 1 << 20,
        })
    }

    #[test]
    fn sdk_request_and_answer() {
        let p = call(
            json!({"from": "Acme <onboarding@example.com>", "to": ["a@example.test"], "cc": "c@example.test",
                   "subject": "Hello", "html": "<p>Hi</p>", "reply_to": "help@example.com",
                   "headers": {"X-Entity-Ref-ID": "123"},
                   "attachments": [{"filename": "a.txt", "content": "aGk="}, {"filename": "b.bin", "content": {"type": "Buffer", "data": [1, 2]}}],
                   "tags": [{"name": "category", "value": "confirm_email"}]}),
            true,
        )
        .unwrap();
        let m = p.items[0].as_ref().unwrap();
        assert_eq!(m.rcpt_to, vec!["a@example.test", "c@example.test"]);
        assert_eq!(m.extras.metadata["category"], "confirm_email");
        let v: Value = serde_json::from_slice(&success(&p).body).unwrap();
        assert_eq!(v, json!({"id": m.extras.provider_message_id}));
        let p = call(
            json!({"from": "a@example.com", "to": "a@example.test", "template": {"id": "tpl_1", "variables": {"x": 1}}}),
            true,
        )
        .unwrap();
        let m = p.items[0].as_ref().unwrap();
        assert_eq!(m.extras.template_id.as_deref(), Some("tpl_1"));
        assert_eq!(m.extras.template_data, Some(json!({"x": 1})));
    }

    #[test]
    fn errors_are_typed() {
        let check = |body: Value, ua: bool, status: u16, name: &str| {
            let r = failure(&call(body, ua).unwrap_err());
            let v: Value = serde_json::from_slice(&r.body).unwrap();
            assert_eq!(
                (r.status, v["name"].as_str().unwrap()),
                (status, name),
                "{v}"
            );
            assert_eq!(v["statusCode"], status);
        };
        check(json!({}), false, 403, "validation_error");
        check(
            json!({"to": "a@example.test", "subject": "s"}),
            true,
            422,
            "missing_required_field",
        );
        check(
            json!({"from": "a@example.com", "subject": "s"}),
            true,
            422,
            "missing_required_field",
        );
        check(
            json!({"from": "bad", "to": "a@example.test", "subject": "s"}),
            true,
            422,
            "validation_error",
        );
        let many: Vec<String> = (0..51).map(|i| format!("u{i}@example.test")).collect();
        check(
            json!({"from": "a@example.com", "to": many, "subject": "s"}),
            true,
            422,
            "validation_error",
        );
        let r = failure(&Fail::MissingAuth);
        assert_eq!(r.status, 401);
        let r = failure(&Fail::Named("rateLimit".into()));
        assert_eq!(r.status, 429);
        assert_eq!(r.header("retry-after"), Some("1"));
        assert_eq!(
            failure(&Fail::Named("daily_quota_exceeded".into())).status,
            429
        );
    }
}
