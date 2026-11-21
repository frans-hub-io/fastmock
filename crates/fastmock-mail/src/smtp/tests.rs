use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fastmock_core::mail::{
    LocalMailEvents, MailMessage, MailQuery, MemoryMailStore, TranscriptDirection,
};
use fastmock_core::model::SpaceFilter;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufStream};
use tokio::sync::watch;

use super::*;
use crate::chaos::{MailChaos, MemoryChaosBackend};
use crate::config::{MailConfig, SmtpUser};
use crate::routing::MailDomains;

const PEER: &str = "198.51.100.7:40000";

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
    tokens: Option<Arc<dyn TokenCheck>>,
    chaos: Option<Arc<MailChaos>>,
}

impl Fixture {
    fn new() -> Self {
        let cfg = MailConfig {
            hostname: "mail.example.test".into(),
            ..Default::default()
        };
        Self {
            cfg,
            tls: None,
            tokens: None,
            chaos: None,
        }
    }

    fn with_tls(mut self) -> Self {
        self.tls = Some(tls());
        self
    }

    fn with_chaos(mut self) -> Self {
        self.chaos = Some(Arc::new(MailChaos::with_cache_ttl(
            Arc::new(MemoryChaosBackend::default()),
            Duration::ZERO,
        )));
        self
    }

    fn server(&self) -> Arc<SmtpServer> {
        let mut domains = MailDomains::default();
        domains.add("billing", "billing.example.test");
        domains.add("crm", "crm.example.test");
        let svc = MailService::new(
            Arc::new(self.cfg.clone()),
            Arc::new(MemoryMailStore::new(100)),
            Arc::new(LocalMailEvents::new(16)),
            domains,
            ["billing", "crm", "ops"].map(String::from).into(),
        );
        Arc::new(SmtpServer::new(
            Arc::new(svc),
            self.tls.as_ref().map(|t| t.server.clone()),
            self.tokens.clone(),
            self.chaos.clone(),
        ))
    }
}

