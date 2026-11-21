//! Provider webhooks: configuration API (secrets never returned), delivery events and inbound payloads
//! through the callbacks outbox to a local receiver, with signatures checked by independent code.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use fastmock_mail::MailConfig;
use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::Connection;
use tower::ServiceExt;

const ADMIN: &str = "fm_admin_token_for_mail_webhook_tests_012";
const VIEWER: &str = "fm_viewer_token_for_mail_webhook_tests_01";

const ACCESS: &str = r#"
spaces:
  - name: billing
    mailDomains: [billing.example.test]
"#;

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
    let db = format!("fm_mailhook_{}", uuid::Uuid::new_v4().simple());
    let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut c)
        .await
        .unwrap();
    v.push(StorageKind::Postgres {
        database_url: format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db),
        valkey_url,
        valkey_prefix: format!("mailhook:{}:", uuid::Uuid::new_v4().simple()),
        max_connections: 5,
        options: Default::default(),
    });
    v
}

struct Fm {
    data: axum::Router,
    admin: axum::Router,
    _stop: tokio::sync::watch::Sender<bool>,
}

async fn server(storage: StorageKind) -> Fm {
    let dir = std::env::temp_dir().join(format!("fm-mail-hooks-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let access = dir.join("access.yaml");
    std::fs::write(&access, ACCESS).unwrap();
    let mut cfg = Config::for_tests(
        storage,
        vec![
            format!("ops:admin:{}", hash_token(ADMIN)),
            format!("dash:viewer:{}", hash_token(VIEWER)),
        ],
    );
    cfg.plugins = Some(vec![]);
    cfg.access_file = Some(access);
    let mut mail = MailConfig::default();
    mail.listeners.smtp = None;
    cfg.mail = Some(mail);
    let (app, data, admin, policy) = fastmock_server::build(cfg, prom()).await.unwrap();
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(fastmock_server::outbox::run(app, policy, rx));
    Fm {
        data,
        admin,
        _stop: tx,
    }
}

#[derive(Clone, Debug)]
struct Got {
    path: String,
    headers: HeaderMap,
    body: Bytes,
}

impl Got {
    fn h(&self, n: &str) -> &str {
        self.headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

#[derive(Clone, Default)]
struct Receiver {
    got: Arc<Mutex<Vec<Got>>>,
    statuses: Arc<Mutex<VecDeque<u16>>>,
}

impl Receiver {
    async fn start() -> (String, Receiver) {
        let r = Receiver::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let rr = r.clone();
        let app = axum::Router::new().fallback(move |req: Request| {
            let rr = rr.clone();
            async move {
                let path = req.uri().path().to_string();
                let headers = req.headers().clone();
                let body = axum::body::to_bytes(req.into_body(), 1 << 24)
                    .await
                    .unwrap();
                rr.got.lock().unwrap().push(Got {
                    path,
                    headers,
                    body,
                });
                let s = rr.statuses.lock().unwrap().pop_front().unwrap_or(200);
                StatusCode::from_u16(s).unwrap()
            }
        });
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), r)
    }

    async fn wait(&self, n: usize) -> Vec<Got> {
        for _ in 0..200 {
            if self.got.lock().unwrap().len() >= n {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let got = self.got.lock().unwrap().clone();
        assert!(
            got.len() >= n,
            "expected {n} webhook calls, got {}",
            got.len()
        );
        got
    }

    fn for_path(&self, p: &str) -> Vec<Got> {
        self.got
            .lock()
            .unwrap()
            .iter()
            .filter(|g| g.path == p)
            .cloned()
            .collect()
    }
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

async fn admin_as(fm: &Fm, token: &str, method: &str, path: &str, body: Value) -> Res {
    send(
        &fm.admin,
        method,
        &format!("/__admin{path}"),
        &[
            ("authorization", &format!("Bearer {token}")),
            ("content-type", "application/json"),
        ],
        body.to_string(),
    )
    .await
}

async fn admin(fm: &Fm, method: &str, path: &str, body: Value) -> Res {
    admin_as(fm, ADMIN, method, path, body).await
}

fn verify_sendgrid(public_key_b64: &str, ts: &str, body: &[u8], sig_b64: &str) {
    let spki = B64.decode(public_key_b64).unwrap();
    assert_eq!(spki.len(), 91, "P-256 SubjectPublicKeyInfo");
    let sig = B64.decode(sig_b64).unwrap();
    let mut msg = ts.as_bytes().to_vec();
    msg.extend_from_slice(body);
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, &spki[26..])
        .verify(&msg, &sig)
        .expect("the SendGrid signature verifies with the published key");
}

fn mailgun_hmac(key: &str, ts: &str, token: &str) -> String {
    let mut m = Hmac::<Sha256>::new_from_slice(key.as_bytes()).unwrap();
    m.update(ts.as_bytes());
    m.update(token.as_bytes());
    hex::encode(m.finalize().into_bytes())
}

/// The value of a multipart form field.
fn form_field(g: &Got, name: &str) -> Option<Vec<u8>> {
    let ct = g.h("content-type");
    let boundary = ct.split("boundary=").nth(1)?.trim_matches('"');
    let sep = format!("--{boundary}");
    let body = &g.body[..];
    let mut start = 0;
    let marker = format!("name=\"{name}\"");
    while let Some(i) = find(&body[start..], sep.as_bytes()) {
        let part_start = start + i + sep.len();
        let next = find(&body[part_start..], sep.as_bytes()).map(|j| part_start + j)?;
        let part = &body[part_start..next];
        if let Some(h_end) = find(part, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&part[..h_end]);
            if head.contains(&marker) {
                let content = &part[h_end + 4..];
                return Some(content[..content.len().saturating_sub(2)].to_vec());
            }
        }
        start = next;
    }
    None
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[tokio::test]
async fn config_api_hides_secrets_and_checks_permissions() {
    let fm = server(StorageKind::Memory).await;
    let pem_key = {
        let doc = ring::signature::EcdsaKeyPair::generate_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING,
            &ring::rand::SystemRandom::new(),
        )
        .unwrap();
        format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            B64.encode(doc.as_ref())
        )
    };
    let body = json!({
        "webhooks": [
            {"id": "sg", "provider": "sendgrid", "kind": "events", "url": "https://hooks.example.com/sg", "events": ["delivered"]},
            {"id": "sg-own", "provider": "sendgrid", "kind": "inbound", "url": "https://hooks.example.com/parse", "signingKey": pem_key, "raw": true},
            {"id": "mg", "provider": "mailgun", "kind": "events", "url": "https://hooks.example.com/mg", "signingKey": "mailgun-secret-key-value"},
            {"id": "pm", "provider": "postmark", "kind": "events", "url": "https://hookuser:hookpassword@hooks.example.com/pm"},
            {"id": "ses", "provider": "ses", "kind": "events", "url": "https://hooks.example.com/ses", "topicArn": "arn:aws:sns:eu-west-1:123456789012:mail"}
        ],
        "apiKeys": {"postmark": [fastmock_server::mail_webhooks::key_digest("pm-token")]}
    });
    let r = admin(&fm, "PUT", "/mail/webhooks?space=billing", body).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let get = admin_as(
        &fm,
        VIEWER,
        "GET",
        "/mail/webhooks?space=billing",
        Value::Null,
    )
    .await;
    assert_eq!(get.status, 200);
    let text = String::from_utf8_lossy(&get.body).to_string();
    for secret in [
        "mailgun-secret-key-value",
        "hookpassword",
        "PRIVATE KEY",
        "sendgridKey",
        "signingKey\"",
    ] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    let v = get.json();
    assert_eq!(v["space"], "billing");
    let hooks = v["webhooks"].as_array().unwrap();
    assert_eq!(hooks.len(), 5);
    assert_eq!(hooks[3]["url"], "https://hookuser:***@hooks.example.com/pm");
    assert_eq!(hooks[2]["hasSigningKey"], true);
    assert_eq!(hooks[0]["hasSigningKey"], false);
    let space_key = v["sendgridPublicKey"].as_str().unwrap();
    assert_eq!(hooks[0]["publicKey"], space_key);
    assert_ne!(hooks[1]["publicKey"].as_str().unwrap(), space_key);
    assert_eq!(
        hooks[4]["topicArn"],
        "arn:aws:sns:eu-west-1:123456789012:mail"
    );
    assert_eq!(
        v["apiKeys"]["postmark"][0],
        fastmock_server::mail_webhooks::key_digest("pm-token")
    );

    let r = admin_as(
        &fm,
        VIEWER,
        "PUT",
        "/mail/webhooks?space=billing",
        json!({}),
    )
    .await;
    assert_eq!(r.status, 403);
    let r = admin_as(
        &fm,
        VIEWER,
        "POST",
        "/mail/inbound?space=billing",
        json!({"provider": "sendgrid", "raw": "x"}),
    )
    .await;
    assert_eq!(r.status, 403);
    let r = admin(&fm, "PUT", "/mail/webhooks?space=billing", json!({"webhooks": [{"provider": "sendgrid", "kind": "events", "url": "ftp://hooks.example.com/"}]})).await;
    assert_eq!(r.status, 400);
    let r = admin(&fm, "PUT", "/mail/webhooks?space=nowhere", json!({})).await;
    assert_eq!(r.status, 404);

    let keep = json!({"webhooks": [
        {"id": "mg", "provider": "mailgun", "kind": "events", "url": "https://hooks.example.com/mg"},
        {"id": "pm", "provider": "postmark", "kind": "events", "url": "https://hookuser:***@hooks.example.com/pm"}
    ]});
    let r = admin(&fm, "PUT", "/mail/webhooks?space=billing", keep).await;
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v["webhooks"][0]["hasSigningKey"], true);
    assert_eq!(v["sendgridPublicKey"].as_str().unwrap(), space_key);
    assert!(v["apiKeys"].as_object().unwrap().is_empty());

    let r = admin(&fm, "DELETE", "/mail/webhooks?space=billing", Value::Null).await;
    assert_eq!(r.status, 204);
    let v = admin(&fm, "GET", "/mail/webhooks?space=billing", Value::Null)
        .await
        .json();
    assert!(v["webhooks"].as_array().unwrap().is_empty());
    assert!(v.get("sendgridPublicKey").is_none());
    let audit = admin(&fm, "GET", "/audit?limit=20", Value::Null).await;
    let audit_text = String::from_utf8_lossy(&audit.body);
    assert!(audit_text.contains("mail.webhooks.set") && audit_text.contains("mail.webhooks.clear"));
    assert!(
        !audit_text.contains("mailgun-secret-key-value") && !audit_text.contains("hookpassword")
    );
}

#[tokio::test]
async fn sendgrid_events_are_signed_with_the_space_key() {
    for storage in storages().await {
        let fm = server(storage).await;
        let (base, rx) = Receiver::start().await;
        let r = admin(
            &fm,
            "PUT",
            "/mail/webhooks?space=billing",
            json!({"webhooks": [{"id": "sg", "provider": "sendgrid", "kind": "events", "url": format!("{base}/sg")}]}),
        )
        .await;
        assert_eq!(r.status, 200);
        let public = r.json()["sendgridPublicKey"].as_str().unwrap().to_string();
        let mail = json!({
            "personalizations": [{"to": [{"email": "ann@billing.example.test"}, {"email": "bounce+42@billing.example.test"}], "custom_args": {"order": "42"}}],
            "from": {"email": "noreply@example.com"}, "subject": "s", "content": [{"type": "text/plain", "value": "x"}],
            "categories": ["receipts"]
        });
        let res = send(
            &fm.data,
            "POST",
            "/__mail/sendgrid/v3/mail/send",
            &[
                ("authorization", "Bearer SG.k"),
                ("content-type", "application/json"),
            ],
            mail.to_string(),
        )
        .await;
        assert_eq!(res.status, 202);
        let xid = res
            .headers
            .get("x-message-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let got = rx.wait(1).await;
        let g = &got[0];
        assert_eq!(g.path, "/sg");
        assert_eq!(g.h("content-type"), "application/json");
        verify_sendgrid(
            &public,
            g.h("x-twilio-email-event-webhook-timestamp"),
            &g.body,
            g.h("x-twilio-email-event-webhook-signature"),
        );
        let events = g.json();
        let events = events.as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["event"], "delivered");
        assert_eq!(events[0]["email"], "ann@billing.example.test");
        assert_eq!(events[1]["event"], "bounce");
        assert_eq!(events[1]["type"], "bounce");
        for e in events {
            assert!(e["sg_message_id"].as_str().unwrap().starts_with(&xid));
            assert_eq!(e["order"], "42");
            assert_eq!(e["category"], "receipts");
            assert!(e["timestamp"].is_i64());
            assert!(e["sg_event_id"].is_string());
        }
        let cbs = admin(
            &fm,
            "GET",
            "/callbacks?source=mail&space=billing",
            Value::Null,
        )
        .await
        .json();
        assert!(!cbs["callbacks"].as_array().unwrap().is_empty());
    }
}

#[tokio::test]
async fn mailgun_postmark_and_ses_events() {
    let fm = server(StorageKind::Memory).await;
    let (base, rx) = Receiver::start().await;
    let auth_base = base.replace("http://", "http://hook%40user:p%40ss@");
    let r = admin(
        &fm,
        "PUT",
        "/mail/webhooks",
        json!({"webhooks": [
            {"id": "mg", "provider": "mailgun", "kind": "events", "url": format!("{base}/mg"), "signingKey": "mg-signing-key"},
            {"id": "pm", "provider": "postmark", "kind": "events", "url": format!("{auth_base}/pm"), "events": ["delivered", "complaint"]},
            {"id": "ses", "provider": "ses", "kind": "events", "url": format!("{base}/ses")}
        ]}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));

    let auth = format!("Basic {}", B64.encode("api:k"));
    let form = "from=shop%40mg.example.com&to=deferred%40example.test&subject=Hi&text=x&o%3Atag=t1&v%3Aorder=7";
    let res = send(
        &fm.data,
        "POST",
        "/__mail/mailgun/v3/mg.example.com/messages",
        &[
            ("authorization", &auth),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        form,
    )
    .await;
    assert_eq!(res.status, 200);
    let mg_id = res.json()["id"]
        .as_str()
        .unwrap()
        .trim_matches(['<', '>'])
        .to_string();
    rx.wait(1).await;
    let g = &rx.for_path("/mg")[0];
    let v = g.json();
    let sig = &v["signature"];
    assert_eq!(
        sig["signature"].as_str().unwrap(),
        mailgun_hmac(
            "mg-signing-key",
            sig["timestamp"].as_str().unwrap(),
            sig["token"].as_str().unwrap()
        )
    );
    let ed = &v["event-data"];
    assert_eq!(
        (ed["event"].as_str(), ed["severity"].as_str()),
        (Some("failed"), Some("temporary"))
    );
    assert_eq!(ed["recipient"], "deferred@example.test");
    assert_eq!(ed["tags"], json!(["t1"]));
    assert_eq!(ed["user-variables"]["order"], "7");
    assert_eq!(ed["message"]["headers"]["message-id"], mg_id.as_str());

    rx.statuses.lock().unwrap().push_back(503);
    let pm = json!({"From": "a@example.com", "To": "ok@example.test, complaint@example.test, bounce@example.test", "TextBody": "x", "Tag": "welcome"}).to_string();
    let res = send(
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
    assert_eq!(res.status, 200);
    let pm_id = res.json()["MessageID"].as_str().unwrap().to_string();
    for _ in 0..100 {
        if rx.for_path("/pm").len() >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let calls = rx.for_path("/pm");
    assert_eq!(
        calls.len(),
        3,
        "one retried delivery and one complaint; bounce is not selected"
    );
    for c in &calls {
        assert_eq!(
            c.h("authorization"),
            format!("Basic {}", B64.encode("hook@user:p@ss"))
        );
    }
    let attempts: Vec<&str> = calls
        .iter()
        .map(|c| c.h("x-fastmock-delivery-attempt"))
        .collect();
    assert!(attempts.contains(&"2"), "{attempts:?}");
    let kinds: Vec<String> = calls
        .iter()
        .map(|c| c.json()["RecordType"].as_str().unwrap().to_string())
        .collect();
    assert!(
        kinds.contains(&"Delivery".to_string()) && kinds.contains(&"SpamComplaint".to_string()),
        "{kinds:?}"
    );
    let delivery = calls
        .iter()
        .map(Got::json)
        .find(|v| v["RecordType"] == "Delivery")
        .unwrap();
    assert_eq!(delivery["MessageID"], pm_id.as_str());
    assert_eq!(delivery["Recipient"], "ok@example.test");
    assert_eq!(delivery["Tag"], "welcome");

    let ses = json!({"FromEmailAddress": "a@example.com", "Destination": {"ToAddresses": ["one@example.test", "bounce@example.test"]}, "Content": {"Simple": {"Subject": {"Data": "SES"}}}}).to_string();
    let sigv4 = "AWS4-HMAC-SHA256 Credential=AKID/20261121/us-east-1/ses/aws4_request, SignedHeaders=host, Signature=00ff";
    let res = send(
        &fm.data,
        "POST",
        "/__mail/ses/v2/email/outbound-emails",
        &[("authorization", sigv4)],
        ses,
    )
    .await;
    assert_eq!(res.status, 200);
    let ses_id = res.json()["MessageId"].as_str().unwrap().to_string();
    for _ in 0..100 {
        if rx.for_path("/ses").len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let calls = rx.for_path("/ses");
    assert_eq!(calls.len(), 2);
    let mut types = Vec::new();
    for c in &calls {
        assert_eq!(c.h("x-amz-sns-message-type"), "Notification");
        assert_eq!(c.h("content-type"), "text/plain; charset=UTF-8");
        assert_eq!(
            c.h("user-agent"),
            "Amazon Simple Notification Service Agent"
        );
        let env = c.json();
        assert_eq!(env["Type"], "Notification");
        assert_eq!(
            env["TopicArn"],
            fastmock_server::mail_webhooks::DEFAULT_TOPIC_ARN
        );
        let msg: Value = serde_json::from_str(env["Message"].as_str().unwrap()).unwrap();
        assert_eq!(msg["mail"]["messageId"], ses_id.as_str());
        types.push(msg["notificationType"].as_str().unwrap().to_string());
        if msg["notificationType"] == "Bounce" {
            assert_eq!(
                msg["bounce"]["bouncedRecipients"][0]["emailAddress"],
                "bounce@example.test"
            );
            assert_eq!(msg["bounce"]["bounceType"], "Permanent");
        } else {
            assert_eq!(msg["delivery"]["recipients"], json!(["one@example.test"]));
        }
    }
    types.sort();
    assert_eq!(types, vec!["Bounce", "Delivery"]);

    let before = rx.got.lock().unwrap().len();
    let pm = json!({"From": "a@example.com", "To": "ok@example.test", "TextBody": "x"}).to_string();
    let res = send(
        &fm.data,
        "POST",
        "/__mail/postmark/email",
        &[
            ("x-postmark-server-token", "POSTMARK_API_TEST"),
            ("content-type", "application/json"),
        ],
        pm,
    )
    .await;
    assert_eq!(res.status, 200);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        rx.got.lock().unwrap().len(),
        before,
        "test mode sends no events"
    );
}

#[tokio::test]
async fn inbound_payloads_per_provider() {
    let fm = server(StorageKind::Memory).await;
    let (base, rx) = Receiver::start().await;
    let r = admin(
        &fm,
        "PUT",
        "/mail/webhooks?space=billing",
        json!({"webhooks": [
            {"id": "sg", "provider": "sendgrid", "kind": "inbound", "url": format!("{base}/parse")},
            {"id": "sg-raw", "provider": "sendgrid", "kind": "inbound", "url": format!("{base}/parse-raw"), "raw": true},
            {"id": "mg", "provider": "mailgun", "kind": "inbound", "url": format!("{base}/mg"), "signingKey": "route-key"},
            {"id": "mg-json", "provider": "mailgun", "kind": "inbound", "url": format!("{base}/mg/json"), "signingKey": "route-key"},
            {"id": "pm", "provider": "postmark", "kind": "inbound", "url": format!("{base}/pm")},
            {"id": "ses", "provider": "ses", "kind": "inbound", "url": format!("{base}/ses")}
        ]}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let public = r.json()["sendgridPublicKey"].as_str().unwrap().to_string();
    let message = json!({
        "from": "Customer <customer@example.com>",
        "to": ["support+case7@billing.example.test"],
        "subject": "Need help",
        "text": "Hello support",
        "html": "<p>Hello support</p>",
        "attachments": [{"name": "log.bin", "contentType": "application/octet-stream", "base64": B64.encode([0u8, 159, 146, 150, 255])}]
    });
    let r = admin(
        &fm,
        "POST",
        "/mail/inbound?space=billing",
        json!({"provider": "sendgrid", "message": message}),
    )
    .await;
    assert_eq!(r.status, 202, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["deliveries"].as_array().unwrap().len(), 2);
    rx.wait(2).await;
    let g = &rx.for_path("/parse")[0];
    assert!(
        g.h("content-type")
            .starts_with("multipart/form-data; boundary=")
    );
    verify_sendgrid(
        &public,
        g.h("x-twilio-email-event-webhook-timestamp"),
        &g.body,
        g.h("x-twilio-email-event-webhook-signature"),
    );
    assert_eq!(form_field(g, "subject").unwrap(), b"Need help");
    assert_eq!(
        form_field(g, "attachment1").unwrap(),
        vec![0u8, 159, 146, 150, 255]
    );
    assert_eq!(form_field(g, "attachments").unwrap(), b"1");
    let env: Value = serde_json::from_slice(&form_field(g, "envelope").unwrap()).unwrap();
    assert_eq!(
        env,
        json!({"to": ["support+case7@billing.example.test"], "from": "customer@example.com"})
    );
    let raw = &rx.for_path("/parse-raw")[0];
    assert!(
        String::from_utf8_lossy(&form_field(raw, "email").unwrap()).contains("Subject: Need help")
    );
    assert!(form_field(raw, "text").is_none());

    let plain = json!({"from": "customer@example.com", "to": ["support@billing.example.test"], "subject": "Route", "text": "Body"});
    let r = admin(
        &fm,
        "POST",
        "/mail/inbound?space=billing",
        json!({"provider": "mailgun", "message": plain}),
    )
    .await;
    assert_eq!(r.status, 202);
    rx.wait(4).await;
    let g = &rx.for_path("/mg")[0];
    assert_eq!(g.h("content-type"), "application/x-www-form-urlencoded");
    let f: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(&g.body).into_owned().collect();
    assert_eq!(f["recipient"], "support@billing.example.test");
    assert_eq!(f["subject"], "Route");
    assert_eq!(f["body-plain"].trim(), "Body");
    assert_eq!(
        f["signature"],
        mailgun_hmac("route-key", &f["timestamp"], &f["token"])
    );
    let j = &rx.for_path("/mg/json")[0];
    let v = j.json();
    assert_eq!(v["subject"], "Route");
    assert_eq!(
        j.h("x-mailgun-signature"),
        mailgun_hmac(
            "route-key",
            j.h("x-mailgun-timestamp"),
            v["token"].as_str().unwrap()
        )
    );

    let raw_mime = "From: Ann <ann@example.com>\r\nTo: inbox+tag@billing.example.test\r\nSubject: Raw inbound\r\nMessage-ID: <raw-1@example.com>\r\n\r\nraw text\r\n";
    let r = send(
        &fm.admin,
        "POST",
        "/__admin/mail/inbound?space=billing&provider=postmark",
        &[
            ("authorization", &format!("Bearer {ADMIN}")),
            ("content-type", "message/rfc822"),
        ],
        raw_mime,
    )
    .await;
    assert_eq!(r.status, 202, "{}", String::from_utf8_lossy(&r.body));
    rx.wait(5).await;
    let v = rx.for_path("/pm")[0].json();
    assert_eq!(v["From"], "ann@example.com");
    assert_eq!(v["FromName"], "Ann");
    assert_eq!(v["Subject"], "Raw inbound");
    assert_eq!(v["MailboxHash"], "tag");
    assert_eq!(v["OriginalRecipient"], "inbox+tag@billing.example.test");
    assert_eq!(v["TextBody"].as_str().unwrap().trim(), "raw text");
    assert_eq!(v["MessageStream"], "inbound");

    let r = admin(
        &fm,
        "POST",
        "/mail/inbound?space=billing",
        json!({"provider": "ses", "raw": raw_mime}),
    )
    .await;
    assert_eq!(r.status, 202);
    rx.wait(6).await;
    let env = rx.for_path("/ses")[0].json();
    let msg: Value = serde_json::from_str(env["Message"].as_str().unwrap()).unwrap();
    assert_eq!(msg["notificationType"], "Received");
    assert_eq!(msg["receipt"]["action"]["type"], "SNS");
    assert_eq!(msg["receipt"]["action"]["encoding"], "UTF8");
    assert_eq!(msg["content"], raw_mime);
    assert_eq!(msg["mail"]["commonHeaders"]["subject"], "Raw inbound");

    let r = admin(
        &fm,
        "POST",
        "/mail/inbound?space=billing",
        json!({"provider": "ses", "webhookId": "nope", "raw": raw_mime}),
    )
    .await;
    assert_eq!(r.status, 404);
    let r = admin(
        &fm,
        "POST",
        "/mail/inbound",
        json!({"provider": "postmark", "raw": raw_mime}),
    )
    .await;
    assert_eq!(r.status, 404);
    let r = admin(
        &fm,
        "POST",
        "/mail/inbound?space=billing",
        json!({"provider": "resend", "raw": raw_mime}),
    )
    .await;
    assert_eq!(r.status, 400);
    let r = admin(
        &fm,
        "POST",
        "/mail/inbound?space=billing",
        json!({"provider": "postmark", "raw": raw_mime, "rawBase64": "eA=="}),
    )
    .await;
    assert_eq!(r.status, 400);
}
