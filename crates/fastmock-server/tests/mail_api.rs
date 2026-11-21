use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use fastmock_mail::MailConfig;
use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use fastmock_server::mail_api::{active_streams, active_waits};
use fastmock_server::state::AppState;
use reqwest::Method;
use serde_json::{Value, json};
use sqlx::Connection;

const ADMIN: &str = "fm_admin_token_for_mail_api_tests_0123456789";
const EDITOR: &str = "fm_editor_token_for_mail_api_tests_0123456789";
const VIEWER: &str = "fm_viewer_token_for_mail_api_tests_0123456789";
const BVIEW: &str = "fm_bview_token_for_mail_api_tests_0123456789";
const BEDIT: &str = "fm_bedit_token_for_mail_api_tests_0123456789";
const ALLVIEW: &str = "fm_allview_token_for_mail_api_tests_0123456789";
const READER: &str = "fm_reader_token_for_mail_api_tests_0123456789";
const WRITER: &str = "fm_writer_token_for_mail_api_tests_0123456789";
const SENDER: &str = "fm_sender_token_for_mail_api_tests_0123456789";

const ACCESS: &str = r#"
roles:
  - name: mail-reader
    permissions: [mail:read]
  - name: mail-writer
    permissions: [mail:read, mail:write]
  - name: mail-sender
    permissions: [mail:write]
spaces:
  - name: billing
    mailDomains: [billing.example.test]
  - name: crm
    mailDomains: [crm.example.test]
bindings:
  - subject: token:bview
    role: viewer
    spaces: [billing]
  - subject: token:bedit
    role: editor
    spaces: [billing]
  - subject: token:allview
    role: viewer
    spaces: ["*"]
  - subject: token:reader
    role: mail-reader
    global: true
  - subject: token:writer
    role: mail-writer
    spaces: [billing]
  - subject: token:sender
    role: mail-sender
    spaces: [billing]
"#;

const CSP: &str = "sandbox; default-src 'none'; img-src data:; style-src 'unsafe-inline'";
const CSP_IMAGES: &str =
    "sandbox; default-src 'none'; img-src data: https:; style-src 'unsafe-inline'";

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

async fn storages() -> Vec<(&'static str, StorageKind)> {
    let mut v = vec![("memory", StorageKind::Memory)];
    let (Ok(admin_url), Ok(valkey_url)) = (
        std::env::var("FASTMOCK_TEST_DATABASE_URL"),
        std::env::var("FASTMOCK_TEST_VALKEY_URL"),
    ) else {
        eprintln!(
            "FASTMOCK_TEST_DATABASE_URL/FASTMOCK_TEST_VALKEY_URL not set: PostgreSQL+Valkey part skipped"
        );
        return v;
    };
    let db = format!("fm_mailapi_{}", uuid::Uuid::new_v4().simple());
    let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut c)
        .await
        .unwrap();
    v.push((
        "postgres+valkey",
        StorageKind::Postgres {
            database_url: format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db),
            valkey_url,
            valkey_prefix: format!("mailapi:{}:", uuid::Uuid::new_v4().simple()),
            max_connections: 5,
            options: Default::default(),
        },
    ));
    v
}

fn mail_config() -> MailConfig {
    let mut m = MailConfig::default();
    m.listeners.smtp = None;
    m
}

struct H {
    base: String,
    http: reqwest::Client,
    app: AppState,
}