struct Client<S> {
    s: BufStream<S>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Client<S> {
    async fn send(&mut self, b: &[u8]) {
        self.s.write_all(b).await.unwrap();
        self.s.flush().await.unwrap();
    }

    /// One reply: all its lines, the code of the last.
    async fn reply(&mut self) -> (u16, Vec<String>) {
        let mut lines = vec![];
        loop {
            let mut l = String::new();
            let n = tokio::time::timeout(Duration::from_secs(10), self.s.read_line(&mut l))
                .await
                .expect("reply in time")
                .unwrap();
            assert!(n > 0, "connection closed; got {lines:?}");
            let l = l.trim_end().to_string();
            let more = l.as_bytes().get(3) == Some(&b'-');
            lines.push(l);
            if !more {
                break;
            }
        }
        let code = lines.last().unwrap()[..3].parse().unwrap();
        (code, lines)
    }

    async fn code(&mut self) -> u16 {
        self.reply().await.0
    }

    async fn cmd(&mut self, line: &str) -> (u16, Vec<String>) {
        self.send(format!("{line}\r\n").as_bytes()).await;
        self.reply().await
    }

    async fn expect(&mut self, line: &str, code: u16) -> Vec<String> {
        let (c, l) = self.cmd(line).await;
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
}

type Duplex = tokio::io::DuplexStream;

fn start(
    srv: &Arc<SmtpServer>,
    implicit_tls: bool,
    shutdown: Option<watch::Receiver<bool>>,
) -> (Client<Duplex>, tokio::task::JoinHandle<SessionReport>) {
    let (a, b) = tokio::io::duplex(1 << 20);
    let srv = srv.clone();
    let h = tokio::spawn(async move {
        srv.serve(b, PEER.parse().unwrap(), implicit_tls, shutdown)
            .await
    });
    (
        Client {
            s: BufStream::new(a),
        },
        h,
    )
}

async fn connect(
    srv: &Arc<SmtpServer>,
) -> (Client<Duplex>, tokio::task::JoinHandle<SessionReport>) {
    let (mut c, h) = start(srv, false, None);
    assert_eq!(c.code().await, 220);
    (c, h)
}

async fn upgrade(c: Client<Duplex>, tls: &Tls) -> Client<tokio_rustls::client::TlsStream<Duplex>> {
    let raw = c.s.into_inner();
    let conn = tokio_rustls::TlsConnector::from(tls.client.clone());
    let s = conn
        .connect(ServerName::try_from("mail.example.test").unwrap(), raw)
        .await
        .unwrap();
    Client {
        s: BufStream::new(s),
    }
}

async fn messages(srv: &SmtpServer) -> Vec<MailMessage> {
    let page = srv
        .service
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
            srv.service
                .store
                .get(e.id, &SpaceFilter::Any)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    out
}

async fn send_one<S: AsyncRead + AsyncWrite + Unpin>(c: &mut Client<S>, to: &str) {
    c.expect("MAIL FROM:<app@example.com>", 250).await;
    c.expect(&format!("RCPT TO:<{to}>"), 250).await;
    c.expect("DATA", 354).await;
    c.send(b"Subject: hi\r\n\r\nbody\r\n.\r\n").await;
    assert_eq!(c.code().await, 250);
}

fn b64(s: &[u8]) -> String {
    sasl::encode(s)
}

#[tokio::test]
async fn session_stores_envelope_and_masked_transcript() {
    let srv = Fixture::new().server();
    let (mut c, h) = connect(&srv).await;
    let ehlo = c.expect("EHLO client.example.test", 250).await;
    assert_eq!(
        ehlo,
        vec![
            "250-mail.example.test greets client.example.test",
            "250-SIZE 10485760",
            "250-8BITMIME",
            "250-SMTPUTF8",
            "250-PIPELINING",
            "250-CHUNKING",
            "250-ENHANCEDSTATUSCODES",
            "250 AUTH PLAIN LOGIN CRAM-MD5",
        ]
    );
    c.expect(&format!("AUTH PLAIN {}", b64(b"\0ops\0s3cret-pass")), 235)
        .await;
    c.expect("MAIL FROM:<app@example.com> SIZE=40", 250).await;
    c.expect("RCPT TO:<a@billing.example.test>", 250).await;
    c.expect("NOOP", 250).await;
    c.expect("VRFY bob", 252).await;
    c.expect("DATA", 354).await;
    c.send(b"Subject: Hello\r\n\r\nCode 1234\r\n.\r\n").await;
    let (code, l) = c.reply().await;
    assert_eq!(code, 250);
    assert!(l[0].starts_with("250 2.0.0 OK: queued as "), "{l:?}");
    c.expect("QUIT", 221).await;
    assert!(c.closed().await);
    let rep = h.await.unwrap();
    assert_eq!(rep.end, SessionEnd::Quit);
    assert_eq!(rep.messages, 1);
    let m = messages(&srv).await;
    assert_eq!(m.len(), 1);
    let m = &m[0];
    assert_eq!(m.space.as_deref(), Some("billing"));
    assert_eq!(m.envelope.mail_from, "app@example.com");
    assert_eq!(m.envelope.rcpt_to, vec!["a@billing.example.test"]);
    assert_eq!(m.envelope.helo.as_deref(), Some("client.example.test"));
    assert_eq!(m.envelope.client_ip.as_deref(), Some("198.51.100.7"));
    assert!(!m.envelope.tls);
    assert_eq!(m.envelope.auth_user.as_deref(), Some("ops"));
    assert_eq!(&m.raw[..], b"Subject: Hello\r\n\r\nCode 1234\r\n");
    assert_eq!(m.summary.subject.as_deref(), Some("Hello"));
    let t: Vec<(TranscriptDirection, &str)> = m
        .transcript
        .iter()
        .map(|l| (l.direction, l.text.as_str()))
        .collect();
    use TranscriptDirection::{Client as C, Server as S};
    assert_eq!(t[0], (S, "220 mail.example.test ESMTP FastMock"));
    assert_eq!(t[1], (C, "EHLO client.example.test"));
    assert!(t.contains(&(C, "AUTH PLAIN ****")));
    assert!(t.contains(&(S, "235 2.7.0 Authentication successful")));
    assert!(t.contains(&(C, "MAIL FROM:<app@example.com> SIZE=40")));
    assert!(t.contains(&(C, "(message: 29 bytes)")));
    assert_eq!(t.last().unwrap(), &(C, "(message: 29 bytes)"));
    let all = serde_json::to_string(&m.transcript).unwrap();
    assert!(!all.contains("s3cret") && !all.contains(&b64(b"\0ops\0s3cret-pass")));
    assert!(!all.contains("Code 1234"));
}

#[tokio::test]
async fn pipelined_commands_get_replies_in_order() {
    let srv = Fixture::new().server();
    let (mut c, _h) = connect(&srv).await;
    c.send(b"EHLO a.example\r\nMAIL FROM:<x@example.com>\r\nRCPT TO:<a@crm.example.test>\r\nRCPT TO:<b@billing.example.test>\r\nRCPT TO:<c@other.example>\r\nDATA\r\n")
        .await;
    assert_eq!(c.code().await, 250);
    for _ in 0..4 {
        assert_eq!(c.code().await, 250);
    }
    assert_eq!(c.code().await, 354);
    c.send(b"Subject: p\r\n\r\nx\r\n.\r\nRSET\r\nMAIL FROM:<>\r\nQUIT\r\n")
        .await;
    for want in [250, 250, 250, 221] {
        assert_eq!(c.code().await, want);
    }
    let m = messages(&srv).await;
    let spaces: Vec<Option<&str>> = m.iter().map(|m| m.space.as_deref()).collect();
    assert_eq!(spaces, vec![None, Some("billing"), Some("crm")]);
    assert_eq!(m[0].envelope.rcpt_to, vec!["c@other.example"]);
}

#[tokio::test]
async fn data_dot_stuffing_and_bare_line_endings() {
    let srv = Fixture::new().server();
    let (mut c, _h) = connect(&srv).await;
    c.expect("EHLO a.example", 250).await;
    c.expect("MAIL FROM:<x@example.com>", 250).await;
    c.expect("RCPT TO:<a@crm.example.test>", 250).await;
    c.expect("DATA", 354).await;
    c.send(b"Subject: s\r\n\r\n..leading\r\nbare\n.\nlf\r.\rcr\n.\r\nMAIL FROM:<evil@example.com>\r\nRCPT TO:<victim@crm.example.test>\r\nDATA\r\n\r\nsmuggled\r\n.\r\n")
        .await;
    assert_eq!(c.code().await, 250);
    c.expect("NOOP", 250).await;
    let m = messages(&srv).await;
    assert_eq!(m.len(), 1, "a bare LF before the dot never ends DATA");
    assert_eq!(
        &m[0].raw[..],
        b"Subject: s\r\n\r\n.leading\r\nbare\n.\nlf\r.\rcr\n.\r\nMAIL FROM:<evil@example.com>\r\nRCPT TO:<victim@crm.example.test>\r\nDATA\r\n\r\nsmuggled\r\n"
    );
    c.expect("MAIL FROM:<x@example.com>", 250).await;
    c.expect("RCPT TO:<a@crm.example.test>", 250).await;
    c.expect("DATA", 354).await;
    c.send(b".\r\n").await;
    assert_eq!(c.code().await, 250);
    assert_eq!(messages(&srv).await[1].raw.len(), 0);
}

#[tokio::test]
async fn bdat_chunks_size_mixing_and_rset() {
    let mut f = Fixture::new();
    f.cfg.max_message_bytes = 64;
    let srv = f.server();
    let (mut c, _h) = connect(&srv).await;
    c.expect("EHLO a.example", 250).await;
    c.expect("MAIL FROM:<x@example.com>", 250).await;
    c.expect("RCPT TO:<a@crm.example.test>", 250).await;
    c.send(b"BDAT 14\r\nSubject: b\r\n\r\nBDAT 9 LAST\r\n.\r\nQUIT\r\n")
        .await;
    let (code, l) = c.reply().await;
    assert_eq!((code, l[0].as_str()), (250, "250 2.0.0 14 octets received"));
    assert_eq!(c.code().await, 250);
    let m = messages(&srv).await;
    assert_eq!(&m[0].raw[..], b"Subject: b\r\n\r\n.\r\nQUIT\r\n");

    c.expect("MAIL FROM:<x@example.com>", 250).await;
    c.expect("RCPT TO:<a@crm.example.test>", 250).await;
    c.send(b"BDAT 3\r\nabc").await;
    assert_eq!(c.code().await, 250);
    c.expect("DATA", 503).await;
    c.expect("RCPT TO:<b@crm.example.test>", 503).await;
    c.expect("RSET", 250).await;
    c.send(b"BDAT 5 LAST\r\nhello").await;
    assert_eq!(
        c.code().await,
        503,
        "RSET dropped the transaction and its chunks"
    );
    c.expect("NOOP", 250).await;

    c.expect("MAIL FROM:<x@example.com>", 250).await;
    c.expect("RCPT TO:<a@crm.example.test>", 250).await;
    c.send(b"BDAT 40\r\n").await;
    c.send(&[b'a'; 40]).await;
    assert_eq!(c.code().await, 250);
    c.send(b"BDAT 30 LAST\r\n").await;
    c.send(&[b'b'; 30]).await;
    let (code, l) = c.reply().await;
    assert_eq!(code, 552, "{l:?}");
    c.send(b"BDAT 2 LAST\r\nzz").await;
    assert_eq!(c.code().await, 503);
    c.expect("NOOP", 250).await;
    assert_eq!(messages(&srv).await.len(), 1);

    c.expect("MAIL FROM:<x@example.com>", 250).await;
    c.send(b"BDAT 4 LAST\r\nRSET").await;
    assert_eq!(c.code().await, 554, "no recipients");
    c.expect("NOOP", 250).await;
}

#[tokio::test]
async fn size_limits() {
    let mut f = Fixture::new();
    f.cfg.max_message_bytes = 100;
    let srv = f.server();
    let (mut c, _h) = connect(&srv).await;
    let ehlo = c.expect("EHLO a.example", 250).await;
    assert!(ehlo.contains(&"250-SIZE 100".to_string()));
    let l = c.expect("MAIL FROM:<x@example.com> SIZE=101", 552).await;
    assert_eq!(
        l[0],
        "552 5.3.4 Message size exceeds fixed maximum message size"
    );
    c.expect("MAIL FROM:<x@example.com> SIZE=100", 250).await;
    c.expect("RCPT TO:<a@crm.example.test>", 250).await;
    c.expect("DATA", 354).await;
    let mut big = vec![b'x'; 300];
    big.extend_from_slice(b"\r\n.\r\n");
    c.send(&big).await;
    assert_eq!(c.code().await, 552);
    c.expect("MAIL FROM:<x@example.com>", 250).await;
    c.expect("RCPT TO:<a@crm.example.test>", 250).await;
    c.expect("DATA", 354).await;
    c.send(b"Subject: ok\r\n\r\nfits\r\n.\r\n").await;
    assert_eq!(c.code().await, 250);
    assert_eq!(messages(&srv).await.len(), 1);
}

#[tokio::test]
async fn recipient_limit_smtputf8_and_fail_addresses() {
    let mut f = Fixture::new();
    f.cfg.max_recipients = 2;
    let srv = f.server();
    let (mut c, _h) = connect(&srv).await;
    c.expect("EHLO a.example", 250).await;
    c.expect("MAIL FROM:<jürgen@example.com>", 553).await;
    c.expect("MAIL FROM:<x@example.com>", 250).await;
    let l = c.expect("RCPT TO:<jürgen@crm.example.test>", 553).await;
    assert_eq!(l[0], "553 5.6.7 Non-ASCII address needs SMTPUTF8");
    let l = c.expect("RCPT TO:<fail+451@crm.example.test>", 451).await;
    assert_eq!(l[0], "451 4.0.0 Rejected by the fail+451 address");
    c.expect("RCPT TO:<fail+550@other.example>", 550).await;
    c.expect("RCPT TO:<fail+250@crm.example.test>", 250).await;
    c.expect("RCPT TO:<b@crm.example.test>", 250).await;
    let l = c.expect("RCPT TO:<c@crm.example.test>", 452).await;
    assert_eq!(l[0], "452 4.5.3 Too many recipients");
    c.expect("RSET", 250).await;
    c.expect("MAIL FROM:<jürgen@example.com> SMTPUTF8", 250)
        .await;
    c.expect("RCPT TO:<ünı@crm.example.test>", 250).await;
    c.expect("DATA", 354).await;
    c.send(b"Subject: u\r\n\r\nx\r\n.\r\n").await;
    assert_eq!(c.code().await, 250);
    let m = messages(&srv).await;
    assert_eq!(m[0].envelope.rcpt_to, vec!["ünı@crm.example.test"]);
    c.send(b"RCPT TO:<fail+421@crm.example.test>\r\n").await;
    assert_eq!(c.code().await, 503);
    c.expect("MAIL FROM:<x@example.com>", 250).await;
    c.expect("RCPT TO:<fail+421@crm.example.test>", 421).await;
    assert!(c.closed().await);
}

#[tokio::test]
async fn command_errors_and_state() {
    let mut f = Fixture::new();
    f.cfg.max_line_bytes = 512;
    let srv = f.server();
    let (mut c, h) = connect(&srv).await;
    c.expect("MAIL FROM:<x@example.com>", 503).await;
    c.expect("AUTH PLAIN", 503).await;
    c.expect("HELO a.example", 250).await;
    c.expect("AUTH PLAIN", 503).await;
    c.expect("RCPT TO:<a@crm.example.test>", 503).await;
    c.expect("DATA", 503).await;
    c.expect("FOO", 500).await;
    c.expect("EXPN list", 502).await;
    c.expect("MAIL FROM:x", 501).await;
    c.expect("MAIL FROM:<x@example.com> BODY=BINARYMIME", 555)
        .await;
    let l = c.expect(&format!("NOOP {}", "x".repeat(600)), 500).await;
    assert_eq!(l[0], "500 5.5.2 Line too long");
    c.expect("MAIL FROM:<x@example.com>", 250).await;
    c.expect("MAIL FROM:<x@example.com>", 503).await;
    c.expect("DATA", 554).await;
    c.expect("NOOP", 250).await;
    for _ in 0..MAX_BAD_COMMANDS {
        c.expect("BAD", 500).await;
    }
    let (code, l) = c.reply().await;
    assert_eq!(
        (code, l[0].as_str()),
        (421, "421 4.7.0 Too many errors; closing connection")
    );
    assert!(c.closed().await);
    assert_eq!(h.await.unwrap().end, SessionEnd::Rejected);
}

#[tokio::test]
async fn starttls_resets_state_and_refuses_injection() {
    let f = Fixture::new().with_tls();
    let srv = f.server();
    let t = f.tls.as_ref().unwrap();

    let (mut c, h) = connect(&srv).await;
    c.expect("EHLO a.example", 250).await;
    c.send(b"STARTTLS\r\nMAIL FROM:<evil@example.com>\r\n")
        .await;
    assert_eq!(c.code().await, 220);
    assert!(c.closed().await);
    assert_eq!(h.await.unwrap().end, SessionEnd::Rejected);

    let (mut c, _h) = connect(&srv).await;
    let ehlo = c.expect("EHLO a.example", 250).await;
    assert_eq!(ehlo.last().unwrap(), "250 STARTTLS");
    assert!(!ehlo.iter().any(|l| l.contains("AUTH")), "{ehlo:?}");
    let l = c
        .expect(&format!("AUTH PLAIN {}", b64(b"\0u\0p")), 538)
        .await;
    assert_eq!(
        l[0],
        "538 5.7.11 Encryption required for requested authentication mechanism"
    );
    c.expect("MAIL FROM:<x@example.com>", 250).await;
    c.expect("STARTTLS", 220).await;
    let mut c = upgrade(c, t).await;
    c.expect("MAIL FROM:<x@example.com>", 503).await;
    let ehlo = c.expect("EHLO a.example", 250).await;
    assert!(!ehlo.iter().any(|l| l.contains("STARTTLS")));
    assert_eq!(ehlo.last().unwrap(), "250 AUTH PLAIN LOGIN CRAM-MD5");
    c.expect("STARTTLS", 503).await;
    c.expect(&format!("AUTH PLAIN {}", b64(b"\0u\0p")), 235)
        .await;
    send_one(&mut c, "a@crm.example.test").await;
    let m = messages(&srv).await;
    assert!(m[0].envelope.tls);
    assert_eq!(m[0].envelope.auth_user.as_deref(), Some("u"));
    assert!(
        m[0].transcript
            .iter()
            .any(|l| l.text == "220 2.0.0 Ready to start TLS")
    );

    let srv2 = Fixture::new().server();
    let (mut c, _h) = connect(&srv2).await;
    c.expect("EHLO a.example", 250).await;
    c.expect("STARTTLS", 502).await;
}

#[tokio::test]
async fn plaintext_auth_can_be_allowed() {
    let mut f = Fixture::new().with_tls();
    f.cfg.allow_plaintext_auth = true;
    let srv = f.server();
    let (mut c, _h) = connect(&srv).await;
    let ehlo = c.expect("EHLO a.example", 250).await;
    assert_eq!(ehlo.last().unwrap(), "250 AUTH PLAIN LOGIN CRAM-MD5");
    assert!(ehlo.contains(&"250-STARTTLS".to_string()));
    c.expect(&format!("AUTH PLAIN {}", b64(b"\0u\0p")), 235)
        .await;
}

#[tokio::test]
async fn implicit_tls_session() {
    let f = Fixture::new().with_tls();
    let srv = f.server();
    let (c, _h) = start(&srv, true, None);
    let raw = c.s.into_inner();
    let conn = tokio_rustls::TlsConnector::from(f.tls.as_ref().unwrap().client.clone());
    let s = conn
        .connect(ServerName::try_from("mail.example.test").unwrap(), raw)
        .await
        .unwrap();
    let mut c = Client {
        s: BufStream::new(s),
    };
    assert_eq!(c.code().await, 220);
    let ehlo = c.expect("EHLO a.example", 250).await;
    assert!(!ehlo.iter().any(|l| l.contains("STARTTLS")));
    c.expect(&format!("AUTH PLAIN {}", b64(b"\0u\0p")), 235)
        .await;
    send_one(&mut c, "a@crm.example.test").await;
    assert!(messages(&srv).await[0].envelope.tls);
}

struct Tokens;

struct Scope(Vec<Option<String>>);

impl WriteScope for Scope {
    fn can_write(&self, area: Option<&str>) -> bool {
        self.0.iter().any(|a| a.is_none() || a.as_deref() == area)
    }
}

#[async_trait]
impl TokenCheck for Tokens {
    async fn allows(&self, user: &str, token: &str) -> Option<Arc<dyn WriteScope>> {
        match (token, user) {
            ("fm_good", "billing") => Some(Arc::new(Scope(vec![Some("billing".into())]))),
            ("fm_global", _) => Some(Arc::new(Scope(vec![None]))),
            _ => None,
        }
    }
}

fn sha(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

#[tokio::test]
async fn required_auth_with_users_and_tokens() {
    let mut f = Fixture::new();
    f.cfg.smtp_require_auth = true;
    f.cfg.smtp_users = vec![SmtpUser {
        name: "app".into(),
        hash: sha("app-pass"),
    }];
    f.tokens = Some(Arc::new(Tokens));
    let srv = f.server();

    let (mut c, _h) = connect(&srv).await;
    let ehlo = c.expect("EHLO a.example", 250).await;
    assert_eq!(ehlo.last().unwrap(), "250 AUTH PLAIN LOGIN");
    let l = c.expect("MAIL FROM:<x@example.com>", 530).await;
    assert_eq!(l[0], "530 5.7.0 Authentication required");
    c.expect("AUTH CRAM-MD5", 504).await;
    let l = c
        .expect(&format!("AUTH PLAIN {}", b64(b"\0app\0wrong")), 535)
        .await;
    assert_eq!(l[0], "535 5.7.8 Authentication credentials invalid");
    c.expect("AUTH LOGIN", 334).await;
    c.expect(&b64(b"app"), 334).await;
    c.expect(&b64(b"app-pass"), 235).await;
    send_one(&mut c, "a@other.example").await;
    let m = messages(&srv).await;
    assert_eq!(m[0].envelope.auth_user.as_deref(), Some("app"));
    let all = serde_json::to_string(&m[0].transcript).unwrap();
    assert!(!all.contains(&b64(b"app-pass")) && !all.contains(&b64(b"app")));
    assert!(all.contains("\"****\""));

    let (mut c, _h) = connect(&srv).await;
    c.expect("EHLO a.example", 250).await;
    c.expect("AUTH PLAIN", 334).await;
    c.expect(&b64(b"\0billing\0fm_good"), 235).await;
    send_one(&mut c, "a@other.example").await;
    let m = messages(&srv).await;
    assert_eq!(
        m[1].space.as_deref(),
        Some("billing"),
        "routed by the AUTH user"
    );

    let (mut c, h) = connect(&srv).await;
    c.expect("EHLO a.example", 250).await;
    c.expect(&format!("AUTH PLAIN {}", b64(b"\0crm\0fm_good")), 535)
        .await;
    c.expect("AUTH PLAIN *", 501).await;
    c.expect("AUTH PLAIN", 334).await;
    c.expect("*", 501).await;
    c.expect("AUTH PLAIN !!!", 535).await;
    let l = c
        .expect(&format!("AUTH PLAIN {}", b64(b"\0app\0nope")), 421)
        .await;
    assert_eq!(
        l[0],
        "421 4.7.0 Too many failed authentication attempts; closing connection"
    );
    assert!(c.closed().await);
    assert_eq!(h.await.unwrap().end, SessionEnd::Rejected);
}

#[tokio::test]
async fn accept_any_credentials_with_every_mechanism() {
    let srv = Fixture::new().server();
    for (first, steps) in [
        (format!("AUTH PLAIN {}", b64(b"zid\0alice\0x")), vec![]),
        (format!("AUTH LOGIN {}", b64(b"alice")), vec![b64(b"pw")]),
        (
            "AUTH CRAM-MD5".to_string(),
            vec![b64(b"alice 0123456789abcdef0123456789abcdef")],
        ),
    ] {
        let (mut c, _h) = connect(&srv).await;
        c.expect("EHLO a.example", 250).await;
        let (mut code, mut l) = c.cmd(&first).await;
        for s in &steps {
            assert_eq!(code, 334, "{l:?}");
            (code, l) = c.cmd(s).await;
        }
        assert_eq!(code, 235, "{first}: {l:?}");
        c.expect(&format!("AUTH PLAIN {}", b64(b"\0b\0c")), 503)
            .await;
    }
    let (mut c, _h) = connect(&srv).await;
    c.expect("EHLO a.example", 250).await;
    c.expect("AUTH XOAUTH2", 504).await;
    let (code, l) = c.cmd("AUTH CRAM-MD5").await;
    assert_eq!(code, 334);
    let challenge = sasl::decode(&l[0][4..]).unwrap();
    assert!(
        String::from_utf8(challenge)
            .unwrap()
            .ends_with("@mail.example.test>")
    );
    c.expect(&b64(b"alice bad"), 535).await;
}

#[tokio::test]
async fn timeouts_answer_421() {
    let mut f = Fixture::new();
    f.cfg.idle_timeout = Duration::from_millis(200);
    let srv = f.server();
    let (mut c, h) = connect(&srv).await;
    let (code, l) = c.reply().await;
    assert_eq!(code, 421);
    assert_eq!(
        l[0],
        "421 4.4.2 Timeout waiting for the client; closing connection"
    );
    assert_eq!(h.await.unwrap().end, SessionEnd::Timeout);

    let mut f = Fixture::new();
    f.cfg.data_timeout = Duration::from_millis(200);
    let srv = f.server();
    let (mut c, _h) = connect(&srv).await;
    c.expect("EHLO a.example", 250).await;
    c.expect("MAIL FROM:<x@example.com>", 250).await;
    c.expect("RCPT TO:<a@crm.example.test>", 250).await;
    c.expect("DATA", 354).await;
    c.send(b"Subject: slow\r\n").await;
    assert_eq!(c.code().await, 421);
    assert!(c.closed().await);

    let mut f = Fixture::new();
    f.cfg.session_timeout = Duration::from_millis(400);
    let srv = f.server();
    let started = std::time::Instant::now();
    let (mut c, h) = connect(&srv).await;
    c.expect("EHLO a.example", 250).await;
    let (code, l) = c.reply().await;
    assert_eq!(code, 421);
    assert_eq!(
        l[0],
        "421 4.4.2 Session time limit reached; closing connection"
    );
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert_eq!(h.await.unwrap().end, SessionEnd::Timeout);
}

#[tokio::test]
async fn shutdown_closes_waiting_sessions() {
    let srv = Fixture::new().server();
    let (tx, rx) = watch::channel(false);
    let (mut c, h) = start(&srv, false, Some(rx));
    assert_eq!(c.code().await, 220);
    c.expect("EHLO a.example", 250).await;
    tx.send(true).unwrap();
    let (code, l) = c.reply().await;
    assert_eq!(
        (code, l[0].as_str()),
        (421, "421 4.3.2 Service shutting down; closing connection")
    );
    assert_eq!(h.await.unwrap().end, SessionEnd::Shutdown);
}

#[tokio::test]
async fn second_message_transcript_leaves_out_the_first() {
    let srv = Fixture::new().server();
    let (mut c, _h) = connect(&srv).await;
    c.expect("EHLO a.example", 250).await;
    send_one(&mut c, "a@crm.example.test").await;
    c.expect("MAIL FROM:<second@example.com>", 250).await;
    c.expect("RCPT TO:<b@crm.example.test>", 250).await;
    c.expect("DATA", 354).await;
    c.send(b"Subject: 2\r\n\r\nx\r\n.\r\n").await;
    assert_eq!(c.code().await, 250);
    let m = messages(&srv).await;
    let t: Vec<&str> = m[1].transcript.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(t[1], "EHLO a.example");
    assert!(!t.contains(&"MAIL FROM:<app@example.com>"), "{t:?}");
    assert!(t.contains(&"MAIL FROM:<second@example.com>"));
}

async fn put(chaos: &MailChaos, area: Option<&str>, rules: serde_json::Value) {
    chaos
        .replace(area, serde_json::from_value(rules).unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn chaos_at_each_stage() {
    let f = Fixture::new().with_chaos().with_tls();
    let chaos = f.chaos.clone().unwrap();
    let srv = f.server();

    put(&chaos, None, json!([{"stage": "connect", "times": 1, "action": {"reply": {"code": 554, "enhanced": "5.7.1", "text": "Go away"}}}])).await;
    let (mut c, h) = start(&srv, false, None);
    let (code, l) = c.reply().await;
    assert_eq!((code, l[0].as_str()), (554, "554 5.7.1 Go away"));
    assert!(c.closed().await);
    assert_eq!(h.await.unwrap().end, SessionEnd::Chaos);
    let (mut c, _h) = connect(&srv).await;

    put(&chaos, None, json!([
        {"stage": "ehlo", "times": 1, "action": {"reply": {"code": 451}}},
        {"stage": "tls", "times": 1, "action": {"reply": {"code": 454, "enhanced": "4.7.0", "text": "TLS not available"}}}
    ])).await;
    let l = c.expect("EHLO a.example", 451).await;
    assert_eq!(l[0], "451 4.0.0 FastMock chaos rule rule-1");
    c.expect("EHLO a.example", 250).await;
    c.expect("STARTTLS", 454).await;

    put(&chaos, Some("ops"), json!([{"stage": "auth", "match": {"authUser": {"equalTo": "ops"}}, "action": {"reply": {"code": 454, "enhanced": "4.7.0"}}}])).await;
    c.expect("RSET", 250).await;
    let srv_plain = {
        let mut f2 = Fixture::new();
        f2.chaos = Some(chaos.clone());
        f2.server()
    };
    let (mut c, _h) = connect(&srv_plain).await;
    c.expect("EHLO a.example", 250).await;
    c.expect(&format!("AUTH PLAIN {}", b64(b"\0ops\0x")), 454)
        .await;
    c.expect(&format!("AUTH PLAIN {}", b64(b"\0crm\0x")), 235)
        .await;

    put(&chaos, Some("crm"), json!([
        {"id": "from", "stage": "mail", "match": {"mailFrom": {"contains": "blocked"}}, "action": {"reply": {"code": 550, "enhanced": "5.7.1", "text": "Sender blocked"}}},
        {"id": "vip", "stage": "rcpt", "match": {"rcptTo": {"equalTo": "vip@other.example"}}, "action": {"reply": {"code": 452, "enhanced": "4.2.2", "text": "Mailbox full"}}},
        {"id": "third", "stage": "data", "every": 3, "action": {"reply": {"code": 451, "enhanced": "4.3.0", "text": "Try again"}}}
    ])).await;
    let l = c.expect("MAIL FROM:<blocked@example.com>", 550).await;
    assert_eq!(l[0], "550 5.7.1 Sender blocked");
    c.expect("MAIL FROM:<ok@example.com>", 250).await;
    c.expect("RCPT TO:<vip@other.example>", 452).await;
    c.expect("RSET", 250).await;
    let mut codes = vec![];
    for _ in 0..4 {
        c.expect("MAIL FROM:<ok@example.com>", 250).await;
        c.expect("RCPT TO:<x@other.example>", 250).await;
        c.expect("DATA", 354).await;
        c.send(b"Subject: c\r\n\r\nx\r\n.\r\n").await;
        codes.push(c.code().await);
    }
    assert_eq!(codes, vec![250, 250, 451, 250]);
    assert_eq!(messages(&srv_plain).await.len(), 3);

    put(&chaos, Some("billing"), json!([
        {"stage": "rcpt", "times": 1, "action": {"delay": "300ms"}},
        {"stage": "rcpt", "match": {"rcptTo": {"contains": "drop"}}, "action": {"disconnect": true}}
    ])).await;
    let started = std::time::Instant::now();
    c.expect("MAIL FROM:<ok@example.com>", 250).await;
    c.expect("RCPT TO:<a@billing.example.test>", 250).await;
    assert!(started.elapsed() >= Duration::from_millis(300));
    c.expect("RCPT TO:<b@billing.example.test>", 250).await;
    c.send(b"RCPT TO:<drop@billing.example.test>\r\n").await;
    assert!(c.closed().await);

    put(
        &chaos,
        None,
        json!([{"stage": "tls", "action": {"disconnect": true}}]),
    )
    .await;
    let (mut c, h) = start(&srv, true, None);
    assert!(c.closed().await);
    assert_eq!(h.await.unwrap().end, SessionEnd::Chaos);
}

// ----- AUTH scope, throttling, RSET and BDAT -----

#[tokio::test]
async fn token_logins_deliver_only_where_the_token_may_write() {
    let mut f = Fixture::new();
    f.cfg.smtp_require_auth = true;
    f.tokens = Some(Arc::new(Tokens));
    let srv = f.server();
    let (mut c, _h) = connect(&srv).await;
    c.expect("EHLO a.example", 250).await;
    c.expect(&format!("AUTH PLAIN {}", b64(b"\0billing\0fm_good")), 235)
        .await;
    c.expect("MAIL FROM:<app@example.com>", 250).await;
    let l = c.expect("RCPT TO:<x@crm.example.test>", 550).await;
    assert_eq!(
        l[0],
        "550 5.7.1 Not authorized to deliver to this recipient"
    );
    c.expect("RCPT TO:<x@billing.example.test>", 250).await;
    c.expect("RCPT TO:<x@other.example>", 250).await;

    let (mut c, _h) = connect(&srv).await;
    c.expect("EHLO a.example", 250).await;
    c.expect(&format!("AUTH PLAIN {}", b64(b"\0crm\0fm_global")), 235)
        .await;
    c.expect("MAIL FROM:<app@example.com>", 250).await;
    c.expect("RCPT TO:<x@billing.example.test>", 250).await;
    c.expect("RCPT TO:<x@crm.example.test>", 250).await;
}

#[tokio::test]
async fn failed_auth_is_throttled_across_connections() {
    let mut f = Fixture::new();
    f.cfg.smtp_require_auth = true;
    f.cfg.smtp_users = vec![SmtpUser {
        name: "app".into(),
        hash: sha("app-pass"),
    }];
    let srv = f.server();
    for _ in 0..5 {
        let (mut c, _h) = connect(&srv).await;
        c.expect("EHLO a.example", 250).await;
        c.expect(&format!("AUTH PLAIN {}", b64(b"\0app\0wrong")), 535)
            .await;
    }
    let (mut c, _h) = connect(&srv).await;
    c.expect("EHLO a.example", 250).await;
    c.expect(&format!("AUTH PLAIN {}", b64(b"\0app\0app-pass")), 535)
        .await;
}

#[tokio::test]
async fn smtp_users_may_have_pbkdf2_hashes() {
    let mut f = Fixture::new();
    f.cfg.smtp_require_auth = true;
    f.cfg.smtp_users = vec![SmtpUser {
        name: "app".into(),
        hash: crate::mailbox_login::hash_password_with("app-pass", 1000),
    }];
    let srv = f.server();
    let (mut c, _h) = connect(&srv).await;
    c.expect("EHLO a.example", 250).await;
    c.expect(&format!("AUTH PLAIN {}", b64(b"\0app\0wrong")), 535)
        .await;
    c.expect(&format!("AUTH PLAIN {}", b64(b"\0app\0app-pass")), 235)
        .await;
}

#[tokio::test]
async fn rset_drops_the_transcript_of_the_aborted_transaction() {
    let srv = Fixture::new().server();
    let (mut c, _h) = connect(&srv).await;
    c.expect("EHLO a.example", 250).await;
    c.expect("MAIL FROM:<aborted@example.com>", 250).await;
    c.expect("RCPT TO:<gone@example.test>", 250).await;
    c.expect("RSET", 250).await;
    send_one(&mut c, "kept@example.test").await;
    let m = messages(&srv).await;
    let all = serde_json::to_string(&m[0].transcript).unwrap();
    assert!(!all.contains("aborted@") && !all.contains("gone@"), "{all}");
    assert!(all.contains("kept@"), "{all}");
}

#[tokio::test]
async fn bdat_sizes_are_validated() {
    let mut f = Fixture::new();
    f.cfg.max_message_bytes = 1000;
    let srv = f.server();
    let (mut c, _h) = connect(&srv).await;
    c.expect("EHLO a.example", 250).await;
    c.expect("MAIL FROM:<app@example.com>", 250).await;
    c.expect("RCPT TO:<a@example.test>", 250).await;
    let l = c.expect("BDAT 99999999999999999999999 LAST", 501).await;
    assert!(l[0].contains("BDAT"), "{l:?}");
    let (mut c, h) = connect(&srv).await;
    c.expect("EHLO a.example", 250).await;
    c.expect("MAIL FROM:<app@example.com>", 250).await;
    c.expect("RCPT TO:<a@example.test>", 250).await;
    c.expect("BDAT 2000 LAST", 552).await;
    assert!(c.closed().await);
    assert_eq!(h.await.unwrap().end, SessionEnd::Rejected);
}

/// A span name with its fields as `name=value`.
type SpanRecord = (String, Vec<String>);

/// Spans recorded by a minimal subscriber.
#[derive(Clone, Default)]
struct Spans(Arc<std::sync::Mutex<Vec<SpanRecord>>>);

struct Fields<'a>(&'a mut Vec<String>);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
        self.0.push(format!("{}={v:?}", f.name()));
    }

    fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
        self.0.push(format!("{}={v}", f.name()));
    }
}

impl tracing::Subscriber for Spans {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, a: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let mut v = self.0.lock().unwrap();
        let mut fields = vec![];
        a.record(&mut Fields(&mut fields));
        v.push((a.metadata().name().to_string(), fields));
        tracing::span::Id::from_u64(v.len() as u64)
    }

