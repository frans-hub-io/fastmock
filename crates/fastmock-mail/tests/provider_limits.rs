//! Size and expansion limits of HTML inlining and provider payloads.

use base64::Engine as _;
use fastmock_mail::providers::{self, Fail, FormPart, Parsed, Request};
use fastmock_mail::{ParseLimits, ParsedMail};
use serde_json::{Value, json};

const JSON: [(&str, &str); 1] = [("content-type", "application/json")];

fn headers() -> Vec<(String, String)> {
    JSON.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn call(
    body: &Value,
    max_bytes: usize,
    f: fn(&Request<'_>) -> Result<Parsed, Fail>,
) -> Result<Parsed, Fail> {
    let h = headers();
    let b = serde_json::to_vec(body).unwrap();
    f(&Request {
        headers: &h,
        query: "",
        body: &b,
        form: &[],
        max_bytes,
    })
}

fn mailgun(form: &[FormPart], max_bytes: usize, mime: bool) -> Result<Parsed, Fail> {
    providers::mailgun::parse(
        &Request {
            headers: &[],
            query: "",
            body: b"",
            form,
            max_bytes,
        },
        "mg.example.com",
        mime,
    )
}

fn text_of(p: &Parsed, i: usize) -> String {
    let m = p.items[i].as_ref().unwrap();
    ParsedMail::parse(&m.raw, &ParseLimits::default())
        .text()
        .unwrap_or_default()
}

fn related(html: &str, parts: &[(&str, Vec<u8>)]) -> String {
    let mut raw = format!(
        "From: a@example.com\r\nTo: b@example.test\r\nSubject: t\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/related; boundary=BB\r\n\r\n--BB\r\nContent-Type: text/html\r\n\r\n{html}\r\n"
    );
    for (cid, data) in parts {
        let b64 = base64::engine::general_purpose::STANDARD.encode(data);
        let lines: Vec<&str> = b64
            .as_bytes()
            .chunks(76)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        raw.push_str(&format!(
            "--BB\r\nContent-Type: image/png\r\nContent-ID: <{cid}>\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n",
            lines.join("\r\n")
        ));
    }
    raw.push_str("--BB--\r\n");
    raw
}

#[test]
fn cid_inlining_is_charged_per_reference_and_capped() {
    let html = "<img src=\"cid:x\">".repeat(200);
    let raw = related(&html, &[("x", vec![b'A'; 30 * 1024])]);
    let p = ParsedMail::parse(raw.as_bytes(), &ParseLimits::default());
    let out = p.html_with_inline_parts(4 * 1024 * 1024).unwrap();
    let cap = (raw.len() * 2).max(4 * 1024 * 1024);
    assert!(out.len() <= cap, "{} > {cap}", out.len());
    assert!(out.contains("data:image/png;base64,"));
    assert!(out.contains("cid:x"), "references beyond the budget stay");

    let small = p.html_with_inline_parts(100 * 1024).unwrap();
    assert_eq!(small.matches("data:image/png").count(), 2);
    assert_eq!(small.matches("cid:x\"").count(), 198);
}

#[test]
fn cid_references_match_whole_tokens() {
    let html = "<img src=\"cid:ab\"><img src='cid:a'><div style=\"background:url(cid:a)\">cid:a>";
    let raw = related(
        html,
        &[("a", b"first".to_vec()), ("ab", b"second".to_vec())],
    );
    let p = ParsedMail::parse(raw.as_bytes(), &ParseLimits::default());
    let out = p.html_with_inline_parts(1 << 20).unwrap();
    let a = base64::engine::general_purpose::STANDARD.encode(b"first");
    let ab = base64::engine::general_purpose::STANDARD.encode(b"second");
    assert!(
        out.contains(&format!("src=\"data:image/png;base64,{ab}\"")),
        "{out}"
    );
    assert!(
        out.contains(&format!("src='data:image/png;base64,{a}'")),
        "{out}"
    );
    assert!(
        out.contains(&format!("url(data:image/png;base64,{a})")),
        "{out}"
    );
    assert!(!out.contains("cid:"), "{out}");
    let prefix = "<img src=\"cid:abc\">";
    let raw = related(
        prefix,
        &[("a", b"first".to_vec()), ("ab", b"second".to_vec())],
    );
    let p = ParsedMail::parse(raw.as_bytes(), &ParseLimits::default());
    assert_eq!(p.html_with_inline_parts(1 << 20).unwrap(), prefix);
}

fn ses_template(html: &str, data: Value) -> Value {
    json!({
        "FromEmailAddress": "shop@example.com",
        "Destination": {"ToAddresses": ["a@example.test"]},
        "Content": {"Template": {
            "TemplateContent": {"Subject": "s", "Html": html},
            "TemplateData": data.to_string()
        }}
    })
}

fn invalid_message(r: Result<Parsed, Fail>) -> String {
    match r {
        Err(Fail::Invalid { message, .. }) => message,
        other => panic!("expected a validation error, got {other:?}"),
    }
}

#[test]
fn ses_templates_refuse_inline_partials() {
    for tpl in [
        "{{#*inline \"p\"}}x{{/inline}}{{> p}}",
        "{{> p}}",
        "{{#> layout}}x{{/layout}}",
        "{{#if a}}{{#*inline \"q\"}}y{{/inline}}{{/if}}",
    ] {
        let m = invalid_message(call(
            &ses_template(tpl, json!({"a": true})),
            1 << 20,
            providers::ses::parse_v2_send,
        ));
        assert!(m.contains("partials"), "{tpl}: {m}");
    }
}

#[test]
fn ses_recursive_inline_partial_is_refused() {
    let tpl = "{{#*inline \"p\"}}x{{> p}}{{/inline}}{{> p}}";
    let m = invalid_message(call(
        &ses_template(tpl, json!({})),
        1 << 20,
        providers::ses::parse_v2_send,
    ));
    assert!(m.contains("partials"), "{m}");
}

#[test]
fn ses_template_nesting_and_output_are_bounded() {
    let deep = format!("{}x{}", "{{#if a}}".repeat(40), "{{/if}}".repeat(40));
    let m = invalid_message(call(
        &ses_template(&deep, json!({"a": true})),
        1 << 20,
        providers::ses::parse_v2_send,
    ));
    assert!(m.contains("deeper"), "{m}");
    let paren = format!("{{{{x {}a{}}}}}", "(lookup ".repeat(40), ")".repeat(40));
    assert!(
        call(
            &ses_template(&paren, json!({})),
            1 << 20,
            providers::ses::parse_v2_send
        )
        .is_err()
    );

    let wide = "{{a}}".repeat(100);
    let m = invalid_message(call(
        &ses_template(&wide, json!({"a": "x".repeat(1000)})),
        64 * 1024,
        providers::ses::parse_v2_send,
    ));
    assert!(m.contains("rendered template is larger"), "{m}");
    let ok = call(
        &ses_template("{{#each a}}{{this}}{{/each}}", json!({"a": [1, 2, 3]})),
        64 * 1024,
        providers::ses::parse_v2_send,
    )
    .unwrap();
    assert!(ok.items[0].is_ok());
}

#[test]
fn sendgrid_substitutions_apply_in_one_pass() {
    let body = json!({
        "personalizations": [{"to": [{"email": "a@example.test"}],
            "substitutions": {"-a-": "-b--b-", "-b-": "-c--c-", "-c-": "z"}}],
        "from": {"email": "s@example.com"}, "subject": "x -a-",
        "content": [{"type": "text/plain", "value": "-a- -b- -c-"}]
    });
    let p = call(&body, 1 << 20, providers::sendgrid::parse).unwrap();
    assert_eq!(text_of(&p, 0).trim_end(), "-b--b- -c--c- z");

    let k = 3000;
    let chain = json!({
        "personalizations": [{"to": [{"email": "a@example.test"}],
            "substitutions": {"a": "b".repeat(k), "b": "c".repeat(k), "c": "d".repeat(k)}}],
        "from": {"email": "s@example.com"}, "subject": "x",
        "content": [{"type": "text/plain", "value": "a".repeat(10)}]
    });
    let p = call(&chain, 1 << 20, providers::sendgrid::parse).unwrap();
    let m = p.items[0].as_ref().unwrap();
    assert!(m.raw.len() < 64 * 1024, "{}", m.raw.len());

    let big = json!({
        "personalizations": [{"to": [{"email": "a@example.test"}],
            "substitutions": {"a": "x".repeat(9000)}}],
        "from": {"email": "s@example.com"}, "subject": "x",
        "content": [{"type": "text/plain", "value": "a".repeat(200)}]
    });
    assert_eq!(
        call(&big, 1 << 20, providers::sendgrid::parse).unwrap_err(),
        Fail::TooLarge
    );
}

fn mg_form(text: &str, to: &[&str], vars: &Value) -> Vec<FormPart> {
    let mut f = vec![
        FormPart::text("from", "s@example.com"),
        FormPart::text("subject", "x"),
        FormPart::text("text", text),
        FormPart::text("recipient-variables", &vars.to_string()),
    ];
    for t in to {
        f.push(FormPart::text("to", t));
    }
    f
}

#[test]
fn mailgun_recipient_variables_apply_in_one_pass() {
    let vars = json!({"a@example.test": {"a": "%recipient.b%", "b": "B"}});
    let p = mailgun(
        &mg_form("%recipient.a% %recipient.b%", &["a@example.test"], &vars),
        1 << 20,
        false,
    )
    .unwrap();
    assert_eq!(text_of(&p, 0).trim_end(), "%recipient.b% B");
}

#[test]
fn mailgun_recipient_variables_count_against_the_fanout_budget() {
    let text = "%recipient.a%".repeat(1000);
    let value = "x".repeat(50_000);
    let to = ["a@example.test", "b@example.test", "c@example.test"];
    let vars: serde_json::Map<String, Value> = to
        .iter()
        .map(|t| (t.to_string(), json!({"a": value})))
        .collect();
    let r = mailgun(&mg_form(&text, &to, &Value::Object(vars)), 64 << 20, false);
    assert_eq!(r.unwrap_err(), Fail::TooLarge);
}

#[test]
fn mailgun_mime_recipients_are_capped() {
    let to: Vec<String> = (0..20_000).map(|i| format!("u{i}@example.test")).collect();
    let raw = "From: s@example.com\r\nTo: a@example.test\r\nSubject: x\r\n\r\nhi\r\n";
    let form = vec![
        FormPart::text("to", &to.join(",")),
        FormPart {
            name: "message".into(),
            filename: Some("m.eml".into()),
            data: bytes::Bytes::from(raw),
            ..Default::default()
        },
    ];
    let m = invalid_message(mailgun(&form, 1 << 20, true));
    assert!(m.contains("Too many recipients"), "{m}");
    let form = vec![
        FormPart::text("to", "a@example.test"),
        FormPart {
            name: "message".into(),
            filename: Some("m.eml".into()),
            data: bytes::Bytes::from(raw),
            ..Default::default()
        },
    ];
    assert!(mailgun(&form, 1 << 20, true).is_ok());
}

#[test]
fn per_message_metadata_is_capped() {
    let big = "x".repeat(200 * 1024);
    let sg = json!({
        "personalizations": [{"to": [{"email": "a@example.test"}]}, {"to": [{"email": "b@example.test"}]}],
        "from": {"email": "s@example.com"}, "subject": "x",
        "custom_args": {"k": big},
        "content": [{"type": "text/plain", "value": "hi"}]
    });
    let m = invalid_message(call(&sg, 1 << 20, providers::sendgrid::parse));
    assert!(m.contains("metadata"), "{m}");

    let brevo = json!({
        "sender": {"email": "s@example.com"}, "to": [{"email": "a@example.test"}],
        "subject": "x", "htmlContent": "<p>x</p>", "params": {"k": big}
    });
    let r = call(&brevo, 1 << 20, providers::brevo::parse);
    assert!(
        matches!(&r, Err(Fail::Invalid { message, .. }) if message.contains("metadata")),
        "{r:?}"
    );

    let ses = json!({
        "FromEmailAddress": "s@example.com",
        "DefaultContent": {"Template": {"TemplateName": "t"}},
        "DefaultEmailTags": [{"Name": "k", "Value": big}],
        "BulkEmailEntries": [{"Destination": {"ToAddresses": ["a@example.test"]}}]
    });
    let p = call(&ses, 1 << 20, providers::ses::parse_v2_bulk).unwrap();
    assert!(
        matches!(&p.items[0], Err(Fail::Invalid { message, .. }) if message.contains("metadata")),
        "{:?}",
        p.items[0]
    );

    let small = json!({
        "personalizations": [{"to": [{"email": "a@example.test"}]}],
        "from": {"email": "s@example.com"}, "subject": "x",
        "custom_args": {"k": "v"}, "categories": ["c"],
        "content": [{"type": "text/plain", "value": "hi"}]
    });
    assert!(call(&small, 1 << 20, providers::sendgrid::parse).is_ok());
}
