//! Provider HTTP APIs (`/__mail/<provider>/...`): SDK-shaped requests, stored messages, exact answers,
//! errors, expected keys, chaos, journal and the reserved prefix.

use std::sync::OnceLock;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use fastmock_core::mail::{MailEntry, MailQuery, MailSource};
use fastmock_core::model::SpaceFilter;
use fastmock_mail::MailConfig;
use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use serde_json::{Value, json};
use sqlx::Connection;
use tower::ServiceExt;

const ADMIN: &str = "fm_admin_token_for_mail_provider_tests_01";

const ACCESS: &str = r#"
spaces:
  - name: billing
    mailDomains: [billing.example.test]
  - name: crm
    mailDomains: [crm.example.test]
"#;

const SIGV4: &str = "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20261121/us-east-1/ses/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7";

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

async fn storages() -> Vec<StorageKind> {
    let mut v = vec![StorageKind::Memory];
    let (Ok(admin_url), Ok(valkey_url)) = (
        std::env::var("FASTMOCK_TEST_DATABASE_URL"),
        std::env::var("FASTMOCK_TEST_VALKEY_URL"),
    ) else {
        return v;
    };
    let db = format!("fm_mailprov_{}", uuid::Uuid::new_v4().simple());
    let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut c)
        .await
        .unwrap();
    v.push(StorageKind::Postgres {
        database_url: format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db),
        valkey_url,
        valkey_prefix: format!("mailprov:{}:", uuid::Uuid::new_v4().simple()),
        max_connections: 5,
        options: Default::default(),
    });
    v
}

struct Fm {
    app: fastmock_server::state::AppState,
    data: axum::Router,
    admin: axum::Router,
}

async fn server(storage: StorageKind, max_message: usize) -> Fm {
    server_with(storage, max_message, None).await
}

