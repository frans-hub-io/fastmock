//! SendGrid v3 Mail Send (`POST /v3/mail/send`): one message per personalization, `202 Accepted`
//! with an empty body and `X-Message-Id` (`200` in sandbox mode).

use base64::Engine as _;
use fastmock_core::mail::ProviderExtras;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::build::{Attachment, Fanout, Spec, decode_base64, substitute_all};
use super::{Fail, Parsed, Reply, Request, json_body, norm};

pub const MAX_PERSONALIZATIONS: usize = 1000;
pub const MAX_RECIPIENTS: usize = 1000;
pub const MAX_CATEGORIES: usize = 10;
/// Substitutions per personalization, in bytes.
pub const MAX_SUBSTITUTIONS: usize = 10_000;

/// Headers the Mail Send API does not let a request set.
const RESTRICTED: &[&str] = &[
    "x-sg-id",
    "x-sg-eid",
    "received",
    "dkim-signature",
    "content-type",
    "content-transfer-encoding",
    "to",
    "from",
    "subject",
    "reply-to",
    "cc",
    "bcc",
];

/// `providerError` names (common names; `authorizationRequired` and `payloadTooLarge` as aliases).
pub const ERROR_NAMES: &[&str] = super::COMMON_ERRORS;

/// A new `X-Message-Id`: 22 URL-safe characters.
pub fn message_id() -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(uuid::Uuid::new_v4().as_bytes())
}

#[derive(Deserialize, Clone, Default)]
struct Email {
    #[serde(default)]
    email: String,
    #[serde(default)]
    name: Option<String>,
}

impl Email {
    fn display(&self) -> String {
        match &self.name {
            Some(n) if !n.trim().is_empty() => {
                format!("\"{}\" <{}>", n.replace(['"', '\\'], ""), self.email)
            }
            _ => self.email.clone(),
        }
    }
}

#[derive(Deserialize, Default)]
struct Personalization {
    #[serde(default)]
    to: Vec<Email>,
    #[serde(default)]
    cc: Vec<Email>,
    #[serde(default)]
    bcc: Vec<Email>,
    #[serde(default)]
    from: Option<Email>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    headers: Map<String, Value>,
    #[serde(default)]
    substitutions: Map<String, Value>,
    #[serde(default)]
    dynamic_template_data: Option<Value>,
    #[serde(default)]
    custom_args: Map<String, Value>,
}

#[derive(Deserialize)]
struct ContentPart {
    #[serde(rename = "type")]
    kind: String,
    value: String,
}

#[derive(Deserialize)]
struct Att {
    content: String,
    filename: String,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    disposition: Option<String>,
    #[serde(default)]
    content_id: Option<String>,
}

#[derive(Deserialize, Default)]
struct Toggle {
    #[serde(default)]
    enable: bool,
}

#[derive(Deserialize, Default)]
struct MailSettings {
    #[serde(default)]
    sandbox_mode: Option<Toggle>,
}

#[derive(Deserialize)]
struct MailSend {
    #[serde(default)]
    personalizations: Vec<Personalization>,
    #[serde(default)]
    from: Option<Email>,
    #[serde(default)]
    reply_to: Option<Email>,
    #[serde(default)]
    reply_to_list: Vec<Email>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    content: Vec<ContentPart>,
    #[serde(default)]
    attachments: Vec<Att>,
    #[serde(default)]
    template_id: Option<String>,
    #[serde(default)]
    headers: Map<String, Value>,
    #[serde(default)]
    categories: Vec<String>,
    #[serde(default)]
    custom_args: Map<String, Value>,
    #[serde(default)]
    mail_settings: Option<MailSettings>,
}

fn value_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn emails(list: &[Email], field: &str) -> Result<Vec<String>, Fail> {
    list.iter()
        .enumerate()
        .map(|(i, e)| {
            if e.email.trim().is_empty() {
                Err(Fail::field(
                    &format!("{field}.{i}.email"),
                    "The email parameter is required.",
                ))
            } else {
                Ok(e.display())
            }
        })
        .collect()
}

