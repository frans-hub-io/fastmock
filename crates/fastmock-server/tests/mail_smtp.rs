use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, header};
use fastmock_core::mail::{MailMessage, MailQuery};
use fastmock_core::model::SpaceFilter;
use fastmock_mail::MailConfig;
use fastmock_mail::config::MailTlsFiles;
use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use fastmock_server::mail_listeners::{self, MailListeners};
use fastmock_server::state::AppState;
use serde_json::{Value, json};
use sqlx::Connection;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufStream};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tower::ServiceExt;

const ADMIN: &str = "fm_admin_token_for_mail_smtp_tests_00001";
const VIEWER: &str = "fm_viewer_token_for_mail_smtp_tests_0001";

const ACCESS: &str = r#"
spaces:
  - name: billing
    mailDomains: [billing.example.test]
  - name: crm
    mailDomains: [crm.example.test]
  - name: ops
"#;

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

async fn postgres() -> Option<StorageKind> {
    let (Ok(admin_url), Ok(valkey_url)) = (
        std::env::var("FASTMOCK_TEST_DATABASE_URL"),
        std::env::var("FASTMOCK_TEST_VALKEY_URL"),
    ) else {
        return None;
    };
    let db = format!("fm_smtp_{}", uuid::Uuid::new_v4().simple());
    let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut c)
        .await
        .unwrap();
    Some(StorageKind::Postgres {
        database_url: format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db),
        valkey_url,
        valkey_prefix: format!("smtp:{}:", uuid::Uuid::new_v4().simple()),
        max_connections: 5,
        options: Default::default(),
    })
}

