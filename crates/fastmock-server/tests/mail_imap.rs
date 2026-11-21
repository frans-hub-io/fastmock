use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, header};
use fastmock_core::mail::{Envelope, MailSource, MailboxKey};
use fastmock_mail::config::MailTlsFiles;
use fastmock_mail::{Incoming, MailConfig};
use fastmock_server::auth::hash_token;
use fastmock_server::config::{Config, StorageKind};
use fastmock_server::mail_listeners::{self, MailListeners};
use fastmock_server::state::AppState;
use serde_json::{Value, json};
use sqlx::Connection;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufStream,
};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tower::ServiceExt;

const ADMIN: &str = "fm_admin_token_for_mail_imap_tests_00001";
const BOX: &str = "alice@billing.example.test";
const PW_BOX: &str = "pay@payments.example.test";

const ACCESS: &str = r#"
spaces:
  - name: billing
    mailDomains: [billing.example.test]
  - name: payments
    mailDomains: [payments.example.test]
"#;

fn prom() -> metrics_exporter_prometheus::PrometheusHandle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    static H: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    H.get_or_init(|| fastmock_server::telemetry::prometheus().unwrap())
        .clone()
}

async fn postgres(prefix: &str) -> Option<StorageKind> {
    let (Ok(admin_url), Ok(valkey_url)) = (
        std::env::var("FASTMOCK_TEST_DATABASE_URL"),
        std::env::var("FASTMOCK_TEST_VALKEY_URL"),
    ) else {
        return None;
    };
    let db = format!("fm_{prefix}_{}", uuid::Uuid::new_v4().simple());
    let mut c = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut c)
        .await
        .unwrap();
    Some(StorageKind::Postgres {
        database_url: format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db),
        valkey_url,
        valkey_prefix: format!("{prefix}:{}:", uuid::Uuid::new_v4().simple()),
        max_connections: 5,
        options: Default::default(),
    })
}