impl H {
    async fn start(storage: StorageKind, access: bool, mail: Option<MailConfig>) -> H {
        let mut tokens = vec![
            format!("ops:admin:{}", hash_token(ADMIN)),
            format!("ci:editor:{}", hash_token(EDITOR)),
            format!("dash:viewer:{}", hash_token(VIEWER)),
        ];
        let mut cfg_access = None;
        if access {
            for (name, value) in [
                ("bview", BVIEW),
                ("bedit", BEDIT),
                ("allview", ALLVIEW),
                ("reader", READER),
                ("writer", WRITER),
                ("sender", SENDER),
            ] {
                tokens.push(format!("{name}::{}", hash_token(value)));
            }
            let dir = std::env::temp_dir().join(format!("fm-mailapi-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let f = dir.join("access.yaml");
            std::fs::write(&f, ACCESS).unwrap();
            cfg_access = Some(f);
        }
        let mut cfg = Config::for_tests(storage, tokens);
        cfg.plugins = Some(vec![]);
        cfg.access_file = cfg_access;
        cfg.mail = mail;
        let (app, _data, admin, _policy) = fastmock_server::build(cfg, prom()).await.unwrap();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, admin).await.unwrap() });
        app.ready.store(true, Ordering::SeqCst);
        H {
            base: format!("http://{addr}/__admin"),
            http: reqwest::Client::new(),
            app,
        }
    }

    fn req(&self, m: Method, path: &str, token: &str) -> reqwest::RequestBuilder {
        self.http
            .request(m, format!("{}{path}", self.base))
            .bearer_auth(token)
    }

    async fn call(&self, m: Method, path: &str, token: &str, body: Option<Value>) -> (u16, Value) {
        let mut rb = self.req(m, path, token);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let r = rb.send().await.unwrap();
        let st = r.status().as_u16();
        let text = r.text().await.unwrap();
        (
            st,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn ok(&self, m: Method, path: &str, token: &str, body: Option<Value>) -> Value {
        let (st, v) = self.call(m.clone(), path, token, body).await;
        assert!((200..300).contains(&st), "{m} {path}: {st} {v}");
        v
    }

    async fn inject(&self, token: &str, query: &str, body: Value) -> Value {
        let (st, v) = self
            .call(
                Method::POST,
                &format!("/mail/messages{query}"),
                token,
                Some(body),
            )
            .await;
        assert_eq!(st, 201, "{v}");
        v["messages"][0].clone()
    }

    async fn ids(&self, path: &str, token: &str) -> Vec<String> {
        let v = self.ok(Method::GET, path, token, None).await;
        v["messages"]
            .as_array()
            .unwrap_or_else(|| panic!("{path}: {v}"))
            .iter()
            .map(|m| m["id"].as_str().unwrap().to_string())
            .collect()
    }

    async fn audit(&self, action: &str) -> Vec<Value> {
        let v = self
            .ok(Method::GET, &format!("/audit?action={action}"), ADMIN, None)
            .await;
        v["events"].as_array().unwrap().clone()
    }

    fn svc(&self) -> std::sync::Arc<fastmock_mail::MailService> {
        self.app.mail.clone().unwrap()
    }
}

fn simple(to: &str, subject: &str) -> Value {
    json!({"from": "Shop <shop@example.com>", "to": [to], "subject": subject, "text": "Hello"})
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_string()
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

async fn until(what: &str, f: impl Fn() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn message_detail_and_renditions() {
    for (mode, storage) in storages().await {
        let h = H::start(storage, true, Some(mail_config())).await;
        let png = [137u8, 80, 78, 71, 13, 10, 26, 10];
        let msg = h
            .inject(
                ADMIN,
                "?space=billing",
                json!({
                    "from": "Billing Team <billing@example.com>",
                    "to": ["alice+welcome@billing.example.test"],
                    "cc": ["carol@billing.example.test"],
                    "subject": "Your code",
                    "text": "Your code is 123456. Order ABC-77. Visit https://example.com/verify?t=1",
                    "html": "<p>Your code is <b>123456</b>. <a href=\"https://example.com/verify?t=1&amp;x=2\">Verify now</a><img src=\"cid:logo\"><script>alert(1)</script></p>",
                    "headers": {"X-FastMock-Labels": "otp", "X-Mailer": "shop 1.0"},
                    "attachments": [
                        {"name": "../evil \"name\".html", "contentType": "text/html", "base64": B64.encode("<script>alert(2)</script>")},
                        {"name": "logo.png", "contentType": "image/png", "base64": B64.encode(png), "contentId": "logo"},
                        {"name": "Grüße.pdf", "contentType": "application/pdf", "base64": B64.encode("%PDF-1")}
                    ]
                }),
            )
            .await;
        let mid = id(&msg);
        assert_eq!(msg["space"], "billing", "{mode}");
        assert_eq!(msg["source"], "inject");
        assert_eq!(msg["labels"], json!(["otp", "welcome"]));
        assert_eq!(
            msg["envelope"]["rcptTo"],
            json!([
                "alice+welcome@billing.example.test",
                "carol@billing.example.test"
            ])
        );

        let d = h
            .ok(Method::GET, &format!("/mail/messages/{mid}"), BVIEW, None)
            .await;
        assert_eq!(d["id"], mid.as_str());
        assert_eq!(d["summary"]["subject"], "Your code");
        assert_eq!(d["subject"], "Your code");
        assert_eq!(d["envelope"]["mailFrom"], "billing@example.com");
        assert_eq!(d["from"][0]["name"], "Billing Team");
        assert_eq!(d["cc"][0]["address"], "carol@billing.example.test");
        assert_eq!(d["hasHtml"], true);
        assert!(d.get("html").is_none(), "HTML is never inline");
        assert!(d["text"].as_str().unwrap().contains("123456"));
        let names: Vec<&str> = d["headers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["name"].as_str().unwrap())
            .collect();
        let at = |n: &str| {
            names
                .iter()
                .position(|x| x.eq_ignore_ascii_case(n))
                .unwrap()
        };
        assert!(at("From") < at("Subject"), "{names:?}");
        assert!(names.contains(&"X-Mailer"));
        assert_eq!(
            d["urls"]["html"],
            format!("/__admin/mail/messages/{mid}/html")
        );
        assert_eq!(
            d["urls"]["raw"],
            format!("/__admin/mail/messages/{mid}/raw")
        );
        let atts = d["attachments"].as_array().unwrap();
        let part_of = |n: &str| {
            atts.iter()
                .find(|a| a["name"] == n)
                .unwrap_or_else(|| panic!("{n} in {atts:?}"))["partId"]
                .as_str()
                .unwrap()
                .to_string()
        };

        let r = h
            .req(Method::GET, &format!("/mail/messages/{mid}/raw"), BVIEW)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["content-type"], "message/rfc822");
        assert_eq!(
            r.headers()["content-disposition"],
            format!("attachment; filename=\"{mid}.eml\"").as_str()
        );
        assert_eq!(r.headers()["x-content-type-options"], "nosniff");
        let raw = r.text().await.unwrap();
        assert!(raw.contains("Subject: Your code"));

        let r = h
            .req(Method::GET, &format!("/mail/messages/{mid}/html"), BVIEW)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["content-security-policy"], CSP);
        assert_eq!(r.headers()["x-content-type-options"], "nosniff");
        assert_eq!(r.headers()["content-type"], "text/html; charset=utf-8");
        let html = r.text().await.unwrap();
        assert!(
            html.contains(&format!(
                "src=\"data:image/png;base64,{}\"",
                B64.encode(png)
            )),
            "{html}"
        );
        assert!(!html.contains("cid:logo"));
        let r = h
            .req(
                Method::GET,
                &format!("/mail/messages/{mid}/html?images=allow"),
                BVIEW,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(r.headers()["content-security-policy"], CSP_IMAGES);
        let (st, v) = h
            .call(
                Method::GET,
                &format!("/mail/messages/{mid}/html?images=sometimes"),
                BVIEW,
                None,
            )
            .await;
        assert_eq!(st, 400, "{v}");

        let r = h
            .req(Method::GET, &format!("/mail/messages/{mid}/text"), BVIEW)
            .send()
            .await
            .unwrap();
        assert_eq!(r.headers()["content-type"], "text/plain; charset=utf-8");
        assert!(r.text().await.unwrap().starts_with("Your code is 123456."));

        let evil = part_of("../evil \"name\".html");
        let r = h
            .req(
                Method::GET,
                &format!("/mail/messages/{mid}/parts/{evil}"),
                BVIEW,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let cd = r.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            cd.starts_with(
                "attachment; filename=\"evil _name_.html\"; filename*=UTF-8''evil%20%22name%22.html"
            ),
            "{cd}"
        );
        assert_eq!(r.headers()["x-content-type-options"], "nosniff");
        assert_eq!(
            r.headers()["content-security-policy"],
            "sandbox; default-src 'none'"
        );
        assert_eq!(r.text().await.unwrap(), "<script>alert(2)</script>");
        let pdf = part_of("Grüße.pdf");
        let r = h
            .req(
                Method::GET,
                &format!("/mail/messages/{mid}/parts/{pdf}"),
                BVIEW,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(r.headers()["content-type"], "application/pdf");
        assert!(
            r.headers()["content-disposition"]
                .to_str()
                .unwrap()
                .contains("filename*=UTF-8''Gr%C3%BC%C3%9Fe.pdf")
        );
        assert_eq!(r.bytes().await.unwrap().as_ref(), b"%PDF-1");
        for bad_part in ["999", "x", "0"] {
            let (st, _) = h
                .call(
                    Method::GET,
                    &format!("/mail/messages/{mid}/parts/{bad_part}"),
                    BVIEW,
                    None,
                )
                .await;
            assert_eq!(st, 404, "part {bad_part}");
        }

        let t = h
            .ok(
                Method::GET,
                &format!("/mail/messages/{mid}/transcript"),
                BVIEW,
                None,
            )
            .await;
        assert_eq!(t["transcript"], json!([]));

        let l = h
            .ok(
                Method::GET,
                &format!("/mail/messages/{mid}/links"),
                BVIEW,
                None,
            )
            .await;
        assert_eq!(
            l["links"],
            json!([
                {"url": "https://example.com/verify?t=1&x=2", "text": "Verify now"},
                {"url": "https://example.com/verify?t=1"}
            ])
        );

        let c = h
            .ok(
                Method::GET,
                &format!("/mail/messages/{mid}/codes"),
                BVIEW,
                None,
            )
            .await;
        assert_eq!(c["codes"], json!(["123456"]), "{c}");
        assert_eq!(c["pattern"], "\\b\\d{4,8}\\b");
        let c = h
            .ok(
                Method::GET,
                &format!(
                    "/mail/messages/{mid}/codes?pattern={}",
                    urlencode("Order ([A-Z]+-\\d+)")
                ),
                BVIEW,
                None,
            )
            .await;
        assert_eq!(c["codes"], json!(["ABC-77"]));
        for bad in ["\\w{1000}{1000}", "(", &"a".repeat(300)] {
            let (st, v) = h
                .call(
                    Method::GET,
                    &format!("/mail/messages/{mid}/codes?pattern={}", urlencode(bad)),
                    BVIEW,
                    None,
                )
                .await;
            assert_eq!(st, 400, "{bad}: {v}");
            assert!(v["error"].as_str().unwrap().contains("pattern"), "{v}");
        }

        let unknown = uuid::Uuid::now_v7();
        for path in [
            format!("/mail/messages/{unknown}"),
            format!("/mail/messages/{unknown}/raw"),
            format!("/mail/messages/{unknown}/html"),
        ] {
            let (st, v) = h.call(Method::GET, &path, ADMIN, None).await;
            assert_eq!(st, 404, "{path}: {v}");
            assert_eq!(v["error"], "message not found");
        }
        let (st, _) = h
            .call(Method::GET, "/mail/messages/not-a-uuid", ADMIN, None)
            .await;
        assert_eq!(st, 400);

        let plain = h
            .inject(
                ADMIN,
                "?space=billing",
                simple("x@billing.example.test", "plain"),
            )
            .await;
        let (st, v) = h
            .call(
                Method::GET,
                &format!("/mail/messages/{}/html", id(&plain)),
                ADMIN,
                None,
            )
            .await;
        assert_eq!(
            (st, v["error"].as_str()),
            (404, Some("HTML body not found"))
        );
        let d = h
            .ok(
                Method::GET,
                &format!("/mail/messages/{}", id(&plain)),
                ADMIN,
                None,
            )
            .await;
        assert!(d["urls"].get("html").is_none());

        let big = vec![7u8; 3 * 1024 * 1024 + 300 * 1024];
        let large = h
            .inject(
                ADMIN,
                "?space=billing",
                json!({
                    "from": "shop@example.com", "to": ["x@billing.example.test"],
                    "html": "<img src=\"cid:big\"><img src=\"cid:small\">",
                    "attachments": [
                        {"name": "big.bin", "contentType": "image/png", "base64": B64.encode(&big), "contentId": "big"},
                        {"name": "small.png", "contentType": "image/png", "base64": B64.encode(png), "contentId": "small"}
                    ]
                }),
            )
            .await;
        let html = h
            .req(
                Method::GET,
                &format!("/mail/messages/{}/html", id(&large)),
                ADMIN,
            )
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(html.contains("cid:big"), "parts beyond the cap keep cid:");
        assert!(!html.contains("cid:small"));
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[tokio::test]
async fn cursor_paging_and_query_operators() {
    for (mode, storage) in storages().await {
        let h = H::start(storage, true, Some(mail_config())).await;
        let m1 = id(&h
            .inject(
                ADMIN,
                "?space=billing",
                json!({
                    "from": "Billing Team <billing@example.com>", "to": ["alice@billing.example.test"],
                    "subject": "Monthly invoice", "text": "Pay 42",
                    "headers": {"X-FastMock-Labels": "finance"},
                    "attachments": [{"name": "invoice.pdf", "contentType": "application/pdf", "base64": B64.encode("%PDF")}]
                }),
            )
            .await);
        let m2 = id(&h
            .inject(
                ADMIN,
                "?space=billing",
                json!({
                    "from": "shop@example.com", "to": ["bob@billing.example.test"],
                    "cc": ["carol@billing.example.test"], "subject": "Welcome",
                    "text": "Hello there unique-word"
                }),
            )
            .await);
        let m3 = id(&h
            .inject(
                ADMIN,
                "?space=billing",
                json!({"from": "shop@example.com", "to": ["dave+signup@billing.example.test"],
                    "subject": "Your code", "text": "Code 123456"}),
            )
            .await);
        let m4 = id(&h
            .inject(
                ADMIN,
                "?space=billing",
                simple("eve@billing.example.test", "Filler 1"),
            )
            .await);
        let m5 = id(&h
            .inject(
                ADMIN,
                "?space=billing",
                simple("eve@billing.example.test", "Filler 2"),
            )
            .await);
        let all = vec![m1.clone(), m2.clone(), m3.clone(), m4.clone(), m5.clone()];

        let first = h
            .ok(
                Method::GET,
                "/mail/messages?space=billing&limit=2",
                BVIEW,
                None,
            )
            .await;
        let mut seen: Vec<String> = Vec::new();
        let mut page = first;
        let mut pages = 0;
        loop {
            pages += 1;
            for m in page["messages"].as_array().unwrap() {
                seen.push(id(m));
            }
            let Some(c) = page["nextCursor"].as_str() else {
                break;
            };
            page = h
                .ok(
                    Method::GET,
                    &format!("/mail/messages?space=billing&limit=2&cursor={c}"),
                    BVIEW,
                    None,
                )
                .await;
        }
        assert_eq!(pages, 3, "{mode}");
        let mut newest_first = all.clone();
        newest_first.reverse();
        assert_eq!(seen, newest_first, "newest first, no duplicates");
        assert_eq!(
            h.ids("/mail/messages?space=billing&order=oldest", BVIEW)
                .await,
            all
        );
        let oldest2 = h
            .ok(
                Method::GET,
                "/mail/messages?space=billing&order=oldest&limit=2",
                BVIEW,
                None,
            )
            .await;
        let c = oldest2["nextCursor"].as_str().unwrap();
        assert_eq!(
            h.ids(
                &format!("/mail/messages?space=billing&order=oldest&limit=2&cursor={c}"),
                BVIEW
            )
            .await,
            vec![m3.clone(), m4.clone()]
        );

        for (q, want) in [
            ("from:billing", vec![&m1]),
            ("from:shop subject:welcome", vec![&m2]),
            ("to:bob", vec![&m2]),
            ("to:dave", vec![&m3]),
            ("cc:carol", vec![&m2]),
            ("subject:\"monthly invoice\"", vec![&m1]),
            ("has:attachment", vec![&m1]),
            ("label:signup", vec![&m3]),
            ("label:finance", vec![&m1]),
            ("unique-word", vec![&m2]),
            ("123456", vec![&m3]),
            ("before:2000-01-01", vec![]),
            ("after:2999-01-01T00:00:00Z", vec![]),
        ] {
            let got = h
                .ids(
                    &format!("/mail/messages?space=billing&query={}", urlencode(q)),
                    BVIEW,
                )
                .await;
            let want: Vec<String> = want.into_iter().cloned().collect();
            assert_eq!(sorted(got), sorted(want), "{mode}: {q}");
        }
        for q in ["after:2000-01-01", "before:2999-01-01"] {
            let got = h
                .ids(
                    &format!("/mail/messages?space=billing&query={}", urlencode(q)),
                    BVIEW,
                )
                .await;
            assert_eq!(got.len(), 5, "{q}");
        }
        for (path, why) in [
            ("/mail/messages?query=has:pictures", "has:attachment"),
            ("/mail/messages?query=after:yesterday", "YYYY-MM-DD"),
            ("/mail/messages?limit=0", "limit"),
            ("/mail/messages?limit=501", "limit"),
            ("/mail/messages?cursor=nonsense", "cursor"),
            ("/mail/messages?order=sideways", "order"),
        ] {
            let (st, v) = h.call(Method::GET, path, ADMIN, None).await;
            assert_eq!(st, 400, "{path}: {v}");
            assert!(v["error"].as_str().unwrap().contains(why), "{path}: {v}");
        }
    }
}

#[tokio::test]
async fn permissions_per_role_and_space() {
    for (mode, storage) in storages().await {
        let h = H::start(storage, true, Some(mail_config())).await;
        let billing = id(&h
            .inject(
                ADMIN,
                "?space=billing",
                simple("a@billing.example.test", "b"),
            )
            .await);
        let crm = id(&h
            .inject(ADMIN, "?space=crm", simple("a@crm.example.test", "c"))
            .await);
        let global = id(&h.inject(ADMIN, "", simple("a@example.test", "g")).await);
        assert_eq!(
            h.app
                .mail
                .as_ref()
                .unwrap()
                .store
                .get(global.parse().unwrap(), &fastmock_core::SpaceFilter::Global)
                .await
                .unwrap()
                .unwrap()
                .space,
            None
        );
        let every = sorted(vec![billing.clone(), crm.clone(), global.clone()]);
        for (token, want) in [
            (ADMIN, every.clone()),
            (VIEWER, every.clone()),
            (READER, every.clone()),
            (BVIEW, vec![billing.clone()]),
            (WRITER, vec![billing.clone()]),
            (ALLVIEW, sorted(vec![billing.clone(), crm.clone()])),
        ] {
            assert_eq!(
                sorted(h.ids("/mail/messages", token).await),
                want,
                "{mode}: {token}"
            );
        }
        for (token, msg, status) in [
            (BVIEW, &billing, 200),
            (BVIEW, &crm, 404),
            (BVIEW, &global, 404),
            (ALLVIEW, &crm, 200),
            (ALLVIEW, &global, 404),
            (READER, &global, 200),
        ] {
            let (st, v) = h
                .call(Method::GET, &format!("/mail/messages/{msg}"), token, None)
                .await;
            assert_eq!(st, status, "{token} {msg}: {v}");
        }
        let (st, _) = h
            .call(
                Method::GET,
                &format!("/mail/messages/{billing}?space=crm"),
                ADMIN,
                None,
            )
            .await;
        assert_eq!(st, 404, "a message outside the requested space");
        let (st, v) = h
            .call(Method::GET, "/mail/messages?space=crm", BVIEW, None)
            .await;
        assert_eq!(st, 404, "{v}");
        let (st, v) = h
            .call(Method::GET, "/mail/messages?space=nope", ADMIN, None)
            .await;
        assert_eq!(
            (st, v["error"].as_str()),
            (404, Some("space nope not found"))
        );

        let (st, v) = h
            .call(
                Method::DELETE,
                &format!("/mail/messages/{billing}"),
                BVIEW,
                None,
            )
            .await;
        assert_eq!(st, 403, "{v}");
        assert_eq!(v["error"], "forbidden");
        assert_eq!(v["required"], "mail:delete");
        let (st, v) = h
            .call(
                Method::DELETE,
                &format!("/mail/messages/{crm}"),
                BEDIT,
                None,
            )
            .await;
        assert_eq!(st, 404, "unreadable messages answer 404: {v}");
        let (st, v) = h
            .call(
                Method::PUT,
                &format!("/mail/messages/{billing}/labels"),
                READER,
                Some(json!({"labels": ["x"]})),
            )
            .await;
        assert_eq!((st, v["required"].as_str()), (403, Some("mail:write")));
        let v = h
            .ok(
                Method::PUT,
                &format!("/mail/messages/{billing}/labels"),
                WRITER,
                Some(json!({"labels": ["Checked", "otp", "otp"]})),
            )
            .await;
        assert_eq!(v["labels"], json!(["checked", "otp"]));
        let (st, v) = h
            .call(
                Method::DELETE,
                &format!("/mail/messages/{billing}"),
                WRITER,
                None,
            )
            .await;
        assert_eq!((st, v["required"].as_str()), (403, Some("mail:delete")));

        let (st, v) = h
            .call(
                Method::POST,
                "/mail/messages",
                BVIEW,
                Some(simple("a@billing.example.test", "x")),
            )
            .await;
        assert_eq!(
            (st, v["required"].as_str()),
            (403, Some("mail:write")),
            "{v}"
        );
        let w = h
            .inject(WRITER, "", simple("w@billing.example.test", "from writer"))
            .await;
        assert_eq!(w["space"], "billing", "the only space of a bound caller");
        let (st, v) = h
            .call(
                Method::POST,
                "/mail/messages?space=crm",
                WRITER,
                Some(simple("a@crm.example.test", "x")),
            )
            .await;
        assert_eq!(st, 404, "{v}");
        let (st, v) = h
            .call(
                Method::POST,
                "/mail/messages",
                ALLVIEW,
                Some(simple("a@crm.example.test", "x")),
            )
            .await;
        assert_eq!(st, 403, "{v}");
        let (st, v) = h
            .call(
                Method::POST,
                "/mail/messages",
                VIEWER,
                Some(simple("a@crm.example.test", "x")),
            )
            .await;
        assert_eq!((st, v["error"].as_str()), (403, Some("forbidden")));
        assert_eq!(v["required"], "editor", "global built-in roles are named");

        let (st, v) = h.call(Method::DELETE, "/mail/messages", BEDIT, None).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["deleted"], 2, "only billing: {v}");
        assert_eq!(
            sorted(h.ids("/mail/messages", ADMIN).await),
            sorted(vec![crm.clone(), global.clone()])
        );
        let (st, _) = h.call(Method::DELETE, "/mail/messages", READER, None).await;
        assert_eq!(st, 403);
        h.ok(
            Method::DELETE,
            &format!("/mail/messages/{global}"),
            EDITOR,
            None,
        )
        .await;
        assert_eq!(h.ids("/mail/messages", ADMIN).await, vec![crm.clone()]);

        let actions: Vec<String> = h
            .audit("mail.*")
            .await
            .iter()
            .map(|e| {
                format!(
                    "{}@{}",
                    e["action"].as_str().unwrap(),
                    e["space"].as_str().unwrap_or("-")
                )
            })
            .collect();
        for want in [
            "mail.inject@billing",
            "mail.inject@crm",
            "mail.inject@-",
            "mail.message.labels@billing",
            "mail.messages.delete@-",
            "mail.message.delete@-",
        ] {
            assert!(
                actions.contains(&want.to_string()),
                "{mode}: {want} in {actions:?}"
            );
        }
        let labels = h.audit("mail.message.labels").await;
        assert_eq!(labels[0]["detail"]["after"], json!(["checked", "otp"]));
        assert_eq!(labels[0]["principal"], "writer");
    }
}

#[tokio::test]
async fn legacy_mode_without_access_file() {
    for (mode, storage) in storages().await {
        let h = H::start(storage, false, Some(mail_config())).await;
        let m = h
            .inject(EDITOR, "", simple("a@example.test", "legacy"))
            .await;
        assert!(m.get("space").is_none(), "{mode}");
        assert_eq!(h.ids("/mail/messages", VIEWER).await, vec![id(&m)]);
        let (st, v) = h
            .call(
                Method::DELETE,
                &format!("/mail/messages/{}", id(&m)),
                VIEWER,
                None,
            )
            .await;
        assert_eq!(st, 403);
        assert_eq!(
            v,
            json!({"error": "forbidden", "details": ["requires role editor"], "required": "editor"})
        );
        let (st, v) = h
            .call(
                Method::POST,
                "/mail/messages",
                VIEWER,
                Some(simple("a@example.test", "x")),
            )
            .await;
        assert_eq!((st, v["required"].as_str()), (403, Some("editor")));
        let (st, _) = h
            .call(Method::GET, "/mail/messages?space=billing", VIEWER, None)
            .await;
        assert_eq!(st, 404);
        h.ok(
            Method::PUT,
            &format!("/mail/messages/{}/labels", id(&m)),
            EDITOR,
            Some(json!({"labels": ["a"]})),
        )
        .await;
        h.ok(
            Method::DELETE,
            &format!("/mail/messages/{}", id(&m)),
            EDITOR,
            None,
        )
        .await;
        assert!(h.ids("/mail/messages", VIEWER).await.is_empty());
    }
}

#[tokio::test]
async fn mail_routes_answer_404_while_mail_is_off() {
    let h = H::start(StorageKind::Memory, true, None).await;
    for (m, path) in [
        (Method::GET, "/mail/messages"),
        (Method::GET, "/mail/stream"),
        (Method::GET, "/mail/mailboxes"),
        (Method::DELETE, "/mail/messages"),
    ] {
        let (st, v) = h.call(m.clone(), path, ADMIN, None).await;
        assert_eq!(st, 404, "{m} {path}");
        assert_eq!(
            v["error"],
            "mail is not enabled; set FASTMOCK_MAIL_ENABLED=true"
        );
    }
    let (st, _) = h
        .call(
            Method::POST,
            "/mail/messages",
            ADMIN,
            Some(simple("a@example.test", "x")),
        )
        .await;
    assert_eq!(st, 404);
}

async fn start_wait(
    h: &H,
    token: &str,
    query: &str,
    body: Value,
) -> tokio::task::JoinHandle<(u16, Value)> {
    let before = active_waits(&h.svc());
    let req = h
        .req(Method::POST, &format!("/mail/messages/wait{query}"), token)
        .json(&body);
    let task = tokio::spawn(async move {
        let r = req.send().await.unwrap();
        let st = r.status().as_u16();
        (st, r.json::<Value>().await.unwrap())
    });
    let svc = h.svc();
    until("the wait to start", || active_waits(&svc) > before).await;
    task
}

#[tokio::test]
async fn wait_for_messages() {
    for (mode, storage) in storages().await {
        let h = H::start(storage, true, Some(mail_config())).await;
        let old = h
            .inject(
                ADMIN,
                "?space=billing",
                simple("a@billing.example.test", "Code old"),
            )
            .await;

        let task = start_wait(
            &h,
            BVIEW,
            "",
            json!({"match": {"subject": {"contains": "Code"}}, "count": 2, "timeoutMs": 20000}),
        )
        .await;
        h.inject(
            ADMIN,
            "?space=billing",
            simple("a@billing.example.test", "Other"),
        )
        .await;
        h.inject(
            ADMIN,
            "?space=crm",
            simple("a@crm.example.test", "Code crm"),
        )
        .await;
        let a = h
            .inject(
                ADMIN,
                "?space=billing",
                simple("a@billing.example.test", "Code 1"),
            )
            .await;
        let b = h
            .inject(
                ADMIN,
                "?space=billing",
                simple("b@billing.example.test", "Code 2"),
            )
            .await;
        let (st, v) = task.await.unwrap();
        assert_eq!(st, 200, "{mode}: {v}");
        assert_eq!(v["count"], 2);
        let got: Vec<String> = v["messages"].as_array().unwrap().iter().map(id).collect();
        assert_eq!(
            got,
            vec![id(&a), id(&b)],
            "new billing messages only, oldest first"
        );

        let task = start_wait(
            &h,
            ADMIN,
            "?space=billing",
            json!({"match": {"headers": {"X-Wanted": {"equalTo": "yes"}}}, "timeoutMs": 20000}),
        )
        .await;
        let wanted = h
            .inject(
                ADMIN,
                "?space=billing",
                json!({"from": "shop@example.com", "to": ["a@billing.example.test"], "subject": "h",
                    "headers": {"X-Wanted": "yes"}}),
            )
            .await;
        let (st, v) = task.await.unwrap();
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["messages"][0]["id"], wanted["id"]);

        let started = std::time::Instant::now();
        let (st, v) = h
            .call(
                Method::POST,
                "/mail/messages/wait?space=billing",
                BVIEW,
                Some(json!({"match": {"subject": {"equalTo": "never"}}, "timeoutMs": 300})),
            )
            .await;
        assert_eq!(st, 408, "{v}");
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert_eq!(v["count"], 0);
        assert_eq!(v["expected"], 1);
        assert_eq!(
            v["error"],
            "timed out after 300 ms: 0 of 1 matching messages received"
        );

        let (st, v) = h
            .call(
                Method::POST,
                "/mail/messages/wait",
                BVIEW,
                Some(json!({"match": {"subject": {"contains": "Code"}}, "count": 5, "timeoutMs": 200, "includeExisting": true})),
            )
            .await;
        assert_eq!(st, 408, "{v}");
        assert_eq!(v["count"], 3, "Code old, Code 1 and Code 2: {v}");
        let (st, v) = h
            .call(
                Method::POST,
                "/mail/messages/wait",
                BVIEW,
                Some(json!({"match": {"subject": {"equalTo": "Code old"}}, "includeExisting": true, "timeoutMs": 0})),
            )
            .await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["messages"][0]["id"], old["id"]);
        let (st, _) = h
            .call(
                Method::POST,
                "/mail/messages/wait",
                BVIEW,
                Some(json!({"match": {"subject": {"equalTo": "Code old"}}, "lookbackMs": 600000, "timeoutMs": 0})),
            )
            .await;
        assert_eq!(st, 200);
        let (st, _) = h
            .call(
                Method::POST,
                "/mail/messages/wait",
                BVIEW,
                Some(json!({"match": {"subject": {"equalTo": "Code old"}}, "since": "2999-01-01T00:00:00Z", "timeoutMs": 0})),
            )
            .await;
        assert_eq!(st, 408);

        for (body, why) in [
            (json!({"timeoutMs": 60001}), "timeoutMs"),
            (json!({"count": 0}), "count"),
            (json!({"count": 501}), "count"),
            (
                json!({"match": {"subject": {"matches": "("}}}),
                "invalid match",
            ),
            (json!({"match": {"nope": {}}}), "nope"),
        ] {
            let (st, v) = h
                .call(
                    Method::POST,
                    "/mail/messages/wait",
                    ADMIN,
                    Some(body.clone()),
                )
                .await;
            assert_eq!(st, 400, "{body}: {v}");
            assert!(v["error"].as_str().unwrap().contains(why), "{body}: {v}");
        }

        let svc = h.svc();
        let idle = active_waits(&svc);
        let task = start_wait(&h, ADMIN, "", json!({"timeoutMs": 60000})).await;
        assert_eq!(active_waits(&svc), idle + 1);
        task.abort();
        until("the wait to stop after the client went away", || {
            active_waits(&svc) == idle
        })
        .await;
    }
}

#[tokio::test]
async fn verify_with_near_misses() {
    for (mode, storage) in storages().await {
        let h = H::start(storage, true, Some(mail_config())).await;
        h.inject(
            ADMIN,
            "?space=billing",
            json!({"from": "shop@example.com", "to": ["alice@billing.example.test"],
                "subject": "Welcome Alice", "text": "Hi Alice", "headers": {"X-Kind": "welcome"}}),
        )
        .await;
        let bob = h
            .inject(
                ADMIN,
                "?space=billing",
                json!({"from": "shop@example.com", "to": ["bob@billing.example.test"],
                    "subject": "Welcome Bob", "text": "Hi Bob"}),
            )
            .await;
        h.inject(
            ADMIN,
            "?space=crm",
            simple("alice@crm.example.test", "Welcome Alice"),
        )
        .await;

        let v = h
            .ok(
                Method::POST,
                "/mail/verify?space=billing",
                BVIEW,
                Some(json!({"match": {"to": {"equalTo": "alice@billing.example.test"}, "subject": {"contains": "Welcome"}}, "count": 1})),
            )
            .await;
        assert_eq!(v["verified"], true, "{mode}: {v}");
        assert_eq!(v["count"], 1);
        assert_eq!(v["expected"], "exactly 1");
        let v = h
            .ok(
                Method::POST,
                "/mail/verify",
                READER,
                Some(json!({"match": {"subject": {"contains": "Welcome"}}, "atLeast": 3})),
            )
            .await;
        assert_eq!(v["count"], 3);
        let v = h
            .ok(
                Method::POST,
                "/mail/verify",
                BVIEW,
                Some(json!({"match": {"subject": {"contains": "Welcome"}}, "atMost": 2})),
            )
            .await;
        assert_eq!(v["count"], 2, "the crm copy is not visible");
        h.ok(
            Method::POST,
            "/mail/verify",
            BVIEW,
            Some(json!({"match": {"headers": {"x-kind": {"equalTo": "welcome"}}, "text": {"contains": "Alice"}}, "count": 1})),
        )
        .await;
        h.ok(
            Method::POST,
            "/mail/verify",
            BVIEW,
            Some(json!({"match": {"subject": {"contains": "nothing"}}, "count": 0})),
        )
        .await;

        let (st, v) = h
            .call(
                Method::POST,
                "/mail/verify",
                BVIEW,
                Some(json!({"match": {"to": {"equalTo": "bob@billing.example.test"}, "subject": {"equalTo": "Welcome Robert"}, "hasAttachment": false}, "count": 1})),
            )
            .await;
        assert_eq!(st, 422, "{v}");
        assert_eq!(v["verified"], false);
        assert_eq!(v["count"], 0);
        assert_eq!(
            v["error"],
            "expected exactly 1 matching messages, received 0"
        );
        let near = v["nearMisses"].as_array().unwrap();
        assert_eq!(near.len(), 2);
        assert_eq!(near[0]["id"], bob["id"], "closest first: {v}");
        assert_eq!(near[0]["mismatches"], json!(["subject"]));
        let score = near[0]["score"].as_f64().unwrap();
        assert!((score - 2.0 / 3.0).abs() < 1e-9, "{score}");
        assert_eq!(near[1]["mismatches"], json!(["to", "subject"]));
        assert!(v["details"][0].as_str().unwrap().contains("Welcome Bob"));

        let (st, v) = h
            .call(
                Method::POST,
                "/mail/verify",
                BVIEW,
                Some(json!({"match": {"subject": {"contains": "Welcome"}}, "atMost": 1})),
            )
            .await;
        assert_eq!(st, 422);
        assert_eq!(v["expected"], "at most 1");

        for body in [
            json!({"count": 1, "atLeast": 1}),
            json!({"atLeast": 3, "atMost": 1}),
            json!({"match": {"subject": {"matches": "("}}}),
            json!({"exactly": 1}),
        ] {
            let (st, v) = h
                .call(Method::POST, "/mail/verify", BVIEW, Some(body.clone()))
                .await;
            assert_eq!(st, 400, "{body}: {v}");
        }
    }
}

#[tokio::test]
async fn inject_json_and_raw_with_limits() {
    for (mode, storage) in storages().await {
        let mut cfg = mail_config();
        cfg.max_message_bytes = 64 * 1024;
        let h = H::start(storage, true, Some(cfg)).await;

        let m = h
            .inject(
                ADMIN,
                "?space=billing",
                json!({"from": "Shop <shop@example.com>", "to": ["Alice <alice+news@billing.example.test>"],
                    "cc": ["carol@billing.example.test"], "subject": "Hi",
                    "html": "<p>Hi</p>", "headers": {"X-FastMock-Labels": "promo"}}),
            )
            .await;
        assert_eq!(m["labels"], json!(["news", "promo"]), "{mode}");
        assert_eq!(m["envelope"]["mailFrom"], "shop@example.com");
        let boxes = h
            .ok(Method::GET, "/mail/mailboxes?space=billing", BVIEW, None)
            .await;
        let addrs: Vec<&str> = boxes["mailboxes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["address"].as_str().unwrap())
            .collect();
        assert_eq!(
            addrs,
            vec!["alice@billing.example.test", "carol@billing.example.test"]
        );

        let with_box = h
            .inject(
                ADMIN,
                "?space=billing",
                json!({"from": "shop@example.com", "to": ["x@billing.example.test"], "subject": "boxed",
                    "mailbox": "Archive+x@Billing.Example.Test"}),
            )
            .await;
        let svc = h.svc();
        let entries = svc
            .store
            .mailbox_entries(&fastmock_core::mail::MailboxKey::inbox(
                Some("billing"),
                "archive@billing.example.test",
            ))
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].message_id.to_string(), id(&with_box));

        let raw = "From: Sender <sender@example.com>\r\nTo: a@billing.example.test\r\nBcc: hidden@billing.example.test\r\nSubject: raw one\r\n\r\nbody\r\n";
        let r = h
            .req(Method::POST, "/mail/messages?space=billing", ADMIN)
            .header("content-type", "message/rfc822")
            .body(raw)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 201);
        let v: Value = r.json().await.unwrap();
        let e = &v["messages"][0];
        assert_eq!(e["envelope"]["mailFrom"], "sender@example.com");
        assert_eq!(
            e["envelope"]["rcptTo"],
            json!(["a@billing.example.test", "hidden@billing.example.test"])
        );
        assert_eq!(e["summary"]["subject"], "raw one");
        let stored = h
            .req(Method::GET, &format!("/mail/messages/{}/raw", id(e)), ADMIN)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(stored, raw, "stored byte for byte");

        let r = h
            .req(
                Method::POST,
                "/mail/messages?space=billing&rcptTo=one@billing.example.test,%20two@billing.example.test&mailFrom=",
                ADMIN,
            )
            .header("content-type", "message/rfc822")
            .body(raw)
            .send()
            .await
            .unwrap();
        let v: Value = r.json().await.unwrap();
        assert_eq!(
            v["messages"][0]["envelope"]["rcptTo"],
            json!(["one@billing.example.test", "two@billing.example.test"])
        );
        assert_eq!(v["messages"][0]["envelope"]["mailFrom"], "");

        for (body, why) in [
            (
                json!({"from": "a@example.com", "to": ["b@example.test"], "subject": "x\r\nBcc: evil@example.com"}),
                "subject",
            ),
            (
                json!({"from": "a@example.com", "to": ["b@example.test"], "headers": {"X-A": "v\r\nBcc: evil@example.com"}}),
                "X-A",
            ),
            (
                json!({"from": "a@example.com", "to": ["b@example.test\r\nBcc: evil@example.com"]}),
                "invalid address",
            ),
            (
                json!({"from": "a@example.com\nBcc: evil@example.com", "to": ["b@example.test"]}),
                "invalid address",
            ),
            (
                json!({"from": "a@example.com", "to": ["b@example.test"], "headers": {"Content-Type": "text/html"}}),
                "cannot be set",
            ),
            (
                json!({"from": "a@example.com", "to": ["b@example.test"], "headers": {"Bad Name": "v"}}),
                "cannot be set",
            ),
            (
                json!({"from": "a@example.com", "to": []}),
                "at least one recipient",
            ),
            (
                json!({"from": "a@example.com", "to": ["b@example.test"], "bcc": ["c@example.test"]}),
                "unknown field",
            ),
            (
                json!({"from": "a@example.com", "to": ["b@example.test"], "attachments": [{"name": "a", "base64": "***"}]}),
                "attachment a",
            ),
            (
                json!({"from": "a@example.com", "to": ["b@example.test"], "mailbox": "not an address"}),
                "mailbox",
            ),
        ] {
            let (st, v) = h
                .call(Method::POST, "/mail/messages", ADMIN, Some(body.clone()))
                .await;
            assert_eq!(st, 400, "{body}: {v}");
            assert!(v["error"].as_str().unwrap().contains(why), "{body}: {v}");
        }

        let big = B64.encode(vec![1u8; 70 * 1024]);
        let (st, v) = h
            .call(
                Method::POST,
                "/mail/messages",
                ADMIN,
                Some(json!({"from": "a@example.com", "to": ["b@example.test"],
                    "attachments": [{"name": "big.bin", "base64": big}]})),
            )
            .await;
        assert_eq!(st, 413, "{v}");
        assert_eq!(
            v["error"],
            "the message exceeds 65536 bytes (FASTMOCK_MAIL_MAX_MESSAGE_BYTES)"
        );
        let mut huge = String::from("To: a@example.test\r\nSubject: big\r\n\r\n");
        huge.push_str(&"x".repeat(65 * 1024));
        let r = h
            .req(Method::POST, "/mail/messages", ADMIN)
            .header("content-type", "message/rfc822")
            .body(huge)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 413);
        let r = h
            .req(Method::POST, "/mail/messages", ADMIN)
            .header("content-type", "message/rfc822")
            .body("Subject: nobody\r\n\r\nbody\r\n")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        let v: Value = r.json().await.unwrap();
        assert!(
            v["error"].as_str().unwrap().contains("no recipients"),
            "{v}"
        );
        let r = h
            .req(Method::POST, "/mail/messages", ADMIN)
            .header("content-type", "text/plain")
            .body("hello")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 415);
        let r = h
            .req(Method::POST, "/mail/messages", ADMIN)
            .header("content-type", "application/json")
            .body("{not json")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);

        let audit = h.audit("mail.inject").await;
        assert!(audit.len() >= 4, "{audit:?}");
        assert!(audit.iter().any(|e| e["detail"]["format"] == "raw"));
        assert!(
            audit
                .iter()
                .all(|e| !e["detail"].to_string().contains("body")),
            "no message content in the audit log"
        );
    }
}