fn headers(map: &Map<String, Value>, out: &mut Vec<(String, String)>) -> Result<(), Fail> {
    for (k, v) in map {
        if RESTRICTED.contains(&k.to_ascii_lowercase().as_str()) {
            return Err(Fail::field(
                "headers",
                format!("The `{k}` header is a reserved header and cannot be set."),
            ));
        }
        out.retain(|(n, _)| !n.eq_ignore_ascii_case(k));
        out.push((k.clone(), value_str(v)));
    }
    Ok(())
}

/// `POST /v3/mail/send`.
pub fn parse(req: &Request<'_>) -> Result<Parsed, Fail> {
    if !json_body(req.headers) {
        return Err(Fail::Media(
            "Content-Type should be application/json.".into(),
        ));
    }
    let body: MailSend =
        serde_json::from_slice(req.body).map_err(|e| Fail::BadJson(format!("Bad Request: {e}")))?;
    if body.personalizations.is_empty() {
        return Err(Fail::field(
            "personalizations",
            "The personalizations field is required and must have at least one personalization.",
        ));
    }
    if body.personalizations.len() > MAX_PERSONALIZATIONS {
        return Err(Fail::field(
            "personalizations",
            format!("The personalizations field may not exceed {MAX_PERSONALIZATIONS} items."),
        ));
    }
    if body.categories.len() > MAX_CATEGORIES {
        return Err(Fail::field(
            "categories",
            format!("You may not have more than {MAX_CATEGORIES} categories per request."),
        ));
    }
    if body.reply_to.is_some() && !body.reply_to_list.is_empty() {
        return Err(Fail::field(
            "reply_to",
            "reply_to and reply_to_list cannot be used together.",
        ));
    }
    let template = body.template_id.clone().filter(|t| !t.is_empty());
    let dynamic = template.as_deref().is_some_and(|t| t.starts_with("d-"));
    let text = body
        .content
        .iter()
        .find(|c| c.kind.eq_ignore_ascii_case("text/plain"))
        .map(|c| c.value.clone());
    let html = body
        .content
        .iter()
        .find(|c| c.kind.eq_ignore_ascii_case("text/html"))
        .map(|c| c.value.clone());
    if template.is_none() {
        if body.content.is_empty() || body.content.iter().any(|c| c.value.is_empty()) {
            return Err(Fail::field(
                "content",
                "Unless a valid template_id is provided, the content parameter is required. There must be at least one defined content block. We typically suggest both text/plain and text/html blocks are included, but only one block is required.",
            ));
        }
        if text.is_none() && html.is_none() {
            return Err(Fail::field(
                "content.0.type",
                "The content type must be text/plain or text/html.",
            ));
        }
    }
    let mut attachments = Vec::with_capacity(body.attachments.len());
    for (i, a) in body.attachments.iter().enumerate() {
        if a.filename.trim().is_empty() {
            return Err(Fail::field(
                &format!("attachments.{i}.filename"),
                "The attachment filename parameter is required.",
            ));
        }
        let inline = a
            .disposition
            .as_deref()
            .is_some_and(|d| d.eq_ignore_ascii_case("inline"));
        if inline && a.content_id.is_none() {
            return Err(Fail::field(
                &format!("attachments.{i}.content_id"),
                "The content_id parameter is required if your attachment disposition is inline.",
            ));
        }
        attachments.push(Attachment {
            name: a.filename.clone(),
            content_type: a.kind.clone(),
            data: decode_base64(&format!("attachments.{i}.content"), &a.content).map_err(|_| {
                Fail::field(
                    &format!("attachments.{i}.content"),
                    "The attachment content must be base64 encoded.",
                )
            })?,
            content_id: if inline { a.content_id.clone() } else { None },
        });
    }
    let shared = text.as_ref().map_or(0, String::len)
        + html.as_ref().map_or(0, String::len)
        + attachments.iter().map(|a| a.data.len()).sum::<usize>();
    let mut fan = Fanout::new(shared, body.personalizations.len())?;
    let reply_to: Vec<String> = match &body.reply_to {
        Some(r) => vec![r.display()],
        None => emails(&body.reply_to_list, "reply_to_list")?,
    };
    let test_mode = body
        .mail_settings
        .as_ref()
        .and_then(|m| m.sandbox_mode.as_ref())
        .is_some_and(|s| s.enable);
    let id = message_id();
    let mut items = Vec::with_capacity(body.personalizations.len());
    for (i, p) in body.personalizations.iter().enumerate() {
        if p.to.is_empty() {
            return Err(Fail::field(
                &format!("personalizations.{i}.to"),
                "The to array is required for all personalization objects, and must have at least one email object with a valid email address.",
            ));
        }
        if p.to.len() + p.cc.len() + p.bcc.len() > MAX_RECIPIENTS {
            return Err(Fail::field(
                &format!("personalizations.{i}"),
                format!("A personalization may not have more than {MAX_RECIPIENTS} recipients."),
            ));
        }
        let subs_size: usize = p
            .substitutions
            .iter()
            .map(|(k, v)| k.len() + value_str(v).len())
            .sum();
        if subs_size > MAX_SUBSTITUTIONS {
            return Err(Fail::field(
                &format!("personalizations.{i}.substitutions"),
                format!(
                    "The substitutions may not exceed {MAX_SUBSTITUTIONS} bytes per personalization."
                ),
            ));
        }
        let from = p
            .from
            .clone()
            .or_else(|| body.from.clone())
            .filter(|f| !f.email.trim().is_empty())
            .ok_or_else(|| {
                Fail::field(
                    "from.email",
                    "The from object must be provided for every email send. It is an object that requires the email parameter, but may also contain a name parameter.",
                )
            })?;
        let subject = p.subject.clone().or_else(|| body.subject.clone());
        if template.is_none() && subject.as_deref().is_none_or(str::is_empty) {
            return Err(Fail::field(
                "subject",
                "The subject is required. You can get around this requirement if you use a template with a subject defined or if every personalization has a subject defined.",
            ));
        }
        let mut hdrs = Vec::new();
        headers(&body.headers, &mut hdrs)?;
        headers(&p.headers, &mut hdrs)?;
        let mut metadata = body.custom_args.clone();
        metadata.extend(p.custom_args.clone());
        let template_data = if dynamic {
            p.dynamic_template_data.clone()
        } else if template.is_some() && !p.substitutions.is_empty() {
            Some(Value::Object(p.substitutions.clone()))
        } else {
            None
        };
        let pairs: Vec<(String, String)> = p
            .substitutions
            .iter()
            .map(|(k, v)| (k.clone(), value_str(v)))
            .collect();
        let render = |s: &Option<String>| -> Result<Option<String>, Fail> {
            s.as_ref()
                .map(|v| {
                    if dynamic {
                        Ok(v.clone())
                    } else {
                        substitute_all(v, &pairs, req.max_bytes)
                    }
                })
                .transpose()
        };
        let subject = render(&subject)?;
        let (text, html) = if dynamic {
            (None, None)
        } else {
            (render(&text)?, render(&html)?)
        };
        fan.check(
            subject.as_ref().map_or(0, String::len)
                + text.as_ref().map_or(0, String::len)
                + html.as_ref().map_or(0, String::len),
        )?;
        let to = emails(&p.to, &format!("personalizations.{i}.to"))?;
        let spec = Spec {
            from: from.display(),
            display_to: to.join(", "),
            to,
            cc: emails(&p.cc, &format!("personalizations.{i}.cc"))?,
            bcc: emails(&p.bcc, &format!("personalizations.{i}.bcc"))?,
            reply_to: reply_to.clone(),
            subject,
            text,
            html,
            headers: hdrs,
            attachments: attachments.clone(),
            extras: ProviderExtras {
                provider_message_id: Some(id.clone()),
                template_id: template.clone(),
                template_data,
                tags: body.categories.clone(),
                metadata,
                test_mode,
            },
            ..Default::default()
        };
        let m = spec.build(req.max_bytes).map_err(|e| match e {
            Fail::Invalid { message, .. } => Fail::field(&format!("personalizations.{i}"), message),
            other => other,
        })?;
        items.push(Ok(fan.message(m)?));
    }
    Ok(Parsed {
        items,
        test_mode,
        ..Default::default()
    })
}

