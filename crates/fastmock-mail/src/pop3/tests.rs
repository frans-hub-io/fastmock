use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fastmock_core::mail::{Envelope, LocalMailEvents, MailSource, MailboxKey, MemoryMailStore};
use fastmock_core::model::SpaceFilter;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::*;
use crate::config::MailConfig;
use crate::routing::MailDomains;
use crate::service::Incoming;

const PEER: &str = "198.51.100.7:40000";
const ALICE: &str = "alice@billing.example.test";
const BILLING_TOKEN: &str = "fm_billing_reader";
const GLOBAL_TOKEN: &str = "fm_global_reader";

struct Tokens;

#[async_trait]
impl MailboxTokenCheck for Tokens {
    async fn can_read(&self, area: Option<&str>, token: &str) -> bool {
        match token {
            GLOBAL_TOKEN => true,
            BILLING_TOKEN => area == Some("billing"),
            _ => false,
        }
    }
}

struct Tls {
    server: Arc<rustls::ServerConfig>,
    client: Arc<rustls::ClientConfig>,
}

fn tls() -> Tls {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["mail.example.test".into()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let der: CertificateDer<'static> = cert.der().clone();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![der.clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(der).unwrap();
    let client = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Tls {
        server: Arc::new(server),
        client: Arc::new(client),
    }
}

struct Fixture {
    cfg: MailConfig,
    tls: Option<Tls>,
    svc: Option<Arc<MailService>>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            cfg: MailConfig {
                hostname: "mail.example.test".into(),
                ..Default::default()
            },
            tls: None,
            svc: None,
        }
    }

    fn with_tls(mut self) -> Self {
        self.tls = Some(tls());
        self
    }

    fn service(&mut self) -> Arc<MailService> {
        if let Some(s) = &self.svc {
            return s.clone();
        }
        let mut domains = MailDomains::default();
        domains.add("billing", "billing.example.test");
        domains.add("crm", "crm.example.test");
        let svc = Arc::new(MailService::new(
            Arc::new(self.cfg.clone()),
            Arc::new(MemoryMailStore::new(100)),
            Arc::new(LocalMailEvents::new(16)),
            domains,
            ["billing", "crm"].map(String::from).into(),
        ));
        self.svc = Some(svc.clone());
        svc
    }

    fn server(&mut self) -> Arc<Pop3Server> {
        let svc = self.service();
        let mut srv = Pop3Server::new(
            svc,
            self.tls.as_ref().map(|t| t.server.clone()),
            Some(Arc::new(Tokens)),
        );
        srv.failure_delay = Duration::from_millis(20);
        Arc::new(srv)
    }

    async fn deliver(&mut self, to: &str, raw: &str) {
        let svc = self.service();
        svc.accept(Incoming::new(
            MailSource::Smtp,
            Envelope {
                mail_from: "shop@example.com".into(),
                rcpt_to: vec![to.into()],
                ..Default::default()
            },
            raw.as_bytes().to_vec(),
        ))
        .await
        .unwrap();
    }

    async fn set_password(&mut self, area: Option<&str>, address: &str, pw: &str) {
        let svc = self.service();
        svc.store
            .set_mailbox_password(area, address, Some(&hash_password_with(pw, 1000)))
            .await
            .unwrap();
    }

    async fn inbox(&mut self, area: Option<&str>, address: &str) -> Vec<u32> {
        let svc = self.service();
        svc.store
            .mailbox_entries(&MailboxKey::inbox(area, address))
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.uid)
            .collect()
    }
}

fn msg(subject: &str) -> String {
    format!(
        "From: shop@example.com\r\nTo: {ALICE}\r\nSubject: {subject}\r\n\r\nHello {subject}\r\n"
    )
}

struct Client<S> {
    s: BufStream<S>,
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

    async fn fails(&mut self, c: &str) -> String {
        let r = self.cmd(c).await;
        assert!(r.starts_with("-ERR"), "{c}: {r}");
        r
    }

    /// The lines of a multi-line body up to the `.` line, as sent.
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