struct SseClient {
    resp: reqwest::Response,
    buf: String,
}

impl SseClient {
    async fn open(h: &H, query: &str, token: &str) -> SseClient {
        let resp = h
            .req(Method::GET, &format!("/mail/stream{query}"), token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["content-type"], "text/event-stream");
        let mut c = SseClient {
            resp,
            buf: String::new(),
        };
        assert!(c.until(|b| b.contains(": ready"), 5).await, "{}", c.buf);
        c
    }

    async fn until(&mut self, pred: impl Fn(&str) -> bool, secs: u64) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        while !pred(&self.buf) {
            match tokio::time::timeout_at(deadline, self.resp.chunk()).await {
                Ok(Ok(Some(c))) => self.buf.push_str(&String::from_utf8_lossy(&c)),
                _ => return false,
            }
        }
        true
    }

    fn events(&self) -> Vec<(String, Value)> {
        self.buf
            .split("\n\n")
            .filter_map(|block| {
                let mut ev = None;
                let mut data = String::new();
                for line in block.lines() {
                    if let Some(e) = line.strip_prefix("event: ") {
                        ev = Some(e.to_string());
                    } else if let Some(d) = line.strip_prefix("data: ") {
                        data.push_str(d);
                    }
                }
                Some((ev?, serde_json::from_str(&data).unwrap_or(Value::Null)))
            })
            .collect()
    }
}