    fn record(&self, id: &tracing::span::Id, r: &tracing::span::Record<'_>) {
        let mut v = self.0.lock().unwrap();
        if let Some((_, fields)) = v.get_mut(id.into_u64() as usize - 1) {
            r.record(&mut Fields(fields));
        }
    }

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[tokio::test]
async fn sessions_and_messages_are_traced() {
    let spans = Spans::default();
    let _g = tracing::subscriber::set_default(spans.clone());
    let srv = Fixture::new().server();
    let (mut c, h) = connect(&srv).await;
    c.expect("EHLO a.example", 250).await;
    send_one(&mut c, "a@billing.example.test").await;
    c.expect("QUIT", 221).await;
    h.await.unwrap();
    let v = spans.0.lock().unwrap().clone();
    let session = v
        .iter()
        .find(|(n, _)| n == "fastmock.mail.session")
        .expect("a session span");
    let has = |fields: &Vec<String>, f: &str| fields.iter().any(|x| x == f);
    assert!(has(&session.1, "network.protocol.name=smtp"), "{session:?}");
    assert!(has(&session.1, "fastmock.mail.result=quit"), "{session:?}");
    assert!(has(&session.1, "fastmock.mail.messages=1"), "{session:?}");
    let msg = v
        .iter()
        .find(|(n, _)| n == "fastmock.mail.message")
        .expect("a message span");
    assert!(has(&msg.1, "fastmock.mail.source=smtp"), "{msg:?}");
    assert!(has(&msg.1, "fastmock.space=billing"), "{msg:?}");
    assert!(has(&msg.1, "fastmock.mail.result=stored"), "{msg:?}");
}