/// `202 Accepted` (sandbox mode: `200 OK`), empty body, `X-Message-Id`.
pub fn success(parsed: &Parsed) -> Reply {
    if let Some(Err(f)) = parsed.items.iter().find(|i| i.is_err()) {
        return failure(f);
    }
    let id = parsed
        .items
        .first()
        .and_then(|i| i.as_ref().ok())
        .and_then(|m| m.extras.provider_message_id.clone())
        .unwrap_or_default();
    Reply::empty(if parsed.test_mode { 200 } else { 202 }).with_header("x-message-id", id)
}

fn errors(status: u16, field: Option<&str>, message: &str) -> Reply {
    let mut body = json!({"errors": [{"field": field, "message": message, "help": null}]});
    if status == 400 || status == 413 {
        body["id"] = json!(uuid::Uuid::new_v4().simple().to_string());
    }
    Reply::json(status, &body)
}

fn rate_limited() -> Reply {
    let reset = chrono::Utc::now().timestamp() + 60;
    errors(429, None, "too many requests")
        .with_header("x-ratelimit-limit", "600")
        .with_header("x-ratelimit-remaining", "0")
        .with_header("x-ratelimit-reset", reset.to_string())
}

fn named(name: &str) -> Reply {
    match norm(name).as_str() {
        "invalidkey" | "unauthorized" | "authorizationrequired" => errors(
            401,
            None,
            "The provided authorization grant is invalid, expired, or revoked",
        ),
        "forbidden" | "accessforbidden" => errors(403, None, "access forbidden"),
        "validation" | "badrequest" => errors(
            400,
            Some("from.email"),
            "The from email does not contain a valid address.",
        ),
        "messagerejected" | "senderidentity" => errors(
            403,
            Some("from"),
            "The from address does not match a verified Sender Identity. Mail cannot be sent until this error is resolved.",
        ),
        "ratelimit" | "toomanyrequests" => rate_limited(),
        "quota" | "creditsexceeded" => errors(401, None, "Maximum credits exceeded"),
        "templatenotfound" => errors(
            400,
            Some("template_id"),
            "The template_id must be a valid GUID, you provided an invalid template id.",
        ),
        "toolarge" | "payloadtoolarge" => errors(413, None, "content too large"),
        "servererror" => errors(500, None, "internal server error"),
        "unavailable" => errors(503, None, "service unavailable"),
        _ => errors(
            500,
            None,
            &format!("FastMock: unknown providerError `{name}` for sendgrid"),
        ),
    }
}

