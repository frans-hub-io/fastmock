//! Amazon SES: v2 `SendEmail`/`SendBulkEmail` (restJson1) and v1 `SendEmail`/`SendRawEmail` (query
//! API with XML answers). SigV4 and SigV4a signatures are accepted when well-formed; the access key id
//! is the key compared with the expected keys of a space.

use std::collections::BTreeMap;

use fastmock_core::mail::ProviderExtras;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::build::{
    Attachment, Spec, check_fanout, decode_base64, merge_data, raw_message, render, template_data,
};
use super::{Fail, FormPart, OutMessage, Parsed, Reply, Request, header, norm};

/// SES allows 50 recipients per message (and 50 entries per bulk call).
pub const MAX_RECIPIENTS: usize = 50;
const XMLNS: &str = "http://ses.amazonaws.com/doc/2010-12-01/";

/// `providerError` names: the common ones and the SES error codes (v2 exception names such as
/// `MessageRejected`, `TooManyRequestsException`, `NotFoundException`; v1 codes such as `Throttling`,
/// `MailFromDomainNotVerified`).
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
    "MessageRejected",
    "MailFromDomainNotVerifiedException",
    "AccountSuspendedException",
    "SendingPausedException",
    "LimitExceededException",
    "NotFoundException",
    "TooManyRequestsException",
    "BadRequestException",
    "AccessDeniedException",
    "Throttling",
    "MailFromDomainNotVerified",
    "ConfigurationSetDoesNotExist",
    "AccountSendingPaused",
];

/// A new SES message id (`0100<ms>-<uuid>-000000`).
pub fn message_id() -> String {
    let ms = chrono::Utc::now().timestamp_millis().max(0) as u64 & 0x0000_ffff_ffff_ffff;
    format!(
        "{:016x}-{}-000000",
        0x0100_0000_0000_0000u64 | ms,
        uuid::Uuid::new_v4()
    )
}

fn request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn scope_ok(alg: &str, credential: &str) -> Option<String> {
    let parts: Vec<&str> = credential.split('/').collect();
    let n = if alg == "AWS4-HMAC-SHA256" { 5 } else { 4 };
    (parts.len() == n
        && parts.iter().all(|p| !p.is_empty())
        && parts[n - 1] == "aws4_request"
        && parts[0].len() <= 128
        && parts[0].bytes().all(|b| b.is_ascii_alphanumeric()))
    .then(|| parts[0].to_string())
}

fn incomplete(m: &str) -> Fail {
    Fail::Invalid {
        field: Some("IncompleteSignature".into()),
        message: m.into(),
    }
}

/// The access key id of a SigV4 (`AWS4-HMAC-SHA256`) or SigV4a (`AWS4-ECDSA-P256-SHA256`)
/// `Authorization` header, or of a presigned query.
pub fn access_key(headers: &[(String, String)], query: &str) -> Result<String, Fail> {
    if let Some(v) = header(headers, "authorization") {
        let v = v.trim();
        let (alg, rest) = v.split_once(' ').unwrap_or((v, ""));
        if alg != "AWS4-HMAC-SHA256" && alg != "AWS4-ECDSA-P256-SHA256" {
            return Err(incomplete(
                "Authorization header requires the AWS4-HMAC-SHA256 or AWS4-ECDSA-P256-SHA256 algorithm.",
            ));
        }
        let mut fields: BTreeMap<String, String> = BTreeMap::new();
        for p in rest.split(',') {
            if let Some((k, val)) = p.trim().split_once('=') {
                fields.insert(k.trim().to_string(), val.trim().to_string());
            }
        }
        let signature_ok = fields
            .get("Signature")
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit()));
        let signed_ok = fields.get("SignedHeaders").is_some_and(|s| !s.is_empty());
        let key = fields.get("Credential").and_then(|c| scope_ok(alg, c));
        return match (key, signed_ok, signature_ok) {
            (Some(k), true, true) => Ok(k),
            _ => Err(incomplete(
                "Authorization header requires 'Credential', 'SignedHeaders' and 'Signature' parameters.",
            )),
        };
    }
    let q: BTreeMap<String, String> = form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    if let (Some(alg), Some(cred), Some(sig)) = (
        q.get("X-Amz-Algorithm"),
        q.get("X-Amz-Credential"),
        q.get("X-Amz-Signature"),
    ) {
        if let Some(k) = scope_ok(alg, cred)
            && !sig.is_empty()
            && sig.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Ok(k);
        }
        return Err(incomplete(
            "The query string requires valid X-Amz-Credential and X-Amz-Signature parameters.",
        ));
    }
    Err(Fail::MissingAuth)
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct Destination {
    #[serde(default)]
    to_addresses: Vec<String>,
    #[serde(default)]
    cc_addresses: Vec<String>,
    #[serde(default)]
    bcc_addresses: Vec<String>,
}

