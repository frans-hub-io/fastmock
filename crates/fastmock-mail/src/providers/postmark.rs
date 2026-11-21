//! Postmark `POST /email`, `/email/withTemplate`, `/email/batch`, `/email/batchWithTemplates`
//! (PascalCase JSON). Batches answer `200` with one result per message in request order.

use fastmock_core::mail::ProviderExtras;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::build::{Attachment, Spec, address_of, decode_base64, split_list};
use super::{Fail, OutMessage, Parsed, Reply, Request, header, json_body, media_type, norm};

/// The token that validates without sending; always accepted, stored with `testMode`.
pub const TEST_TOKEN: &str = "POSTMARK_API_TEST";
pub const MAX_RECIPIENTS: usize = 50;
pub const MAX_BATCH: usize = 500;

/// `providerError` names: the common ones, `inactiveRecipient`, `accountPending`, and any Postmark
/// `ErrorCode` as digits (for example `406`), answered with HTTP 422.
pub const ERROR_NAMES: &[&str] = &[
    "invalidKey",
    "forbidden",
    "validation",
    "messageRejected",
    "inactiveRecipient",
    "accountPending",
    "rateLimit",
    "quota",
    "templateNotFound",
    "tooLarge",
    "serverError",
    "unavailable",
];

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct NameValue {
    name: String,
    value: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Att {
    name: String,
    content: String,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default, rename = "ContentID")]
    content_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Message {
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    cc: Option<String>,
    #[serde(default)]
    bcc: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    html_body: Option<String>,
    #[serde(default)]
    text_body: Option<String>,
    #[serde(default)]
    reply_to: Option<String>,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    headers: Vec<NameValue>,
    #[serde(default)]
    metadata: Map<String, Value>,
    #[serde(default)]
    attachments: Vec<Att>,
    #[serde(default)]
    template_id: Option<Value>,
    #[serde(default)]
    template_alias: Option<String>,
    #[serde(default)]
    template_model: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TemplateBatch {
    #[serde(default)]
    messages: Vec<Value>,
}

/// A Postmark error: HTTP status, `ErrorCode`, `Message`.
fn code(status: u16, error_code: u32, m: &str) -> Fail {
    Fail::Invalid {
        field: Some(format!("{status}:{error_code}")),
        message: m.to_string(),
    }
}

fn list(field: &str, s: &Option<String>) -> Result<Vec<String>, Fail> {
    let items = s.as_deref().map(split_list).unwrap_or_default();
    for a in &items {
        if address_of(a).is_err() {
            return Err(code(
                422,
                300,
                &format!(
                    "Error parsing '{field}': Illegal email address '{}'. It must contain the '@' symbol.",
                    a.chars().take(200).collect::<String>()
                ),
            ));
        }
    }
    Ok(items)
}

fn message(v: Value, template: bool, test: bool, max_bytes: usize) -> Result<OutMessage, Fail> {
    let m: Message = serde_json::from_value(v)
        .map_err(|e| code(422, 402, &format!("Received invalid JSON input: {e}")))?;
    let from = m.from.clone().unwrap_or_default();
    if from.trim().is_empty() || address_of(&from).is_err() {
        return Err(code(
            422,
            300,
            &format!(
                "Invalid 'From' address: '{}'.",
                from.chars().take(200).collect::<String>()
            ),
        ));
    }
    let to = list("To", &m.to)?;
    let cc = list("Cc", &m.cc)?;
    let bcc = list("Bcc", &m.bcc)?;
    if to.is_empty() && cc.is_empty() && bcc.is_empty() {
        return Err(code(422, 300, "Zero recipients specified"));
    }
    if to.len() + cc.len() + bcc.len() > MAX_RECIPIENTS {
        return Err(code(
            422,
            300,
            &format!(
                "Too many recipients: the maximum is {MAX_RECIPIENTS} (To, Cc and Bcc together)."
            ),
        ));
    }
    let template_id = match (&m.template_id, &m.template_alias) {
        (Some(Value::Number(n)), _) => Some(n.to_string()),
        (Some(Value::String(s)), _) if !s.is_empty() => Some(s.clone()),
        (_, Some(a)) if !a.is_empty() => Some(a.clone()),
        _ => None,
    };
    if template {
        if template_id.is_none() {
            return Err(code(
                422,
                1101,
                "The 'TemplateId' or 'TemplateAlias' associated with this request is not valid or was not found.",
            ));
        }
        if m.template_model.is_none() {
            return Err(code(422, 1109, "The 'TemplateModel' field is required."));
        }
    } else if m.html_body.as_deref().is_none_or(str::is_empty)
        && m.text_body.as_deref().is_none_or(str::is_empty)
    {
        return Err(code(
            422,
            300,
            "Provide either email TextBody or HtmlBody or both.",
        ));
    }
    let mut attachments = Vec::with_capacity(m.attachments.len());
    for a in m.attachments {
        attachments.push(Attachment {
            data: decode_base64("Attachment content", &a.content)
                .map_err(|e| code(422, 300, &e.message()))?,
            name: a.name,
            content_type: a.content_type,
            content_id: a.content_id.filter(|c| !c.is_empty()),
        });
    }
    let spec = Spec {
        from,
        display_to: m.to.clone().unwrap_or_default(),
        to,
        cc,
        bcc,
        reply_to: m.reply_to.as_deref().map(split_list).unwrap_or_default(),
        subject: if template { None } else { m.subject },
        text: if template { None } else { m.text_body },
        html: if template { None } else { m.html_body },
        headers: m.headers.into_iter().map(|h| (h.name, h.value)).collect(),
        attachments,
        extras: ProviderExtras {
            provider_message_id: Some(uuid::Uuid::new_v4().to_string()),
            template_id: if template { template_id } else { None },
            template_data: if template { m.template_model } else { None },
            tags: m.tag.into_iter().filter(|t| !t.is_empty()).collect(),
            metadata: m.metadata,
            test_mode: test,
        },
        ..Default::default()
    };
    spec.build(max_bytes).map_err(|e| match e {
        Fail::Invalid { message, .. } => code(422, 300, &message),
        other => other,
    })
}

fn check_media(req: &Request<'_>) -> Result<(), Fail> {
    if !json_body(req.headers) || media_type(req.headers).is_none() {
        return Err(Fail::Media("Content-Type must be application/json.".into()));
    }
    if let Some(a) = header(req.headers, "accept")
        && !(a.contains("application/json") || a.contains("*/*"))
    {
        return Err(Fail::Media("Accept must allow application/json.".into()));
    }
    Ok(())
}

fn body(req: &Request<'_>) -> Result<Value, Fail> {
    check_media(req)?;
    serde_json::from_slice(req.body).map_err(|_| code(422, 402, "Received invalid JSON input."))
}

fn test_token(req: &Request<'_>) -> bool {
    header(req.headers, "x-postmark-server-token").is_some_and(|t| t.trim() == TEST_TOKEN)
}

/// `POST /email` and `/email/withTemplate`.
pub fn parse_one(req: &Request<'_>, template: bool) -> Result<Parsed, Fail> {
    let v = body(req)?;
    if !v.is_object() {
        return Err(code(422, 402, "Received invalid JSON input."));
    }
    let test = test_token(req);
    let m = message(v, template, test, req.max_bytes)?;
    Ok(Parsed::one(m))
}

/// `POST /email/batch` (a JSON array) and `/email/batchWithTemplates` (`{"Messages": [...]}`).
pub fn parse_batch(req: &Request<'_>, template: bool) -> Result<Parsed, Fail> {
    let v = body(req)?;
    let list = if template {
        serde_json::from_value::<TemplateBatch>(v)
            .map_err(|_| code(422, 402, "Received invalid JSON input."))?
            .messages
    } else {
        match v {
            Value::Array(a) => a,
            _ => return Err(code(422, 402, "Received invalid JSON input.")),
        }
    };
    if list.len() > MAX_BATCH {
        return Err(code(
            422,
            410,
            &format!("Too many batch messages: the maximum is {MAX_BATCH}."),
        ));
    }
    let test = test_token(req);
    let items = list
        .into_iter()
        .map(|m| message(m, template, test, req.max_bytes))
        .collect();
    Ok(Parsed {
        items,
        test_mode: test,
        ..Default::default()
    })
}

fn submitted_at() -> String {
    let now = chrono::Utc::now();
    format!(
        "{}.{:07}+00:00",
        now.format("%Y-%m-%dT%H:%M:%S"),
        now.timestamp_subsec_nanos() / 100
    )
}

fn result(m: &OutMessage) -> Value {
    json!({
        "To": m.display_to,
        "SubmittedAt": submitted_at(),
        "MessageID": m.extras.provider_message_id,
        "ErrorCode": 0,
        "Message": if m.extras.test_mode { "Test job accepted" } else { "OK" },
    })
}

fn parts(f: &Fail) -> (u16, u32, String) {
    match f {
        Fail::Invalid {
            field: Some(c),
            message,
        } if c.contains(':') => {
            let (s, e) = c.split_once(':').unwrap_or_default();
            (s.parse().unwrap_or(422), e.parse().unwrap_or(300), message.clone())
        }
        Fail::MissingAuth => (
            401,
            10,
            "No Account or Server API tokens were supplied in the HTTP headers. Please add a header for either X-Postmark-Server-Token or X-Postmark-Account-Token.".into(),
        ),
        Fail::BadKey => (
            401,
            10,
            "The Server Token you provided in the X-Postmark-Server-Token request header was invalid. Please verify that you are using a valid token.".into(),
        ),
        Fail::Invalid { message, .. } => (422, 300, message.clone()),
        Fail::BadJson(_) => (422, 402, "Received invalid JSON input.".into()),
        Fail::TooLarge => (413, 300, "The request exceeds the size limit.".into()),
        Fail::Media(m) => (415, 0, m.clone()),
        Fail::NotFound(m) => (404, 0, m.clone()),
        Fail::Method => (405, 0, "Method not allowed.".into()),
        Fail::Named(n) => named(n),
        Fail::Internal(_) => (500, 101, "Internal Server Error".into()),
    }
}

fn named(name: &str) -> (u16, u32, String) {
    let n = norm(name);
    if let Ok(c) = n.parse::<u32>() {
        return (422, c, format!("FastMock chaos: Postmark error {c}"));
    }
    match n.as_str() {
        "invalidkey" | "unauthorized" => parts(&Fail::BadKey),
        "forbidden" | "accountpending" => (
            422,
            412,
            "Your account is pending approval. You can only send to recipients on the same domain as the sender.".into(),
        ),
        "validation" => (422, 300, "Invalid email request".into()),
        "messagerejected" | "inactiverecipient" => (
            422,
            406,
            "You tried to send to recipient(s) that have been marked as inactive. Found inactive addresses. Inactive recipients are ones that have generated a hard bounce, a spam complaint, or a manual suppression.".into(),
        ),
        "ratelimit" | "toomanyrequests" => (429, 429, "Rate limit exceeded.".into()),
        "quota" => (
            422,
            405,
            "Not allowed to send: you have run out of credits.".into(),
        ),
        "templatenotfound" => (
            422,
            1101,
            "The 'TemplateId' or 'TemplateAlias' associated with this request is not valid or was not found.".into(),
        ),
        "toolarge" => (413, 300, "The request exceeds the size limit.".into()),
        "servererror" => (500, 101, "Internal Server Error".into()),
        "unavailable" => (503, 100, "Service Unavailable".into()),
        _ => (
            500,
            101,
            format!("FastMock: unknown providerError `{name}` for postmark"),
        ),
    }
}

fn error_value(f: &Fail) -> Value {
    let (_, c, m) = parts(f);
    json!({"ErrorCode": c, "Message": m})
}

/// `{"ErrorCode": n, "Message": ...}` with the code also in `X-PM-ApiErrorCode`.
pub fn failure(fail: &Fail) -> Reply {
    let (status, c, m) = parts(fail);
    Reply::json(status, &json!({"ErrorCode": c, "Message": m}))
        .with_header("x-pm-apierrorcode", c.to_string())
}

/// `{"To", "SubmittedAt", "MessageID", "ErrorCode": 0, "Message": "OK"}`.
pub fn success_one(parsed: &Parsed) -> Reply {
    match parsed.items.first() {
        Some(Ok(m)) => Reply::json(200, &result(m)),
        Some(Err(f)) => failure(f),
        None => failure(&Fail::invalid("no message")),
    }
}

/// An array of results in request order; failed messages carry their `ErrorCode`.
pub fn success_batch(parsed: &Parsed) -> Reply {
    let out: Vec<Value> = parsed
        .items
        .iter()
        .map(|i| match i {
            Ok(m) => result(m),
            Err(f) => error_value(f),
        })
        .collect();
    Reply::json(200, &Value::Array(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(token: &str, body: &Value, template: bool, batch: bool) -> Result<Parsed, Fail> {
        let bytes = serde_json::to_vec(body).unwrap();
        let h = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("accept".to_string(), "application/json".to_string()),
            ("x-postmark-server-token".to_string(), token.to_string()),
        ];
        let req = Request {
            headers: &h,
            query: "",
            body: &bytes,
            form: &[],
            max_bytes: 1 << 20,
        };
        if batch {
            parse_batch(&req, template)
        } else {
            parse_one(&req, template)
        }
    }

    #[test]
    fn single_message_and_answer() {
        let p = call(
            "server-token",
            &json!({
                "From": "Shop <shop@example.com>", "To": "a@example.test, \"B, Bee\" <b@example.test>",
                "Bcc": "c@example.test", "Subject": "Hi", "TextBody": "Code 1", "HtmlBody": "<b>1</b>",
                "ReplyTo": "help@example.com", "Tag": "otp", "Metadata": {"order": "42"},
                "Headers": [{"Name": "X-Trace", "Value": "t"}],
                "Attachments": [{"Name": "a.txt", "Content": "aGk=", "ContentType": "text/plain"}],
                "MessageStream": "outbound", "TrackOpens": true
            }),
            false,
            false,
        )
        .unwrap();
        let m = p.items[0].as_ref().unwrap();
        assert_eq!(
            m.rcpt_to,
            vec!["a@example.test", "b@example.test", "c@example.test"]
        );
        assert_eq!(m.extras.tags, vec!["otp"]);
        assert_eq!(m.extras.metadata["order"], "42");
        let r = success_one(&p);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["To"], "a@example.test, \"B, Bee\" <b@example.test>");
        assert_eq!(v["ErrorCode"], 0);
        assert_eq!(v["Message"], "OK");
        assert_eq!(v["MessageID"], json!(m.extras.provider_message_id));
        assert!(v["SubmittedAt"].as_str().unwrap().ends_with("+00:00"));

        let p = call(
            TEST_TOKEN,
            &json!({"From": "s@example.com", "To": "a@example.test", "TemplateAlias": "welcome", "TemplateModel": {"name": "Ann"}}),
            true,
            false,
        )
        .unwrap();
        let m = p.items[0].as_ref().unwrap();
        assert!(m.extras.test_mode);
        assert_eq!(m.extras.template_id.as_deref(), Some("welcome"));
        assert_eq!(m.extras.template_data, Some(json!({"name": "Ann"})));
        let v: Value = serde_json::from_slice(&success_one(&p).body).unwrap();
        assert_eq!(v["Message"], "Test job accepted");
    }

    #[test]
    fn batches_report_each_message() {
        let p = call(
            "t",
            &json!([
                {"From": "s@example.com", "To": "a@example.test", "TextBody": "x"},
                {"From": "s@example.com", "To": "a@example.test"},
                {"From": "", "To": "a@example.test", "TextBody": "x"}
            ]),
            false,
            true,
        )
        .unwrap();
        let v: Value = serde_json::from_slice(&success_batch(&p).body).unwrap();
        assert_eq!(v[0]["ErrorCode"], 0);
        assert_eq!(
            v[1],
            json!({"ErrorCode": 300, "Message": "Provide either email TextBody or HtmlBody or both."})
        );
        assert_eq!(v[2]["Message"], "Invalid 'From' address: ''.");
        let p = call(
            "t",
            &json!({"Messages": [{"From": "s@example.com", "To": "a@example.test", "TemplateId": 12345, "TemplateModel": {}}]}),
            true,
            true,
        )
        .unwrap();
        assert_eq!(
            p.items[0].as_ref().unwrap().extras.template_id.as_deref(),
            Some("12345")
        );
        let many: Vec<Value> = (0..501).map(|_| json!({})).collect();
        let e = call("t", &Value::Array(many), false, true).unwrap_err();
        let r = failure(&e);
        assert_eq!(r.status, 422);
        assert_eq!(r.header("x-pm-apierrorcode"), Some("410"));
    }

    #[test]
    fn errors() {
        let e = call(
            "t",
            &json!({"From": "s@example.com", "TextBody": "x"}),
            false,
            false,
        )
        .unwrap_err();
        let r = failure(&e);
        assert_eq!(r.status, 422);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(
            v,
            json!({"ErrorCode": 300, "Message": "Zero recipients specified"})
        );
        let e = call(
            "t",
            &json!({"From": "s@example.com", "To": "a@example.test"}),
            true,
            false,
        )
        .unwrap_err();
        assert_eq!(failure(&e).header("x-pm-apierrorcode"), Some("1101"));
        let r = failure(&Fail::MissingAuth);
        assert_eq!((r.status, r.header("x-pm-apierrorcode")), (401, Some("10")));
        let r = failure(&Fail::Named("406".into()));
        assert_eq!(
            (r.status, r.header("x-pm-apierrorcode")),
            (422, Some("406"))
        );
        let h = vec![("content-type".to_string(), "text/plain".to_string())];
        let req = Request {
            headers: &h,
            query: "",
            body: b"{}",
            form: &[],
            max_bytes: 1 << 20,
        };
        assert_eq!(failure(&parse_one(&req, false).unwrap_err()).status, 415);
    }
}
