//! Mailgun `POST /v3/{domain}/messages` (form fields) and `/messages.mime` (raw MIME in `message`):
//! `200 {"id": "<...@domain>", "message": "Queued. Thank you."}`. With `recipient-variables` each `to`
//! recipient gets its own message with `%recipient.<key>%` replaced (batch sending).

use fastmock_core::mail::ProviderExtras;
use serde_json::{Map, Value, json};

use super::build::{Attachment, Fanout, Spec, raw_message, split_list, substitute_all};
use super::{Fail, FormPart, Parsed, Reply, Request, norm};

/// All `o:`, `h:`, `v:` and `t:` values together.
pub const MAX_OPTION_BYTES: usize = 16 * 1024;
/// Recipients of `recipient-variables` (batch sending).
pub const MAX_BATCH: usize = 1000;
/// Recipients of one message.
pub const MAX_RECIPIENTS: usize = 1000;

/// `providerError` names: the common ones and `recipientsRateLimit` (HTTP 420).
pub const ERROR_NAMES: &[&str] = &[
    "invalidKey",
    "forbidden",
    "validation",
    "messageRejected",
    "rateLimit",
    "recipientsRateLimit",
    "quota",
    "templateNotFound",
    "tooLarge",
    "serverError",
    "unavailable",
];

/// A sending domain in the path: letters, digits, `-` and `.`.
pub fn domain_ok(d: &str) -> bool {
    !d.is_empty()
        && d.len() <= 253
        && d.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
}

/// A new message id `<YYYYMMDDhhmmss.<hex>@<domain>>`.
pub fn message_id(domain: &str) -> String {
    format!(
        "<{}.{}@{domain}>",
        chrono::Utc::now().format("%Y%m%d%H%M%S"),
        &uuid::Uuid::new_v4().simple().to_string()[..16]
    )
}

fn values<'a>(form: &'a [FormPart], name: &str) -> impl Iterator<Item = String> + 'a {
    let name = name.to_string();
    form.iter()
        .filter(move |p| p.name == name && p.filename.is_none())
        .map(FormPart::value)
}

fn first(form: &[FormPart], name: &str) -> Option<String> {
    values(form, name).next().filter(|v| !v.is_empty())
}

fn addresses(form: &[FormPart], name: &str) -> Vec<String> {
    values(form, name).flat_map(|v| split_list(&v)).collect()
}

fn json_field(form: &[FormPart], names: &[&str]) -> Result<Option<Value>, Fail> {
    for n in names {
        if let Some(v) = first(form, n) {
            return serde_json::from_str(&v)
                .map(Some)
                .map_err(|_| Fail::invalid(format!("{n} must be valid JSON")));
        }
    }
    Ok(None)
}

fn files(form: &[FormPart], name: &str) -> Vec<Attachment> {
    form.iter()
        .filter(|p| {
            (p.name == name || p.name.starts_with(&format!("{name}[")))
                && (p.filename.is_some() || name == "attachment")
        })
        .map(|p| {
            let fname = p.filename.clone().unwrap_or_else(|| "attachment".into());
            Attachment {
                content_id: (name == "inline").then(|| fname.clone()),
                name: fname,
                content_type: p.content_type.clone(),
                data: p.data.to_vec(),
            }
        })
        .collect()
}

fn missing(field: &str) -> Fail {
    Fail::field(field, format!("'{field}' parameter is missing"))
}