impl Destination {
    fn count(&self) -> usize {
        self.to_addresses.len() + self.cc_addresses.len() + self.bcc_addresses.len()
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct Part {
    #[serde(default)]
    data: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct Body {
    #[serde(default)]
    text: Option<Part>,
    #[serde(default)]
    html: Option<Part>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct NameValue {
    name: String,
    value: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct Att {
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    content_disposition: Option<String>,
    #[serde(default)]
    content_id: Option<String>,
    #[serde(default)]
    raw_content: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct Simple {
    #[serde(default)]
    subject: Option<Part>,
    #[serde(default)]
    body: Option<Body>,
    #[serde(default)]
    headers: Vec<NameValue>,
    #[serde(default)]
    attachments: Vec<Att>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawContent {
    data: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct TemplateContent {
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    html: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct Template {
    #[serde(default)]
    template_name: Option<String>,
    #[serde(default)]
    template_arn: Option<String>,
    #[serde(default)]
    template_content: Option<TemplateContent>,
    #[serde(default)]
    template_data: Option<String>,
    #[serde(default)]
    headers: Vec<NameValue>,
    #[serde(default)]
    attachments: Vec<Att>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct Content {
    #[serde(default)]
    simple: Option<Simple>,
    #[serde(default)]
    raw: Option<RawContent>,
    #[serde(default)]
    template: Option<Template>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SendEmail {
    #[serde(default)]
    from_email_address: Option<String>,
    #[serde(default)]
    destination: Option<Destination>,
    #[serde(default)]
    reply_to_addresses: Vec<String>,
    #[serde(default)]
    content: Option<Content>,
    #[serde(default)]
    email_tags: Vec<NameValue>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct ReplacementTemplate {
    #[serde(default)]
    replacement_template_data: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct ReplacementContent {
    #[serde(default)]
    replacement_template: Option<ReplacementTemplate>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct BulkEntry {
    #[serde(default)]
    destination: Option<Destination>,
    #[serde(default)]
    replacement_email_content: Option<ReplacementContent>,
    #[serde(default)]
    replacement_headers: Vec<NameValue>,
    #[serde(default)]
    replacement_tags: Vec<NameValue>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct BulkContent {
    #[serde(default)]
    template: Option<Template>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SendBulk {
    #[serde(default)]
    from_email_address: Option<String>,
    #[serde(default)]
    reply_to_addresses: Vec<String>,
    #[serde(default)]
    default_content: Option<BulkContent>,
    #[serde(default)]
    bulk_email_entries: Vec<BulkEntry>,
    #[serde(default)]
    default_email_tags: Vec<NameValue>,
}

fn json_of<T: serde::de::DeserializeOwned>(req: &Request<'_>) -> Result<T, Fail> {
    serde_json::from_slice(req.body).map_err(|e| Fail::BadJson(format!("invalid request: {e}")))
}

fn attachments(list: Vec<Att>) -> Result<Vec<Attachment>, Fail> {
    list.into_iter()
        .map(|a| {
            let inline = a
                .content_disposition
                .as_deref()
                .is_some_and(|d| d.eq_ignore_ascii_case("INLINE"));
            Ok(Attachment {
                name: a.file_name.unwrap_or_else(|| "attachment".into()),
                content_type: a.content_type,
                data: decode_base64("RawContent", a.raw_content.as_deref().unwrap_or(""))?,
                content_id: if inline { a.content_id } else { None },
            })
        })
        .collect()
}

fn tags(list: &[NameValue]) -> Map<String, Value> {
    list.iter()
        .map(|t| (t.name.clone(), Value::String(t.value.clone())))
        .collect()
}

fn from_required(from: Option<String>) -> Result<String, Fail> {
    from.filter(|f| !f.trim().is_empty())
        .ok_or_else(|| Fail::field("FromEmailAddress", "FromEmailAddress is required"))
}

fn template_spec(t: &Template, data: &Value, spec: &mut Spec) -> Result<(), Fail> {
    spec.extras.template_id = t.template_name.clone().or_else(|| t.template_arn.clone());
    if spec.extras.template_id.is_none() && t.template_content.is_none() {
        return Err(Fail::field(
            "Template",
            "Template requires TemplateName, TemplateArn or TemplateContent",
        ));
    }
    if !data.is_null() {
        spec.extras.template_data = Some(data.clone());
    }
    if let Some(c) = &t.template_content {
        if let Some(s) = &c.subject {
            spec.subject = Some(render(s, data, false)?);
        }
        if let Some(s) = &c.text {
            spec.text = Some(render(s, data, false)?);
        }
        if let Some(s) = &c.html {
            spec.html = Some(render(s, data, true)?);
        }
    }
    spec.headers
        .extend(t.headers.iter().map(|h| (h.name.clone(), h.value.clone())));
    Ok(())
}

fn check_count(n: usize) -> Result<(), Fail> {
    if n > MAX_RECIPIENTS {
        return Err(Fail::invalid(format!(
            "Recipient count exceeds {MAX_RECIPIENTS}."
        )));
    }
    Ok(())
}

/// `POST /v2/email/outbound-emails`.
pub fn parse_v2_send(req: &Request<'_>) -> Result<Parsed, Fail> {
    let body: SendEmail = json_of(req)?;
    let content = body
        .content
        .ok_or_else(|| Fail::field("Content", "Content is required"))?;
    let kinds = [
        content.simple.is_some(),
        content.raw.is_some(),
        content.template.is_some(),
    ];
    if kinds.iter().filter(|k| **k).count() != 1 {
        return Err(Fail::field(
            "Content",
            "Content must contain exactly one of Simple, Raw or Template",
        ));
    }
    let dest = body.destination.unwrap_or_default();
    check_count(dest.count())?;
    let mut extras = ProviderExtras {
        provider_message_id: Some(message_id()),
        metadata: tags(&body.email_tags),
        ..Default::default()
    };
    if let Some(raw) = content.raw {
        let bytes = decode_base64("Raw.Data", &raw.data)?;
        let rcpt: Vec<String> = dest
            .to_addresses
            .into_iter()
            .chain(dest.cc_addresses)
            .chain(dest.bcc_addresses)
            .collect();
        extras.template_id = None;
        let m = raw_message(
            bytes,
            body.from_email_address.as_deref(),
            &rcpt,
            extras,
            req.max_bytes,
        )?;
        check_count(m.rcpt_to.len())?;
        return Ok(Parsed::one(m));
    }
    if dest.count() == 0 {
        return Err(Fail::field(
            "Destination",
            "Destination must name at least one recipient",
        ));
    }
    let mut spec = Spec {
        from: from_required(body.from_email_address)?,
        display_to: dest.to_addresses.join(", "),
        to: dest.to_addresses,
        cc: dest.cc_addresses,
        bcc: dest.bcc_addresses,
        reply_to: body.reply_to_addresses,
        ..Default::default()
    };
    spec.extras = std::mem::take(&mut extras);
    if let Some(s) = content.simple {
        spec.subject = s.subject.and_then(|p| p.data);
        if let Some(b) = s.body {
            spec.text = b.text.and_then(|p| p.data);
            spec.html = b.html.and_then(|p| p.data);
        }
        spec.headers = s.headers.into_iter().map(|h| (h.name, h.value)).collect();
        spec.attachments = attachments(s.attachments)?;
    } else if let Some(t) = content.template {
        let data = match &t.template_data {
            Some(d) => template_data("TemplateData", d)?,
            None => Value::Null,
        };
        template_spec(&t, &data, &mut spec)?;
        spec.attachments = attachments(t.attachments)?;
    }
    Ok(Parsed::one(spec.build(req.max_bytes)?))
}

/// `POST /v2/email/outbound-bulk-emails`: one message per entry, per-entry failures in the answer.
pub fn parse_v2_bulk(req: &Request<'_>) -> Result<Parsed, Fail> {
    let body: SendBulk = json_of(req)?;
    let mut template = body
        .default_content
        .and_then(|c| c.template)
        .ok_or_else(|| Fail::field("DefaultContent", "DefaultContent.Template is required"))?;
    if body.bulk_email_entries.is_empty() || body.bulk_email_entries.len() > MAX_RECIPIENTS {
        return Err(Fail::field(
            "BulkEmailEntries",
            format!("BulkEmailEntries must hold 1 to {MAX_RECIPIENTS} entries"),
        ));
    }
    let from = from_required(body.from_email_address)?;
    let defaults = match &template.template_data {
        Some(d) => template_data("TemplateData", d)?,
        None => Value::Null,
    };
    let default_tags = tags(&body.default_email_tags);
    let template_attachments = attachments(std::mem::take(&mut template.attachments))?;
    let shared = template_attachments
        .iter()
        .map(|a| a.data.len())
        .sum::<usize>()
        + template.template_content.as_ref().map_or(0, |c| {
            c.subject.as_ref().map_or(0, String::len)
                + c.text.as_ref().map_or(0, String::len)
                + c.html.as_ref().map_or(0, String::len)
        });
    check_fanout(shared, body.bulk_email_entries.len())?;
    let mut items = Vec::with_capacity(body.bulk_email_entries.len());
    for e in body.bulk_email_entries {
        let item = (|| -> Result<OutMessage, Fail> {
            let dest = e.destination.unwrap_or_default();
            if dest.count() == 0 {
                return Err(Fail::invalid("the entry has no recipients"));
            }
            check_count(dest.count())?;
            let replacement = match e
                .replacement_email_content
                .and_then(|c| c.replacement_template)
                .and_then(|t| t.replacement_template_data)
            {
                Some(d) => template_data("ReplacementTemplateData", &d)?,
                None => Value::Null,
            };
            let data = merge_data(&defaults, &replacement);
            let mut metadata = default_tags.clone();
            metadata.extend(tags(&e.replacement_tags));
            let mut spec = Spec {
                from: from.clone(),
                display_to: dest.to_addresses.join(", "),
                to: dest.to_addresses,
                cc: dest.cc_addresses,
                bcc: dest.bcc_addresses,
                reply_to: body.reply_to_addresses.clone(),
                extras: ProviderExtras {
                    provider_message_id: Some(message_id()),
                    metadata,
                    ..Default::default()
                },
                ..Default::default()
            };
            template_spec(&template, &data, &mut spec)?;
            spec.attachments = template_attachments.clone();
            for h in e.replacement_headers {
                spec.headers
                    .retain(|(n, _)| !n.eq_ignore_ascii_case(&h.name));
                spec.headers.push((h.name, h.value));
            }
            spec.build(req.max_bytes)
        })();
        items.push(item);
    }
    Ok(Parsed {
        items,
        ..Default::default()
    })
}

fn form_map(form: &[FormPart]) -> BTreeMap<String, String> {
    form.iter().map(|p| (p.name.clone(), p.value())).collect()
}

/// Values of `<prefix>.member.N` in order of N.
fn members(f: &BTreeMap<String, String>, prefix: &str) -> Vec<String> {
    let start = format!("{prefix}.member.");
    let mut v: Vec<(u32, String)> = f
        .iter()
        .filter_map(|(k, val)| {
            let n = k.strip_prefix(&start)?;
            n.parse::<u32>().ok().map(|n| (n, val.clone()))
        })
        .collect();
    v.sort_by_key(|(n, _)| *n);
    v.into_iter().map(|(_, s)| s).collect()
}

fn v1_tags(f: &BTreeMap<String, String>) -> Map<String, Value> {
    let mut out = Map::new();
    for n in 1..=50 {
        let (Some(name), Some(value)) = (
            f.get(&format!("Tags.member.{n}.Name")),
            f.get(&format!("Tags.member.{n}.Value")),
        ) else {
            continue;
        };
        out.insert(name.clone(), Value::String(value.clone()));
    }
    out
}

/// `POST /` with `Action=SendEmail` or `Action=SendRawEmail`.
pub fn parse_v1(req: &Request<'_>) -> Result<Parsed, Fail> {
    let f = form_map(req.form);
    let action = f.get("Action").cloned().unwrap_or_default();
    let extras = ProviderExtras {
        provider_message_id: Some(message_id()),
        metadata: v1_tags(&f),
        ..Default::default()
    };
    let m = match action.as_str() {
        "SendEmail" => {
            let to = members(&f, "Destination.ToAddresses");
            let cc = members(&f, "Destination.CcAddresses");
            let bcc = members(&f, "Destination.BccAddresses");
            check_count(to.len() + cc.len() + bcc.len())?;
            if to.len() + cc.len() + bcc.len() == 0 {
                return Err(Fail::field(
                    "Destination",
                    "Destination must name at least one recipient",
                ));
            }
            let from = f
                .get("Source")
                .filter(|s| !s.trim().is_empty())
                .cloned()
                .ok_or_else(|| Fail::field("Source", "Source is required"))?;
            Spec {
                from,
                display_to: to.join(", "),
                to,
                cc,
                bcc,
                reply_to: members(&f, "ReplyToAddresses"),
                subject: f.get("Message.Subject.Data").cloned(),
                text: f.get("Message.Body.Text.Data").cloned(),
                html: f.get("Message.Body.Html.Data").cloned(),
                extras,
                ..Default::default()
            }
            .build(req.max_bytes)?
        }
        "SendRawEmail" => {
            let data = f
                .get("RawMessage.Data")
                .ok_or_else(|| Fail::field("RawMessage.Data", "RawMessage.Data is required"))?;
            let raw = decode_base64("RawMessage.Data", data)?;
            let rcpt = members(&f, "Destinations");
            check_count(rcpt.len())?;
            let m = raw_message(
                raw,
                f.get("Source").map(String::as_str),
                &rcpt,
                extras,
                req.max_bytes,
            )?;
            check_count(m.rcpt_to.len())?;
            m
        }
        other => {
            return Err(Fail::Invalid {
                field: Some("InvalidAction".into()),
                message: format!(
                    "Could not find operation {} for version 2010-12-01",
                    other.chars().take(64).collect::<String>()
                ),
            });
        }
    };
    let mut p = Parsed::one(m);
    p.action = Some(action);
    Ok(p)
}

fn first_id(parsed: &Parsed) -> String {
    parsed
        .items
        .iter()
        .find_map(|i| i.as_ref().ok())
        .and_then(|m| m.extras.provider_message_id.clone())
        .unwrap_or_default()
}

/// `{"MessageId": ...}`.
pub fn success_v2_send(parsed: &Parsed) -> Reply {
    match parsed.items.first() {
        Some(Err(f)) => failure_v2(f),
        _ => Reply::json(200, &json!({"MessageId": first_id(parsed)}))
            .with_header("x-amzn-requestid", request_id()),
    }
}

fn bulk_status(f: &Fail) -> &'static str {
    match f {
        Fail::Invalid { .. } | Fail::BadJson(_) => "INVALID_PARAMETER",
        Fail::TooLarge => "MESSAGE_REJECTED",
        Fail::Internal(_) => "TRANSIENT_FAILURE",
        _ => "FAILED",
    }
}

/// `{"BulkEmailEntryResults": [...]}`, one result per entry.
pub fn success_v2_bulk(parsed: &Parsed) -> Reply {
    let results: Vec<Value> = parsed
        .items
        .iter()
        .map(|i| match i {
            Ok(m) => json!({"Status": "SUCCESS", "MessageId": m.extras.provider_message_id}),
            Err(f) => json!({"Status": bulk_status(f), "Error": f.message()}),
        })
        .collect();
    Reply::json(200, &json!({"BulkEmailEntryResults": results}))
        .with_header("x-amzn-requestid", request_id())
}

fn xml_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c => o.push(c),
        }
    }
    o
}

/// `<SendEmailResponse>` or `<SendRawEmailResponse>`.
pub fn success_v1(parsed: &Parsed) -> Reply {
    if let Some(Err(f)) = parsed.items.first() {
        return failure_v1(f);
    }
    let action = parsed.action.as_deref().unwrap_or("SendEmail");
    let rid = request_id();
    let body = format!(
        "<{action}Response xmlns=\"{XMLNS}\">\n  <{action}Result>\n    <MessageId>{}</MessageId>\n  </{action}Result>\n  <ResponseMetadata>\n    <RequestId>{rid}</RequestId>\n  </ResponseMetadata>\n</{action}Response>\n",
        xml_escape(&first_id(parsed))
    );
    Reply {
        status: 200,
        headers: vec![
            ("content-type".into(), "text/xml".into()),
            ("x-amzn-requestid".into(), rid),
        ],
        body: body.into_bytes(),
    }
}

/// v2 error names with status and default message.
const V2: &[(&str, u16, &str)] = &[
    ("BadRequestException", 400, "The request is invalid."),
    (
        "MessageRejected",
        400,
        "Email address is not verified. The following identities failed the check in region US-EAST-1.",
    ),
    (
        "MailFromDomainNotVerifiedException",
        400,
        "The MAIL FROM domain is not verified.",
    ),
    (
        "AccountSuspendedException",
        400,
        "The account is suspended from sending email.",
    ),
    (
        "SendingPausedException",
        400,
        "Sending is paused for this account.",
    ),
    (
        "LimitExceededException",
        400,
        "Daily message quota exceeded.",
    ),
    ("NotFoundException", 404, "Template does not exist."),
    (
        "TooManyRequestsException",
        429,
        "Maximum sending rate exceeded.",
    ),
    (
        "AccessDeniedException",
        403,
        "User is not authorized to perform ses:SendEmail.",
    ),
    (
        "IncompleteSignature",
        403,
        "Authorization header requires 'Credential', 'SignedHeaders' and 'Signature' parameters.",
    ),
    (
        "UnrecognizedClientException",
        403,
        "The security token included in the request is invalid.",
    ),
    (
        "ExpiredTokenException",
        403,
        "The security token included in the request is expired.",
    ),
    ("ThrottlingException", 400, "Rate exceeded."),
    ("ValidationError", 400, "The request is invalid."),
    (
        "RequestEntityTooLargeException",
        413,
        "Request size exceeded the maximum allowed size.",
    ),
    (
        "InternalFailure",
        500,
        "The request processing has failed because of an unknown error.",
    ),
    (
        "ServiceUnavailable",
        503,
        "The request has failed due to a temporary failure of the server.",
    ),
];

fn v2_common(n: &str) -> Option<&'static str> {
    Some(match n {
        "invalidkey" => "UnrecognizedClientException",
        "forbidden" => "AccessDeniedException",
        "validation" => "BadRequestException",
        "messagerejected" => "MessageRejected",
        "ratelimit" => "TooManyRequestsException",
        "quota" => "LimitExceededException",
        "templatenotfound" => "NotFoundException",
        "toolarge" => "RequestEntityTooLargeException",
        "servererror" => "InternalFailure",
        "unavailable" => "ServiceUnavailable",
        _ => return None,
    })
}

fn v2_error(name: &str, status: u16, message: &str) -> Reply {
    Reply::json(status, &json!({"message": message}))
        .with_header("x-amzn-errortype", name)
        .with_header("x-amzn-requestid", request_id())
}

fn v2_named(name: &str, message: Option<&str>) -> Reply {
    let key = v2_common(&norm(name)).unwrap_or(name);
    match V2.iter().find(|(n, _, _)| n.eq_ignore_ascii_case(key)) {
        Some((n, s, m)) => v2_error(n, *s, message.unwrap_or(m)),
        None => v2_error(
            "InternalFailure",
            500,
            &format!("FastMock: unknown providerError `{name}` for ses"),
        ),
    }
}

/// v2 (restJson1) errors: `X-Amzn-ErrorType` and `{"message": ...}`.
pub fn failure_v2(fail: &Fail) -> Reply {
    match fail {
        Fail::MissingAuth => v2_named("IncompleteSignature", None),
        Fail::BadKey => v2_named("UnrecognizedClientException", None),
        Fail::Invalid {
            field: Some(f),
            message,
        } if f == "IncompleteSignature" => v2_named(f, Some(message)),
        Fail::Invalid { message, .. } | Fail::BadJson(message) => {
            v2_named("BadRequestException", Some(message))
        }
        Fail::TooLarge => v2_named("RequestEntityTooLargeException", None),
        Fail::Media(m) => v2_named("BadRequestException", Some(m)),
        Fail::NotFound(m) => v2_error("UnknownOperationException", 404, m),
        Fail::Method => v2_error("UnknownOperationException", 405, "Method not allowed."),
        Fail::Named(n) => v2_named(n, None),
        Fail::Internal(_) => v2_named("InternalFailure", None),
    }
}

/// v1 codes with status, fault type and default message.
const V1: &[(&str, u16, &str, &str)] = &[
    (
        "MessageRejected",
        400,
        "Sender",
        "Email address is not verified. The following identities failed the check in region US-EAST-1.",
    ),
    (
        "MailFromDomainNotVerified",
        400,
        "Sender",
        "The MAIL FROM domain is not verified.",
    ),
    (
        "ConfigurationSetDoesNotExist",
        400,
        "Sender",
        "Configuration set does not exist.",
    ),
    (
        "ConfigurationSetSendingPaused",
        400,
        "Sender",
        "Sending is paused for this configuration set.",
    ),
    (
        "AccountSendingPaused",
        400,
        "Sender",
        "Sending is paused for this account.",
    ),
    (
        "TemplateDoesNotExist",
        400,
        "Sender",
        "Template does not exist.",
    ),
    (
        "Throttling",
        400,
        "Sender",
        "Maximum sending rate exceeded.",
    ),
    (
        "InvalidParameterValue",
        400,
        "Sender",
        "The request is invalid.",
    ),
    (
        "MissingParameter",
        400,
        "Sender",
        "A required parameter is missing.",
    ),
    (
        "InvalidAction",
        400,
        "Sender",
        "The action is not valid for this endpoint.",
    ),
    (
        "AccessDenied",
        403,
        "Sender",
        "User is not authorized to perform ses:SendEmail.",
    ),
    (
        "IncompleteSignature",
        403,
        "Sender",
        "Authorization header requires 'Credential', 'SignedHeaders' and 'Signature' parameters.",
    ),
    (
        "InvalidClientTokenId",
        403,
        "Sender",
        "The security token included in the request is invalid.",
    ),
    (
        "ExpiredToken",
        403,
        "Sender",
        "The security token included in the request is expired.",
    ),
    (
        "RequestEntityTooLarge",
        413,
        "Sender",
        "Request size exceeded the maximum allowed size.",
    ),
    (
        "InternalFailure",
        500,
        "Receiver",
        "The request processing has failed because of an unknown error.",
    ),
    (
        "ServiceUnavailable",
        503,
        "Receiver",
        "The request has failed due to a temporary failure of the server.",
    ),
];

fn v1_common(n: &str) -> Option<(&'static str, Option<&'static str>)> {
    Some(match n {
        "invalidkey" => ("InvalidClientTokenId", None),
        "forbidden" => ("AccessDenied", None),
        "validation" => ("InvalidParameterValue", None),
        "messagerejected" => ("MessageRejected", None),
        "ratelimit" | "throttlingexception" | "toomanyrequestsexception" => ("Throttling", None),
        "quota" | "limitexceededexception" => ("Throttling", Some("Daily message quota exceeded.")),
        "templatenotfound" | "notfoundexception" => ("TemplateDoesNotExist", None),
        "toolarge" => ("RequestEntityTooLarge", None),
        "servererror" => ("InternalFailure", None),
        "unavailable" => ("ServiceUnavailable", None),
        "mailfromdomainnotverifiedexception" => ("MailFromDomainNotVerified", None),
        "sendingpausedexception" | "accountsuspendedexception" => ("AccountSendingPaused", None),
        "accessdeniedexception" => ("AccessDenied", None),
        "badrequestexception" => ("InvalidParameterValue", None),
        _ => return None,
    })
}

fn v1_error(code: &str, status: u16, kind: &str, message: &str) -> Reply {
    let rid = request_id();
    let body = format!(
        "<ErrorResponse xmlns=\"{XMLNS}\">\n  <Error>\n    <Type>{kind}</Type>\n    <Code>{}</Code>\n    <Message>{}</Message>\n  </Error>\n  <RequestId>{rid}</RequestId>\n</ErrorResponse>\n",
        xml_escape(code),
        xml_escape(message)
    );
    Reply {
        status,
        headers: vec![
            ("content-type".into(), "text/xml".into()),
            ("x-amzn-requestid".into(), rid),
        ],
        body: body.into_bytes(),
    }
}

fn v1_named(name: &str, message: Option<&str>) -> Reply {
    let (key, msg) = v1_common(&norm(name)).unwrap_or((name, None));
    match V1.iter().find(|(c, ..)| c.eq_ignore_ascii_case(key)) {
        Some((c, s, k, m)) => v1_error(c, *s, k, message.or(msg).unwrap_or(m)),
        None => v1_error(
            "InternalFailure",
            500,
            "Receiver",
            &format!("FastMock: unknown providerError `{name}` for ses"),
        ),
    }
}

/// v1 (query) errors: `<ErrorResponse><Error><Type/><Code/><Message/></Error><RequestId/>`.
pub fn failure_v1(fail: &Fail) -> Reply {
    match fail {
        Fail::MissingAuth => v1_named("IncompleteSignature", None),
        Fail::BadKey => v1_named("InvalidClientTokenId", None),
        Fail::Invalid {
            field: Some(f),
            message,
        } if f == "IncompleteSignature" || f == "InvalidAction" => v1_named(f, Some(message)),
        Fail::Invalid { message, .. } | Fail::BadJson(message) | Fail::Media(message) => {
            v1_named("InvalidParameterValue", Some(message))
        }
        Fail::TooLarge => v1_named("MessageRejected", Some("Message length exceeds the limit.")),
        Fail::NotFound(m) => v1_error("InvalidAction", 404, "Sender", m),
        Fail::Method => v1_error("InvalidAction", 405, "Sender", "Method not allowed."),
        Fail::Named(n) => v1_named(n, None),
        Fail::Internal(_) => v1_named("InternalFailure", None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as B64;

    fn h(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn json_req<'a>(body: &'a [u8]) -> Request<'a> {
        Request {
            headers: &[],
            query: "",
            body,
            form: &[],
            max_bytes: 1 << 20,
        }
    }

    #[test]
    fn accepts_well_formed_signatures_only() {
        let v4 = "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20261121/us-east-1/ses/aws4_request, SignedHeaders=host;x-amz-date, Signature=0123abcd";
        assert_eq!(
            access_key(&h(&[("authorization", v4)]), ""),
            Ok("AKIDEXAMPLE".into())
        );
        let v4a = "AWS4-ECDSA-P256-SHA256 Credential=AKID2/20261121/ses/aws4_request, SignedHeaders=host;x-amz-region-set, Signature=3045022100ab";
        assert_eq!(
            access_key(&h(&[("authorization", v4a)]), ""),
            Ok("AKID2".into())
        );
        assert_eq!(access_key(&[], ""), Err(Fail::MissingAuth));
        for bad in [
            "AWS4-HMAC-SHA256 Credential=AKID/20261121/us-east-1/ses/aws4_request, Signature=00",
            "AWS4-HMAC-SHA256 Credential=AKID/x, SignedHeaders=host, Signature=00",
            "AWS4-HMAC-SHA256 Credential=AKID/20261121/us-east-1/ses/aws4_request, SignedHeaders=host, Signature=zz",
            "Bearer x",
        ] {
            let e = access_key(&h(&[("authorization", bad)]), "").unwrap_err();
            let r = failure_v2(&e);
            assert_eq!(r.status, 403, "{bad}");
            assert_eq!(r.header("x-amzn-errortype"), Some("IncompleteSignature"));
        }
        assert_eq!(
            access_key(
                &[],
                "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKQ%2F20261121%2Fus-east-1%2Fses%2Faws4_request&X-Amz-Signature=ab"
            ),
            Ok("AKQ".into())
        );
    }

    #[test]
    fn v2_simple_raw_and_template() {
        let body = json!({
            "FromEmailAddress": "Shop <shop@example.com>",
            "Destination": {"ToAddresses": ["a@example.test"], "BccAddresses": ["b@example.test"]},
            "ReplyToAddresses": ["help@example.com"],
            "Content": {"Simple": {
                "Subject": {"Data": "Hi", "Charset": "UTF-8"},
                "Body": {"Text": {"Data": "Code 1234"}, "Html": {"Data": "<b>1234</b>"}},
                "Headers": [{"Name": "X-Trace", "Value": "t1"}],
                "Attachments": [{"FileName": "a.txt", "ContentType": "text/plain", "RawContent": B64.encode("hello")}]
            }},
            "EmailTags": [{"Name": "campaign", "Value": "spring"}]
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let p = parse_v2_send(&json_req(&bytes)).unwrap();
        let m = p.items[0].as_ref().unwrap();
        assert_eq!(m.rcpt_to, vec!["a@example.test", "b@example.test"]);
        assert_eq!(m.extras.metadata["campaign"], "spring");
        let raw = String::from_utf8_lossy(&m.raw);
        assert!(raw.contains("X-Trace: t1") && raw.contains("Subject: Hi"));
        let r = success_v2_send(&p);
        assert_eq!(r.status, 200);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["MessageId"], json!(m.extras.provider_message_id));
        assert!(r.header("x-amzn-requestid").is_some());

        let mime = "From: x@example.com\r\nTo: y@example.test\r\nSubject: Raw\r\n\r\nbody\r\n";
        let body = json!({"Content": {"Raw": {"Data": B64.encode(mime)}}});
        let bytes = serde_json::to_vec(&body).unwrap();
        let p = parse_v2_send(&json_req(&bytes)).unwrap();
        let m = p.items[0].as_ref().unwrap();
        assert_eq!(m.mail_from, "x@example.com");
        assert_eq!(m.rcpt_to, vec!["y@example.test"]);
        assert_eq!(m.raw, mime.as_bytes());

        let body = json!({
            "FromEmailAddress": "shop@example.com",
            "Destination": {"ToAddresses": ["a@example.test"]},
            "Content": {"Template": {
                "TemplateContent": {"Subject": "Hi {{name}}", "Html": "<p>{{name}}</p>", "Text": "{{name}}"},
                "TemplateData": "{\"name\":\"<Ann>\"}"
            }}
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let p = parse_v2_send(&json_req(&bytes)).unwrap();
        let m = p.items[0].as_ref().unwrap();
        let raw = String::from_utf8_lossy(&m.raw);
        assert!(raw.contains("Subject: Hi <Ann>"), "{raw}");
        assert!(raw.contains("&lt;Ann&gt;"));
        assert_eq!(m.extras.template_data, Some(json!({"name": "<Ann>"})));

        let body = json!({
            "FromEmailAddress": "shop@example.com",
            "Destination": {"ToAddresses": ["a@example.test"]},
            "Content": {"Template": {"TemplateName": "welcome", "TemplateData": "{\"n\":1}"}}
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let p = parse_v2_send(&json_req(&bytes)).unwrap();
        let m = p.items[0].as_ref().unwrap();
        assert_eq!(m.extras.template_id.as_deref(), Some("welcome"));
        assert_eq!(m.extras.template_data, Some(json!({"n": 1})));
    }

    #[test]
    fn v2_validation_errors() {
        let check = |body: Value, msg: &str| {
            let bytes = serde_json::to_vec(&body).unwrap();
            let e = parse_v2_send(&json_req(&bytes)).unwrap_err();
            let r = failure_v2(&e);
            assert_eq!(r.status, 400, "{body}");
            assert_eq!(r.header("x-amzn-errortype"), Some("BadRequestException"));
            let v: Value = serde_json::from_slice(&r.body).unwrap();
            assert!(v["message"].as_str().unwrap().contains(msg), "{v}");
        };
        check(json!({"Destination": {}}), "Content is required");
        check(
            json!({"FromEmailAddress": "a@example.com", "Content": {"Simple": {}}}),
            "Destination",
        );
        check(
            json!({"Destination": {"ToAddresses": ["a@example.test"]}, "Content": {"Simple": {}}}),
            "FromEmailAddress",
        );
        let many: Vec<String> = (0..51).map(|i| format!("u{i}@example.test")).collect();
        check(
            json!({"FromEmailAddress": "a@example.com", "Destination": {"ToAddresses": many}, "Content": {"Simple": {}}}),
            "exceeds 50",
        );
        let r = failure_v2(&Fail::BadJson("x".into()));
        assert_eq!(r.status, 400);
        assert_eq!(failure_v2(&Fail::Named("rateLimit".into())).status, 429);
        let r = failure_v2(&Fail::Named("NotFoundException".into()));
        assert_eq!(
            (r.status, r.header("x-amzn-errortype")),
            (404, Some("NotFoundException"))
        );
        assert_eq!(failure_v2(&Fail::Named("nope".into())).status, 500);
    }

    #[test]
    fn v2_bulk_has_per_entry_results() {
        let body = json!({
            "FromEmailAddress": "shop@example.com",
            "DefaultContent": {"Template": {"TemplateName": "promo", "TemplateData": "{\"a\":1,\"b\":1}"}},
            "BulkEmailEntries": [
                {"Destination": {"ToAddresses": ["a@example.test"]},
                 "ReplacementEmailContent": {"ReplacementTemplate": {"ReplacementTemplateData": "{\"b\":2}"}}},
                {"Destination": {"ToAddresses": ["not an address"]}}
            ]
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let p = parse_v2_bulk(&json_req(&bytes)).unwrap();
        assert_eq!(
            p.items[0].as_ref().unwrap().extras.template_data,
            Some(json!({"a": 1, "b": 2}))
        );
        let r = success_v2_bulk(&p);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        let res = v["BulkEmailEntryResults"].as_array().unwrap();
        assert_eq!(res[0]["Status"], "SUCCESS");
        assert!(res[0]["MessageId"].is_string());
        assert_eq!(res[1]["Status"], "INVALID_PARAMETER");
        assert!(res[1]["Error"].is_string());
    }

    #[test]
    fn v1_send_email_and_raw() {
        let pairs = [
            ("Action", "SendEmail"),
            ("Version", "2010-12-01"),
            ("Source", "shop@example.com"),
            ("Destination.ToAddresses.member.1", "a@example.test"),
            ("Destination.ToAddresses.member.2", "b@example.test"),
            ("Destination.BccAddresses.member.1", "c@example.test"),
            ("Message.Subject.Data", "Hello & bye"),
            ("Message.Body.Text.Data", "text"),
            ("Tags.member.1.Name", "k"),
            ("Tags.member.1.Value", "v"),
        ];
        let form: Vec<FormPart> = pairs.iter().map(|(k, v)| FormPart::text(k, v)).collect();
        let req = Request {
            headers: &[],
            query: "",
            body: b"",
            form: &form,
            max_bytes: 1 << 20,
        };
        let p = parse_v1(&req).unwrap();
        let m = p.items[0].as_ref().unwrap();
        assert_eq!(
            m.rcpt_to,
            vec!["a@example.test", "b@example.test", "c@example.test"]
        );
        assert_eq!(m.extras.metadata["k"], "v");
        let r = success_v1(&p);
        let body = String::from_utf8(r.body.clone()).unwrap();
        assert!(
            body.starts_with(
                "<SendEmailResponse xmlns=\"http://ses.amazonaws.com/doc/2010-12-01/\">"
            )
        );
        assert!(body.contains(&format!(
            "<MessageId>{}</MessageId>",
            m.extras.provider_message_id.as_deref().unwrap()
        )));
        assert_eq!(r.header("content-type"), Some("text/xml"));

        let mime = "From: x@example.com\r\nTo: y@example.test\r\n\r\nbody\r\n";
        let form = vec![
            FormPart::text("Action", "SendRawEmail"),
            FormPart::text("RawMessage.Data", &B64.encode(mime)),
            FormPart::text("Destinations.member.1", "z@example.test"),
        ];
        let req = Request { form: &form, ..req };
        let p = parse_v1(&req).unwrap();
        assert_eq!(p.items[0].as_ref().unwrap().rcpt_to, vec!["z@example.test"]);
        assert!(
            String::from_utf8(success_v1(&p).body)
                .unwrap()
                .contains("<SendRawEmailResult>")
        );

        let form = vec![FormPart::text("Action", "ListIdentities")];
        let e = parse_v1(&Request { form: &form, ..req }).unwrap_err();
        let r = failure_v1(&e);
        let body = String::from_utf8(r.body).unwrap();
        assert_eq!(r.status, 400);
        assert!(body.contains("<Code>InvalidAction</Code>"), "{body}");
        let r = failure_v1(&Fail::MissingAuth);
        let body = String::from_utf8(r.body).unwrap();
        assert_eq!(r.status, 403);
        assert!(
            body.contains("<Type>Sender</Type>")
                && body.contains("<Code>IncompleteSignature</Code>")
        );
        let many: Vec<FormPart> = std::iter::once(FormPart::text("Action", "SendEmail"))
            .chain(std::iter::once(FormPart::text("Source", "s@example.com")))
            .chain((1..=51).map(|i| {
                FormPart::text(
                    &format!("Destination.ToAddresses.member.{i}"),
                    &format!("u{i}@example.test"),
                )
            }))
            .collect();
        let e = parse_v1(&Request { form: &many, ..req }).unwrap_err();
        assert!(e.message().contains("exceeds 50"));
        let r = failure_v1(&Fail::Named("rateLimit".into()));
        assert!(
            String::from_utf8(r.body)
                .unwrap()
                .contains("<Code>Throttling</Code>")
        );
    }
}