async fn storages() -> Vec<StorageKind> {
    let mut v = vec![StorageKind::Memory];
    v.extend(postgres("imap").await);
    v
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fm-mail-imap-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn cert(dir: &Path) -> (PathBuf, PathBuf) {
    let (c, k) = (dir.join("mail.pem"), dir.join("mail-key.pem"));
    if !c.exists() {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert =
            rcgen::CertificateParams::new(vec!["mail.example.test".into(), "127.0.0.1".into()])
                .unwrap()
                .self_signed(&key)
                .unwrap();
        std::fs::write(&c, cert.pem()).unwrap();
        std::fs::write(&k, key.serialize_pem()).unwrap();
    }
    (c, k)
}

fn mail_config(dir: &Path) -> MailConfig {
    let mut m = MailConfig {
        hostname: "mail.example.test".into(),
        ..Default::default()
    };
    let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
    m.listeners.smtp = Some(any);
    m.listeners.imap = Some(any);
    m.listeners.imaps = Some(any);
    let (cert_file, key_file) = cert(dir);
    m.tls = Some(MailTlsFiles {
        cert_file,
        key_file,
        from_server: false,
    });
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
    async fn start(storage: StorageKind, edit: impl FnOnce(&mut MailConfig)) -> Server {
        Self::start_in(storage, temp_dir(), edit).await
    }

    async fn start_in(
        storage: StorageKind,
        dir: PathBuf,
        edit: impl FnOnce(&mut MailConfig),
    ) -> Server {
        let mut m = mail_config(&dir);
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

    fn imap(&self) -> SocketAddr {
        self.addr("FASTMOCK_MAIL_IMAP_LISTEN")
    }

    fn imaps(&self) -> SocketAddr {
        self.addr("FASTMOCK_MAIL_IMAPS_LISTEN")
    }

    fn smtp(&self) -> SocketAddr {
        self.addr("FASTMOCK_MAIL_SMTP_LISTEN")
    }

    fn ca(&self) -> String {
        self.dir.join("mail.pem").display().to_string()
    }

    async fn token(&self, name: &str, body: Value) -> String {
        let mut body = body;
        body["name"] = json!(name);
        let res = self
            .admin
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/__admin/tokens")
                    .header(header::AUTHORIZATION, format!("Bearer {ADMIN}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let st = res.status().as_u16();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(st, 201, "{v}");
        v["token"].as_str().unwrap().to_string()
    }

    async fn deliver(&self, to: &str, subject: &str) {
        let raw = format!(
            "From: Shop <shop@example.com>\r\nTo: {to}\r\nSubject: {subject}\r\n\r\nHello {subject}\r\n"
        );
        self.app
            .mail
            .clone()
            .unwrap()
            .accept(Incoming::new(
                MailSource::Smtp,
                Envelope {
                    mail_from: "shop@example.com".into(),
                    rcpt_to: vec![to.into()],
                    ..Default::default()
                },
                raw.into_bytes(),
            ))
            .await
            .unwrap();
    }

    async fn set_password(&self, space: &str, address: &str, password: &str) {
        let hash = fastmock_mail::mailbox_login::hash_password_with(password, 1000);
        self.app
            .mail
            .clone()
            .unwrap()
            .store
            .set_mailbox_password(Some(space), address, Some(&hash))
            .await
            .unwrap();
    }

    async fn stop(mut self) {
        let _ = self.shutdown.send(true);
        if let Some(m) = self.mail.take() {
            tokio::time::timeout(Duration::from_secs(30), m.wait())
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
        let mut out = Vec::new();
        loop {
            let mut l = Vec::new();
            let n = tokio::time::timeout(Duration::from_secs(15), self.s.read_until(b'\n', &mut l))
                .await
                .expect("a line in time")
                .unwrap();
            assert!(
                n > 0,
                "connection closed after {:?}",
                String::from_utf8_lossy(&out)
            );
            out.extend_from_slice(&l);
            let t = l.strip_suffix(b"\r\n").unwrap_or(&l);
            let lit = t
                .strip_suffix(b"}")
                .and_then(|t| t.iter().rposition(|b| *b == b'{').map(|p| &t[p + 1..]))
                .and_then(|d| std::str::from_utf8(d).ok()?.parse::<usize>().ok());
            match lit {
                Some(len) => {
                    let mut buf = vec![0; len];
                    self.s.read_exact(&mut buf).await.unwrap();
                    out.extend_from_slice(&buf);
                }
                None => break,
            }
        }
        String::from_utf8_lossy(&out).trim_end().to_string()
    }

    async fn cmd(&mut self, tag: &str, line: &str) -> (Vec<String>, String) {
        self.send(format!("{tag} {line}\r\n").as_bytes()).await;
        let mut lines = vec![];
        loop {
            let l = self.line().await;
            if l.starts_with(&format!("{tag} ")) {
                return (lines, l);
            }
            lines.push(l);
        }
    }

    async fn ok(&mut self, tag: &str, line: &str) -> Vec<String> {
        let (u, t) = self.cmd(tag, line).await;
        assert!(t.starts_with(&format!("{tag} OK")), "{line}: {t} {u:?}");
        u
    }

    async fn closed(&mut self) -> bool {
        let mut l = String::new();
        matches!(
            tokio::time::timeout(Duration::from_secs(15), self.s.read_line(&mut l)).await,
            Ok(Ok(0)) | Ok(Err(_))
        )
    }
}

async fn plain_login(addr: SocketAddr, user: &str, password: &str) -> Client<TcpStream> {
    let mut c = tcp(addr).await;
    let g = c.line().await;
    assert!(g.starts_with("* OK [CAPABILITY IMAP4rev1"), "{g}");
    c.ok("l", &format!("LOGIN {user} {password}")).await;
    c
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
async fn login_with_tokens_and_passwords_over_the_listeners() {
    for storage in storages().await {
        let s = Server::start(storage, |m| m.allow_plaintext_auth = true).await;
        let billing = s
            .token(
                "imap-billing",
                json!({"role": "viewer", "spaces": ["billing"]}),
            )
            .await;
        let global = s
            .token("imap-global", json!({"role": "viewer", "global": true}))
            .await;
        s.deliver(BOX, "hello").await;
        s.set_password("payments", PW_BOX, "s3cret pass").await;

        let mut c = plain_login(s.imap(), "Alice+x@Billing.Example.Test", &billing).await;
        let u = c.ok("a", "SELECT INBOX").await;
        assert!(u.contains(&"* 1 EXISTS".to_string()), "{u:?}");
        let u = c
            .ok("b", "FETCH 1 (BODY.PEEK[HEADER.FIELDS (SUBJECT)])")
            .await;
        assert!(u[0].contains("Subject: hello"), "{u:?}");
        c.ok("c", "LOGOUT").await;

        let mut c = tcp(s.imap()).await;
        c.line().await;
        let (_, t) = c.cmd("a", &format!("LOGIN {PW_BOX} {billing}")).await;
        assert_eq!(
            t, "a NO [AUTHENTICATIONFAILED] Authentication failed",
            "a token for billing cannot open a payments mailbox"
        );
        c.ok("b", &format!("LOGIN {PW_BOX} \"s3cret pass\"")).await;
        let mut c = plain_login(s.imap(), PW_BOX, &global).await;
        c.ok("a", "SELECT INBOX").await;
        let mut c = plain_login(s.imap(), "ops@example.org", &global).await;
        c.ok("a", "SELECT INBOX").await;
        let mut c = tcp(s.imap()).await;
        c.line().await;
        let (_, t) = c
            .cmd("a", &format!("LOGIN ops@example.org {billing}"))
            .await;
        assert!(t.starts_with("a NO"), "{t}");
        s.stop().await;
    }
}

const PYTHON: &str = r#"
import imaplib, ssl, sys
mode, port, tls_port, ca, user, password = sys.argv[1:7]
ctx = ssl.create_default_context(cafile=ca)
ctx.check_hostname = False
if mode == "starttls":
    M = imaplib.IMAP4("127.0.0.1", int(port))
    assert "LOGINDISABLED" in M.capabilities, M.capabilities
    M.starttls(ssl_context=ctx)
    assert "AUTH=PLAIN" in M.capabilities, M.capabilities
else:
    M = imaplib.IMAP4_SSL("127.0.0.1", int(tls_port), ssl_context=ctx)
M.login(user, password)
typ, data = M.select("INBOX")
assert typ == "OK" and data == [b"2"], data
typ, data = M.search(None, "UNSEEN")
assert data == [b"1 2"], data
typ, data = M.fetch("1", "(BODY.PEEK[])")
assert b"Subject: first" in data[0][1], data
typ, data = M.search(None, "UNSEEN")
assert data == [b"1 2"], ("peek keeps unseen", data)
typ, data = M.fetch("2", "(RFC822)")
assert b"Hello second" in data[0][1], data
assert b"\\Seen" in data[0][0] or any(b"\\Seen" in x for x in data if isinstance(x, bytes)), data
typ, data = M.search(None, "UNSEEN")
assert data == [b"1"], data
M.store("1", "+FLAGS", "\\Seen")
typ, data = M.search(None, "SEEN")
assert data == [b"1 2"], data
typ, data = M.uid("SEARCH", None, "SUBJECT", "second")
uid = data[0]
M.uid("STORE", uid, "+FLAGS", "(\\Deleted)")
typ, data = M.expunge()
assert data == [b"2"], data
typ, data = M.fetch("1:*", "(UID FLAGS ENVELOPE BODYSTRUCTURE)")
assert len([x for x in data if x]) == 1, data
assert b"ENVELOPE" in data[0] and b"BODYSTRUCTURE" in data[0], data
typ, data = M.list()
assert data == [b'() "/" "INBOX"'], data
M.create("Archive")
typ, data = M.uid("COPY", "1:*", "Archive")
assert typ == "OK", data
typ, data = M.status("Archive", "(MESSAGES UIDNEXT)")
assert b"MESSAGES 1" in data[0], data
M.close()
M.logout()
print("ok")
"#;

#[tokio::test]
async fn python_imaplib_as_a_client() {
    if !have("python3") {
        eprintln!("python3 not found; skipping");
        return;
    }
    let s = Server::start(StorageKind::Memory, |_| {}).await;
    let billing = s
        .token(
            "py-billing",
            json!({"role": "viewer", "spaces": ["billing"]}),
        )
        .await;
    s.set_password("payments", PW_BOX, "pw-secret").await;
    for (mode, user, password) in [
        ("starttls", BOX, billing.as_str()),
        ("imaps", PW_BOX, "pw-secret"),
    ] {
        s.deliver(user, "first").await;
        s.deliver(user, "second").await;
        let (ok, out) = run(
            "python3",
            &[
                "-I".into(),
                "-c".into(),
                PYTHON.into(),
                mode.into(),
                s.imap().port().to_string(),
                s.imaps().port().to_string(),
                s.ca(),
                user.into(),
                password.into(),
            ],
        )
        .await;
        assert!(ok, "{mode}: {out}");
        assert!(out.contains("ok"), "{out}");
    }
    s.stop().await;
}

const PYTHON_IDLE: &str = r#"
import imaplib, ssl, sys
tls_port, ca, user, password = sys.argv[1:5]
ctx = ssl.create_default_context(cafile=ca)
ctx.check_hostname = False
M = imaplib.IMAP4_SSL("127.0.0.1", int(tls_port), ssl_context=ctx)
M.login(user, password)
M.select("INBOX")
tag = M._new_tag()
M.send(tag + b" IDLE\r\n")
assert M.readline().startswith(b"+"), "continuation"
print("IDLING", flush=True)
seen = []
while len(seen) < 2:
    line = M.readline()
    if line.endswith(b"EXISTS\r\n"):
        seen.append(line.strip().decode())
        print(line.strip().decode(), flush=True)
M.send(b"DONE\r\n")
line = M.readline()
assert line.startswith(tag + b" OK"), line
typ, data = M.search(None, "ALL")
print("SEARCH", data[0].decode(), flush=True)
M.logout()
"#;

async fn next(out: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>) -> String {
    tokio::time::timeout(Duration::from_secs(20), out.next_line())
        .await
        .expect("python output in time")
        .unwrap()
        .unwrap_or_default()
}

#[tokio::test]
async fn python_idle_sees_injected_and_smtp_messages() {
    if !have("python3") {
        eprintln!("python3 not found; skipping");
        return;
    }
    let s = Server::start(StorageKind::Memory, |_| {}).await;
    let billing = s
        .token("py-idle", json!({"role": "viewer", "spaces": ["billing"]}))
        .await;
    let mut child = tokio::process::Command::new("python3")
        .args([
            "-I".to_string(),
            "-c".into(),
            PYTHON_IDLE.into(),
            s.imaps().port().to_string(),
            s.ca(),
            BOX.into(),
            billing,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap()).lines();
    assert_eq!(next(&mut out).await, "IDLING");
    s.deliver(BOX, "injected").await;
    assert_eq!(next(&mut out).await, "* 1 EXISTS");
    let mut smtp = tcp(s.smtp()).await;
    smtp.line().await;
    for l in [
        "EHLO client.example.test",
        "MAIL FROM:<app@example.com>",
        "RCPT TO:<alice+idle@billing.example.test>",
        "DATA",
    ] {
        smtp.send(format!("{l}\r\n").as_bytes()).await;
        loop {
            let r = smtp.line().await;
            if r.as_bytes().get(3) != Some(&b'-') {
                break;
            }
        }
    }
    smtp.send(b"Subject: via smtp\r\n\r\nbody\r\n.\r\n").await;
    assert!(smtp.line().await.starts_with("250"));
    assert_eq!(next(&mut out).await, "* 2 EXISTS");
    assert_eq!(next(&mut out).await, "SEARCH 1 2");
    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    s.stop().await;
}

#[tokio::test]
async fn curl_lists_and_fetches() {
    if !have("curl") {
        eprintln!("curl not found; skipping");
        return;
    }
    let s = Server::start(StorageKind::Memory, |_| {}).await;
    let billing = s
        .token(
            "curl-billing",
            json!({"role": "viewer", "spaces": ["billing"]}),
        )
        .await;
    s.deliver(BOX, "curl one").await;
    let user = format!("{BOX}:{billing}");
    let tls = |port: u16| -> Vec<String> {
        vec![
            "-sS".into(),
            "--max-time".into(),
            "20".into(),
            "--resolve".into(),
            format!("mail.example.test:{port}:127.0.0.1"),
            "--cacert".into(),
            s.ca(),
            "--user".into(),
            user.clone(),
        ]
    };
    let port = s.imap().port();
    let mut args = tls(port);
    args.extend([
        "--ssl-reqd".into(),
        format!("imap://mail.example.test:{port}/"),
    ]);
    let (ok, out) = run("curl", &args).await;
    assert!(ok, "{out}");
    assert!(out.contains("* LIST () \"/\" \"INBOX\""), "{out}");
    let mut args = tls(port);
    args.extend([
        "--ssl-reqd".into(),
        format!("imap://mail.example.test:{port}/INBOX;UID=1"),
    ]);
    let (ok, out) = run("curl", &args).await;
    assert!(ok, "{out}");
    assert!(
        out.contains("Subject: curl one") && out.contains("Hello curl one"),
        "{out}"
    );
    let tport = s.imaps().port();
    let mut args = tls(tport);
    args.push(format!(
        "imaps://mail.example.test:{tport}/INBOX;UID=1;SECTION=TEXT"
    ));
    let (ok, out) = run("curl", &args).await;
    assert!(ok, "{out}");
    assert_eq!(out.trim_end(), "Hello curl one");
    let mut args = tls(port);
    args.push(format!("imap://mail.example.test:{port}/"));
    let (ok, out) = run("curl", &args).await;
    assert!(!ok, "plain LOGIN is disabled before STARTTLS: {out}");
    s.stop().await;
}

#[tokio::test]
async fn starttls_refuses_pipelined_commands_over_tcp() {
    let s = Server::start(StorageKind::Memory, |_| {}).await;
    let mut c = tcp(s.imap()).await;
    let g = c.line().await;
    assert!(g.contains("STARTTLS LOGINDISABLED"), "{g}");
    c.send(b"a STARTTLS\r\nb CAPABILITY\r\n").await;
    assert_eq!(c.line().await, "a OK Begin TLS negotiation now");
    assert!(c.closed().await);
    s.stop().await;
}

#[tokio::test]
async fn connection_limits_are_shared_with_smtp() {
    let s = Server::start(StorageKind::Memory, |m| m.max_connections_per_ip = 2).await;
    let mut a = tcp(s.smtp()).await;
    a.line().await;
    let mut b = tcp(s.imap()).await;
    b.line().await;
    let mut c = tcp(s.imap()).await;
    assert_eq!(
        c.line().await,
        "* BYE Too many connections, try again later"
    );
    assert!(c.closed().await);
    s.stop().await;
}

#[tokio::test]
async fn shutdown_ends_idle_sessions() {
    let s = Server::start(StorageKind::Memory, |m| m.allow_plaintext_auth = true).await;
    let billing = s
        .token(
            "idle-stop",
            json!({"role": "viewer", "spaces": ["billing"]}),
        )
        .await;
    let mut c = plain_login(s.imap(), BOX, &billing).await;
    c.ok("a", "SELECT INBOX").await;
    c.send(b"b IDLE\r\n").await;
    assert_eq!(c.line().await, "+ idling");
    s.shutdown.send(true).unwrap();
    assert_eq!(c.line().await, "* BYE Server shutting down");
    assert!(c.closed().await);
    s.stop().await;
}

/// Two replicas sharing PostgreSQL and Valkey: IDLE on one sees a message delivered on the other, and
/// concurrent flag changes from both end up merged.
#[tokio::test]
async fn replicas_share_mailboxes_idle_and_flags() {
    let Some(storage) = postgres("imaprep").await else {
        eprintln!("FASTMOCK_TEST_DATABASE_URL/FASTMOCK_TEST_VALKEY_URL not set: skipped");
        return;
    };
    let dir = temp_dir();
    let a = Server::start_in(storage.clone(), dir.clone(), |m| {
        m.allow_plaintext_auth = true;
    })
    .await;
    let b = Server::start_in(storage, temp_dir(), |m| m.allow_plaintext_auth = true).await;
    let token = a
        .token("replicas", json!({"role": "viewer", "spaces": ["billing"]}))
        .await;
    a.deliver(BOX, "first").await;
    let mut on_b = plain_login(b.imap(), BOX, &token).await;
    let u = on_b.ok("a", "SELECT INBOX").await;
    assert!(u.contains(&"* 1 EXISTS".to_string()), "{u:?}");
    on_b.send(b"b IDLE\r\n").await;
    assert_eq!(on_b.line().await, "+ idling");
    a.deliver("Alice+x@billing.example.test", "second").await;
    assert_eq!(on_b.line().await, "* 2 EXISTS");
    on_b.send(b"DONE\r\n").await;
    assert_eq!(on_b.line().await, "b OK IDLE terminated");

    let mut on_a = plain_login(a.imap(), BOX, &token).await;
    on_a.ok("a", "SELECT INBOX").await;
    let flags_a = ["\\Seen", "\\Flagged", "$a1", "$a2", "$a3"];
    let flags_b = ["\\Answered", "\\Draft", "$b1", "$b2", "$b3"];
    let ta = tokio::spawn(async move {
        for (i, f) in flags_a.iter().enumerate() {
            on_a.ok(&format!("s{i}"), &format!("STORE 1:2 +FLAGS.SILENT ({f})"))
                .await;
        }
        on_a
    });
    let tb = tokio::spawn(async move {
        for (i, f) in flags_b.iter().enumerate() {
            on_b.ok(
                &format!("s{i}"),
                &format!("UID STORE 1:* +FLAGS.SILENT ({f})"),
            )
            .await;
        }
        on_b
    });
    let mut on_a = ta.await.unwrap();
    let mut on_b = tb.await.unwrap();
    let want = "FLAGS ($a1 $a2 $a3 $b1 $b2 $b3 \\Answered \\Draft \\Flagged \\Seen)";
    for c in [&mut on_a, &mut on_b] {
        let u = c.ok("f", "FETCH 1:2 FLAGS").await;
        let flags: Vec<&String> = u.iter().filter(|l| l.contains("FETCH (FLAGS")).collect();
        assert!(flags.len() >= 2, "{u:?}");
        for l in &flags[flags.len() - 2..] {
            assert!(l.ends_with(&format!("({want})")), "{l}");
        }
    }
    let svc = b.app.mail.clone().unwrap();
    let entries = svc
        .store
        .mailbox_entries(&MailboxKey::inbox(Some("billing"), BOX))
        .await
        .unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|e| e.flags.len() == 10), "{entries:?}");
    a.stop().await;
    b.stop().await;
    let _ = std::fs::remove_dir_all(dir);
}