fn extras(form: &[FormPart], id: &str) -> Result<ProviderExtras, Fail> {
    let option_bytes: usize = form
        .iter()
        .filter(|p| {
            ["o:", "h:", "v:", "t:"]
                .iter()
                .any(|pre| p.name.starts_with(pre))
        })
        .map(|p| p.data.len())
        .sum();
    if option_bytes > MAX_OPTION_BYTES {
        return Err(Fail::invalid(format!(
            "The total size of o:, h:, v: and t: values exceeds {MAX_OPTION_BYTES} bytes"
        )));
    }
    let mut metadata: Map<String, Value> = Map::new();
    if let Some(Value::Object(m)) = json_field(form, &["h:X-Mailgun-Variables"])? {
        metadata.extend(m);
    }
    for p in form.iter().filter(|p| p.filename.is_none()) {
        if let Some(k) = p.name.strip_prefix("v:")
            && !k.is_empty()
        {
            let v = p.value();
            metadata.insert(
                k.to_string(),
                serde_json::from_str::<Value>(&v)
                    .ok()
                    .filter(|x| x.is_object() || x.is_array())
                    .unwrap_or(Value::String(v)),
            );
        }
    }
    let template = first(form, "template").map(|t| match first(form, "t:version") {
        Some(v) => format!("{t}@{v}"),
        None => t,
    });
    let test_mode = first(form, "o:testmode")
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "yes" | "true" | "1"));
    Ok(ProviderExtras {
        provider_message_id: Some(id.to_string()),
        template_id: template,
        template_data: json_field(form, &["t:variables", "h:X-Mailgun-Template-Variables"])?,
        tags: values(form, "o:tag").filter(|t| !t.is_empty()).collect(),
        metadata,
        test_mode,
    })
}

fn custom_headers(form: &[FormPart]) -> (Vec<(String, String)>, Vec<String>) {
    let mut headers = Vec::new();
    let mut reply_to = Vec::new();
    for p in form.iter().filter(|p| p.filename.is_none()) {
        let Some(name) = p.name.strip_prefix("h:") else {
            continue;
        };
        if name.eq_ignore_ascii_case("Reply-To") {
            reply_to.extend(split_list(&p.value()));
        } else if !name.eq_ignore_ascii_case("X-Mailgun-Variables")
            && !name.eq_ignore_ascii_case("X-Mailgun-Template-Variables")
        {
            headers.push((name.to_string(), p.value()));
        }
    }
    (headers, reply_to)
}

/// `%recipient.<key>%` pairs of one recipient's variables.
fn recipient_pairs(vars: &Value) -> Vec<(String, String)> {
    match vars {
        Value::Object(m) => m
            .iter()
            .map(|(k, v)| {
                let val = match v {
                    Value::String(x) => x.clone(),
                    other => other.to_string(),
                };
                (format!("%recipient.{k}%"), val)
            })
            .collect(),
        _ => vec![],
    }
}

fn substitute(
    s: &Option<String>,
    pairs: &[(String, String)],
    max: usize,
) -> Result<Option<String>, Fail> {
    s.as_deref()
        .map(|v| substitute_all(v, pairs, max))
        .transpose()
}