async fn server_with(storage: StorageKind, max_message: usize, max_body: Option<usize>) -> Fm {
    let dir = std::env::temp_dir().join(format!("fm-mail-prov-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let access = dir.join("access.yaml");
    std::fs::write(&access, ACCESS).unwrap();
    let mut cfg = Config::for_tests(storage, vec![format!("ops:admin:{}", hash_token(ADMIN))]);
    cfg.plugins = Some(vec![]);
    cfg.access_file = Some(access);
    if let Some(b) = max_body {
        cfg.max_body_bytes = b;
        cfg.max_inflight_body_bytes = cfg.max_inflight_body_bytes.max(b * 4);
    }
    let mut mail = MailConfig::default();
    mail.listeners.smtp = None;
    mail.max_message_bytes = max_message;
    cfg.mail = Some(mail);
    let (app, data, admin, _) = fastmock_server::build(cfg, prom()).await.unwrap();
    Fm { app, data, admin }
}

struct Res {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Res {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&self.body)))
    }

    fn header(&self, n: &str) -> Option<&str> {
        self.headers.get(n).and_then(|v| v.to_str().ok())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

async fn send(
    r: &axum::Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: impl Into<Body>,
) -> Res {
    let mut b = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let res = r
        .clone()
        .oneshot(b.body(body.into()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = axum::body::to_bytes(res.into_body(), 1 << 24)
        .await
        .unwrap();
    Res {
        status,
        headers,
        body,
    }
}

async fn admin(fm: &Fm, method: &str, path: &str, body: Value) -> Res {
    send(
        &fm.admin,
        method,
        &format!("/__admin{path}"),
        &[
            ("authorization", &format!("Bearer {ADMIN}")),
            ("content-type", "application/json"),
        ],
        body.to_string(),
    )
    .await
}

async fn messages(fm: &Fm, space: Option<&str>) -> Vec<MailEntry> {
    let svc = fm.app.mail.clone().unwrap();
    let spaces = match space {
        Some(s) => SpaceFilter::In(vec![s.to_string()]),
        None => SpaceFilter::Any,
    };
    let mut items = svc
        .store
        .list(&MailQuery {
            spaces,
            limit: 200,
            ..Default::default()
        })
        .await
        .unwrap()
        .items;
    items.retain(|e| space.is_some() || e.space.is_none());
    items.reverse();
    items
}

async fn raw_of(fm: &Fm, e: &MailEntry) -> String {
    let svc = fm.app.mail.clone().unwrap();
    let m = svc
        .store
        .get(e.id, &SpaceFilter::Any)
        .await
        .unwrap()
        .unwrap();
    String::from_utf8_lossy(&m.raw).into_owned()
}

fn multipart(fields: &[(&str, Option<&str>, &[u8])]) -> (String, Vec<u8>) {
    let boundary = "----fastmockTestBoundary7MA4YWxkTrZu0gW";
    let mut body = Vec::new();
    for (name, filename, data) in fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match filename {
            Some(f) => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
            ),
            None => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            ),
        }
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for b in data {
        crc ^= *b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
    out.extend(miniz_oxide::deflate::compress_to_vec(data, 6));
    out.extend(crc32(data).to_le_bytes());
    out.extend((data.len() as u32).to_le_bytes());
    out
}

#[tokio::test]
async fn ses_v2_v1_and_bulk() {
    for storage in storages().await {
        let fm = server(storage, 1 << 20).await;
        let ses_headers = [
            ("authorization", SIGV4),
            ("x-amz-date", "20261121T101500Z"),
            ("content-type", "application/json"),
            ("x-amz-user-agent", "aws-sdk-js/3.700.0"),
        ];
        let body = json!({
            "FromEmailAddress": "Shop <shop@example.com>",
            "Destination": {"ToAddresses": ["ann@billing.example.test"], "BccAddresses": ["audit@billing.example.test"]},
            "Content": {"Simple": {
                "Subject": {"Data": "Your invoice", "Charset": "UTF-8"},
                "Body": {"Text": {"Data": "Total 42", "Charset": "UTF-8"}, "Html": {"Data": "<p>Total 42</p>"}},
                "Attachments": [{"FileName": "invoice.pdf", "ContentType": "application/pdf", "RawContent": B64.encode("%PDF-1.4")}]
            }},
            "EmailTags": [{"Name": "kind", "Value": "invoice"}]
        });
        let r = send(
            &fm.data,
            "POST",
            "/__mail/ses/v2/email/outbound-emails",
            &ses_headers,
            body.to_string(),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        let id = r.json()["MessageId"].as_str().unwrap().to_string();
        assert!(id.ends_with("-000000") && id.starts_with("0100"), "{id}");
        assert!(r.header("x-amzn-requestid").is_some());
        let stored = messages(&fm, Some("billing")).await;
        assert_eq!(stored.len(), 1);
        let m = &stored[0];
        assert_eq!(m.source, MailSource::Provider);
        assert_eq!(m.provider.as_deref(), Some("ses"));
        assert_eq!(m.envelope.mail_from, "shop@example.com");
        assert_eq!(
            m.envelope.rcpt_to,
            vec!["ann@billing.example.test", "audit@billing.example.test"]
        );
        assert_eq!(m.summary.subject.as_deref(), Some("Your invoice"));
        assert!(m.summary.has_attachments);
        let ex = m.provider_extras.clone().unwrap();
        assert_eq!(ex.provider_message_id.as_deref(), Some(id.as_str()));
        assert_eq!(ex.metadata["kind"], "invoice");
        assert!(!raw_of(&fm, m).await.contains("audit@"));

        let r = send(
            &fm.data,
            "POST",
            "/__mail/ses/v2/email/outbound-emails",
            &[("content-type", "application/json")],
            body.to_string(),
        )
        .await;
        assert_eq!(r.status, 403);
        assert_eq!(r.header("x-amzn-errortype"), Some("IncompleteSignature"));
        assert!(r.json()["message"].is_string());
        let sigv4a = "AWS4-ECDSA-P256-SHA256 Credential=AKIDEXAMPLE/20261121/ses/aws4_request, SignedHeaders=host;x-amz-region-set, Signature=3045022100aa";
        let r = send(
            &fm.data,
            "POST",
            "/__mail/ses/@crm/v2/email/outbound-emails",
            &[
                ("authorization", sigv4a),
                ("content-type", "application/json"),
            ],
            body.to_string(),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        assert_eq!(messages(&fm, Some("crm")).await.len(), 1);

        let bulk = json!({
            "FromEmailAddress": "shop@example.com",
            "DefaultContent": {"Template": {
                "TemplateContent": {"Subject": "Hi {{name}}", "Text": "Code {{code}}"},
                "TemplateData": "{\"name\":\"friend\",\"code\":\"0000\"}"
            }},
            "BulkEmailEntries": [
                {"Destination": {"ToAddresses": ["a@crm.example.test"]},
                 "ReplacementEmailContent": {"ReplacementTemplate": {"ReplacementTemplateData": "{\"name\":\"Ann\",\"code\":\"1111\"}"}}},
                {"Destination": {"ToAddresses": ["broken address"]}}
            ]
        });
        let r = send(
            &fm.data,
            "POST",
            "/__mail/ses/v2/email/outbound-bulk-emails",
            &ses_headers,
            bulk.to_string(),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        let v = r.json();
        assert_eq!(v["BulkEmailEntryResults"][0]["Status"], "SUCCESS");
        assert_eq!(v["BulkEmailEntryResults"][1]["Status"], "INVALID_PARAMETER");
        let crm = messages(&fm, Some("crm")).await;
        assert_eq!(crm.len(), 2);
        assert_eq!(crm[1].summary.subject.as_deref(), Some("Hi Ann"));
        assert_eq!(
            crm[1].provider_extras.as_ref().unwrap().template_data,
            Some(json!({"name": "Ann", "code": "1111"}))
        );

        let form = "Action=SendEmail&Version=2010-12-01&Source=shop%40example.com&Destination.ToAddresses.member.1=v1%40crm.example.test&Message.Subject.Data=Query+API&Message.Body.Text.Data=hello";
        let r = send(
            &fm.data,
            "POST",
            "/__mail/ses/",
            &[
                ("authorization", SIGV4),
                (
                    "content-type",
                    "application/x-www-form-urlencoded; charset=utf-8",
                ),
            ],
            form,
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        assert_eq!(r.header("content-type"), Some("text/xml"));
        let xml = r.text();
        assert!(
            xml.starts_with(
                "<SendEmailResponse xmlns=\"http://ses.amazonaws.com/doc/2010-12-01/\">"
            ),
            "{xml}"
        );
        assert!(xml.contains("<MessageId>0100") && xml.contains("<RequestId>"));
        let raw = B64.encode(
            "From: x@example.com\r\nTo: raw@crm.example.test\r\nSubject: Raw v1\r\n\r\nbody\r\n",
        );
        let form = format!("Action=SendRawEmail&RawMessage.Data={}", url_encode(&raw));
        let r = send(
            &fm.data,
            "POST",
            "/__mail/ses",
            &[
                ("authorization", SIGV4),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            form,
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        assert!(r.text().contains("<SendRawEmailResult>"));
        let crm = messages(&fm, Some("crm")).await;
        assert_eq!(
            crm.last().unwrap().summary.subject.as_deref(),
            Some("Raw v1")
        );
        let r = send(
            &fm.data,
            "POST",
            "/__mail/ses/",
            &[("content-type", "application/x-www-form-urlencoded")],
            "Action=SendEmail",
        )
        .await;
        assert_eq!(r.status, 403);
        assert!(r.text().contains("<Code>IncompleteSignature</Code>"));
    }
}

fn url_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[tokio::test]
async fn sendgrid_personalizations_gzip_sandbox_and_errors() {
    let fm = server(StorageKind::Memory, 1 << 20).await;
    let body = json!({
        "personalizations": [
            {"to": [{"email": "ann@billing.example.test", "name": "Ann"}], "dynamic_template_data": {"code": "1234"}},
            {"to": [{"email": "bob@billing.example.test"}], "dynamic_template_data": {"code": "5678"}}
        ],
        "from": {"email": "noreply@example.com", "name": "Example"},
        "template_id": "d-f43daeeaef504760851f727007e0b5d0",
        "categories": ["otp"],
        "custom_args": {"tenant": "t1"}
    });
    let gz = gzip(body.to_string().as_bytes());
    let r = send(
        &fm.data,
        "POST",
        "/__mail/sendgrid/v3/mail/send",
        &[
            ("authorization", "Bearer SG.test-key"),
            ("content-type", "application/json"),
            ("content-encoding", "gzip"),
            ("user-agent", "sendgrid/8.1.0;nodejs"),
        ],
        gz,
    )
    .await;
    assert_eq!(r.status, 202, "{}", r.text());
    assert!(r.body.is_empty());
    let xid = r.header("x-message-id").unwrap().to_string();
    let stored = messages(&fm, Some("billing")).await;
    assert_eq!(stored.len(), 2);
    let ex = stored[1].provider_extras.clone().unwrap();
    assert_eq!(ex.provider_message_id.as_deref(), Some(xid.as_str()));
    assert_eq!(
        ex.template_id.as_deref(),
        Some("d-f43daeeaef504760851f727007e0b5d0")
    );
    assert_eq!(ex.template_data, Some(json!({"code": "5678"})));
    assert_eq!(ex.tags, vec!["otp"]);
    assert_eq!(ex.metadata["tenant"], "t1");
    assert_eq!(stored[1].envelope.rcpt_to, vec!["bob@billing.example.test"]);

    let sandbox = json!({
        "personalizations": [{"to": [{"email": "s@billing.example.test"}]}],
        "from": {"email": "noreply@example.com"}, "subject": "Sandbox",
        "content": [{"type": "text/plain", "value": "x"}],
        "mail_settings": {"sandbox_mode": {"enable": true}}
    });
    let r = send(
        &fm.data,
        "POST",
        "/__mail/sendgrid/v3/mail/send",
        &[
            ("authorization", "Bearer SG.k"),
            ("content-type", "application/json"),
        ],
        sandbox.to_string(),
    )
    .await;
    assert_eq!(r.status, 200);
    let stored = messages(&fm, Some("billing")).await;
    assert!(
        stored
            .last()
            .unwrap()
            .provider_extras
            .as_ref()
            .unwrap()
            .test_mode
    );

    let r = send(
        &fm.data,
        "POST",
        "/__mail/sendgrid/v3/mail/send",
        &[("content-type", "application/json")],
        sandbox.to_string(),
    )
    .await;
    assert_eq!(r.status, 401);
    assert_eq!(
        r.json(),
        json!({"errors": [{"field": null, "message": "authorization required", "help": null}]})
    );
    let r = send(
        &fm.data,
        "POST",
        "/__mail/sendgrid/v3/mail/send",
        &[
            ("authorization", "Bearer SG.k"),
            ("content-type", "application/json"),
        ],
        "{\"personalizations\": []}",
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["errors"][0]["field"], "personalizations");
    let r = send(
        &fm.data,
        "GET",
        "/__mail/sendgrid/v3/mail/send",
        &[],
        Body::empty(),
    )
    .await;
    assert_eq!(r.status, 405);
    let r = send(
        &fm.data,
        "POST",
        "/__mail/sendgrid/v3/mail/send",
        &[
            ("authorization", "Bearer SG.k"),
            ("content-type", "application/json"),
            ("content-encoding", "br"),
        ],
        "{}",
    )
    .await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn mailgun_multipart_urlencoded_and_mime() {
    let fm = server(StorageKind::Memory, 1 << 20).await;
    let auth = format!(
        "Basic {}",
        B64.encode("api:key-3ax6xnjp29jd6fds4gc373sgvjxteol0")
    );
    let (ct, body) = multipart(&[
        ("from", None, b"Excited User <mailgun@mg.example.com>"),
        ("to", None, b"ann@billing.example.test"),
        ("to", None, b"Bob <bob@billing.example.test>"),
        ("subject", None, b"Hello"),
        ("text", None, b"Testing some Mailgun awesomeness!"),
        ("o:tag", None, b"newsletter"),
        ("v:order-id", None, b"123"),
        ("h:X-Campaign", None, b"spring"),
        ("attachment", Some("notes.txt"), b"some notes"),
    ]);
    let r = send(
        &fm.data,
        "POST",
        "/__mail/mailgun/v3/mg.example.com/messages",
        &[("authorization", &auth), ("content-type", &ct)],
        body,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!(v["message"], "Queued. Thank you.");
    let id = v["id"].as_str().unwrap().to_string();
    assert!(id.starts_with('<') && id.ends_with("@mg.example.com>"));
    let stored = messages(&fm, Some("billing")).await;
    let m = &stored[0];
    assert_eq!(m.provider.as_deref(), Some("mailgun"));
    assert_eq!(
        m.summary.message_id.as_deref(),
        Some(id.trim_matches(['<', '>']))
    );
    assert_eq!(m.summary.attachment_names, vec!["notes.txt"]);
    let ex = m.provider_extras.clone().unwrap();
    assert_eq!(ex.tags, vec!["newsletter"]);
    assert_eq!(ex.metadata["order-id"], "123");
    assert!(raw_of(&fm, m).await.contains("X-Campaign: spring"));

    let form = "from=shop%40mg.example.com&to=c%40billing.example.test&subject=Urlencoded&html=%3Cb%3Ehi%3C%2Fb%3E&o%3Atestmode=yes";
    let r = send(
        &fm.data,
        "POST",
        "/__mail/mailgun/@crm/v3/mg.example.com/messages",
        &[
            ("authorization", &auth),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        form,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let crm = messages(&fm, Some("crm")).await;
    assert!(crm[0].provider_extras.as_ref().unwrap().test_mode);

    let mime =
        b"From: a@example.com\r\nTo: d@billing.example.test\r\nSubject: MIME\r\n\r\nraw body\r\n";
    let (ct, body) = multipart(&[
        ("to", None, b"d@billing.example.test"),
        ("message", Some("message.mime"), mime),
    ]);
    let r = send(
        &fm.data,
        "POST",
        "/__mail/mailgun/v3/mg.example.com/messages.mime",
        &[("authorization", &auth), ("content-type", &ct)],
        body,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(
        messages(&fm, Some("billing"))
            .await
            .last()
            .unwrap()
            .summary
            .subject
            .as_deref(),
        Some("MIME")
    );

    let r = send(
        &fm.data,
        "POST",
        "/__mail/mailgun/v3/mg.example.com/messages",
        &[("content-type", "application/x-www-form-urlencoded")],
        form,
    )
    .await;
    assert_eq!(
        (r.status, r.text().as_str()),
        (StatusCode::UNAUTHORIZED, "Forbidden")
    );
    let r = send(
        &fm.data,
        "POST",
        "/__mail/mailgun/v3/mg.example.com/messages",
        &[
            ("authorization", &auth),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        "to=a%40example.test&text=x",
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json(), json!({"message": "'from' parameter is missing"}));
    let r = send(
        &fm.data,
        "POST",
        "/__mail/mailgun/v3/mg.example.com/messages",
        &[
            ("authorization", &auth),
            ("content-type", "application/json"),
        ],
        "{}",
    )
    .await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn postmark_single_batch_and_templates() {
    let fm = server(StorageKind::Memory, 1 << 20).await;
    let h = [
        ("x-postmark-server-token", "server-token-1"),
        ("accept", "application/json"),
        ("content-type", "application/json"),
    ];
    let one = json!({
        "From": "sender@example.com", "To": "receiver@billing.example.test", "Subject": "Postmark test",
        "TextBody": "Hello dear Postmark user.", "HtmlBody": "<html><body><strong>Hello</strong> dear Postmark user.</body></html>",
        "MessageStream": "outbound", "Tag": "welcome", "Metadata": {"color": "blue"}
    });
    let r = send(
        &fm.data,
        "POST",
        "/__mail/postmark/email",
        &h,
        one.to_string(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!(v["To"], "receiver@billing.example.test");
    assert_eq!(
        (v["ErrorCode"].clone(), v["Message"].clone()),
        (json!(0), json!("OK"))
    );
    let stored = messages(&fm, Some("billing")).await;
    assert_eq!(
        stored[0]
            .provider_extras
            .as_ref()
            .unwrap()
            .provider_message_id,
        v["MessageID"].as_str().map(String::from)
    );

    let batch = json!([one, {"From": "sender@example.com", "To": "x@billing.example.test"}]);
    let r = send(
        &fm.data,
        "POST",
        "/__mail/postmark/email/batch",
        &h,
        batch.to_string(),
    )
    .await;
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v[0]["ErrorCode"], 0);
    assert_eq!(v[1]["ErrorCode"], 300);
    assert_eq!(messages(&fm, Some("billing")).await.len(), 2);

    let tpl = json!({"From": "sender@example.com", "To": "t@billing.example.test", "TemplateAlias": "welcome-v1", "TemplateModel": {"name": "Ann"}});
    let test_h = [
        ("x-postmark-server-token", "POSTMARK_API_TEST"),
        ("accept", "application/json"),
        ("content-type", "application/json"),
    ];
    let r = send(
        &fm.data,
        "POST",
        "/__mail/postmark/email/withTemplate",
        &test_h,
        tpl.to_string(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["Message"], "Test job accepted");
    let r = send(
        &fm.data,
        "POST",
        "/__mail/postmark/email/batchWithTemplates",
        &h,
        json!({"Messages": [tpl]}).to_string(),
    )
    .await;
    assert_eq!(r.status, 200);
    let stored = messages(&fm, Some("billing")).await;
    let last = stored.last().unwrap().provider_extras.clone().unwrap();
    assert_eq!(last.template_id.as_deref(), Some("welcome-v1"));
    assert_eq!(last.template_data, Some(json!({"name": "Ann"})));
    assert!(
        stored[stored.len() - 2]
            .provider_extras
            .as_ref()
            .unwrap()
            .test_mode
    );

    let r = send(
        &fm.data,
        "POST",
        "/__mail/postmark/email",
        &[("content-type", "application/json")],
        one.to_string(),
    )
    .await;
    assert_eq!(r.status, 401);
    assert_eq!(r.json()["ErrorCode"], 10);
    assert_eq!(r.header("x-pm-apierrorcode"), Some("10"));
    let r = send(&fm.data, "POST", "/__mail/postmark/email", &h, "{not json").await;
    assert_eq!(
        (r.status.as_u16(), r.json()["ErrorCode"].clone()),
        (422, json!(402))
    );
}

#[tokio::test]
async fn resend_idempotency_and_brevo() {
    for storage in storages().await {
        let fm = server(storage, 1 << 20).await;
        let h = [
            ("authorization", "Bearer re_123456789"),
            ("content-type", "application/json"),
            ("user-agent", "resend-node:6.1.0"),
            ("idempotency-key", "welcome-user/123"),
        ];
        let body = json!({"from": "Acme <onboarding@example.com>", "to": ["delivered@crm.example.test"], "subject": "hello world", "html": "<p>it works!</p>", "tags": [{"name": "category", "value": "confirm_email"}]});
        let r = send(
            &fm.data,
            "POST",
            "/__mail/resend/emails",
            &h,
            body.to_string(),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        let id = r.json()["id"].as_str().unwrap().to_string();
        let again = send(
            &fm.data,
            "POST",
            "/__mail/resend/emails",
            &h,
            body.to_string(),
        )
        .await;
        assert_eq!(again.json()["id"], id.as_str());
        assert_eq!(messages(&fm, Some("crm")).await.len(), 1);
        let mut other = body.clone();
        other["subject"] = json!("changed");
        let r = send(
            &fm.data,
            "POST",
            "/__mail/resend/emails",
            &h,
            other.to_string(),
        )
        .await;
        assert_eq!(r.status, 409);
        assert_eq!(r.json()["name"], "invalid_idempotent_request");
        let r = send(
            &fm.data,
            "POST",
            "/__mail/resend/emails",
            &[("content-type", "application/json"), ("user-agent", "x")],
            body.to_string(),
        )
        .await;
        assert_eq!(r.status, 401);
        assert_eq!(r.json()["name"], "missing_api_key");

        let b = json!({"sender": {"name": "Shop", "email": "shop@example.com"}, "to": [{"email": "b@crm.example.test", "name": "B"}], "subject": "Brevo", "htmlContent": "<p>Hi</p>", "tags": ["t1"], "params": {"x": 1}});
        let r = send(
            &fm.data,
            "POST",
            "/__mail/brevo/v3/smtp/email",
            &[
                ("api-key", "xkeysib-abc"),
                ("content-type", "application/json"),
                ("accept", "application/json"),
            ],
            b.to_string(),
        )
        .await;
        assert_eq!(r.status, 201, "{}", r.text());
        let mid = r.json()["messageId"].as_str().unwrap().to_string();
        let crm = messages(&fm, Some("crm")).await;
        assert_eq!(
            crm.last()
                .unwrap()
                .provider_extras
                .as_ref()
                .unwrap()
                .provider_message_id
                .as_deref(),
            Some(mid.as_str())
        );
        let r = send(
            &fm.data,
            "POST",
            "/__mail/brevo/v3/smtp/email",
            &[("content-type", "application/json")],
            b.to_string(),
        )
        .await;
        assert_eq!(r.status, 401);
        assert_eq!(
            r.json(),
            json!({"code": "unauthorized", "message": "Key not found"})
        );
    }
}

#[tokio::test]
async fn spaces_keys_journal_and_reserved_prefix() {
    let fm = server(StorageKind::Memory, 4096).await;
    let msg = |to: &str| {
        json!({"from": "a@example.com", "to": to, "subject": "s", "text": "t"}).to_string()
    };
    let h = [
        ("authorization", "Bearer re_key_one"),
        ("content-type", "application/json"),
        ("user-agent", "t"),
    ];
    let r = send(
        &fm.data,
        "POST",
        "/__mail/resend/@nowhere/emails",
        &h,
        msg("a@example.test"),
    )
    .await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["name"], "not_found");
    let r = send(
        &fm.data,
        "POST",
        "/__mail/nope/emails",
        &h,
        msg("a@example.test"),
    )
    .await;
    assert_eq!(r.status, 404);
    assert!(
        r.json()["error"]
            .as_str()
            .unwrap()
            .contains("unknown mail provider")
    );
    let r = send(
        &fm.data,
        "POST",
        "/__mail/resend/emails/batch",
        &h,
        msg("a@example.test"),
    )
    .await;
    assert_eq!(r.status, 404);

    let r = send(
        &fm.data,
        "POST",
        "/__mail/resend/emails",
        &h,
        msg("a@other.example"),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(messages(&fm, None).await.len(), 1);

    let digest = fastmock_server::mail_webhooks::key_digest("re_key_two");
    let r = admin(
        &fm,
        "PUT",
        "/mail/webhooks?space=billing",
        json!({"apiKeys": {"resend": [digest]}}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = send(
        &fm.data,
        "POST",
        "/__mail/resend/emails",
        &h,
        msg("x@billing.example.test"),
    )
    .await;
    assert_eq!(r.status, 403);
    assert_eq!(r.json()["name"], "invalid_api_key");
    let r = send(
        &fm.data,
        "POST",
        "/__mail/resend/emails",
        &h,
        msg("x@crm.example.test"),
    )
    .await;
    assert_eq!(r.status, 200);
    let good = [
        ("authorization", "Bearer re_key_two"),
        ("content-type", "application/json"),
        ("user-agent", "t"),
    ];
    let r = send(
        &fm.data,
        "POST",
        "/__mail/resend/@billing/emails",
        &good,
        msg("x@elsewhere.example"),
    )
    .await;
    assert_eq!(r.status, 200);
    let ak = fastmock_server::mail_webhooks::key_digest("AKIDEXAMPLE");
    admin(
        &fm,
        "PUT",
        "/mail/webhooks?space=crm",
        json!({"apiKeys": {"ses": [ak]}}),
    )
    .await;
    let other_key = SIGV4.replace("AKIDEXAMPLE", "AKIDOTHER");
    let ses = json!({"FromEmailAddress": "a@example.com", "Destination": {"ToAddresses": ["c@crm.example.test"]}, "Content": {"Simple": {"Subject": {"Data": "s"}}}}).to_string();
    let r = send(
        &fm.data,
        "POST",
        "/__mail/ses/v2/email/outbound-emails",
        &[("authorization", &other_key)],
        ses.clone(),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.header("x-amzn-errortype")),
        (403, Some("UnrecognizedClientException"))
    );
    let r = send(
        &fm.data,
        "POST",
        "/__mail/ses/v2/email/outbound-emails",
        &[("authorization", SIGV4)],
        ses,
    )
    .await;
    assert_eq!(r.status, 200);

    let big = json!({"from": "a@example.com", "to": "a@example.test", "subject": "s", "text": "x".repeat(5000)}).to_string();
    let r = send(&fm.data, "POST", "/__mail/resend/emails", &h, big).await;
    assert_eq!(r.status, 422);

    let stub = json!({"request": {"method": "POST", "urlPath": "/__mail/resend/emails"}, "response": {"status": 299}});
    let r = admin(&fm, "POST", "/stubs", stub).await;
    assert!(r.status.is_success(), "{}", r.text());
    let r = send(
        &fm.data,
        "POST",
        "/__mail/resend/emails",
        &h,
        msg("z@example.test"),
    )
    .await;
    assert_eq!(r.status, 200);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let r = send(
        &fm.admin,
        "GET",
        "/__admin/requests?limit=100",
        &[("authorization", &format!("Bearer {ADMIN}"))],
        Body::empty(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    let list = v["requests"]
        .as_array()
        .or_else(|| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mail: Vec<&Value> = list
        .iter()
        .filter(|e| e["path"].as_str().unwrap_or("").starts_with("/__mail/"))
        .collect();
    assert!(!mail.is_empty(), "{v}");
    assert!(
        mail.iter()
            .all(|e| e["source"] == "mail" && e["matched"] == true),
        "{v}"
    );
    assert!(mail.iter().any(|e| e["space"] == "billing"), "{v}");
}

#[tokio::test]
async fn chaos_rules_answer_provider_errors() {
    let fm = server(StorageKind::Memory, 1 << 20).await;
    let rules = json!({"rules": [
        {"id": "sg-rate", "stage": "provider", "match": {"provider": {"equalTo": "sendgrid"}}, "action": {"providerError": "rateLimit"}, "times": 1},
        {"id": "ses-reject", "stage": "provider", "match": {"provider": {"equalTo": "ses"}, "rcptTo": {"contains": "reject"}}, "action": {"providerError": "MessageRejected"}},
        {"id": "mg-420", "stage": "provider", "match": {"provider": {"equalTo": "mailgun"}}, "action": {"providerError": "recipientsRateLimit"}},
        {"id": "pm-slow", "stage": "provider", "match": {"provider": {"equalTo": "postmark"}}, "action": {"delay": "150ms"}},
        {"id": "brevo-credits", "stage": "provider", "match": {"provider": {"equalTo": "brevo"}}, "action": {"providerError": "credits"}}
    ]});
    let r = admin(&fm, "PUT", "/mail/chaos", rules).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let sg = json!({"personalizations": [{"to": [{"email": "a@example.test"}]}], "from": {"email": "s@example.com"}, "subject": "s", "content": [{"type": "text/plain", "value": "x"}]}).to_string();
    let sgh = [
        ("authorization", "Bearer SG.k"),
        ("content-type", "application/json"),
    ];
    let r = send(
        &fm.data,
        "POST",
        "/__mail/sendgrid/v3/mail/send",
        &sgh,
        sg.clone(),
    )
    .await;
    assert_eq!(r.status, 429);
    assert_eq!(r.json()["errors"][0]["message"], "too many requests");
    assert_eq!(r.header("x-ratelimit-remaining"), Some("0"));
    assert_eq!(r.header("x-fastmock-chaos"), Some("sg-rate"));
    let r = send(&fm.data, "POST", "/__mail/sendgrid/v3/mail/send", &sgh, sg).await;
    assert_eq!(r.status, 202);

    let ses = |to: &str| {
        json!({"FromEmailAddress": "a@example.com", "Destination": {"ToAddresses": [to]}, "Content": {"Simple": {"Subject": {"Data": "s"}}}}).to_string()
    };
    let r = send(
        &fm.data,
        "POST",
        "/__mail/ses/v2/email/outbound-emails",
        &[("authorization", SIGV4)],
        ses("reject@example.test"),
    )
    .await;
    assert_eq!(
        (r.status.as_u16(), r.header("x-amzn-errortype")),
        (400, Some("MessageRejected"))
    );
    let r = send(
        &fm.data,
        "POST",
        "/__mail/ses/v2/email/outbound-emails",
        &[("authorization", SIGV4)],
        ses("ok@example.test"),
    )
    .await;
    assert_eq!(r.status, 200);
    let form = "Action=SendEmail&Source=a%40example.com&Destination.ToAddresses.member.1=reject%40example.test&Message.Subject.Data=s";
    let r = send(
        &fm.data,
        "POST",
        "/__mail/ses/",
        &[
            ("authorization", SIGV4),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        form,
    )
    .await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("<Code>MessageRejected</Code>"));

    let auth = format!("Basic {}", B64.encode("api:k"));
    let r = send(
        &fm.data,
        "POST",
        "/__mail/mailgun/v3/mg.example.com/messages",
        &[
            ("authorization", &auth),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        "from=a%40example.com&to=b%40example.test&text=x",
    )
    .await;
    assert_eq!(r.status.as_u16(), 420);

    let start = std::time::Instant::now();
    let pm = json!({"From": "a@example.com", "To": "b@example.test", "TextBody": "x"}).to_string();
    let r = send(
        &fm.data,
        "POST",
        "/__mail/postmark/email",
        &[
            ("x-postmark-server-token", "t"),
            ("content-type", "application/json"),
        ],
        pm,
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(start.elapsed() >= std::time::Duration::from_millis(150));
    assert_eq!(r.header("x-fastmock-chaos"), Some("pm-slow"));

    let b = json!({"sender": {"email": "a@example.com"}, "to": [{"email": "b@example.test"}], "subject": "s", "textContent": "x"}).to_string();
    let r = send(
        &fm.data,
        "POST",
        "/__mail/brevo/v3/smtp/email",
        &[("api-key", "k"), ("content-type", "application/json")],
        b,
    )
    .await;
    assert_eq!(r.status.as_u16(), 402);
    assert_eq!(r.json()["code"], "not_enough_credits");
}

// ----- body limits, expected keys, idempotency and webhook fan-out -----

#[tokio::test]
async fn mailgun_multipart_attachments_above_two_mib() {
    let fm = server_with(StorageKind::Memory, 10 << 20, Some(16 << 20)).await;
    let auth = format!("Basic {}", B64.encode("api:key-big"));
    let big = vec![b'a'; 3 << 20];
    let (ct, body) = multipart(&[
        ("from", None, b"shop@mg.example.com"),
        ("to", None, b"ann@billing.example.test"),
        ("subject", None, b"Big"),
        ("text", None, b"see attachment"),
        ("attachment", Some("big.bin"), &big),
    ]);
    let r = send(
        &fm.data,
        "POST",
        "/__mail/mailgun/v3/mg.example.com/messages",
        &[("authorization", &auth), ("content-type", &ct)],
        body,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let stored = messages(&fm, Some("billing")).await;
    assert_eq!(stored[0].summary.attachment_names, vec!["big.bin"]);
}

#[tokio::test]
async fn the_postmark_test_token_follows_expected_keys() {
    let fm = server(StorageKind::Memory, 1 << 20).await;
    let digest = fastmock_server::mail_webhooks::key_digest("server-token-real");
    let r = admin(
        &fm,
        "PUT",
        "/mail/webhooks?space=billing",
        json!({"apiKeys": {"postmark": [digest]}}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let one = json!({"From": "sender@example.com", "To": "r@billing.example.test", "Subject": "s", "TextBody": "t"});
    let with = |token: &'static str| {
        [
            ("x-postmark-server-token", token),
            ("accept", "application/json"),
            ("content-type", "application/json"),
        ]
    };
    let r = send(
        &fm.data,
        "POST",
        "/__mail/postmark/email",
        &with("POSTMARK_API_TEST"),
        one.to_string(),
    )
    .await;
    assert_eq!(r.status, 401, "{}", r.text());
    assert!(messages(&fm, Some("billing")).await.is_empty());
    let r = send(
        &fm.data,
        "POST",
        "/__mail/postmark/email",
        &with("server-token-real"),
        one.to_string(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let crm = json!({"From": "sender@example.com", "To": "r@crm.example.test", "Subject": "s", "TextBody": "t"});
    let r = send(
        &fm.data,
        "POST",
        "/__mail/postmark/email",
        &with("POSTMARK_API_TEST"),
        crm.to_string(),
    )
    .await;
    assert_eq!(r.status, 200, "a space without expected keys: {}", r.text());
}

#[tokio::test]
async fn resend_idempotency_keys_belong_to_the_api_key() {
    for storage in storages().await {
        let fm = server(storage, 1 << 20).await;
        let headers = |auth: &'static str| {
            [
                ("authorization", auth),
                ("content-type", "application/json"),
                ("user-agent", "t"),
                ("idempotency-key", "shared-key-1"),
            ]
        };
        let a = json!({"from": "a@example.com", "to": ["x@example.test"], "subject": "from a", "html": "<p>a</p>"});
        let b = json!({"from": "b@example.com", "to": ["y@example.test"], "subject": "from b", "html": "<p>b</p>"});
        let r = send(
            &fm.data,
            "POST",
            "/__mail/resend/emails",
            &headers("Bearer re_tenant_a"),
            a.to_string(),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        let first = r.json()["id"].clone();
        let r = send(
            &fm.data,
            "POST",
            "/__mail/resend/emails",
            &headers("Bearer re_tenant_b"),
            b.to_string(),
        )
        .await;
        assert_eq!(r.status, 200, "another key, another scope: {}", r.text());
        assert_ne!(r.json()["id"], first);
        assert_eq!(messages(&fm, None).await.len(), 2);
    }
}

#[tokio::test]
async fn webhook_requests_per_call_are_capped() {
    let fm = server(StorageKind::Memory, 1 << 20).await;
    let r = admin(
        &fm,
        "PUT",
        "/mail/webhooks?space=billing",
        json!({"webhooks": [{"provider": "postmark", "kind": "events", "url": "https://hooks.example.com/pm"}]}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let batch: Vec<Value> = (0..500)
        .map(|i| {
            json!({"From": "s@example.com", "To": format!("a{i}@billing.example.test, b{i}@billing.example.test, c{i}@billing.example.test"), "Subject": "s", "TextBody": "t"})
        })
        .collect();
    let r = send(
        &fm.data,
        "POST",
        "/__mail/postmark/email/batch",
        &[
            ("x-postmark-server-token", "tok"),
            ("content-type", "application/json"),
        ],
        Value::Array(batch).to_string(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(messages(&fm, Some("billing")).await.len(), 200);
    let pending = fm.app.stores.outbox.pending_count().await.unwrap();
    assert_eq!(pending, 1000, "1500 events, 1000 queued");
}
