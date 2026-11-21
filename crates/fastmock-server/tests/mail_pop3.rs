use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, header};
use fastmock_core::mail::{MailMessage, MailQuery, MailboxKey};
use fastmock_core::model::SpaceFilter;
use fastmock_mail::MailConfig;
use fastmock_mail::config::MailTlsFiles;
use fastmock_mail::pop3::hash_password_with;
use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use fastmock_server::mail_listeners::{self, MailListeners};
use fastmock_server::state::AppState;
use serde_json::{Value, json};
use sqlx::Connection;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufStream};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tower::ServiceExt;

const ADMIN: &str = "fm_admin_token_for_mail_pop3_tests_00001";
const ALICE: &str = "alice@billing.example.test";
const CAROL: &str = "carol@crm.example.test";
const DAVE: &str = "dave@elsewhere.example";

const ACCESS: &str = r#"
spaces:
  - name: billing
    mailDomains: [billing.example.test]
  - name: crm
    mailDomains: [crm.example.test]
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
    let db = format!("fm_pop3_{}", uuid::Uuid::new_v4().simple());
    let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut c)
        .await
        .unwrap();
    Some(StorageKind::Postgres {
        database_url: format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db),
        valkey_url,
        valkey_prefix: format!("pop3:{}:", uuid::Uuid::new_v4().simple()),
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
    let dir = std::env::temp_dir().join(format!("fm-mail-pop3-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

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

struct Server {
    app: AppState,
    admin: axum::Router,
    mail: Option<MailListeners>,
    shutdown: watch::Sender<bool>,
    dir: PathBuf,
}

impl Server {
    /// SMTP, POP3 and POP3S on free ports with a certificate (so STLS is offered).
    async fn start(storage: StorageKind, edit: impl FnOnce(&mut MailConfig)) -> Server {
        let dir = temp_dir();
        let (cert_file, key_file) = cert(&dir);
        let mut m = MailConfig {
            hostname: "mail.example.test".into(),
            tls: Some(MailTlsFiles {
                cert_file,
                key_file,
                from_server: false,
            }),
            ..Default::default()
        };
        m.listeners.smtp = Some("127.0.0.1:0".parse().unwrap());
        m.listeners.pop3 = Some("127.0.0.1:0".parse().unwrap());
        m.listeners.pop3s = Some("127.0.0.1:0".parse().unwrap());
        edit(&mut m);
        let access = dir.join("access.yaml");
        std::fs::write(&access, ACCESS).unwrap();
        let mut cfg = Config::for_tests(storage, vec![format!("ops:admin:{}", hash_token(ADMIN))]);
        cfg.plugins = Some(vec![]);
        cfg.access_file = Some(access);
        cfg.mail = Some(m);
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

    fn addr(&self, var: &str) -> SocketAddr {
        self.mail.as_ref().unwrap().addr(var).unwrap()
    }

    fn smtp(&self) -> SocketAddr {
        self.addr("FASTMOCK_MAIL_SMTP_LISTEN")
    }

    fn pop3(&self) -> SocketAddr {
        self.addr("FASTMOCK_MAIL_POP3_LISTEN")
    }

    fn pop3s(&self) -> SocketAddr {
        self.addr("FASTMOCK_MAIL_POP3S_LISTEN")
    }

    fn ca(&self) -> String {
        self.dir.join("mail.pem").display().to_string()
    }

    async fn token(&self, body: Value) -> String {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/__admin/tokens")
            .header(header::AUTHORIZATION, format!("Bearer {ADMIN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = self.admin.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status().as_u16(), 201);
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        v["token"].as_str().unwrap().to_string()
    }

    async fn set_password(&self, area: Option<&str>, address: &str, pw: &str) {
        self.app
            .mail
            .as_ref()
            .unwrap()
            .store
            .set_mailbox_password(area, address, Some(&hash_password_with(pw, 1000)))
            .await
            .unwrap();
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

    async fn inbox(&self, area: Option<&str>, address: &str) -> Vec<u32> {
        self.app
            .mail
            .as_ref()
            .unwrap()
            .store
            .mailbox_entries(&MailboxKey::inbox(area, address))
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.uid)
            .collect()
    }

    /// Sends a message over SMTP.
    async fn send(&self, to: &str, subject: &str, body: &str) {
        let mut c = tcp(self.smtp()).await;
        assert!(c.line().await.starts_with("220"));
        c.send(b"EHLO client.example.test\r\n").await;
        loop {
            let l = c.line().await;
            if l.starts_with("250 ") {
                break;
            }
        }
        for line in [
            "MAIL FROM:<shop@example.com>".to_string(),
            format!("RCPT TO:<{to}>"),
        ] {
            assert!(c.cmd(&line).await.starts_with("250"), "{line}");
        }
        assert!(c.cmd("DATA").await.starts_with("354"));
        let stuffed: String = body
            .split("\r\n")
            .map(|l| {
                if l.starts_with('.') {
                    format!(".{l}\r\n")
                } else {
                    format!("{l}\r\n")
                }
            })
            .collect();
        c.send(
            format!(
                "From: shop@example.com\r\nTo: {to}\r\nSubject: {subject}\r\n\r\n{stuffed}.\r\n"
            )
            .as_bytes(),
        )
        .await;
        assert!(c.line().await.starts_with("250"));
        c.cmd("QUIT").await;
    }

    async fn stop(mut self) {
        let _ = self.shutdown.send(true);
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

impl<S: AsyncRead + AsyncWrite + Unpin> Client<S> {
    async fn send(&mut self, b: &[u8]) {
        self.s.write_all(b).await.unwrap();
        self.s.flush().await.unwrap();
    }

    async fn line(&mut self) -> String {
        let mut l = String::new();
        let n = tokio::time::timeout(Duration::from_secs(10), self.s.read_line(&mut l))
            .await
            .expect("reply in time")
            .unwrap();
        assert!(n > 0, "connection closed");
        l.trim_end_matches(['\r', '\n']).to_string()
    }

    async fn cmd(&mut self, c: &str) -> String {
        self.send(format!("{c}\r\n").as_bytes()).await;
        self.line().await
    }

    async fn ok(&mut self, c: &str) -> String {
        let r = self.cmd(c).await;
        assert!(r.starts_with("+OK"), "{c}: {r}");
        r
    }

    async fn body(&mut self) -> Vec<String> {
        let mut v = vec![];
        loop {
            let l = self.line().await;
            if l == "." {
                return v;
            }
            v.push(l);
        }
    }

    async fn closed(&mut self) -> bool {
        let mut l = vec![];
        matches!(
            tokio::time::timeout(Duration::from_secs(10), self.s.read_to_end(&mut l)).await,
            Ok(Ok(0)) | Ok(Err(_))
        )
    }
}

/// A POP3 client after the greeting, logged in when `login` is given.
async fn pop3(addr: SocketAddr, login: Option<(&str, &str)>) -> Client<TcpStream> {
    let mut c = tcp(addr).await;
    assert_eq!(
        c.line().await,
        "+OK mail.example.test FastMock POP3 server ready"
    );
    if let Some((u, p)) = login {
        c.ok(&format!("USER {u}")).await;
        let r = c.cmd(&format!("PASS {p}")).await;
        assert!(r.starts_with("+OK Logged in"), "{u}: {r}");
    }
    c
}

#[tokio::test]
async fn logins_with_tokens_and_passwords_stay_in_their_space() {
    for storage in storages().await {
        let s = Server::start(storage, |m| m.allow_plaintext_auth = true).await;
        s.send(ALICE, "for alice", "hello alice").await;
        s.send("Alice+news@billing.example.test", "tagged", "tagged")
            .await;
        s.send(CAROL, "for carol", "hello carol").await;
        s.send(DAVE, "for dave", "hello dave").await;
        let billing = s
            .token(json!({"name": "pop-billing", "role": "viewer", "spaces": ["billing"]}))
            .await;
        let global = s
            .token(json!({"name": "pop-global", "role": "viewer", "global": true}))
            .await;
        s.set_password(Some("crm"), CAROL, "carol-pw").await;
        s.set_password(None, "bob@billing.example.test", "bob-pw")
            .await;

        let mut c = pop3(s.pop3(), Some((ALICE, &billing))).await;
        assert!(c.cmd("STAT").await.starts_with("+OK 2 "));
        c.ok("RETR 2").await;
        assert!(c.body().await.contains(&"Subject: tagged".to_string()));
        c.ok("QUIT").await;

        let mut c = pop3(s.pop3(), None).await;
        for (user, pw) in [
            (CAROL, billing.as_str()),
            (DAVE, billing.as_str()),
            ("bob@billing.example.test", "bob-pw"),
        ] {
            c.ok(&format!("USER {user}")).await;
            let r = c.cmd(&format!("PASS {pw}")).await;
            if r.starts_with("-ERR [AUTH] Too many") {
                break;
            }
            assert_eq!(r, "-ERR [AUTH] Invalid credentials", "{user}");
        }
        assert!(c.closed().await);

        for user in [ALICE, CAROL, DAVE] {
            let mut c = pop3(s.pop3(), Some((user, &global))).await;
            c.ok("STAT").await;
            c.ok("QUIT").await;
        }
        let mut c = pop3(s.pop3(), Some((CAROL, "carol-pw"))).await;
        c.ok("RETR 1").await;
        assert!(c.body().await.contains(&"hello carol".to_string()));
        c.ok("DELE 1").await;
        assert!(c.ok("QUIT").await.contains("1 messages deleted, 0 left"));
        assert!(s.inbox(Some("crm"), CAROL).await.is_empty());
        let kept = s.messages().await;
        assert_eq!(kept.len(), 4, "the admin API keeps deleted messages");
        let metrics = prom().render();
        assert!(metrics.contains("fastmock_mail_pop3_sessions_total"));
        s.stop().await;
    }
}

#[tokio::test]
async fn connection_limits_are_shared_with_smtp_and_shutdown_closes_sessions() {
    let s = Server::start(StorageKind::Memory, |m| {
        m.max_connections_per_ip = 1;
        m.allow_plaintext_auth = true;
    })
    .await;
    let mut smtp = tcp(s.smtp()).await;
    assert!(smtp.line().await.starts_with("220"));
    let mut c = tcp(s.pop3()).await;
    assert_eq!(
        c.line().await,
        "-ERR [SYS/TEMP] Too many connections, try again later"
    );
    assert!(c.closed().await);
    smtp.cmd("QUIT").await;
    assert!(smtp.closed().await);
    let mut c = pop3(s.pop3(), None).await;
    c.ok("CAPA").await;
    c.body().await;
    s.shutdown.send(true).unwrap();
    assert_eq!(
        c.line().await,
        "-ERR [SYS/TEMP] Server shutting down; closing connection"
    );
    assert!(c.closed().await);
    s.stop().await;
}

#[tokio::test]
async fn pop3_listeners_are_off_unless_configured() {
    let s = Server::start(StorageKind::Memory, |m| {
        m.listeners.pop3 = None;
        m.listeners.pop3s = None;
    })
    .await;
    let l = s.mail.as_ref().unwrap();
    assert!(l.addr("FASTMOCK_MAIL_POP3_LISTEN").is_none());
    assert!(l.addr("FASTMOCK_MAIL_POP3S_LISTEN").is_none());
    assert!(l.addr("FASTMOCK_MAIL_SMTP_LISTEN").is_some());
    s.stop().await;
}

#[tokio::test]
async fn stls_over_tcp_refuses_pipelined_plaintext() {
    let s = Server::start(StorageKind::Memory, |_| {}).await;
    let mut c = pop3(s.pop3(), None).await;
    assert_eq!(
        c.cmd(&format!("USER {ALICE}")).await,
        "-ERR [AUTH] Plaintext authentication is disabled on this connection; use STLS first"
    );
    c.send(b"STLS\r\nCAPA\r\n").await;
    assert_eq!(c.line().await, "+OK Begin TLS negotiation");
    assert!(c.closed().await);
    s.stop().await;
}

#[tokio::test]
async fn dele_on_one_replica_is_seen_by_the_other() {
    let Some(storage) = postgres().await else {
        eprintln!("FASTMOCK_TEST_DATABASE_URL / FASTMOCK_TEST_VALKEY_URL not set; skipping");
        return;
    };
    let a = Server::start(storage.clone(), |m| m.allow_plaintext_auth = true).await;
    let b = Server::start(storage, |m| m.allow_plaintext_auth = true).await;
    a.send(ALICE, "one", "first").await;
    b.send(ALICE, "two", "second").await;
    let token = a
        .token(json!({"name": "pop-replicas", "role": "viewer", "global": true}))
        .await;
    a.set_password(Some("billing"), ALICE, "pw").await;

    let mut cb = pop3(b.pop3(), Some((ALICE, "pw"))).await;
    cb.ok("UIDL").await;
    let ids_b = cb.body().await;
    assert_eq!(ids_b.len(), 2);
    cb.ok("QUIT").await;

    let mut ca = pop3(a.pop3(), Some((ALICE, &token))).await;
    ca.ok("UIDL").await;
    assert_eq!(ca.body().await, ids_b, "same unique ids on both replicas");
    ca.ok("DELE 1").await;
    let mut cb = pop3(b.pop3(), Some((ALICE, &token))).await;
    assert!(
        cb.cmd("STAT").await.starts_with("+OK 2 "),
        "not before QUIT"
    );
    cb.ok("QUIT").await;
    ca.ok("QUIT").await;

    let mut cb = pop3(b.pop3(), Some((ALICE, &token))).await;
    assert!(cb.cmd("STAT").await.starts_with("+OK 1 "));
    cb.ok("UIDL").await;
    let left = cb.body().await;
    assert_eq!(
        left[0].split(' ').nth(1),
        ids_b[1].split(' ').nth(1),
        "the remaining message keeps its unique id"
    );
    cb.ok("RETR 1").await;
    assert!(cb.body().await.contains(&"second".to_string()));
    cb.ok("QUIT").await;
    assert_eq!(b.messages().await.len(), 2);
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

const PYTHON: &str = r#"
import poplib, ssl, sys
mode, port, tls_port, ca, user, pw = sys.argv[1:7]
port, tls_port = int(port), int(tls_port)
ctx = ssl.create_default_context(cafile=ca)

def show(p):
    count, size = p.stat()
    print("STAT", count, size)
    print("LIST", [l.decode() for l in p.list()[1]])
    print("UIDL", [l.decode() for l in p.uidl()[1]])
    print("RETR", repr("\n".join(l.decode() for l in p.retr(1)[1])))
    print("TOP", repr("\n".join(l.decode() for l in p.top(1, 1)[1])))

if mode == "refused":
    p = poplib.POP3("127.0.0.1", port, timeout=10)
    caps = p.capa()
    assert "STLS" in caps and "USER" not in caps, caps
    try:
        p.user(user)
        print("accepted")
    except poplib.error_proto as e:
        print("refused", e)
    p.quit()
elif mode == "stls":
    p = poplib.POP3("127.0.0.1", port, timeout=10)
    p.stls(context=ctx)
    caps = p.capa()
    assert "USER" in caps and "STLS" not in caps, caps
    p.user(user)
    p.pass_(pw)
    show(p)
    p.dele(1)
    print("QUIT", p.quit().decode())
elif mode == "ssl":
    p = poplib.POP3_SSL("127.0.0.1", tls_port, context=ctx, timeout=10)
    p.user(user)
    p.pass_(pw)
    show(p)
    p.quit()
elif mode == "plain":
    p = poplib.POP3("127.0.0.1", port, timeout=10)
    p.user(user)
    p.pass_(pw)
    show(p)
    p.dele(1)
    p.rset()
    p.quit()
print("ok")
"#;

#[tokio::test]
async fn python_poplib_as_a_client() {
    if !have("python3") {
        eprintln!("python3 not found; skipping");
        return;
    }
    let s = Server::start(StorageKind::Memory, |_| {}).await;
    s.send(
        ALICE,
        "python one",
        "line one\r\n.leading dot\r\nline three",
    )
    .await;
    s.send(ALICE, "python two", "second").await;
    let token = s
        .token(json!({"name": "pop-python", "role": "viewer", "spaces": ["billing"]}))
        .await;
    s.set_password(Some("billing"), ALICE, "py-pw").await;
    let py = |mode: &str, s: &Server, pw: &str| {
        vec![
            "-I".to_string(),
            "-c".into(),
            PYTHON.into(),
            mode.into(),
            s.pop3().port().to_string(),
            s.pop3s().port().to_string(),
            s.ca(),
            ALICE.into(),
            pw.into(),
        ]
    };
    let (ok, out) = run("python3", &py("refused", &s, &token)).await;
    assert!(ok, "{out}");
    assert!(out.contains("refused b'-ERR [AUTH] Plaintext"), "{out}");

    let (ok, out) = run("python3", &py("ssl", &s, "py-pw")).await;
    assert!(ok, "{out}");
    assert!(out.contains("STAT 2 "), "{out}");
    assert!(
        out.contains("Subject: python one") && out.contains("\\n.leading dot\\n"),
        "dot-stuffing undone by poplib: {out}"
    );
    assert!(
        out.contains("TOP '") && out.contains("\\n\\nline one'"),
        "{out}"
    );

    let (ok, out) = run("python3", &py("stls", &s, &token)).await;
    assert!(ok, "{out}");
    assert!(out.contains("1 messages deleted, 1 left"), "{out}");
    assert_eq!(s.inbox(Some("billing"), ALICE).await, vec![2]);

    let p = Server::start(StorageKind::Memory, |m| m.allow_plaintext_auth = true).await;
    p.send(ALICE, "plain", "plain body").await;
    let token = p
        .token(json!({"name": "pop-python", "role": "viewer", "global": true}))
        .await;
    let (ok, out) = run("python3", &py("plain", &p, &token)).await;
    assert!(ok, "{out}");
    assert!(
        out.contains("STAT 1 ") && out.contains("plain body"),
        "{out}"
    );
    assert_eq!(p.inbox(Some("billing"), ALICE).await, vec![1], "RSET");
    s.stop().await;
    p.stop().await;
}

#[tokio::test]
async fn curl_as_a_client() {
    if !have("curl") {
        eprintln!("curl not found; skipping");
        return;
    }
    let s = Server::start(StorageKind::Memory, |m| m.allow_plaintext_auth = true).await;
    s.send(ALICE, "curl one", "first line\r\n.dot line").await;
    s.send(ALICE, "curl two", "second").await;
    let token = s
        .token(json!({"name": "pop-curl", "role": "viewer", "spaces": ["billing"]}))
        .await;
    let user = format!("{ALICE}:{token}");
    let base = |url: String| -> Vec<String> {
        vec![
            "-sS".into(),
            "--max-time".into(),
            "20".into(),
            url,
            "--user".into(),
            user.clone(),
        ]
    };
    let port = s.pop3().port();
    let (ok, out) = run("curl", &base(format!("pop3://127.0.0.1:{port}/"))).await;
    assert!(ok, "{out}");
    let sizes: Vec<&str> = out.lines().collect();
    assert_eq!(sizes.len(), 2, "{out}");
    assert!(
        sizes[0].starts_with("1 ") && sizes[1].starts_with("2 "),
        "{out}"
    );
    let (ok, out) = run("curl", &base(format!("pop3://127.0.0.1:{port}/1"))).await;
    assert!(ok, "{out}");
    assert!(out.contains("Subject: curl one"), "{out}");
    assert!(out.contains("first line\r\n.dot line"), "{out:?}");

    let mut args = base(format!("pop3://mail.example.test:{port}/2"));
    args.extend([
        "--resolve".into(),
        format!("mail.example.test:{port}:127.0.0.1"),
        "--ssl-reqd".into(),
        "--cacert".into(),
        s.ca(),
    ]);
    let (ok, out) = run("curl", &args).await;
    assert!(ok, "{out}");
    assert!(out.contains("Subject: curl two"), "{out}");
    let tls_port = s.pop3s().port();
    let mut args = base(format!("pop3s://mail.example.test:{tls_port}/"));
    args.extend([
        "--resolve".into(),
        format!("mail.example.test:{tls_port}:127.0.0.1"),
        "--cacert".into(),
        s.ca(),
    ]);
    let (ok, out) = run("curl", &args).await;
    assert!(ok, "{out}");
    assert_eq!(out.lines().count(), 2, "{out}");
    let mut args = base(format!("pop3://127.0.0.1:{port}/1"));
    args.extend(["--request".into(), "DELE".into(), "--head".into()]);
    let (ok, out) = run("curl", &args).await;
    assert!(ok, "{out}");
    assert_eq!(s.inbox(Some("billing"), ALICE).await, vec![2]);
    s.stop().await;
}