/// `POST /v3/{domain}/messages` and `/messages.mime`.
pub fn parse(req: &Request<'_>, domain: &str, mime: bool) -> Result<Parsed, Fail> {
    let form = req.form;
    let id = message_id(domain);
    let mut ex = extras(form, &id)?;
    let to = addresses(form, "to");
    if to.is_empty() {
        return Err(missing("to"));
    }
    if mime {
        if to.len() > MAX_RECIPIENTS {
            return Err(Fail::invalid(format!(
                "Too many recipients: at most {MAX_RECIPIENTS}"
            )));
        }
        let part = form
            .iter()
            .find(|p| p.name == "message")
            .ok_or_else(|| missing("message"))?;
        ex.template_id = None;
        let m = raw_message(part.data.to_vec(), None, &to, ex, req.max_bytes)?;
        return Ok(Parsed::one(m));
    }
    let from = first(form, "from").ok_or_else(|| missing("from"))?;
    let text = first(form, "text");
    let html = first(form, "html").or_else(|| first(form, "amp-html"));
    if text.is_none() && html.is_none() && ex.template_id.is_none() {
        return Err(Fail::invalid(
            "Need at least one of 'text', 'html' or 'template' parameters specified",
        ));
    }
    let cc = addresses(form, "cc");
    let bcc = addresses(form, "bcc");
    if to.len() + cc.len() + bcc.len() > MAX_RECIPIENTS {
        return Err(Fail::invalid(format!(
            "Too many recipients: at most {MAX_RECIPIENTS}"
        )));
    }
    for (field, list) in [("to", &to), ("cc", &cc), ("bcc", &bcc)] {
        for a in list.iter() {
            super::build::address_of(a).map_err(|_| {
                Fail::field(
                    field,
                    format!(
                        "'{field}' parameter is not a valid address. please check documentation"
                    ),
                )
            })?;
        }
    }
    let (headers, reply_to) = custom_headers(form);
    let mut attachments = files(form, "attachment");
    attachments.extend(files(form, "inline"));
    let msg_id = id.trim_matches(['<', '>']).to_string();
    let batch = json_field(form, &["recipient-variables"])?;
    let subject = first(form, "subject");
    let spec = |to: Vec<String>, vars: &Value, fan: &Fanout| -> Result<Spec, Fail> {
        let pairs = recipient_pairs(vars);
        let subject = substitute(&subject, &pairs, req.max_bytes)?;
        let text = substitute(&text, &pairs, req.max_bytes)?;
        let html = substitute(&html, &pairs, req.max_bytes)?;
        fan.check(
            subject.as_ref().map_or(0, String::len)
                + text.as_ref().map_or(0, String::len)
                + html.as_ref().map_or(0, String::len),
        )?;
        Ok(Spec {
            from: from.clone(),
            display_to: to.join(", "),
            to,
            cc: cc.clone(),
            bcc: bcc.clone(),
            reply_to: reply_to.clone(),
            subject,
            text,
            html,
            headers: headers.clone(),
            attachments: attachments.clone(),
            message_id: Some(msg_id.clone()),
            extras: ex.clone(),
        })
    };
    let wrap = |e: Fail| match e {
        Fail::Invalid { message, .. } => Fail::invalid(message),
        other => other,
    };
    let test_mode = ex.test_mode;
    let items = match batch {
        Some(Value::Object(vars)) => {
            if to.len() > MAX_BATCH {
                return Err(Fail::invalid(format!(
                    "Batch sending is limited to {MAX_BATCH} recipients"
                )));
            }
            let shared = text.as_ref().map_or(0, String::len)
                + html.as_ref().map_or(0, String::len)
                + attachments.iter().map(|a| a.data.len()).sum::<usize>();
            let mut fan = Fanout::new(shared, to.len())?;
            let lower: Map<String, Value> = vars
                .iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
                .collect();
            let mut items = Vec::with_capacity(to.len());
            for r in &to {
                let addr = super::build::address_of(r)
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let v = lower.get(&addr).unwrap_or(&Value::Null);
                let m = spec(vec![r.clone()], v, &fan)?
                    .build(req.max_bytes)
                    .map_err(wrap)?;
                items.push(Ok(fan.message(m)?));
            }
            items
        }
        Some(_) => {
            return Err(Fail::invalid(
                "recipient-variables must be a JSON object keyed by recipient address",
            ));
        }
        None => vec![Ok(spec(to.clone(), &Value::Null, &Fanout::default())?
            .build(req.max_bytes)
            .map_err(wrap)?)],
    };
    Ok(Parsed {
        items,
        test_mode,
        ..Default::default()
    })
}

/// `{"id": "<...>", "message": "Queued. Thank you."}`.
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
    Reply::json(200, &json!({"id": id, "message": "Queued. Thank you."}))
}

fn message(status: u16, m: &str) -> Reply {
    Reply::json(status, &json!({"message": m}))
}

fn forbidden() -> Reply {
    Reply {
        status: 401,
        headers: vec![("content-type".into(), "text/plain; charset=utf-8".into())],
        body: b"Forbidden".to_vec(),
    }
}

fn rate_limited(status: u16, m: &str) -> Reply {
    let reset = (chrono::Utc::now().timestamp() + 60) * 1000;
    message(status, m)
        .with_header("x-ratelimit-limit", "300")
        .with_header("x-ratelimit-remaining", "0")
        .with_header("x-ratelimit-reset", reset.to_string())
}