/// `{"errors": [{"field", "message", "help"}], "id"}`.
pub fn failure(fail: &Fail) -> Reply {
    match fail {
        Fail::MissingAuth => errors(401, None, "authorization required"),
        Fail::BadKey => named("invalidKey"),
        Fail::Invalid { field, message } => errors(400, field.as_deref(), message),
        Fail::BadJson(m) | Fail::Media(m) => errors(400, None, m),
        Fail::TooLarge => errors(413, None, "content too large"),
        Fail::NotFound(m) => errors(404, None, m),
        Fail::Method => errors(405, None, "method not allowed"),
        Fail::Named(n) => named(n),
        Fail::Internal(_) => errors(500, None, "internal server error"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD as B64;

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
    fn sdk_shaped_request_becomes_one_message_per_personalization() {
        let p = call(json!({
            "personalizations": [
                {"to": [{"email": "a@example.test", "name": "Ann"}], "cc": [{"email": "c@example.test"}],
                 "bcc": [{"email": "b@example.test"}], "substitutions": {"-name-": "Ann"},
                 "custom_args": {"order": "42"}, "headers": {"X-P": "1"}},
                {"to": [{"email": "d@example.test"}], "subject": "Own subject", "substitutions": {"-name-": "Dan"}}
            ],
            "from": {"email": "shop@example.com", "name": "Shop"},
            "reply_to": {"email": "help@example.com"},
            "subject": "Hi -name-",
            "content": [{"type": "text/plain", "value": "Hello -name-"}, {"type": "text/html", "value": "<p>Hello -name-</p>"}],
            "attachments": [{"content": B64.encode("x"), "filename": "a.txt", "type": "text/plain"},
                            {"content": B64.encode([1u8]), "filename": "l.png", "type": "image/png", "disposition": "inline", "content_id": "logo"}],
            "categories": ["welcome"],
            "custom_args": {"tenant": "t1"},
            "headers": {"X-Top": "t"}
        }))
        .unwrap();
        assert_eq!(p.items.len(), 2);
        let a = p.items[0].as_ref().unwrap();
        assert_eq!(
            a.rcpt_to,
            vec!["a@example.test", "c@example.test", "b@example.test"]
        );
        assert_eq!(a.extras.tags, vec!["welcome"]);
        assert_eq!(a.extras.metadata["tenant"], "t1");
        assert_eq!(a.extras.metadata["order"], "42");
        let raw = String::from_utf8_lossy(&a.raw);
        assert!(
            raw.contains("Subject: Hi Ann") && raw.contains("Hello Ann"),
            "{raw}"
        );
        assert!(raw.contains("X-Top: t") && raw.contains("X-P: 1"));
        let d = p.items[1].as_ref().unwrap();
        assert!(String::from_utf8_lossy(&d.raw).contains("Subject: Own subject"));
        assert_eq!(a.extras.provider_message_id, d.extras.provider_message_id);
        let r = success(&p);
        assert_eq!(r.status, 202);
        assert!(r.body.is_empty());
        assert_eq!(
            r.header("x-message-id"),
            a.extras.provider_message_id.as_deref()
        );
        assert_eq!(r.header("x-message-id").unwrap().len(), 22);
    }

    #[test]
    fn dynamic_templates_and_sandbox() {
        let p = call(json!({
            "personalizations": [{"to": [{"email": "a@example.test"}], "dynamic_template_data": {"code": "1234"}}],
            "from": {"email": "shop@example.com"},
            "template_id": "d-0123456789abcdef",
            "mail_settings": {"sandbox_mode": {"enable": true}}
        }))
        .unwrap();
        let m = p.items[0].as_ref().unwrap();
        assert_eq!(m.extras.template_id.as_deref(), Some("d-0123456789abcdef"));
        assert_eq!(m.extras.template_data, Some(json!({"code": "1234"})));
        assert!(m.extras.test_mode && p.test_mode);
        assert_eq!(success(&p).status, 200);
    }

    #[test]
    fn validation_errors_name_the_field() {
        let check = |body: Value, field: &str| {
            let e = call(body).unwrap_err();
            let r = failure(&e);
            assert_eq!(r.status, 400);
            let v: Value = serde_json::from_slice(&r.body).unwrap();
            assert_eq!(v["errors"][0]["field"], json!(field), "{v}");
            assert!(v["id"].is_string());
        };
        let base = || {
            json!({"personalizations": [{"to": [{"email": "a@example.test"}]}],
                   "from": {"email": "s@example.com"}, "subject": "s",
                   "content": [{"type": "text/plain", "value": "x"}]})
        };
        check(
            json!({"from": {"email": "s@example.com"}}),
            "personalizations",
        );
        let mut b = base();
        b.as_object_mut().unwrap().remove("from");
        check(b, "from.email");
        let mut b = base();
        b.as_object_mut().unwrap().remove("subject");
        check(b, "subject");
        let mut b = base();
        b.as_object_mut().unwrap().remove("content");
        check(b, "content");
        let mut b = base();
        b["headers"] = json!({"From": "x@example.com"});
        check(b, "headers");
        let mut b = base();
        b["categories"] = json!((0..11).map(|i| i.to_string()).collect::<Vec<_>>());
        check(b, "categories");
        let mut b = base();
        b["personalizations"][0]["to"] = json!([]);
        check(b, "personalizations.0.to");
        let r = failure(&Fail::MissingAuth);
        assert_eq!(r.status, 401);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(
            v,
            json!({"errors": [{"field": null, "message": "authorization required", "help": null}]})
        );
        let r = failure(&Fail::Named("rateLimit".into()));
        assert_eq!(r.status, 429);
        assert_eq!(r.header("x-ratelimit-remaining"), Some("0"));
    }
}