async fn storages() -> Vec<StorageKind> {
    let mut v = vec![StorageKind::Memory];
    v.extend(postgres().await);
    v
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fm-mail-smtp-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A self-signed certificate for `mail.example.test` and 127.0.0.1: (cert, key) paths.
fn cert(dir: &Path) -> (PathBuf, PathBuf) {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["mail.example.test".into(), "127.0.0.1".into()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let (c, k) = (dir.join("mail.pem"), dir.join("mail-key.pem"));
    std::fs::write(&c, cert.pem()).unwrap();
    std::fs::write(&k, key.serialize_pem()).unwrap();
    (c, k)
}

fn mail_config(dir: &Path, tls: bool) -> MailConfig {
    let mut m = MailConfig {
        hostname: "mail.example.test".into(),
        ..Default::default()
    };
    m.listeners.smtp = Some("127.0.0.1:0".parse().unwrap());
    if tls {
        let (cert_file, key_file) = cert(dir);
        m.tls = Some(MailTlsFiles {
            cert_file,
            key_file,
            from_server: false,
        });
        m.listeners.smtps = Some("127.0.0.1:0".parse().unwrap());
    }
    m
}

struct Server {
    app: AppState,
    admin: axum::Router,
    mail: Option<MailListeners>,
    shutdown: watch::Sender<bool>,
    dir: PathBuf,
}

impl Server {
    async fn start(storage: StorageKind, edit: impl FnOnce(&mut MailConfig, &Path)) -> Server {
        let dir = temp_dir();
        let mut m = mail_config(&dir, true);
        edit(&mut m, &dir);
        Self::start_with(storage, Some(m), dir).await
    }

    async fn start_with(storage: StorageKind, mail: Option<MailConfig>, dir: PathBuf) -> Server {
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
        cfg.mail = mail;
        let (app, _data, admin, _) = fastmock_server::build(cfg, prom()).await.unwrap();
        let (tx, rx) = watch::channel(false);
        let mail = mail_listeners::start(&app, rx).await.unwrap();
        Server {
            app,
            admin,
            mail,
            shutdown: tx,
            dir,
        }
    }

    fn smtp(&self) -> SocketAddr {
        self.mail
            .as_ref()
            .unwrap()
            .addr("FASTMOCK_MAIL_SMTP_LISTEN")
            .unwrap()
    }

    fn smtps(&self) -> SocketAddr {
        self.mail
            .as_ref()
            .unwrap()
            .addr("FASTMOCK_MAIL_SMTPS_LISTEN")
            .unwrap()
    }

    async fn call(&self, m: Method, path: &str, token: &str, body: Option<Value>) -> (u16, Value) {
        let mut rb = Request::builder()
            .method(m)
            .uri(format!("/__admin{path}"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        let body = match body {
            Some(b) => {
                rb = rb.header(header::CONTENT_TYPE, "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let res = self
            .admin
            .clone()
            .oneshot(rb.body(body).unwrap())
            .await
            .unwrap();
        let st = res.status().as_u16();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (st, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn messages(&self) -> Vec<MailMessage> {
        let svc = self.app.mail.clone().unwrap();
        let page = svc
            .store
            .list(&MailQuery {
                oldest_first: true,
                ..Default::default()
            })
            .await
            .unwrap();
        let mut out = vec![];
        for e in page.items {
            out.push(
                svc.store
                    .get(e.id, &SpaceFilter::Any)
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        out
    }

    async fn stop(mut self) {
        self.shutdown.send(true).unwrap();
        if let Some(m) = self.mail.take() {
            tokio::time::timeout(Duration::from_secs(10), m.wait())
                .await
                .expect("listeners stop");
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Client<S> {
    s: BufStream<S>,
}

async fn tcp(addr: SocketAddr) -> Client<TcpStream> {
    Client {
        s: BufStream::new(TcpStream::connect(addr).await.unwrap()),
    }
}

async fn tcp_from(local: &str, addr: SocketAddr) -> Client<TcpStream> {
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.bind(format!("{local}:0").parse().unwrap()).unwrap();
    Client {
        s: BufStream::new(sock.connect(addr).await.unwrap()),
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Client<S> {
    async fn send(&mut self, b: &[u8]) {
        self.s.write_all(b).await.unwrap();
        self.s.flush().await.unwrap();
    }

    async fn reply(&mut self) -> (u16, Vec<String>) {
        let mut lines = vec![];
        loop {
            let mut l = String::new();
            let n = tokio::time::timeout(Duration::from_secs(10), self.s.read_line(&mut l))
                .await
                .expect("reply in time")
                .unwrap();
            assert!(n > 0, "connection closed after {lines:?}");
            let l = l.trim_end().to_string();
            let more = l.as_bytes().get(3) == Some(&b'-');
            lines.push(l);
            if !more {
                break;
            }
        }
        (lines.last().unwrap()[..3].parse().unwrap(), lines)
    }

    async fn expect(&mut self, line: &str, code: u16) -> Vec<String> {
        self.send(format!("{line}\r\n").as_bytes()).await;
        let (c, l) = self.reply().await;
        assert_eq!(c, code, "{line}: {l:?}");
        l
    }

    async fn closed(&mut self) -> bool {
        let mut l = String::new();
        matches!(
            tokio::time::timeout(Duration::from_secs(10), self.s.read_line(&mut l)).await,
            Ok(Ok(0)) | Ok(Err(_))
        )
    }

    /// Sends one message and returns the reply code to the end of the data.
    async fn message(&mut self, from: &str, to: &[&str]) -> u16 {
        self.expect(&format!("MAIL FROM:<{from}>"), 250).await;
        for t in to {
            self.expect(&format!("RCPT TO:<{t}>"), 250).await;
        }
        self.expect("DATA", 354).await;
        self.send(b"Subject: test\r\n\r\nhello\r\n.\r\n").await;
        self.reply().await.0
    }
}

async fn greeted(addr: SocketAddr) -> Client<TcpStream> {
    let mut c = tcp(addr).await;
    assert_eq!(c.reply().await.0, 220);
    c.expect("EHLO client.example.test", 250).await;
    c
}

#[tokio::test]
async fn smtp_over_tcp_routes_and_records_the_session() {
    for storage in storages().await {
        let s = Server::start(storage, |_, _| {}).await;
        let mut c = tcp(s.smtp()).await;
        let (code, l) = c.reply().await;
        assert_eq!(
            (code, l[0].as_str()),
            (220, "220 mail.example.test ESMTP FastMock")
        );
        c.send(b"EHLO client.example.test\r\nMAIL FROM:<app@example.com>\r\nRCPT TO:<a@billing.example.test>\r\nRCPT TO:<b@crm.example.test>\r\nRCPT TO:<c@elsewhere.example>\r\nDATA\r\n").await;
        let (_, ehlo) = c.reply().await;
        assert!(ehlo.contains(&"250 STARTTLS".to_string()), "{ehlo:?}");
        for _ in 0..4 {
            assert_eq!(c.reply().await.0, 250);
        }
        assert_eq!(c.reply().await.0, 354);
        c.send(b"Subject: Routed\r\n\r\nbody\r\n.\r\nQUIT\r\n")
            .await;
        assert_eq!(c.reply().await.0, 250);
        assert_eq!(c.reply().await.0, 221);
        let m = s.messages().await;
        let spaces: Vec<Option<&str>> = m.iter().map(|m| m.space.as_deref()).collect();
        assert_eq!(spaces, vec![None, Some("billing"), Some("crm")]);
        assert_eq!(m[1].envelope.rcpt_to, vec!["a@billing.example.test"]);
        assert_eq!(m[1].envelope.client_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(m[1].envelope.helo.as_deref(), Some("client.example.test"));
        assert!(!m[1].envelope.tls);
        assert!(
            m[1].transcript
                .iter()
                .any(|l| l.text == "(message: 25 bytes)")
        );

        let mut c = tcp(s.smtp()).await;
        c.reply().await;
        c.expect("EHLO client.example.test", 250).await;
        assert_eq!(
            c.message("x@example.com", &["d@elsewhere.example"]).await,
            250
        );
        let m = s.messages().await;
        assert_eq!(m.last().unwrap().space, None);
        s.stop().await;
    }
}

#[tokio::test]
async fn auth_user_selects_the_space() {
    let s = Server::start(StorageKind::Memory, |m, _| m.allow_plaintext_auth = true).await;
    let mut c = greeted(s.smtp()).await;
    let plain = fastmock_mail::smtp::sasl::encode(b"\0ops\0any");
    c.expect(&format!("AUTH PLAIN {plain}"), 235).await;
    assert_eq!(
        c.message(
            "x@example.com",
            &["a@elsewhere.example", "b@crm.example.test"]
        )
        .await,
        250
    );
    let m = s.messages().await;
    let spaces: Vec<Option<&str>> = m.iter().map(|m| m.space.as_deref()).collect();
    assert_eq!(spaces, vec![Some("crm"), Some("ops")]);
    assert_eq!(m[1].envelope.auth_user.as_deref(), Some("ops"));
    s.stop().await;
}

#[tokio::test]
async fn connection_limits_global_and_per_ip() {
    let s = Server::start(StorageKind::Memory, |m, _| {
        m.max_connections = 3;
        m.max_connections_per_ip = 2;
    })
    .await;
    let addr = s.smtp();
    let mut a = tcp_from("127.0.0.1", addr).await;
    assert_eq!(a.reply().await.0, 220);
    let mut b = tcp_from("127.0.0.1", addr).await;
    assert_eq!(b.reply().await.0, 220);
    let mut over = tcp_from("127.0.0.1", addr).await;
    let (code, l) = over.reply().await;
    assert_eq!(
        (code, l[0].as_str()),
        (421, "421 4.7.0 Too many connections, try again later")
    );
    assert!(over.closed().await);
    let mut c = tcp_from("127.0.0.2", addr).await;
    assert_eq!(c.reply().await.0, 220);
    let mut d = tcp_from("127.0.0.3", addr).await;
    assert_eq!(d.reply().await.0, 421, "global limit");
    let tls = tokio::net::TcpStream::connect(s.smtps()).await.unwrap();
    let mut tls = Client {
        s: BufStream::new(tls),
    };
    assert!(
        tls.closed().await,
        "SMTPS over the limit closes without a reply"
    );
    a.expect("QUIT", 221).await;
    assert!(a.closed().await);
    let limits = s.mail.as_ref().unwrap().limits.clone();
    for _ in 0..50 {
        if limits.open() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut e = tcp_from("127.0.0.1", addr).await;
    assert_eq!(e.reply().await.0, 220);
    drop((b, c, e));
    s.stop().await;
}

#[tokio::test]
async fn idle_timeout_and_graceful_shutdown() {
    let s = Server::start(StorageKind::Memory, |m, _| {
        m.idle_timeout = Duration::from_millis(300);
    })
    .await;
    let mut c = tcp(s.smtp()).await;
    assert_eq!(c.reply().await.0, 220);
    let (code, l) = c.reply().await;
    assert_eq!(
        (code, l[0].as_str()),
        (
            421,
            "421 4.4.2 Timeout waiting for the client; closing connection"
        )
    );
    s.stop().await;

    let s = Server::start(StorageKind::Memory, |_, _| {}).await;
    let addr = s.smtp();
    let mut c = greeted(addr).await;
    s.shutdown.send(true).unwrap();
    let (code, l) = c.reply().await;
    assert_eq!(
        (code, l[0].as_str()),
        (421, "421 4.3.2 Service shutting down; closing connection")
    );
    let m = s.mail.unwrap();
    tokio::time::timeout(Duration::from_secs(10), m.wait())
        .await
        .unwrap();
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "no new connections"
    );
}

#[tokio::test]
async fn mail_listeners_are_off_without_the_mail_module() {
    let s = Server::start_with(StorageKind::Memory, None, temp_dir()).await;
    assert!(s.mail.is_none());
    let (st, v) = s.call(Method::GET, "/mail/chaos", ADMIN, None).await;
    assert_eq!(st, 404, "{v}");
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("FASTMOCK_MAIL_ENABLED")
    );
}

#[tokio::test]
async fn required_auth_with_users_and_managed_tokens() {
    for storage in storages().await {
        let s = Server::start(storage, |m, _| {
            m.smtp_require_auth = true;
            m.allow_plaintext_auth = true;
            m.smtp_users = fastmock_mail::config::parse_smtp_users(&format!(
                "app:{}",
                hash_token("app-secret")
            ))
            .unwrap();
        })
        .await;
        let create = |body: Value| {
            let s = &s;
            async move {
                let (st, v) = s.call(Method::POST, "/tokens", ADMIN, Some(body)).await;
                assert_eq!(st, 201, "{v}");
                v["token"].as_str().unwrap().to_string()
            }
        };
        let billing_writer =
            create(json!({"name": "smtp-billing", "role": "editor", "spaces": ["billing"]})).await;
        let global_writer =
            create(json!({"name": "smtp-global", "role": "editor", "global": true})).await;
        let viewer = create(json!({"name": "smtp-viewer", "role": "viewer", "global": true})).await;
        let enc =
            |u: &str, p: &str| fastmock_mail::smtp::sasl::encode(format!("\0{u}\0{p}").as_bytes());

        let mut c = greeted(s.smtp()).await;
        c.expect("MAIL FROM:<x@example.com>", 530).await;
        c.expect(&format!("AUTH PLAIN {}", enc("app", "app-secret")), 235)
            .await;
        assert_eq!(
            c.message("x@example.com", &["a@elsewhere.example"]).await,
            250
        );

        let mut c = greeted(s.smtp()).await;
        c.expect(
            &format!("AUTH PLAIN {}", enc("billing", &billing_writer)),
            235,
        )
        .await;
        assert_eq!(
            c.message("x@example.com", &["a@elsewhere.example"]).await,
            250
        );
        let mut c = greeted(s.smtp()).await;
        c.expect(&format!("AUTH PLAIN {}", enc("crm", &billing_writer)), 535)
            .await;
        c.expect(&format!("AUTH PLAIN {}", enc("anyone", &viewer)), 535)
            .await;
        c.expect(
            &format!("AUTH PLAIN {}", enc("anyone", &global_writer)),
            235,
        )
        .await;
        let mut c = greeted(s.smtp()).await;
        for _ in 0..2 {
            c.expect(&format!("AUTH PLAIN {}", enc("app", "wrong")), 535)
                .await;
        }
        c.expect(&format!("AUTH PLAIN {}", enc("app", "wrong")), 421)
            .await;
        assert!(c.closed().await);
        let m = s.messages().await;
        assert_eq!(m[1].space.as_deref(), Some("billing"));
        for msg in &m {
            let t = serde_json::to_string(&msg.transcript).unwrap();
            assert!(!t.contains("app-secret") && !t.contains(&billing_writer));
        }
        s.stop().await;
    }
}

#[tokio::test]
async fn chaos_api_permissions_validation_and_audit() {
    for storage in storages().await {
        let s = Server::start(storage, |_, _| {}).await;
        let (st, v) = s
            .call(Method::GET, "/mail/chaos?space=billing", VIEWER, None)
            .await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v, json!({"space": "billing", "rules": [], "counters": []}));
        let rules = json!({"rules": [
            {"stage": "rcpt", "match": {"rcptTo": {"contains": "slow"}}, "action": {"delay": "10ms"}},
            {"id": "third", "stage": "data", "every": 3, "action": {"reply": {"code": 451, "enhanced": "4.3.0", "text": "Try later"}}}
        ]});
        let (st, _) = s
            .call(
                Method::PUT,
                "/mail/chaos?space=billing",
                VIEWER,
                Some(rules.clone()),
            )
            .await;
        assert_eq!(st, 403);
        let (st, v) = s
            .call(
                Method::PUT,
                "/mail/chaos?space=billing",
                ADMIN,
                Some(rules.clone()),
            )
            .await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["rules"][0]["id"], "rule-1");
        assert_eq!(
            v["counters"],
            json!([{"id": "rule-1", "matched": 0, "fired": 0}, {"id": "third", "matched": 0, "fired": 0}])
        );
        let (st, v) = s.call(Method::PUT, "/mail/chaos?space=billing", ADMIN, Some(json!({"rules": [{"stage": "mail", "match": {"rcptTo": {"equalTo": "a"}}, "action": {"disconnect": true}}]}))).await;
        assert_eq!(st, 400);
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .contains("rules[0]: match.rcptTo does not apply at stage mail"),
            "{v}"
        );
        let (st, v) = s
            .call(
                Method::PUT,
                "/mail/chaos",
                ADMIN,
                Some(json!({"rules": [{"stage": "nope", "action": {}}]})),
            )
            .await;
        assert_eq!(st, 400);
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .contains("invalid mail chaos rules"),
            "{v}"
        );
        let (st, _) = s
            .call(Method::GET, "/mail/chaos?space=unknown", ADMIN, None)
            .await;
        assert_eq!(st, 404);
        let (st, v) = s.call(Method::GET, "/mail/chaos", ADMIN, None).await;
        assert_eq!((st, v["rules"].clone()), (200, json!([])));
        assert!(v.get("space").is_none());

        let mut c = greeted(s.smtp()).await;
        let mut codes = vec![];
        for _ in 0..3 {
            codes.push(
                c.message("x@example.com", &["a@billing.example.test"])
                    .await,
            );
        }
        assert_eq!(codes, vec![250, 250, 451]);
        let (_, v) = s
            .call(Method::GET, "/mail/chaos?space=billing", VIEWER, None)
            .await;
        assert_eq!(
            v["counters"][1],
            json!({"id": "third", "matched": 3, "fired": 1})
        );

        let (st, _) = s
            .call(Method::DELETE, "/mail/chaos?space=billing", VIEWER, None)
            .await;
        assert_eq!(st, 403);
        let (st, _) = s
            .call(Method::DELETE, "/mail/chaos?space=billing", ADMIN, None)
            .await;
        assert_eq!(st, 204);
        let (_, v) = s
            .call(Method::GET, "/mail/chaos?space=billing", VIEWER, None)
            .await;
        assert_eq!(v["rules"], json!([]));
        let (_, audit) = s.call(Method::GET, "/audit?limit=20", ADMIN, None).await;
        let actions: Vec<(String, String)> = audit["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["action"].as_str().unwrap_or("").starts_with("mail.chaos"))
            .map(|e| {
                (
                    e["action"].as_str().unwrap().to_string(),
                    e["space"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect();
        assert!(
            actions.contains(&("mail.chaos.set".into(), "billing".into())),
            "{audit}"
        );
        assert!(actions.contains(&("mail.chaos.clear".into(), "billing".into())));
        s.stop().await;
    }
}

#[tokio::test]
async fn chaos_counters_are_shared_between_replicas() {
    let Some(storage) = postgres().await else {
        eprintln!("FASTMOCK_TEST_DATABASE_URL / FASTMOCK_TEST_VALKEY_URL not set; skipping");
        return;
    };
    let a = Server::start(storage.clone(), |_, _| {}).await;
    let b = Server::start(storage, |_, _| {}).await;
    let (st, v) = a
        .call(
            Method::PUT,
            "/mail/chaos?space=crm",
            ADMIN,
            Some(json!({"rules": [
                {"id": "every-second", "stage": "data", "every": 2, "action": {"reply": {"code": 451, "enhanced": "4.3.0"}}},
                {"id": "once", "stage": "rcpt", "match": {"rcptTo": {"contains": "once@"}}, "times": 1, "action": {"reply": {"code": 550, "enhanced": "5.1.1"}}},
                {"id": "seeded", "stage": "rcpt", "match": {"rcptTo": {"contains": "seeded@"}}, "probability": 50, "seed": 7, "action": {"reply": {"code": 451}}}
            ]})),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    let mut ca = greeted(a.smtp()).await;
    let mut cb = greeted(b.smtp()).await;
    let mut codes = vec![];
    for i in 0..4 {
        let c = if i % 2 == 0 { &mut ca } else { &mut cb };
        codes.push(c.message("x@example.com", &["a@crm.example.test"]).await);
    }
    assert_eq!(codes, vec![250, 451, 250, 451]);
    ca.expect("MAIL FROM:<x@example.com>", 250).await;
    ca.expect("RCPT TO:<once@crm.example.test>", 550).await;
    cb.expect("MAIL FROM:<x@example.com>", 250).await;
    cb.expect("RCPT TO:<once@crm.example.test>", 250).await;
    ca.expect("RSET", 250).await;
    cb.expect("RSET", 250).await;
    let mut seeded = vec![];
    for i in 0..10 {
        let c = if i % 2 == 0 { &mut ca } else { &mut cb };
        c.expect("MAIL FROM:<x@example.com>", 250).await;
        c.send(b"RCPT TO:<seeded@crm.example.test>\r\n").await;
        let code = c.reply().await.0;
        c.expect("RSET", 250).await;
        seeded.push(code == 451);
    }
    let expected: Vec<bool> = (1..=10)
        .map(|n| fastmock_mail::chaos::seeded_fires(7, n, 50.0))
        .collect();
    assert_eq!(
        seeded, expected,
        "the seeded sequence continues across replicas"
    );
    let (_, v) = b
        .call(Method::GET, "/mail/chaos?space=crm", ADMIN, None)
        .await;
    assert_eq!(
        v["counters"],
        json!([
            {"id": "every-second", "matched": 4, "fired": 2},
            {"id": "once", "matched": 2, "fired": 2},
            {"id": "seeded", "matched": 10, "fired": expected.iter().filter(|x| **x).count()}
        ])
    );
    a.stop().await;
    b.stop().await;
}

fn have(cmd: &str) -> bool {
    std::process::Command::new(cmd)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

async fn run(cmd: &str, args: &[String]) -> (bool, String) {
    let out = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await
        .unwrap();
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[tokio::test]
async fn curl_as_a_client() {
    if !have("curl") {
        eprintln!("curl not found; skipping");
        return;
    }
    let s = Server::start(StorageKind::Memory, |_, _| {}).await;
    let ca = s.dir.join("mail.pem").display().to_string();
    let msg = s.dir.join("msg.eml");
    std::fs::write(&msg, "From: app@example.com\r\nTo: a@billing.example.test\r\nSubject: curl\r\n\r\nline one\r\n.dot line\r\n").unwrap();
    let msg = msg.display().to_string();
    let port = s.smtp().port();
    let tls_port = s.smtps().port();
    let base = |url: String| -> Vec<String> {
        vec![
            "-sS".into(),
            "--max-time".into(),
            "20".into(),
            url,
            "--mail-from".into(),
            "app@example.com".into(),
            "--mail-rcpt".into(),
            "a@billing.example.test".into(),
            "-T".into(),
            msg.clone(),
        ]
    };
    let (ok, out) = run(
        "curl",
        &base(format!("smtp://127.0.0.1:{port}/client.example.test")),
    )
    .await;
    assert!(ok, "{out}");
    let mut args = base(format!("smtp://mail.example.test:{port}"));
    args.extend([
        "--resolve".into(),
        format!("mail.example.test:{port}:127.0.0.1"),
        "--ssl-reqd".into(),
        "--cacert".into(),
        ca.clone(),
        "--user".into(),
        "billing:any-password".into(),
    ]);
    let (ok, out) = run("curl", &args).await;
    assert!(ok, "{out}");
    let mut args = base(format!("smtps://mail.example.test:{tls_port}"));
    args.extend([
        "--resolve".into(),
        format!("mail.example.test:{tls_port}:127.0.0.1"),
        "--cacert".into(),
        ca,
    ]);
    let (ok, out) = run("curl", &args).await;
    assert!(ok, "{out}");
    let m = s.messages().await;
    assert_eq!(m.len(), 3);
    for x in &m {
        assert_eq!(x.space.as_deref(), Some("billing"));
        assert_eq!(x.summary.subject.as_deref(), Some("curl"));
        assert!(x.raw.ends_with(b"line one\r\n.dot line\r\n"), "{:?}", x.raw);
    }
    assert!(!m[0].envelope.tls);
    assert!(m[1].envelope.tls);
    assert_eq!(m[1].envelope.auth_user.as_deref(), Some("billing"));
    assert!(m[2].envelope.tls);
    s.stop().await;
}

const PYTHON: &str = r#"
import smtplib, ssl, sys
from email.message import EmailMessage
mode, port, tls_port, ca = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
msg = EmailMessage()
msg["From"] = "app@example.com"
msg["To"] = "a@crm.example.test"
msg["Subject"] = "python " + mode
msg.set_content("hello from smtplib\n.leading dot\n")
ctx = ssl.create_default_context(cafile=ca)
if mode == "plain":
    with smtplib.SMTP("127.0.0.1", port, local_hostname="py.example.test") as s:
        s.send_message(msg)
elif mode == "starttls":
    with smtplib.SMTP("127.0.0.1", port, local_hostname="py.example.test") as s:
        s.ehlo()
        assert "auth" not in s.esmtp_features, s.esmtp_features
        s.starttls(context=ctx)
        s.ehlo()
        assert "auth" in s.esmtp_features, s.esmtp_features
        s.login("crm", "secret")
        s.send_message(msg)
elif mode == "smtps":
    with smtplib.SMTP_SSL("127.0.0.1", tls_port, context=ctx) as s:
        s.login("crm", "secret")
        s.send_message(msg)
elif mode == "utf8":
    msg.replace_header("To", "ünı@crm.example.test")
    with smtplib.SMTP("127.0.0.1", port) as s:
        s.send_message(msg, from_addr="jürgen@example.com", to_addrs=["ünı@crm.example.test"])
elif mode == "bigsize":
    with smtplib.SMTP("127.0.0.1", port) as s:
        s.ehlo()
        code, _ = s.mail("app@example.com", ["SIZE=999999999"])
        print(code)
print("ok")
"#;

#[tokio::test]
async fn python_smtplib_as_a_client() {
    if !have("python3") {
        eprintln!("python3 not found; skipping");
        return;
    }
    let s = Server::start(StorageKind::Memory, |_, _| {}).await;
    let ca = s.dir.join("mail.pem").display().to_string();
    for mode in ["plain", "starttls", "smtps", "utf8", "bigsize"] {
        let (ok, out) = run(
            "python3",
            &[
                "-I".into(),
                "-c".into(),
                PYTHON.into(),
                mode.into(),
                s.smtp().port().to_string(),
                s.smtps().port().to_string(),
                ca.clone(),
            ],
        )
        .await;
        assert!(ok, "{mode}: {out}");
        if mode == "bigsize" {
            assert!(out.contains("552"), "{out}");
        }
    }
    let m = s.messages().await;
    assert_eq!(m.len(), 4);
    let subjects: Vec<&str> = m
        .iter()
        .map(|m| m.summary.subject.as_deref().unwrap())
        .collect();
    assert_eq!(
        subjects,
        vec![
            "python plain",
            "python starttls",
            "python smtps",
            "python utf8"
        ]
    );
    assert_eq!(m[0].envelope.helo.as_deref(), Some("py.example.test"));
    assert!(!m[0].envelope.tls && m[1].envelope.tls && m[2].envelope.tls);
    assert_eq!(m[1].envelope.auth_user.as_deref(), Some("crm"));
    assert_eq!(m[3].envelope.mail_from, "jürgen@example.com");
    assert_eq!(m[3].envelope.rcpt_to, vec!["ünı@crm.example.test"]);
    assert!(
        String::from_utf8_lossy(&m[0].raw).contains("\n.leading dot"),
        "dot-stuffing removed"
    );
    let t = serde_json::to_string(&m[1].transcript).unwrap();
    assert!(!t.contains("secret"));
    assert!(t.contains("\"****\"") || t.contains(" ****"), "{t}");
    s.stop().await;
}

#[tokio::test]
async fn starttls_over_tcp_refuses_pipelined_plaintext() {
    let s = Server::start(StorageKind::Memory, |_, _| {}).await;
    let mut c = greeted(s.smtp()).await;
    c.send(b"STARTTLS\r\nRSET\r\n").await;
    assert_eq!(c.reply().await.0, 220);
    assert!(c.closed().await);
    let s2 = s;
    let mut c = greeted(s2.smtp()).await;
    c.expect("STARTTLS", 220).await;
    let raw = c.s.into_inner();
    let mut roots = rustls::RootCertStore::empty();
    let pem = std::fs::read(s2.dir.join("mail.pem")).unwrap();
    use rustls::pki_types::pem::PemObject;
    for cert in rustls::pki_types::CertificateDer::pem_slice_iter(&pem) {
        roots.add(cert.unwrap()).unwrap();
    }
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let tls = tokio_rustls::TlsConnector::from(Arc::new(cfg))
        .connect(
            rustls::pki_types::ServerName::try_from("mail.example.test").unwrap(),
            raw,
        )
        .await
        .unwrap();
    let mut c = Client {
        s: BufStream::new(tls),
    };
    c.expect("MAIL FROM:<x@example.com>", 503).await;
    c.expect("EHLO again.example.test", 250).await;
    c.expect("STARTTLS", 503).await;
    assert_eq!(
        c.message("x@example.com", &["a@crm.example.test"]).await,
        250
    );
    assert!(s2.messages().await[0].envelope.tls);
}