    /// The bytes of a multi-line body with the `.` line.
    async fn raw_body(&mut self) -> Vec<u8> {
        let mut out = vec![];
        loop {
            let mut l = vec![];
            self.s.read_until(b'\n', &mut l).await.unwrap();
            assert!(!l.is_empty(), "connection closed");
            out.extend_from_slice(&l);
            if l == b".\r\n" {
                return out;
            }
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

type Session = (Client<tokio::io::DuplexStream>, JoinHandle<Pop3Report>);

fn open_with(
    srv: &Arc<Pop3Server>,
    implicit_tls: bool,
    shutdown: Option<watch::Receiver<bool>>,
) -> Session {
    let (client, server) = tokio::io::duplex(1 << 20);
    let srv = srv.clone();
    let task = tokio::spawn(async move {
        srv.serve(server, PEER.parse().unwrap(), implicit_tls, shutdown)
            .await
    });
    (
        Client {
            s: BufStream::new(client),
        },
        task,
    )
}

async fn open(srv: &Arc<Pop3Server>) -> Session {
    let (mut c, t) = open_with(srv, false, None);
    let g = c.line().await;
    assert_eq!(g, "+OK mail.example.test FastMock POP3 server ready");
    (c, t)
}

async fn logged_in(srv: &Arc<Pop3Server>, user: &str, pw: &str) -> Session {
    let (mut c, t) = open(srv).await;
    c.ok(&format!("USER {user}")).await;
    c.ok(&format!("PASS {pw}")).await;
    (c, t)
}

async fn report(t: JoinHandle<Pop3Report>) -> Pop3Report {
    tokio::time::timeout(Duration::from_secs(10), t)
        .await
        .expect("session ends")
        .unwrap()
}

async fn upgrade<S: AsyncRead + AsyncWrite + Unpin>(
    c: Client<S>,
    client: Arc<rustls::ClientConfig>,
) -> Client<tokio_rustls::client::TlsStream<S>> {
    let tls = tokio_rustls::TlsConnector::from(client)
        .connect(
            ServerName::try_from("mail.example.test").unwrap(),
            c.s.into_inner(),
        )
        .await
        .unwrap();
    Client {
        s: BufStream::new(tls),
    }
}

#[tokio::test]
async fn greeting_capa_noop_and_quit_before_login() {
    let mut f = Fixture::new();
    let srv = f.server();
    let (mut c, t) = open(&srv).await;
    c.ok("CAPA").await;
    let caps = c.body().await;
    assert_eq!(
        caps,
        vec![
            "TOP",
            "UIDL",
            "RESP-CODES",
            "AUTH-RESP-CODE",
            "PIPELINING",
            "USER",
            "SASL PLAIN",
            "IMPLEMENTATION FastMock"
        ]
    );
    assert_eq!(c.cmd("NOOP").await, "+OK");
    assert_eq!(
        c.cmd("QUIT").await,
        "+OK mail.example.test FastMock POP3 server signing off"
    );
    assert!(c.closed().await);
    let r = report(t).await;
    assert_eq!(r.end, SessionEnd::Quit);
    assert_eq!(r.mailbox, None);
}

#[tokio::test]
async fn user_pass_with_the_mailbox_password_stat_list_uidl() {
    let mut f = Fixture::new();
    f.deliver("Alice+news@Billing.Example.test", &msg("one"))
        .await;
    f.deliver(ALICE, &msg("two")).await;
    f.deliver("bob@billing.example.test", &msg("bob")).await;
    f.set_password(Some("billing"), ALICE, "s3cret pw").await;
    let srv = f.server();
    let (mut c, t) = open(&srv).await;
    assert_eq!(
        c.cmd("user ALICE+x@billing.example.test").await,
        "+OK Send PASS"
    );
    let size1 = msg("one").len();
    let size2 = msg("two").len();
    assert_eq!(
        c.cmd("pass s3cret pw").await,
        format!("+OK Logged in, 2 messages ({} octets)", size1 + size2)
    );
    assert_eq!(c.cmd("STAT").await, format!("+OK 2 {}", size1 + size2));
    c.ok("LIST").await;
    assert_eq!(
        c.body().await,
        vec![format!("1 {size1}"), format!("2 {size2}")]
    );
    assert_eq!(c.cmd("LIST 2").await, format!("+OK 2 {size2}"));
    assert_eq!(
        c.cmd("LIST 3").await,
        "-ERR No such message, only 2 messages in maildrop"
    );
    assert_eq!(
        c.cmd("LIST 0").await,
        "-ERR No such message, only 2 messages in maildrop"
    );
    let info = f
        .service()
        .store
        .mailbox_info(&MailboxKey::inbox(Some("billing"), ALICE))
        .await
        .unwrap()
        .unwrap();
    let v = info.uid_validity;
    c.ok("UIDL").await;
    assert_eq!(c.body().await, vec![format!("1 {v}.1"), format!("2 {v}.2")]);
    assert_eq!(c.cmd("UIDL 2").await, format!("+OK 2 {v}.2"));
    c.fails("UIDL 9").await;
    c.ok("QUIT").await;
    let r = report(t).await;
    assert_eq!(r.mailbox.as_deref(), Some(ALICE));
    assert_eq!(r.deleted, 0);
}

#[tokio::test]
async fn managed_tokens_and_space_isolation() {
    let mut f = Fixture::new();
    f.deliver(ALICE, &msg("billing")).await;
    f.deliver("carol@crm.example.test", &msg("crm")).await;
    f.deliver("dave@elsewhere.example", &msg("global")).await;
    f.set_password(None, "bob@billing.example.test", "global-pw")
        .await;
    f.set_password(Some("crm"), "carol@crm.example.test", "crm-pw")
        .await;
    let srv = f.server();

    let (mut c, _) = logged_in(&srv, ALICE, BILLING_TOKEN).await;
    assert!(c.cmd("STAT").await.starts_with("+OK 1 "));
    c.ok("QUIT").await;

    let (mut c, _) = open(&srv).await;
    c.ok("USER carol@crm.example.test").await;
    assert_eq!(
        c.cmd(&format!("PASS {BILLING_TOKEN}")).await,
        "-ERR [AUTH] Invalid credentials"
    );
    c.ok("USER dave@elsewhere.example").await;
    c.fails(&format!("PASS {BILLING_TOKEN}")).await;

    for user in ["carol@crm.example.test", "dave@elsewhere.example", ALICE] {
        let (mut c, _) = logged_in(&srv, user, GLOBAL_TOKEN).await;
        assert!(c.cmd("STAT").await.starts_with("+OK 1 "), "{user}");
    }

    let (mut c, _) = open(&srv).await;
    c.ok("USER bob@billing.example.test").await;
    c.fails("PASS global-pw").await;
    c.ok("USER carol@crm.example.test").await;
    assert!(
        c.cmd("PASS crm-pw")
            .await
            .starts_with("+OK Logged in, 1 messages")
    );
    c.ok("RETR 1").await;
    let body = c.body().await;
    assert!(body.contains(&"Subject: crm".to_string()), "{body:?}");
}

#[tokio::test]
async fn retr_normalises_line_breaks_and_dot_stuffs() {
    let mut f = Fixture::new();
    let raw = "Subject: dots\n\n.\n..two\r\n.three\rend";
    f.deliver(ALICE, raw).await;
    let srv = f.server();
    let (mut c, _) = logged_in(&srv, ALICE, GLOBAL_TOKEN).await;
    assert_eq!(c.cmd("RETR 1").await, format!("+OK {} octets", raw.len()));
    let got = c.raw_body().await;
    assert_eq!(
        got,
        b"Subject: dots\r\n\r\n..\r\n...two\r\n..three\r\nend\r\n.\r\n".to_vec()
    );
    assert_eq!(
        render::unstuff(&got).unwrap(),
        b"Subject: dots\r\n\r\n.\r\n..two\r\n.three\r\nend\r\n".to_vec()
    );
    assert_eq!(c.cmd("TOP 1 0").await, "+OK Top of message follows");
    assert_eq!(c.body().await, vec!["Subject: dots", ""]);
    c.ok("TOP 1 2").await;
    assert_eq!(c.body().await, vec!["Subject: dots", "", "..", "...two"]);
    c.fails("TOP 2 0").await;
    c.fails("RETR 2").await;
    c.ok("QUIT").await;
}

#[tokio::test]
async fn dele_and_rset_change_nothing_until_quit() {
    let mut f = Fixture::new();
    for s in ["one", "two", "three"] {
        f.deliver(ALICE, &msg(s)).await;
    }
    let srv = f.server();
    let (mut c, t) = logged_in(&srv, ALICE, GLOBAL_TOKEN).await;
    c.ok("UIDL").await;
    let before = c.body().await;
    assert_eq!(c.cmd("DELE 1").await, "+OK Message 1 deleted");
    assert_eq!(c.cmd("DELE 1").await, "-ERR Message 1 already deleted");
    c.fails("RETR 1").await;
    c.fails("LIST 1").await;
    c.fails("UIDL 1").await;
    assert!(c.cmd("STAT").await.starts_with("+OK 2 "));
    c.ok("LIST").await;
    assert_eq!(c.body().await.len(), 2);
    let size = 3 * msg("one").len() + 4;
    assert_eq!(
        c.cmd("RSET").await,
        format!("+OK Maildrop has 3 messages ({size} octets)")
    );
    c.ok("RETR 1").await;
    c.body().await;
    c.ok("DELE 2").await;
    assert_eq!(f.inbox(Some("billing"), ALICE).await, vec![1, 2, 3]);
    assert_eq!(
        c.cmd("QUIT").await,
        "+OK mail.example.test FastMock POP3 server signing off (1 messages deleted, 2 left)"
    );
    let r = report(t).await;
    assert_eq!(r.deleted, 1);
    assert_eq!(f.inbox(Some("billing"), ALICE).await, vec![1, 3]);
    let svc = f.service();
    let entries = svc
        .store
        .mailbox_entries(&MailboxKey::inbox(Some("billing"), ALICE))
        .await
        .unwrap();
    assert!(entries[0].has_flag("\\Seen"), "RETR marks seen at QUIT");
    assert!(!entries[1].has_flag("\\Seen"));
    let all = svc
        .store
        .list(&fastmock_core::mail::MailQuery::default())
        .await
        .unwrap();
    assert_eq!(all.items.len(), 3, "the admin API keeps the message");

    let (mut c, _) = logged_in(&srv, ALICE, GLOBAL_TOKEN).await;
    c.ok("UIDL").await;
    let after = c.body().await;
    let ids = |v: &[String]| -> Vec<String> {
        v.iter()
            .map(|l| l.split(' ').nth(1).unwrap().to_string())
            .collect()
    };
    assert_eq!(
        ids(&after),
        vec![ids(&before)[0].clone(), ids(&before)[2].clone()]
    );
    assert_eq!(after[1].split(' ').next(), Some("2"), "renumbered");
}

#[tokio::test]
async fn a_dropped_connection_or_shutdown_removes_nothing() {
    let mut f = Fixture::new();
    f.deliver(ALICE, &msg("one")).await;
    let srv = f.server();
    let (mut c, t) = logged_in(&srv, ALICE, GLOBAL_TOKEN).await;
    c.ok("DELE 1").await;
    drop(c);
    assert_eq!(report(t).await.end, SessionEnd::Disconnected);
    assert_eq!(f.inbox(Some("billing"), ALICE).await, vec![1]);

    let (tx, rx) = watch::channel(false);
    let (mut c, t) = open_with(&srv, false, Some(rx));
    c.line().await;
    c.ok(&format!("USER {ALICE}")).await;
    c.ok(&format!("PASS {GLOBAL_TOKEN}")).await;
    c.ok("DELE 1").await;
    tx.send(true).unwrap();
    assert_eq!(
        c.line().await,
        "-ERR [SYS/TEMP] Server shutting down; closing connection"
    );
    assert!(c.closed().await);
    assert_eq!(report(t).await.end, SessionEnd::Shutdown);
    assert_eq!(f.inbox(Some("billing"), ALICE).await, vec![1]);
}

#[tokio::test]
async fn the_maildrop_is_a_snapshot_and_uidls_are_stable() {
    let mut f = Fixture::new();
    f.deliver(ALICE, &msg("one")).await;
    let srv = f.server();
    let (mut c, _) = logged_in(&srv, ALICE, GLOBAL_TOKEN).await;
    c.ok("UIDL").await;
    let first = c.body().await;
    f.deliver(ALICE, &msg("two")).await;
    assert!(c.cmd("STAT").await.starts_with("+OK 1 "));
    c.ok("QUIT").await;
    let (mut c, _) = logged_in(&srv, ALICE, GLOBAL_TOKEN).await;
    assert!(c.cmd("STAT").await.starts_with("+OK 2 "));
    c.ok("UIDL").await;
    let second = c.body().await;
    assert_eq!(second[0], first[0]);
    assert_ne!(second[1].split(' ').nth(1), first[0].split(' ').nth(1));

    let svc = f.service();
    let id = svc
        .store
        .mailbox_entries(&MailboxKey::inbox(Some("billing"), ALICE))
        .await
        .unwrap()[0]
        .message_id;
    svc.store.delete(id, &SpaceFilter::Any).await.unwrap();
    assert_eq!(
        c.cmd("RETR 1").await,
        "-ERR Message 1 is no longer available"
    );
    c.ok("RETR 2").await;
}

#[tokio::test]
async fn failed_logins_wait_and_close_after_the_limit() {
    let mut f = Fixture::new();
    f.set_password(Some("billing"), ALICE, "right").await;
    let mut srv = Pop3Server::new(f.service(), None, Some(Arc::new(Tokens)));
    srv.failure_delay = Duration::from_millis(300);
    let srv = Arc::new(srv);
    let (mut c, t) = open(&srv).await;
    assert_eq!(c.cmd("PASS right").await, "-ERR Send USER first");
    c.ok(&format!("USER {ALICE}")).await;
    let started = std::time::Instant::now();
    assert_eq!(c.cmd("PASS wrong").await, "-ERR [AUTH] Invalid credentials");
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert_eq!(
        c.cmd("PASS right").await,
        "-ERR Send USER first",
        "a failed PASS forgets the user"
    );
    c.ok("USER nobody@billing.example.test").await;
    assert_eq!(c.cmd("PASS right").await, "-ERR [AUTH] Invalid credentials");
    c.ok("USER not-an-address").await;
    assert_eq!(
        c.cmd("PASS right").await,
        "-ERR [AUTH] Too many failed login attempts; closing connection"
    );
    assert!(c.closed().await);
    assert_eq!(report(t).await.end, SessionEnd::Rejected);
}

#[tokio::test]
async fn auth_plain_with_initial_response_and_continuation() {
    let mut f = Fixture::new();
    f.deliver(ALICE, &msg("one")).await;
    f.set_password(Some("billing"), ALICE, "pw").await;
    let srv = f.server();
    let enc =
        |z: &str, u: &str, p: &str| crate::smtp::sasl::encode(format!("{z}\0{u}\0{p}").as_bytes());

    let (mut c, _) = open(&srv).await;
    c.ok("AUTH").await;
    assert_eq!(c.body().await, vec!["PLAIN"]);
    assert_eq!(
        c.cmd("AUTH LOGIN").await,
        "-ERR Unsupported authentication mechanism"
    );
    assert!(
        c.cmd(&format!("AUTH PLAIN {}", enc("", ALICE, "pw")))
            .await
            .starts_with("+OK Logged in, 1 messages")
    );
    c.fails("AUTH PLAIN").await;
    c.fails(&format!("USER {ALICE}")).await;

    let (mut c, _) = open(&srv).await;
    assert_eq!(c.cmd("AUTH PLAIN").await, "+ ");
    assert_eq!(c.cmd("*").await, "-ERR Authentication cancelled");
    assert_eq!(c.cmd("auth plain").await, "+ ");
    assert!(
        c.cmd(&enc(ALICE, "Alice+x@billing.example.test", GLOBAL_TOKEN))
            .await
            .starts_with("+OK Logged in")
    );

    let (mut c, _) = open(&srv).await;
    assert_eq!(
        c.cmd("AUTH PLAIN !!notbase64").await,
        "-ERR [AUTH] Invalid credentials"
    );
    assert_eq!(
        c.cmd(&format!(
            "AUTH PLAIN {}",
            enc("bob@billing.example.test", ALICE, "pw")
        ))
        .await,
        "-ERR [AUTH] Invalid credentials",
        "another authorization identity"
    );
    c.cmd("AUTH PLAIN =").await;
    assert!(c.closed().await);
}

#[tokio::test]
async fn plaintext_logins_wait_for_stls() {
    let mut f = Fixture::new().with_tls();
    f.deliver(ALICE, &msg("one")).await;
    let client_tls = f.tls.as_ref().unwrap().client.clone();
    let srv = f.server();
    let (mut c, t) = open(&srv).await;
    c.ok("CAPA").await;
    let caps = c.body().await;
    assert!(caps.contains(&"STLS".to_string()), "{caps:?}");
    assert!(!caps.iter().any(|c| c == "USER" || c.starts_with("SASL")));
    let refused =
        "-ERR [AUTH] Plaintext authentication is disabled on this connection; use STLS first";
    assert_eq!(c.cmd(&format!("USER {ALICE}")).await, refused);
    assert_eq!(c.cmd("PASS x").await, refused);
    assert_eq!(c.cmd("AUTH PLAIN AGEAYg==").await, refused);
    assert_eq!(c.cmd("STLS").await, "+OK Begin TLS negotiation");
    let mut c = upgrade(c, client_tls).await;
    c.ok("CAPA").await;
    let caps = c.body().await;
    assert!(caps.contains(&"USER".to_string()) && caps.contains(&"SASL PLAIN".to_string()));
    assert!(!caps.contains(&"STLS".to_string()));
    assert_eq!(c.cmd("STLS").await, "-ERR TLS is already active");
    c.ok(&format!("USER {ALICE}")).await;
    c.ok(&format!("PASS {GLOBAL_TOKEN}")).await;
    c.ok("CAPA").await;
    let caps = c.body().await;
    assert!(!caps.iter().any(|c| c == "USER" || c == "STLS"));
    c.ok("QUIT").await;
    assert_eq!(report(t).await.end, SessionEnd::Quit);
}

#[tokio::test]
async fn bytes_after_stls_close_the_connection() {
    let mut f = Fixture::new().with_tls();
    let srv = f.server();
    let (mut c, t) = open(&srv).await;
    c.send(b"STLS\r\nUSER x@example.test\r\n").await;
    assert_eq!(c.line().await, "+OK Begin TLS negotiation");
    assert!(c.closed().await);
    assert_eq!(report(t).await.end, SessionEnd::Rejected);
}

#[tokio::test]
async fn stls_reset_state_and_unavailable_stls() {
    let mut f = Fixture::new();
    let srv = f.server();
    let (mut c, _) = open(&srv).await;
    assert_eq!(c.cmd("STLS").await, "-ERR STLS is not offered");

    let mut f = Fixture::new().with_tls();
    f.cfg.allow_plaintext_auth = true;
    f.set_password(Some("billing"), ALICE, "pw").await;
    let client_tls = f.tls.as_ref().unwrap().client.clone();
    let srv = f.server();
    let (mut c, _) = open(&srv).await;
    c.ok("CAPA").await;
    let caps = c.body().await;
    assert!(caps.contains(&"USER".to_string()) && caps.contains(&"STLS".to_string()));
    c.ok(&format!("USER {ALICE}")).await;
    c.ok("STLS").await;
    let mut c = upgrade(c, client_tls).await;
    assert_eq!(
        c.cmd("PASS pw").await,
        "-ERR Send USER first",
        "STLS forgets the user"
    );
    c.ok(&format!("USER {ALICE}")).await;
    c.ok("PASS pw").await;
    assert_eq!(
        c.cmd("STLS").await,
        "-ERR STLS is only allowed before login"
    );
}

#[tokio::test]
async fn pop3s_starts_with_tls() {
    let mut f = Fixture::new().with_tls();
    f.deliver(ALICE, &msg("one")).await;
    let client_tls = f.tls.as_ref().unwrap().client.clone();
    let srv = f.server();
    let (c, t) = open_with(&srv, true, None);
    let mut c = upgrade(c, client_tls).await;
    assert!(c.line().await.starts_with("+OK mail.example.test"));
    c.ok("CAPA").await;
    let caps = c.body().await;
    assert!(caps.contains(&"USER".to_string()) && !caps.contains(&"STLS".to_string()));
    c.ok(&format!("USER {ALICE}")).await;
    c.ok(&format!("PASS {GLOBAL_TOKEN}")).await;
    c.ok("RETR 1").await;
    assert!(c.body().await.contains(&"Hello one".to_string()));
    c.ok("QUIT").await;
    assert_eq!(report(t).await.end, SessionEnd::Quit);
}

#[tokio::test]
async fn errors_by_state_syntax_and_too_many_errors() {
    let mut f = Fixture::new();
    f.deliver(ALICE, &msg("one")).await;
    let srv = f.server();
    let (mut c, _) = open(&srv).await;
    assert_eq!(
        c.cmd("RETR 1").await,
        "-ERR RETR is not allowed before login"
    );
    assert_eq!(c.cmd("STAT").await, "-ERR STAT is not allowed before login");
    assert_eq!(c.cmd("XTND XMIT").await, "-ERR Unknown command");
    assert_eq!(
        c.cmd("APOP alice@example.test 0123").await,
        "-ERR APOP is not supported; use USER/PASS or AUTH PLAIN"
    );
    assert_eq!(c.cmd("USER").await, "-ERR Syntax: USER <mailbox address>");
    c.ok(&format!("USER {ALICE}")).await;
    c.ok(&format!("PASS {GLOBAL_TOKEN}")).await;
    assert_eq!(c.cmd("STAT 1").await, "-ERR Syntax: STAT");
    assert_eq!(c.cmd("RETR x").await, "-ERR Invalid message number");
    assert_eq!(c.cmd("TOP 1").await, "-ERR Syntax: TOP <msg> <lines>");
    assert_eq!(
        c.cmd(&format!("USER {ALICE}")).await,
        "-ERR USER is not allowed after login"
    );
    assert_eq!(
        c.cmd("PASS x").await,
        "-ERR PASS is not allowed after login"
    );
    assert_eq!(
        c.cmd("AUTH PLAIN").await,
        "-ERR AUTH is not allowed after login"
    );
    assert_eq!(c.cmd("NOOP").await, "+OK");
    let long = format!("NOOP {}", "x".repeat(5000));
    assert_eq!(c.cmd(&long).await, "-ERR Line too long");
    c.send(b"\xff\r\n").await;
    assert_eq!(c.line().await, "-ERR Invalid characters in command");
    for _ in 0..(MAX_BAD_COMMANDS - 3) {
        c.fails("BOGUS").await;
    }
    assert_eq!(
        c.cmd("BOGUS").await,
        "-ERR Too many errors; closing connection"
    );
    assert!(c.closed().await);
    assert_eq!(f.inbox(Some("billing"), ALICE).await, vec![1]);
}

#[tokio::test]
async fn pipelined_commands_are_answered_in_order() {
    let mut f = Fixture::new();
    f.deliver(ALICE, &msg("one")).await;
    f.deliver(ALICE, &msg("two")).await;
    let srv = f.server();
    let (mut c, t) = open(&srv).await;
    c.send(
        format!(
            "USER {ALICE}\r\nPASS {GLOBAL_TOKEN}\r\nSTAT\r\nTOP 2 0\r\nDELE 1\r\nLIST\r\nQUIT\r\n"
        )
        .as_bytes(),
    )
    .await;
    assert!(c.line().await.starts_with("+OK Send PASS"));
    assert!(c.line().await.starts_with("+OK Logged in, 2"));
    assert!(c.line().await.starts_with("+OK 2 "));
    assert!(c.line().await.starts_with("+OK Top"));
    assert!(c.body().await.contains(&"Subject: two".to_string()));
    assert_eq!(c.line().await, "+OK Message 1 deleted");
    assert!(c.line().await.starts_with("+OK 1 messages"));
    assert_eq!(c.body().await.len(), 1);
    assert!(c.line().await.contains("1 messages deleted, 1 left"));
    assert_eq!(report(t).await.deleted, 1);
    assert_eq!(f.inbox(Some("billing"), ALICE).await, vec![2]);
}

#[tokio::test]
async fn idle_and_session_timeouts() {
    let mut f = Fixture::new();
    f.cfg.idle_timeout = Duration::from_millis(150);
    let srv = f.server();
    let (mut c, t) = open(&srv).await;
    assert_eq!(
        c.line().await,
        "-ERR Timeout waiting for the client; closing connection"
    );
    assert!(c.closed().await);
    assert_eq!(report(t).await.end, SessionEnd::Timeout);

    let mut f = Fixture::new();
    f.cfg.session_timeout = Duration::from_millis(300);
    let srv = f.server();
    let (mut c, t) = open(&srv).await;
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(80)).await;
        c.ok("NOOP").await;
    }
    assert_eq!(
        c.line().await,
        "-ERR Session time limit reached; closing connection"
    );
    assert_eq!(report(t).await.end, SessionEnd::Timeout);
}

#[tokio::test]
async fn large_messages_are_streamed_whole() {
    let mut f = Fixture::new();
    let mut raw = String::from("Subject: big\r\n\r\n");
    for i in 0..20_000 {
        raw.push_str(&format!(".line {i:08} {}\r\n", "x".repeat(40)));
    }
    f.deliver(ALICE, &raw).await;
    let srv = f.server();
    let (mut c, _) = logged_in(&srv, ALICE, GLOBAL_TOKEN).await;
    c.ok("RETR 1").await;
    let got = c.raw_body().await;
    assert_eq!(render::unstuff(&got).unwrap(), raw.as_bytes());
    c.ok("NOOP").await;
}