#[tokio::test]
async fn stream_events_filtered_by_space() {
    for (mode, storage) in storages().await {
        let h = H::start(storage, true, Some(mail_config())).await;
        let svc = h.svc();
        let mut billing = SseClient::open(&h, "", BVIEW).await;
        let mut everything = SseClient::open(&h, "", ADMIN).await;
        let mut crm_only = SseClient::open(&h, "?space=crm", ALLVIEW).await;
        assert_eq!(active_streams(&svc), 3, "{mode}");
        let (st, _) = h
            .call(Method::GET, "/mail/stream?space=crm", BVIEW, None)
            .await;
        assert_eq!(st, 404);

        let c = h
            .inject(ADMIN, "?space=crm", simple("a@crm.example.test", "to crm"))
            .await;
        let b = h
            .inject(
                ADMIN,
                "?space=billing",
                simple("a@billing.example.test", "to billing"),
            )
            .await;
        let g = h
            .inject(ADMIN, "", simple("a@example.test", "global"))
            .await;
        h.ok(
            Method::DELETE,
            &format!("/mail/messages/{}", id(&b)),
            ADMIN,
            None,
        )
        .await;

        let gid = id(&g);
        assert!(
            everything
                .until(|s| s.contains(&gid) && s.contains("message.deleted"), 5)
                .await
        );
        let bid = id(&b);
        assert!(billing.until(|s| s.contains("message.deleted"), 5).await);
        let cid = id(&c);
        assert!(crm_only.until(|s| s.contains(&cid), 5).await);

        let seen = |c: &SseClient| -> Vec<String> {
            c.events()
                .iter()
                .map(|(k, v)| {
                    let id = v["ids"][0].as_str().unwrap_or_default().to_string();
                    format!("{k}:{id}")
                })
                .collect()
        };
        assert_eq!(
            seen(&billing),
            vec![
                format!("message.received:{bid}"),
                format!("message.deleted:{bid}")
            ]
        );
        assert_eq!(
            seen(&everything),
            vec![
                format!("message.received:{cid}"),
                format!("message.received:{bid}"),
                format!("message.received:{gid}"),
                format!("message.deleted:{bid}")
            ]
        );
        assert_eq!(seen(&crm_only), vec![format!("message.received:{cid}")]);
        let received = &billing.events()[0].1;
        assert_eq!(received["type"], "message.received");
        assert_eq!(received["space"], "billing");
        assert_eq!(received["entry"]["summary"]["subject"], "to billing");
        assert_eq!(received["mailboxes"], json!(["a@billing.example.test"]));

        let mut beat = SseClient::open(&h, "?heartbeatSeconds=1", READER).await;
        assert!(
            beat.until(|s| s.contains(": heartbeat"), 5).await,
            "{}",
            beat.buf
        );
        let (st, _) = h
            .call(Method::GET, "/mail/stream?heartbeatSeconds=0", READER, None)
            .await;
        assert_eq!(st, 400);

        assert_eq!(active_streams(&svc), 4);
        drop(billing);
        drop(everything);
        drop(crm_only);
        drop(beat);
        for space in ["", "?space=billing", "?space=crm"] {
            h.inject(ADMIN, space, simple("a@example.test", "wake"))
                .await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        until("streams to close after the clients went away", || {
            active_streams(&svc) == 0
        })
        .await;

        let mut open = SseClient::open(&h, "", ADMIN).await;
        let wait = start_wait(&h, ADMIN, "", json!({"timeoutMs": 60000})).await;
        h.app.ready.store(false, Ordering::SeqCst);
        let (st, v) = wait.await.unwrap();
        assert_eq!(st, 503, "{v}");
        assert!(v["error"].as_str().unwrap().contains("shutting down"));
        let ended = tokio::time::timeout(Duration::from_secs(5), async {
            while let Ok(Some(_)) = open.resp.chunk().await {}
        })
        .await;
        assert!(ended.is_ok(), "the stream ends when the server shuts down");
        until("streams to close at shutdown", || active_streams(&svc) == 0).await;
        h.app.ready.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn mailbox_passwords() {
    for (mode, storage) in storages().await {
        let h = H::start(storage, true, Some(mail_config())).await;
        h.inject(
            ADMIN,
            "?space=billing",
            simple("Alice+tag@billing.example.test", "x"),
        )
        .await;
        h.inject(ADMIN, "?space=crm", simple("zed@crm.example.test", "x"))
            .await;
        let v = h.ok(Method::GET, "/mail/mailboxes", BVIEW, None).await;
        assert_eq!(
            v["mailboxes"],
            json!([{"space": "billing", "address": "alice@billing.example.test", "folders": ["INBOX"], "hasPassword": false}]),
            "{mode}"
        );
        assert_eq!(
            h.ok(Method::GET, "/mail/mailboxes", ADMIN, None).await["mailboxes"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        let path = "/mail/mailboxes/Alice+imap@billing.example.test/password?space=billing";
        let (st, v) = h
            .call(
                Method::PUT,
                path,
                BVIEW,
                Some(json!({"password": "s3cret"})),
            )
            .await;
        assert_eq!((st, v["required"].as_str()), (403, Some("mail:write")));
        let (st, _) = h
            .call(
                Method::PUT,
                path,
                WRITER,
                Some(json!({"password": "s3cret"})),
            )
            .await;
        assert_eq!(st, 204);
        let v = h
            .ok(Method::GET, "/mail/mailboxes?space=billing", BVIEW, None)
            .await;
        assert_eq!(v["mailboxes"][0]["hasPassword"], true);
        assert!(!v.to_string().contains("s3cret") && !v.to_string().contains("pbkdf2"));
        let stored = h
            .svc()
            .store
            .mailbox_password(Some("billing"), "alice@billing.example.test")
            .await
            .unwrap()
            .unwrap();
        assert!(stored.starts_with("pbkdf2-sha256$"), "{stored}");
        assert!(fastmock_server::mail_api::password::verify(
            &stored, "s3cret"
        ));
        assert!(!fastmock_server::mail_api::password::verify(
            &stored, "wrong"
        ));
        let audit = h.audit("mail.mailbox.password.*").await;
        assert_eq!(
            audit[0]["detail"],
            json!({"address": "alice@billing.example.test"})
        );
        assert!(!audit[0].to_string().contains("s3cret"));

        let fresh = h
            .ok(Method::GET, "/mail/mailboxes?space=crm", ADMIN, None)
            .await;
        assert_eq!(fresh["mailboxes"][0]["hasPassword"], false);
        let (st, _) = h
            .call(
                Method::PUT,
                "/mail/mailboxes/new@crm.example.test/password?space=crm",
                ADMIN,
                Some(json!({"password": "x"})),
            )
            .await;
        assert_eq!(st, 204, "setting a password creates the mailbox");
        assert_eq!(
            h.ok(Method::GET, "/mail/mailboxes?space=crm", ADMIN, None)
                .await["mailboxes"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        for (p, body, why) in [
            (
                "/mail/mailboxes/nobody/password",
                json!({"password": "x"}),
                "invalid mailbox address",
            ),
            (
                "/mail/mailboxes/a%20b@example.test/password",
                json!({"password": "x"}),
                "invalid mailbox address",
            ),
            (
                "/mail/mailboxes/a@example.test/password",
                json!({"password": ""}),
                "password must be",
            ),
            (
                "/mail/mailboxes/a@example.test/password",
                json!({"password": "a\nb"}),
                "control",
            ),
            (
                "/mail/mailboxes/a@example.test/password",
                json!({"password": "x".repeat(1025)}),
                "password must be",
            ),
        ] {
            let (st, v) = h.call(Method::PUT, p, ADMIN, Some(body)).await;
            assert_eq!(st, 400, "{p}: {v}");
            assert!(v["error"].as_str().unwrap().contains(why), "{p}: {v}");
        }

        let (st, _) = h.call(Method::DELETE, path, WRITER, None).await;
        assert_eq!(st, 204);
        let (st, v) = h.call(Method::DELETE, path, WRITER, None).await;
        assert_eq!(st, 404);
        assert_eq!(
            v["error"],
            "no password is set for alice@billing.example.test"
        );
        assert!(
            h.svc()
                .store
                .mailbox_password(Some("billing"), "alice@billing.example.test")
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn delete_by_query_and_labels() {
    for (mode, storage) in storages().await {
        let h = H::start(storage, true, Some(mail_config())).await;
        let svc = h.svc();
        let mut sub = svc.events.subscribe(fastmock_core::SpaceFilter::Any);
        let keep = h
            .inject(
                ADMIN,
                "?space=billing",
                simple("a@billing.example.test", "keep me"),
            )
            .await;
        let d1 = h
            .inject(
                ADMIN,
                "?space=billing",
                simple("a@billing.example.test", "drop 1"),
            )
            .await;
        let d2 = h
            .inject(
                ADMIN,
                "?space=billing",
                simple("a@billing.example.test", "drop 2"),
            )
            .await;
        let other = h
            .inject(
                ADMIN,
                "?space=crm",
                simple("a@crm.example.test", "drop crm"),
            )
            .await;
        for _ in 0..4 {
            sub.recv().await.unwrap();
        }
        let v = h
            .ok(
                Method::DELETE,
                &format!(
                    "/mail/messages?space=billing&query={}",
                    urlencode("subject:drop")
                ),
                ADMIN,
                None,
            )
            .await;
        assert_eq!(v["deleted"], 2, "{mode}");
        assert_eq!(
            sorted(vec![
                v["ids"][0].as_str().unwrap().into(),
                v["ids"][1].as_str().unwrap().into()
            ]),
            sorted(vec![id(&d1), id(&d2)])
        );
        let e = sub.recv().await.unwrap();
        assert_eq!(e.kind, fastmock_core::mail::MailEventKind::Deleted);
        assert_eq!(e.space.as_deref(), Some("billing"));
        assert_eq!(e.ids.len(), 2);
        assert_eq!(
            h.ids("/mail/messages?space=billing", ADMIN).await,
            vec![id(&keep)]
        );
        assert_eq!(
            h.ids("/mail/messages?space=crm", ADMIN).await,
            vec![id(&other)]
        );
        let (st, _) = h
            .call(
                Method::DELETE,
                "/mail/messages?query=has:nothing",
                ADMIN,
                None,
            )
            .await;
        assert_eq!(st, 400);
        let a = h.audit("mail.messages.delete").await;
        assert_eq!(
            a[0]["detail"],
            json!({"deleted": 2, "query": "subject:drop"})
        );
        assert_eq!(a[0]["space"], "billing");

        let path = format!("/mail/messages/{}/labels", id(&keep));
        for bad in [
            json!({"labels": ["two words"]}),
            json!({"labels": (0..40).map(|i| format!("l{i}")).collect::<Vec<_>>()}),
            json!({"tags": []}),
        ] {
            let (st, v) = h.call(Method::PUT, &path, ADMIN, Some(bad.clone())).await;
            assert!(st == 400 || st == 422, "{bad}: {st} {v}");
        }
        let v = h
            .ok(Method::PUT, &path, ADMIN, Some(json!({"labels": []})))
            .await;
        assert_eq!(v["labels"], json!([]));
        let v = h
            .ok(
                Method::PUT,
                &format!("{path}?space=billing"),
                ADMIN,
                Some(json!({"labels": ["Team/OTP:v2"]})),
            )
            .await;
        assert_eq!(v["labels"], json!(["team/otp:v2"]));
        assert_eq!(
            h.ids(
                &format!("/mail/messages?query={}", urlencode("label:team/otp:v2")),
                ADMIN
            )
            .await,
            vec![id(&keep)]
        );
        let (st, _) = h
            .call(
                Method::PUT,
                &format!("{path}?space=crm"),
                ADMIN,
                Some(json!({"labels": []})),
            )
            .await;
        assert_eq!(st, 404);

        let v = h.ok(Method::DELETE, "/mail/messages", ADMIN, None).await;
        assert_eq!(v["deleted"], 2);
        assert!(h.ids("/mail/messages", ADMIN).await.is_empty());
    }
}

/// An event bus that loses events (like Valkey pub/sub while reconnecting) and asks to be polled.
struct LossyEvents {
    inner: fastmock_core::mail::LocalMailEvents,
    deliver: bool,
}

#[async_trait::async_trait]
impl fastmock_core::mail::MailEvents for LossyEvents {
    async fn publish(
        &self,
        event: &fastmock_core::mail::MailEvent,
    ) -> fastmock_core::StoreResult<()> {
        if self.deliver {
            self.inner.publish(event).await?;
        }
        Ok(())
    }

    fn subscribe(
        &self,
        filter: fastmock_core::SpaceFilter,
    ) -> fastmock_core::mail::MailSubscription {
        self.inner.subscribe(filter)
    }

    fn poll_interval(&self) -> Option<Duration> {
        Some(Duration::from_millis(200))
    }
}

async fn start_lossy(deliver: bool) -> H {
    let h = H::start(StorageKind::Memory, true, Some(mail_config())).await;
    let old = h.svc();
    let mut app = h.app.clone();
    app.mail = Some(std::sync::Arc::new(fastmock_mail::MailService::new(
        old.config.clone(),
        std::sync::Arc::new(fastmock_core::mail::MemoryMailStore::new(1000)),
        std::sync::Arc::new(LossyEvents {
            inner: fastmock_core::mail::LocalMailEvents::new(64),
            deliver,
        }),
        old.domains.clone(),
        old.spaces.clone(),
    )));
    let admin = fastmock_server::admin::router(app.clone(), vec![]);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, admin).await.unwrap() });
    H {
        base: format!("http://{addr}/__admin"),
        http: reqwest::Client::new(),
        app,
    }
}

#[tokio::test]
async fn waits_and_streams_poll_the_store_when_events_are_lost() {
    for deliver in [false, true] {
        let h = start_lossy(deliver).await;
        let mut stream = SseClient::open(&h, "?space=billing", BVIEW).await;
        let task = start_wait(
            &h,
            BVIEW,
            "",
            json!({"match": {"subject": {"equalTo": "lost"}}, "timeoutMs": 10000}),
        )
        .await;
        let m = h
            .inject(
                ADMIN,
                "?space=billing",
                simple("a@billing.example.test", "lost"),
            )
            .await;
        h.inject(ADMIN, "?space=crm", simple("a@crm.example.test", "lost"))
            .await;
        let (st, v) = task.await.unwrap();
        assert_eq!(st, 200, "deliver={deliver}: {v}");
        assert_eq!(v["messages"][0]["id"], m["id"]);
        let mid = id(&m);
        assert!(
            stream.until(|s| s.contains(&mid), 5).await,
            "{}",
            stream.buf
        );
        stream.until(|_| false, 1).await;
        let received: Vec<Value> = stream
            .events()
            .into_iter()
            .filter(|(k, _)| k == "message.received")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(
            received.len(),
            1,
            "deliver={deliver}: announced once: {received:?}"
        );
        assert_eq!(received[0]["entry"]["summary"]["subject"], "lost");
        assert_eq!(received[0]["space"], "billing");
    }
}

// ----- permission boundaries, paging and stream limits -----

#[tokio::test]
async fn mailbox_passwords_need_read_and_write() {
    let h = H::start(StorageKind::Memory, true, Some(mail_config())).await;
    let path = "/mail/mailboxes/alice@billing.example.test/password?space=billing";
    let (st, v) = h
        .call(
            Method::PUT,
            path,
            SENDER,
            Some(json!({"password": "s3cret"})),
        )
        .await;
    assert_eq!(st, 403, "{v}");
    let (st, v) = h.call(Method::DELETE, path, SENDER, None).await;
    assert_eq!(st, 403, "{v}");
    let (st, v) = h
        .call(
            Method::PUT,
            path,
            WRITER,
            Some(json!({"password": "s3cret"})),
        )
        .await;
    assert_eq!(st, 204, "{v}");
}

#[tokio::test]
async fn mailboxes_are_listed_in_pages() {
    let h = H::start(StorageKind::Memory, true, Some(mail_config())).await;
    for a in ["a", "b", "c"] {
        h.inject(
            ADMIN,
            "?space=billing",
            simple(&format!("{a}@billing.example.test"), "x"),
        )
        .await;
    }
    let v = h
        .ok(Method::GET, "/mail/mailboxes?limit=2", ADMIN, None)
        .await;
    let names = |v: &Value| -> Vec<String> {
        v["mailboxes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["address"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        names(&v),
        vec!["a@billing.example.test", "b@billing.example.test"]
    );
    let next = v["nextCursor"].as_str().expect("a next page").to_string();
    let v = h
        .ok(
            Method::GET,
            &format!("/mail/mailboxes?limit=2&cursor={next}"),
            ADMIN,
            None,
        )
        .await;
    assert_eq!(names(&v), vec!["c@billing.example.test"]);
    assert!(v.get("nextCursor").is_none(), "{v}");
    let (st, _) = h
        .call(Method::GET, "/mail/mailboxes?limit=0", ADMIN, None)
        .await;
    assert_eq!(st, 400);
    let (st, _) = h
        .call(Method::GET, "/mail/mailboxes?cursor=%21%21", ADMIN, None)
        .await;
    assert_eq!(st, 400);
}

#[tokio::test]
async fn streams_are_limited_per_caller_and_in_time() {
    let h = H::start(StorageKind::Memory, true, Some(mail_config())).await;
    let mut open = Vec::new();
    for _ in 0..8 {
        open.push(SseClient::open(&h, "", READER).await);
    }
    let (st, v) = h.call(Method::GET, "/mail/stream", READER, None).await;
    assert_eq!(st, 429, "{v}");
    let other = SseClient::open(&h, "", ADMIN).await;
    drop(other);
    drop(open);
    let mut short = None;
    for _ in 0..50 {
        let r = h
            .req(Method::GET, "/mail/stream?maxSeconds=1", READER)
            .send()
            .await
            .unwrap();
        if r.status() == 200 {
            short = Some(r);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut short = short.expect("a stream once the others closed");
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        while let Ok(Some(_)) = short.chunk().await {}
    })
    .await;
    assert!(ended.is_ok(), "the stream ends after maxSeconds");
    let (st, _) = h
        .call(Method::GET, "/mail/stream?maxSeconds=0", READER, None)
        .await;
    assert_eq!(st, 400);
}

#[tokio::test]
async fn streams_end_when_the_token_is_revoked() {
    let h = H::start(StorageKind::Memory, true, Some(mail_config())).await;
    let (st, t) = h
        .call(
            Method::POST,
            "/tokens",
            ADMIN,
            Some(json!({"name": "stream-reader", "role": "mail-reader", "global": true})),
        )
        .await;
    assert_eq!(st, 201, "{t}");
    let token = t["token"].as_str().unwrap().to_string();
    let id = t["id"].as_str().unwrap().to_string();
    let mut stream = SseClient::open(&h, "", &token).await;
    h.ok(Method::DELETE, &format!("/tokens/{id}"), ADMIN, None)
        .await;
    let ended = tokio::time::timeout(Duration::from_secs(45), async {
        while let Ok(Some(c)) = stream.resp.chunk().await {
            stream.buf.push_str(&String::from_utf8_lossy(&c));
        }
    })
    .await;
    assert!(ended.is_ok(), "the stream of a revoked token ends");
    let late = h
        .inject(ADMIN, "", simple("late@example.test", "late"))
        .await;
    assert!(!stream.buf.contains(&id_of(&late)), "{}", stream.buf);
}

fn id_of(v: &Value) -> String {
    v["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn ui_config_lists_the_mail_ports() {
    let mut m = mail_config();
    m.listeners.imap = Some("127.0.0.1:1143".parse().unwrap());
    let h = H::start(StorageKind::Memory, true, Some(m)).await;
    let root = h.base.trim_end_matches("/__admin");
    let v: Value = h
        .http
        .get(format!("{root}/ui/config.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["mailImapPort"], 1143, "{v}");
    assert_eq!(v["mailSmtpPort"], Value::Null, "{v}");
    assert_eq!(v["mailPop3Port"], Value::Null, "{v}");
}