fn named(name: &str) -> Reply {
    match norm(name).as_str() {
        "invalidkey" | "unauthorized" => forbidden(),
        "forbidden" => message(
            403,
            "Domain is not allowed to send: The domain is unverified and requires DNS configuration.",
        ),
        "validation" | "badrequest" => message(
            400,
            "'to' parameter is not a valid address. please check documentation",
        ),
        "messagerejected" => message(
            403,
            "Sandbox subdomains are for test purposes only. Please add your own domain or add the address to authorized recipients in Account Settings.",
        ),
        "ratelimit" | "toomanyrequests" => rate_limited(429, "Too many requests"),
        "recipientsratelimit" | "420" => {
            rate_limited(420, "Recipients rate limit exceeded. Try again later.")
        }
        "quota" => message(
            403,
            "The account has reached its sending limit. Please upgrade your plan.",
        ),
        "templatenotfound" => message(400, "template not found"),
        "toolarge" => message(413, "Request entity too large"),
        "servererror" => message(500, "Internal Server Error"),
        "unavailable" => message(503, "Service Unavailable"),
        _ => message(
            500,
            &format!("FastMock: unknown providerError `{name}` for mailgun"),
        ),
    }
}

/// `{"message": ...}`; a wrong or missing key answers `401 Forbidden` in plain text.
pub fn failure(fail: &Fail) -> Reply {
    match fail {
        Fail::MissingAuth | Fail::BadKey => forbidden(),
        Fail::Invalid { message: m, .. } | Fail::BadJson(m) | Fail::Media(m) => message(400, m),
        Fail::TooLarge => message(413, "Request entity too large"),
        Fail::NotFound(_) => message(404, "Not Found"),
        Fail::Method => message(405, "Method Not Allowed"),
        Fail::Named(n) => named(n),
        Fail::Internal(_) => message(500, "Internal Server Error"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn req(form: &[FormPart]) -> Request<'_> {
        Request {
            headers: &[],
            query: "",
            body: b"",
            form,
            max_bytes: 1 << 20,
        }
    }

    fn file(name: &str, filename: &str, data: &[u8]) -> FormPart {
        FormPart {
            name: name.into(),
            filename: Some(filename.into()),
            content_type: Some("text/plain".into()),
            data: Bytes::copy_from_slice(data),
        }
    }

    #[test]
    fn form_fields_become_a_message() {
        let form = vec![
            FormPart::text("from", "Shop <shop@mg.example.com>"),
            FormPart::text("to", "Ann <a@example.test>, b@example.test"),
            FormPart::text("bcc", "c@example.test"),
            FormPart::text("subject", "Hello"),
            FormPart::text("text", "Code 4242"),
            FormPart::text("o:tag", "welcome"),
            FormPart::text("o:tag", "otp"),
            FormPart::text("o:testmode", "yes"),
            FormPart::text("h:X-Trace", "t1"),
            FormPart::text("h:Reply-To", "help@example.com"),
            FormPart::text("v:order", "42"),
            FormPart::text("v:meta", "{\"a\":1}"),
            file("attachment", "a.txt", b"hello"),
            file("inline", "logo.png", b"\x01"),
        ];
        let p = parse(&req(&form), "mg.example.com", false).unwrap();
        let m = p.items[0].as_ref().unwrap();
        assert_eq!(
            m.rcpt_to,
            vec!["a@example.test", "b@example.test", "c@example.test"]
        );
        assert_eq!(m.extras.tags, vec!["welcome", "otp"]);
        assert!(m.extras.test_mode && p.test_mode);
        assert_eq!(m.extras.metadata["order"], "42");
        assert_eq!(m.extras.metadata["meta"], json!({"a": 1}));
        let id = m.extras.provider_message_id.clone().unwrap();
        assert!(id.starts_with('<') && id.ends_with("@mg.example.com>"));
        let raw = String::from_utf8_lossy(&m.raw);
        assert!(raw.contains(&format!("Message-ID: {id}")), "{raw}");
        assert!(raw.contains("X-Trace: t1") && raw.contains("Reply-To: <help@example.com>"));
        let r = success(&p);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v, json!({"id": id, "message": "Queued. Thank you."}));
    }

    #[test]
    fn batch_sending_templates_and_mime() {
        let form = vec![
            FormPart::text("from", "shop@mg.example.com"),
            FormPart::text("to", "a@example.test"),
            FormPart::text("to", "b@example.test"),
            FormPart::text("subject", "Hi %recipient.name%"),
            FormPart::text("text", "Hello %recipient.name%"),
            FormPart::text(
                "recipient-variables",
                r#"{"a@example.test": {"name": "Ann"}, "b@example.test": {"name": "Bob"}}"#,
            ),
        ];
        let p = parse(&req(&form), "mg.example.com", false).unwrap();
        assert_eq!(p.items.len(), 2);
        let b = p.items[1].as_ref().unwrap();
        assert_eq!(b.rcpt_to, vec!["b@example.test"]);
        assert!(String::from_utf8_lossy(&b.raw).contains("Subject: Hi Bob"));

        let form = vec![
            FormPart::text("from", "shop@mg.example.com"),
            FormPart::text("to", "a@example.test"),
            FormPart::text("template", "welcome"),
            FormPart::text("t:version", "v2"),
            FormPart::text("t:variables", r#"{"code": "1234"}"#),
        ];
        let p = parse(&req(&form), "mg.example.com", false).unwrap();
        let m = p.items[0].as_ref().unwrap();
        assert_eq!(m.extras.template_id.as_deref(), Some("welcome@v2"));
        assert_eq!(m.extras.template_data, Some(json!({"code": "1234"})));

        let mime = b"From: x@example.com\r\nTo: y@example.test\r\nSubject: M\r\n\r\nbody\r\n";
        let form = vec![
            FormPart::text("to", "y@example.test"),
            file("message", "message.mime", mime),
        ];
        let p = parse(&req(&form), "mg.example.com", true).unwrap();
        assert_eq!(p.items[0].as_ref().unwrap().raw, mime);
    }

    #[test]
    fn errors_use_the_mailgun_messages() {
        let check = |form: Vec<FormPart>, msg: &str| {
            let e = parse(&req(&form), "mg.example.com", false).unwrap_err();
            let r = failure(&e);
            assert_eq!(r.status, 400);
            let v: Value = serde_json::from_slice(&r.body).unwrap();
            assert_eq!(v["message"], msg);
        };
        check(
            vec![FormPart::text("to", "a@example.test")],
            "'from' parameter is missing",
        );
        check(
            vec![FormPart::text("from", "s@example.com")],
            "'to' parameter is missing",
        );
        check(
            vec![
                FormPart::text("from", "s@example.com"),
                FormPart::text("to", "a@example.test"),
            ],
            "Need at least one of 'text', 'html' or 'template' parameters specified",
        );
        check(
            vec![
                FormPart::text("from", "s@example.com"),
                FormPart::text("to", "nobody"),
                FormPart::text("text", "x"),
            ],
            "'to' parameter is not a valid address. please check documentation",
        );
        check(
            vec![
                FormPart::text("from", "s@example.com"),
                FormPart::text("to", "a@example.test"),
                FormPart::text("text", "x"),
                FormPart::text("v:big", &"x".repeat(MAX_OPTION_BYTES + 1)),
            ],
            "The total size of o:, h:, v: and t: values exceeds 16384 bytes",
        );
        let r = failure(&Fail::MissingAuth);
        assert_eq!(
            (r.status, r.body.as_slice()),
            (401, b"Forbidden".as_slice())
        );
        let r = failure(&Fail::Named("recipientsRateLimit".into()));
        assert_eq!(r.status, 420);
        assert!(r.header("x-ratelimit-reset").unwrap().len() >= 13);
    }
}
