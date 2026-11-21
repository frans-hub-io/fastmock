use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use fastmock_core::mail::{Envelope, LocalMailEvents, MailSource, MailboxKey, MemoryMailStore};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufStream};
use tokio::sync::watch;

use super::*;
use crate::config::MailConfig;
use crate::mailbox_login::{MailboxAccess, MailboxTokenCheck, hash_password_with};
use crate::routing::MailDomains;
use crate::service::{Incoming, MailService};

const PEER: &str = "198.51.100.7:40000";
const BOX: &str = "alice@billing.example.test";

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

/// Tokens `tok-<space>` (`tok-global` for the global area) allow `mail:read`, `mail:write` and
/// `mail:delete` there; `ro-<space>` only `mail:read`.
struct Tokens;

#[async_trait]
impl MailboxTokenCheck for Tokens {
    async fn access(&self, area: Option<&str>, token: &str) -> Option<MailboxAccess> {
        let area = area.unwrap_or("global");
        if token == format!("tok-{area}") {
            Some(MailboxAccess::FULL)
        } else if token == format!("ro-{area}") {
            Some(MailboxAccess::READ_ONLY)
        } else {
            None
        }
    }
}

struct Fixture {
    cfg: MailConfig,
    tls: Option<Tls>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            cfg: MailConfig::default(),
            tls: None,
        }
    }

    fn with_tls(mut self) -> Self {
        self.tls = Some(tls());
        self
    }

    fn server(&self) -> Arc<ImapServer> {
        let mut domains = MailDomains::default();
        domains.add("billing", "billing.example.test");
        domains.add("payments", "payments.example.test");
        let svc = MailService::new(
            Arc::new(self.cfg.clone()),
            Arc::new(MemoryMailStore::new(100)),
            Arc::new(LocalMailEvents::new(16)),
            domains,
            ["billing", "payments"].map(String::from).into(),
        );
        Arc::new(ImapServer::new(
            Arc::new(svc),
            self.tls.as_ref().map(|t| t.server.clone()),
            Some(Arc::new(Tokens)),
        ))
    }
}

struct Client<S> {
    s: BufStream<S>,
    /// How long to wait for a line; long in tests with paused time.
    wait: Duration,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Client<S> {
    async fn send(&mut self, b: &[u8]) {
        self.s.write_all(b).await.unwrap();
        self.s.flush().await.unwrap();
    }

    /// One response line; literals are read into it.
    async fn line(&mut self) -> String {
        let mut out = Vec::new();
        loop {
            let mut l = Vec::new();
            let n = tokio::time::timeout(self.wait, self.s.read_until(b'\n', &mut l))
                .await
                .expect("a line in time")
                .unwrap();
            assert!(
                n > 0,
                "connection closed; got {:?}",
                String::from_utf8_lossy(&out)
            );
            out.extend_from_slice(&l);
            let trimmed = l.strip_suffix(b"\r\n").unwrap_or(&l);
            let lit = trimmed
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

    /// Sends a tagged command and returns the untagged lines and the tagged status line.
    async fn cmd(&mut self, tag: &str, line: &str) -> (Vec<String>, String) {
        self.send(format!("{tag} {line}\r\n").as_bytes()).await;
        self.until(tag).await
    }

    async fn until(&mut self, tag: &str) -> (Vec<String>, String) {
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
            tokio::time::timeout(self.wait, self.s.read_line(&mut l)).await,
            Ok(Ok(0)) | Ok(Err(_))
        )
    }
}

type Duplex = tokio::io::DuplexStream;

fn start(
    srv: &Arc<ImapServer>,
    implicit_tls: bool,
    shutdown: Option<watch::Receiver<bool>>,
) -> (Client<Duplex>, tokio::task::JoinHandle<SessionEnd>) {
    let (a, b) = tokio::io::duplex(1 << 20);
    let srv = srv.clone();
    let h = tokio::spawn(async move {
        srv.serve(b, PEER.parse().unwrap(), implicit_tls, shutdown)
            .await
    });
    (
        Client {
            s: BufStream::new(a),
            wait: Duration::from_secs(10),
        },
        h,
    )
}

async fn connect(srv: &Arc<ImapServer>) -> (Client<Duplex>, tokio::task::JoinHandle<SessionEnd>) {
    let (mut c, h) = start(srv, false, None);
    let g = c.line().await;
    assert!(g.starts_with("* OK [CAPABILITY IMAP4rev1"), "{g}");
    (c, h)
}

async fn logged_in(srv: &Arc<ImapServer>) -> Client<Duplex> {
    let (mut c, _h) = connect(srv).await;
    c.ok("l", &format!("LOGIN {BOX} tok-billing")).await;
    c
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
        wait: c.wait,
    }
}

async fn deliver(srv: &ImapServer, to: &str, subject: &str) {
    let raw = format!(
        "From: Shop <shop@example.com>\r\nTo: {to}\r\nSubject: {subject}\r\nMessage-ID: <{subject}@example.com>\r\n\r\nHello {subject}\r\n"
    );
    srv.service
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

async fn validity(srv: &ImapServer, key: &MailboxKey) -> u32 {
    srv.service
        .store
        .mailbox_info(key)
        .await
        .unwrap()
        .unwrap()
        .uid_validity
}

fn inbox() -> MailboxKey {
    MailboxKey::inbox(Some("billing"), BOX)
}

#[tokio::test]
async fn capabilities_follow_login_and_tls_state() {
    let srv = Fixture::new().server();
    let (mut c, _h) = start(&srv, false, None);
    let g = c.line().await;
    assert_eq!(
        g,
        "* OK [CAPABILITY IMAP4rev1 AUTH=PLAIN SASL-IR ID ENABLE IDLE NAMESPACE UNSELECT UIDPLUS MOVE UTF8=ACCEPT APPENDLIMIT=10485760] FastMock IMAP4rev1 ready"
    );
    let (u, _) = c.cmd("a", "CAPABILITY").await;
    assert!(
        u[0].starts_with("* CAPABILITY IMAP4rev1 AUTH=PLAIN"),
        "{u:?}"
    );
    let (_, t) = c.cmd("b", "SELECT INBOX").await;
    assert_eq!(t, "b BAD Log in first");
    let (_, t) = c.cmd("c", &format!("LOGIN {BOX} tok-billing")).await;
    assert!(t.starts_with("c OK [CAPABILITY IMAP4rev1 ID ENABLE"), "{t}");
    let (u, _) = c.cmd("d", "CAPABILITY").await;
    assert!(!u[0].contains("AUTH="), "{u:?}");
    let (_, t) = c.cmd("e", &format!("LOGIN {BOX} tok-billing")).await;
    assert_eq!(t, "e BAD Already logged in");
    let (_, t) = c.cmd("f", "FETCH 1 FLAGS").await;
    assert_eq!(t, "f BAD Select a mailbox first");
    let u = c.ok("g", "ID (\"name\" \"imaplib\")").await;
    assert_eq!(u, vec!["* ID (\"name\" \"FastMock\")"]);
    let u = c.ok("h", "NAMESPACE").await;
    assert_eq!(u, vec!["* NAMESPACE ((\"\" \"/\")) NIL NIL"]);
    let u = c.ok("i", "ENABLE UTF8=ACCEPT CONDSTORE").await;
    assert_eq!(u, vec!["* ENABLED UTF8=ACCEPT"]);
    let (u, t) = c.cmd("j", "LOGOUT").await;
    assert_eq!(u, vec!["* BYE FastMock logging out"]);
    assert_eq!(t, "j OK LOGOUT completed");
    assert!(c.closed().await);
}

#[tokio::test]
async fn login_is_disabled_before_starttls_and_starttls_resets() {
    let f = Fixture::new().with_tls();
    let srv = f.server();
    let (mut c, _h) = start(&srv, false, None);
    let g = c.line().await;
    assert!(g.contains(" STARTTLS LOGINDISABLED "), "{g}");
    assert!(!g.contains("AUTH=PLAIN"), "{g}");
    let (_, t) = c.cmd("a", &format!("LOGIN {BOX} tok-billing")).await;
    assert_eq!(
        t,
        "a NO [PRIVACYREQUIRED] LOGIN is disabled before STARTTLS"
    );
    let (_, t) = c.cmd("b", "AUTHENTICATE PLAIN AGFiAGM=").await;
    assert!(t.starts_with("b NO [PRIVACYREQUIRED]"), "{t}");
    c.ok("c", "STARTTLS").await;
    let mut c = upgrade(c, f.tls.as_ref().unwrap()).await;
    let (u, _) = c.cmd("d", "CAPABILITY").await;
    assert!(u[0].contains("AUTH=PLAIN"), "{u:?}");
    assert!(
        !u[0].contains("STARTTLS") && !u[0].contains("LOGINDISABLED"),
        "{u:?}"
    );
    let (_, t) = c.cmd("e", "STARTTLS").await;
    assert_eq!(t, "e BAD TLS is already active");
    c.ok("f", &format!("LOGIN {BOX} tok-billing")).await;
}

#[tokio::test]
async fn plaintext_login_can_be_allowed_with_tls_available() {
    let mut f = Fixture::new().with_tls();
    f.cfg.allow_plaintext_auth = true;
    let srv = f.server();
    let (mut c, _h) = start(&srv, false, None);
    let g = c.line().await;
    assert!(g.contains(" STARTTLS AUTH=PLAIN SASL-IR "), "{g}");
    c.ok("a", &format!("LOGIN {BOX} tok-billing")).await;
}

#[tokio::test]
async fn bytes_after_starttls_close_the_connection() {
    let f = Fixture::new().with_tls();
    let srv = f.server();
    let (mut c, h) = start(&srv, false, None);
    c.line().await;
    c.send(b"a STARTTLS\r\nb LOGIN x y\r\n").await;
    assert_eq!(c.line().await, "a OK Begin TLS negotiation now");
    assert!(c.closed().await);
    assert_eq!(h.await.unwrap(), SessionEnd::Rejected);
}

#[tokio::test]
async fn imaps_starts_with_the_handshake() {
    let f = Fixture::new().with_tls();
    let srv = f.server();
    let (c, _h) = start(&srv, true, None);
    let mut c = upgrade(c, f.tls.as_ref().unwrap()).await;
    let g = c.line().await;
    assert!(g.contains("AUTH=PLAIN") && !g.contains("STARTTLS"), "{g}");
    c.ok("a", &format!("LOGIN {BOX} tok-billing")).await;
}

#[tokio::test(start_paused = true)]
async fn failed_logins_wait_and_close_after_the_limit() {
    let srv = Fixture::new().server();
    let (mut c, h) = connect(&srv).await;
    let t0 = tokio::time::Instant::now();
    let (_, t) = c.cmd("a", &format!("LOGIN {BOX} wrong")).await;
    assert_eq!(t, "a NO [AUTHENTICATIONFAILED] Authentication failed");
    assert!(t0.elapsed() >= AUTH_FAILURE_DELAY);
    let (_, t) = c
        .cmd("b", "LOGIN a@payments.example.test tok-billing")
        .await;
    assert!(
        t.starts_with("b NO"),
        "a billing token cannot open a payments mailbox: {t}"
    );
    let (_, t) = c.cmd("c", "LOGIN not-an-address tok-global").await;
    assert!(t.starts_with("c NO"), "{t}");
    assert_eq!(
        c.line().await,
        "* BYE Too many failed authentication attempts"
    );
    assert!(c.closed().await);
    assert_eq!(h.await.unwrap(), SessionEnd::Rejected);
}

#[tokio::test]
async fn authenticate_plain_with_and_without_initial_response() {
    let srv = Fixture::new().server();
    srv.service
        .store
        .set_mailbox_password(
            Some("payments"),
            "p@payments.example.test",
            Some(&hash_password_with("secret", 1000)),
        )
        .await
        .unwrap();
    let ir = B64.encode(b"\0P+tag@payments.example.test\0secret");
    let (mut c, _h) = connect(&srv).await;
    c.ok("a", &format!("AUTHENTICATE PLAIN {ir}")).await;
    let (mut c, _h) = connect(&srv).await;
    c.send(b"a AUTHENTICATE PLAIN\r\n").await;
    assert_eq!(c.line().await, "+");
    c.send(b"*\r\n").await;
    assert_eq!(c.line().await, "a BAD Authentication cancelled");
    c.send(b"b AUTHENTICATE PLAIN\r\n").await;
    assert_eq!(c.line().await, "+");
    let data = B64.encode(format!("{BOX}\0{BOX}\0tok-billing"));
    c.send(format!("{data}\r\n").as_bytes()).await;
    let t = c.line().await;
    assert!(t.starts_with("b OK [CAPABILITY"), "{t}");
    let (mut c, _h) = connect(&srv).await;
    let (_, t) = c.cmd("a", "AUTHENTICATE CRAM-MD5").await;
    assert_eq!(t, "a NO Unsupported authentication mechanism");
    let bad = B64.encode(format!("other@x.example\0{BOX}\0tok-billing"));
    let (_, t) = c.cmd("b", &format!("AUTHENTICATE PLAIN {bad}")).await;
    assert!(t.starts_with("b NO [AUTHENTICATIONFAILED]"), "{t}");
}

#[tokio::test]
async fn select_fetch_store_search_expunge() {
    let srv = Fixture::new().server();
    for s in ["one", "two", "three"] {
        deliver(&srv, "Alice+tag@billing.example.test", s).await;
    }
    let mut c = logged_in(&srv).await;
    let v = validity(&srv, &inbox()).await;
    let u = c.ok("a", "SELECT INBOX").await;
    assert_eq!(
        u,
        vec![
            "* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)",
            "* OK [PERMANENTFLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft \\*)] Flags permitted",
            "* 3 EXISTS",
            "* 0 RECENT",
            "* OK [UNSEEN 1] First unseen",
            &format!("* OK [UIDVALIDITY {v}] UIDs valid"),
            "* OK [UIDNEXT 4] Predicted next UID",
        ]
    );
    let u = c.ok("b", "SEARCH UNSEEN").await;
    assert_eq!(u, vec!["* SEARCH 1 2 3"]);
    let u = c.ok("c", "SEARCH SUBJECT two").await;
    assert_eq!(u, vec!["* SEARCH 2"]);
    let u = c.ok("c2", "UID SEARCH FROM shop NOT SUBJECT one").await;
    assert_eq!(u, vec!["* SEARCH 2 3"]);
    let u = c
        .ok("d", "FETCH 1 (BODY.PEEK[HEADER.FIELDS (SUBJECT)] FLAGS)")
        .await;
    assert_eq!(
        u,
        vec!["* 1 FETCH (BODY[HEADER.FIELDS (SUBJECT)] {16}\r\nSubject: one\r\n\r\n FLAGS ())"]
    );
    let u = c.ok("e", "FETCH 1 (BODY[TEXT])").await;
    assert_eq!(
        u,
        vec!["* 1 FETCH (BODY[TEXT] {11}\r\nHello one\r\n FLAGS (\\Seen))"]
    );
    let u = c.ok("e2", "FETCH 1:2 (FLAGS)").await;
    assert_eq!(
        u,
        vec!["* 1 FETCH (FLAGS (\\Seen))", "* 2 FETCH (FLAGS ())"]
    );
    let u = c.ok("f", "UID FETCH 2 (RFC822.SIZE BODY[]<0.5>)").await;
    assert_eq!(u.len(), 1);
    assert!(u[0].starts_with("* 2 FETCH (UID 2 RFC822.SIZE "), "{:?}", u);
    assert!(
        u[0].ends_with("BODY[]<0> {5}\r\nFrom: FLAGS (\\Seen))"),
        "{:?}",
        u
    );
    let u = c
        .ok("g", "FETCH 3 (ENVELOPE BODYSTRUCTURE INTERNALDATE UID)")
        .await;
    assert!(
        u[0].contains("ENVELOPE (NIL \"three\" ((\"Shop\" NIL \"shop\" \"example.com\"))"),
        "{u:?}"
    );
    assert!(u[0].contains("BODYSTRUCTURE (\"TEXT\" \"PLAIN\""), "{u:?}");
    assert!(u[0].contains("INTERNALDATE \""), "{u:?}");
    let u = c.ok("h", "SEARCH SEEN").await;
    assert_eq!(u, vec!["* SEARCH 1 2"]);
    let u = c.ok("i", "STORE 1 +FLAGS (\\Deleted)").await;
    assert_eq!(u, vec!["* 1 FETCH (FLAGS (\\Deleted \\Seen))"]);
    let u = c
        .ok("j", "UID STORE 3 +FLAGS.SILENT (\\Flagged $Work)")
        .await;
    assert!(u.is_empty(), "{u:?}");
    let u = c.ok("k", "SEARCH FLAGGED KEYWORD $Work").await;
    assert_eq!(u, vec!["* SEARCH 3"]);
    let u = c.ok("l", "SEARCH DELETED").await;
    assert_eq!(u, vec!["* SEARCH 1"]);
    let u = c.ok("m", "EXPUNGE").await;
    assert_eq!(u, vec!["* 1 EXPUNGE"]);
    let u = c.ok("n", "UID SEARCH ALL").await;
    assert_eq!(u, vec!["* SEARCH 2 3"]);
    let (_, t) = c.cmd("o", "FETCH 3 FLAGS").await;
    assert_eq!(t, "o BAD Invalid message sequence number");
    let (_, t) = c.cmd("p", "SEARCH CHARSET KOI8-R ALL").await;
    assert_eq!(t, "p NO [BADCHARSET (US-ASCII UTF-8)] Unsupported charset");
    let u = c.ok("q", "STORE 1 FLAGS (\\Deleted)").await;
    assert_eq!(u, vec!["* 1 FETCH (FLAGS (\\Deleted))"]);
    c.ok("r", "UID EXPUNGE 3").await;
    let u = c.ok("s", "UID EXPUNGE 2").await;
    assert_eq!(u, vec!["* 1 EXPUNGE"]);
    c.ok("t", "CLOSE").await;
    let (_, t) = c.cmd("u", "FETCH 1 FLAGS").await;
    assert_eq!(t, "u BAD Select a mailbox first");
    let left = srv.service.store.mailbox_entries(&inbox()).await.unwrap();
    assert_eq!(left.iter().map(|e| e.uid).collect::<Vec<_>>(), vec![3]);
}

#[tokio::test]
async fn examine_is_read_only() {
    let srv = Fixture::new().server();
    deliver(&srv, BOX, "one").await;
    let mut c = logged_in(&srv).await;
    let u = c.ok("a", "EXAMINE INBOX").await;
    assert!(u.contains(&"* OK [PERMANENTFLAGS ()] Read-only mailbox".to_string()));
    let (_, t) = c.cmd("a2", "EXAMINE INBOX").await;
    assert_eq!(t, "a2 OK [READ-ONLY] EXAMINE completed");
    let u = c.ok("b", "FETCH 1 BODY[]").await;
    assert!(!u[0].contains("\\Seen"), "{u:?}");
    let (_, t) = c.cmd("c", "STORE 1 +FLAGS (\\Seen)").await;
    assert_eq!(t, "c NO Mailbox is read-only");
    let (_, t) = c.cmd("d", "EXPUNGE").await;
    assert_eq!(t, "d NO Mailbox is read-only");
    let entries = srv.service.store.mailbox_entries(&inbox()).await.unwrap();
    assert!(entries[0].flags.is_empty());
}

#[tokio::test]
async fn folders_append_copy_move_status() {
    let srv = Fixture::new().server();
    deliver(&srv, BOX, "one").await;
    deliver(&srv, BOX, "two").await;
    let mut c = logged_in(&srv).await;
    c.ok("a", "CREATE Archive").await;
    let (_, t) = c.cmd("b", "CREATE archive/").await;
    assert_eq!(t, "b OK CREATE completed", "names are case-sensitive");
    let (_, t) = c.cmd("c", "CREATE Archive").await;
    assert_eq!(t, "c NO [ALREADYEXISTS] Mailbox already exists");
    let (_, t) = c.cmd("d", "CREATE inbox").await;
    assert_eq!(t, "d NO [ALREADYEXISTS] INBOX always exists");
    let u = c.ok("e", "LIST \"\" *").await;
    assert_eq!(
        u,
        vec![
            "* LIST () \"/\" \"INBOX\"",
            "* LIST () \"/\" \"Archive\"",
            "* LIST () \"/\" \"archive\""
        ]
    );
    let u = c.ok("f", "LSUB \"\" A%").await;
    assert_eq!(u, vec!["* LSUB () \"/\" \"Archive\""]);
    let u = c.ok("g", "LIST \"\" \"\"").await;
    assert_eq!(u, vec!["* LIST (\\Noselect) \"/\" \"\""]);
    let u = c.ok("h", "LIST \"\" inbox").await;
    assert_eq!(u, vec!["* LIST () \"/\" \"INBOX\""]);
    c.ok("i", "DELETE archive").await;
    let (_, t) = c.cmd("j", "DELETE INBOX").await;
    assert_eq!(t, "j NO INBOX cannot be deleted");
    let (_, t) = c.cmd("k", "DELETE Nope").await;
    assert_eq!(t, "k NO [NONEXISTENT] No such mailbox");

    c.send(b"l APPEND Nope {3}\r\n").await;
    assert_eq!(c.line().await, "+ Ready for literal data");
    c.send(b"abc\r\n").await;
    assert_eq!(c.line().await, "l NO [TRYCREATE] No such mailbox");
    let archive = MailboxKey {
        folder: "Archive".into(),
        ..inbox()
    };
    let va = validity(&srv, &archive).await;
    c.send(b"m APPEND Archive (\\Seen) {24}\r\n").await;
    assert_eq!(c.line().await, "+ Ready for literal data");
    c.send(b"Subject: kept\r\n\r\nsaved\r\n\r\n").await;
    let (_, t) = c.until("m").await;
    assert_eq!(t, format!("m OK [APPENDUID {va} 1] APPEND completed"));
    let u = c
        .ok(
            "n",
            "STATUS Archive (MESSAGES UIDNEXT UIDVALIDITY UNSEEN RECENT)",
        )
        .await;
    assert_eq!(
        u,
        vec![format!(
            "* STATUS \"Archive\" (MESSAGES 1 UIDNEXT 2 UIDVALIDITY {va} UNSEEN 0 RECENT 0)"
        )]
    );
    c.ok("o", "SELECT INBOX").await;
    let (_, t) = c.cmd("p", "COPY 1:2 Archive").await;
    assert_eq!(t, format!("p OK [COPYUID {va} 1:2 2:3] COPY completed"));
    let (_, t) = c.cmd("q", "COPY 1 Missing").await;
    assert_eq!(t, "q NO [TRYCREATE] No such mailbox");
    let (u, t) = c.cmd("r", "UID MOVE 2 Archive").await;
    assert_eq!(
        u,
        vec![
            format!("* OK [COPYUID {va} 2 4] Moved"),
            "* 2 EXPUNGE".into()
        ]
    );
    assert_eq!(t, "r OK UID MOVE completed");
    let (_, t) = c.cmd("s", "MOVE 1 INBOX").await;
    assert_eq!(t, "s NO Cannot move messages into the same mailbox");
    let u = c.ok("t", "STATUS Archive (MESSAGES DELETED)").await;
    assert_eq!(u, vec!["* STATUS \"Archive\" (MESSAGES 4 DELETED 0)"]);
    c.ok("u", "UNSELECT").await;
    let u = c.ok("v", "SELECT Archive").await;
    assert!(u.contains(&"* 4 EXISTS".to_string()), "{u:?}");
    let u = c.ok("w", "UID FETCH 1 (FLAGS BODY.PEEK[])").await;
    assert!(u[0].contains("FLAGS (\\Seen)"), "{u:?}");
    assert!(u[0].contains("saved"), "{u:?}");
    let all = srv
        .service
        .store
        .list(&Default::default())
        .await
        .unwrap()
        .items;
    assert_eq!(
        all.len(),
        3,
        "APPEND stores a message; COPY and MOVE do not"
    );
    assert_eq!(
        all.iter()
            .filter(|m| m.source == MailSource::Inject)
            .count(),
        1
    );
}

#[tokio::test]
async fn noop_reports_changes_made_elsewhere() {
    let srv = Fixture::new().server();
    deliver(&srv, BOX, "one").await;
    deliver(&srv, BOX, "two").await;
    let mut a = logged_in(&srv).await;
    let mut b = logged_in(&srv).await;
    a.ok("a", "SELECT INBOX").await;
    b.ok("a", "SELECT INBOX").await;
    b.ok("b", "STORE 1 +FLAGS (\\Seen \\Deleted)").await;
    b.ok("c", "EXPUNGE").await;
    b.ok("d", "STORE 1 +FLAGS (\\Flagged)").await;
    deliver(&srv, BOX, "three").await;
    let u = a.ok("b", "NOOP").await;
    assert_eq!(
        u,
        vec!["* 1 EXPUNGE", "* 2 EXISTS", "* 1 FETCH (FLAGS (\\Flagged))"]
    );
    let u = a.ok("c", "FETCH 1:* UID").await;
    assert_eq!(u, vec!["* 1 FETCH (UID 2)", "* 2 FETCH (UID 3)"]);
}

#[tokio::test]
async fn idle_reports_new_messages_and_expunges() {
    let srv = Fixture::new().server();
    deliver(&srv, BOX, "one").await;
    let mut c = logged_in(&srv).await;
    c.ok("a", "SELECT INBOX").await;
    c.send(b"b IDLE\r\n").await;
    assert_eq!(c.line().await, "+ idling");
    deliver(&srv, "Alice+x@billing.example.test", "two").await;
    assert_eq!(c.line().await, "* 2 EXISTS");
    deliver(&srv, "bob@billing.example.test", "other").await;
    srv.service
        .store
        .mailbox_remove(&inbox(), &[1])
        .await
        .unwrap();
    deliver(&srv, BOX, "three").await;
    assert_eq!(c.line().await, "* 1 EXPUNGE");
    assert_eq!(c.line().await, "* 2 EXISTS");
    c.send(b"DONE\r\n").await;
    assert_eq!(c.line().await, "b OK IDLE terminated");
    c.ok("c", "NOOP").await;
}

#[tokio::test(start_paused = true)]
async fn idle_ends_at_its_time_limit() {
    let mut f = Fixture::new();
    f.cfg.imap_idle_timeout = Duration::from_secs(60);
    let srv = f.server();
    let mut c = logged_in(&srv).await;
    c.wait = Duration::from_secs(7200);
    c.send(b"a IDLE\r\n").await;
    assert_eq!(c.line().await, "+ idling");
    assert_eq!(c.line().await, "* BYE IDLE time limit reached");
    assert!(c.closed().await);
}

#[tokio::test(start_paused = true)]
async fn idle_and_command_waits_end_at_shutdown_and_timeouts() {
    let srv = Fixture::new().server();
    let (tx, rx) = watch::channel(false);
    let (mut c, h) = start(&srv, false, Some(rx));
    c.line().await;
    c.ok("l", &format!("LOGIN {BOX} tok-billing")).await;
    c.send(b"a IDLE\r\n").await;
    assert_eq!(c.line().await, "+ idling");
    tx.send(true).unwrap();
    assert_eq!(c.line().await, "* BYE Server shutting down");
    assert_eq!(h.await.unwrap(), SessionEnd::Shutdown);

    let (mut c, h) = connect(&srv).await;
    c.wait = Duration::from_secs(7200);
    assert_eq!(c.line().await, "* BYE Autologout; idle for too long");
    assert_eq!(h.await.unwrap(), SessionEnd::Timeout);
}

#[tokio::test]
async fn literal_and_line_limits() {
    let mut f = Fixture::new();
    f.cfg.max_message_bytes = 100;
    f.cfg.max_line_bytes = 512;
    let srv = f.server();
    let mut c = logged_in(&srv).await;
    let (_, t) = c.cmd("a", "APPEND INBOX {101}").await;
    assert_eq!(t, "a NO [TOOBIG] Message too large");
    let (_, t) = c.cmd("b", "LOGIN {513}").await;
    assert_eq!(t, "b BAD Literal too large");
    c.send(b"c APPEND INBOX {10+}\r\n0123456789\r\n").await;
    let (_, t) = c.until("c").await;
    assert!(
        t.starts_with("c OK [APPENDUID"),
        "non-synchronizing literal: {t}"
    );
    c.send(format!("d NOOP {}\r\n", "x".repeat(600)).as_bytes())
        .await;
    assert_eq!(c.line().await, "* BYE Line too long");
    assert!(c.closed().await);

    let mut c = logged_in(&srv).await;
    c.send(b"e APPEND INBOX {200+}\r\n").await;
    assert_eq!(c.line().await, "* BYE Literal too large");
    assert!(c.closed().await);
}

#[tokio::test]
async fn bad_commands_close_after_the_limit() {
    let srv = Fixture::new().server();
    let (mut c, h) = connect(&srv).await;
    for i in 0..MAX_BAD_COMMANDS - 1 {
        let (_, t) = c.cmd(&format!("t{i}"), "FROB").await;
        assert_eq!(t, format!("t{i} BAD Command syntax error"));
    }
    c.send(b"* NOOP\r\n").await;
    assert_eq!(c.line().await, "* BAD Command syntax error");
    assert_eq!(c.line().await, "* BYE Too many errors");
    assert_eq!(h.await.unwrap(), SessionEnd::Rejected);
}

#[tokio::test]
async fn utf8_accept_changes_strings() {
    let srv = Fixture::new().server();
    srv.service
        .accept(Incoming::new(
            MailSource::Smtp,
            Envelope {
                rcpt_to: vec![BOX.into()],
                ..Default::default()
            },
            "From: Jürgen <j@example.com>\r\nSubject: Grüße\r\n\r\nx\r\n"
                .as_bytes()
                .to_vec(),
        ))
        .await
        .unwrap();
    let mut c = logged_in(&srv).await;
    c.ok("a", "SELECT INBOX").await;
    let u = c.ok("b", "FETCH 1 ENVELOPE").await;
    assert!(u[0].contains("\"=?utf-8?b?R3LDvMOfZQ==?=\""), "{u:?}");
    c.ok("c", "ENABLE UTF8=ACCEPT").await;
    let u = c.ok("d", "FETCH 1 ENVELOPE").await;
    assert!(
        u[0].contains("\"Grüße\" ((\"Jürgen\" NIL \"j\" \"example.com\"))"),
        "{u:?}"
    );
}

#[tokio::test]
async fn global_mailboxes_need_a_global_token() {
    let srv = Fixture::new().server();
    deliver(&srv, "ops@example.org", "global").await;
    let (mut c, _h) = connect(&srv).await;
    let (_, t) = c.cmd("a", "LOGIN ops@example.org tok-billing").await;
    assert!(t.starts_with("a NO"), "{t}");
    c.ok("b", "LOGIN ops@example.org tok-global").await;
    let u = c.ok("c", "SELECT INBOX").await;
    assert!(u.contains(&"* 1 EXISTS".to_string()), "{u:?}");
}

#[tokio::test]
async fn a_new_address_gets_an_empty_inbox() {
    let srv = Fixture::new().server();
    let mut c = logged_in(&srv).await;
    let u = c.ok("a", "SELECT INBOX").await;
    assert!(u.contains(&"* 0 EXISTS".to_string()), "{u:?}");
    let u = c.ok("b", "FETCH 1:* FLAGS").await;
    assert!(u.is_empty());
    let u = c.ok("c", "SEARCH ALL").await;
    assert_eq!(u, vec!["* SEARCH"]);
}

// ----- literal and FETCH limits -----

#[tokio::test]
async fn literals_before_login_are_limited_to_login() {
    let srv = Fixture::new().server();
    let (mut c, h) = connect(&srv).await;
    c.send(b"a APPEND INBOX {1000}\r\n").await;
    assert_eq!(
        c.line().await,
        "a BAD Literals are accepted only for LOGIN before login"
    );
    c.send(b"b LOGIN {2000}\r\n").await;
    assert_eq!(c.line().await, "b BAD Literal too large");
    c.send(format!("c LOGIN {{{}}}\r\n", BOX.len()).as_bytes())
        .await;
    assert_eq!(c.line().await, "+ Ready for literal data");
    c.send(format!("{BOX} {{11}}\r\n").as_bytes()).await;
    assert_eq!(c.line().await, "+ Ready for literal data");
    c.send(b"tok-billing\r\n").await;
    let (_, t) = c.until("c").await;
    assert!(t.starts_with("c OK"), "{t}");
    drop(c);
    let _ = h.await;

    let (mut c, h) = connect(&srv).await;
    c.send(b"d LOGIN {5+}\r\nalice tok\r\n").await;
    assert_eq!(
        c.line().await,
        "* BYE Non-synchronizing literals are not accepted before login"
    );
    assert!(c.closed().await);
    assert_eq!(h.await.unwrap(), SessionEnd::Rejected);

    let (mut c, _h) = connect(&srv).await;
    c.send(b"e APPEND INBOX {1048576+}\r\n").await;
    assert_eq!(
        c.line().await,
        "* BYE Non-synchronizing literals are not accepted before login"
    );
    assert!(c.closed().await);
}

#[tokio::test]
async fn literal_bytes_per_command_are_capped_after_login() {
    let mut f = Fixture::new();
    f.cfg.max_line_bytes = 4096;
    f.cfg.max_message_bytes = 100_000;
    let srv = f.server();
    let mut c = logged_in(&srv).await;
    c.send(b"a SEARCH TEXT {3000}\r\n").await;
    assert_eq!(c.line().await, "+ Ready for literal data");
    c.send(format!("{} TEXT {{3000}}\r\n", "x".repeat(3000)).as_bytes())
        .await;
    assert_eq!(c.line().await, "a BAD Literal too large");

    c.send(b"b APPEND INBOX {60000}\r\n").await;
    assert_eq!(c.line().await, "+ Ready for literal data");
    let mut first = b"From: a@example.com\r\n\r\n".to_vec();
    first.resize(60_000, b'x');
    c.send(&first).await;
    c.send(b" {50000}\r\n").await;
    assert_eq!(c.line().await, "b NO [TOOBIG] Message too large");
}

async fn deliver_sized(srv: &ImapServer, size: usize) {
    let mut raw = format!("From: a@example.com\r\nTo: {BOX}\r\nSubject: big\r\n\r\n").into_bytes();
    raw.resize(size, b'y');
    srv.service
        .accept(Incoming::new(
            MailSource::Smtp,
            Envelope {
                mail_from: "a@example.com".into(),
                rcpt_to: vec![BOX.into()],
                ..Default::default()
            },
            raw,
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn fetch_sections_and_response_size_are_capped() {
    let mut f = Fixture::new();
    f.cfg.max_message_bytes = 32 * 1024;
    let srv = f.server();
    deliver_sized(&srv, 20 * 1024).await;
    let mut c = logged_in(&srv).await;
    c.ok("a", "SELECT INBOX").await;
    let items = ["BODY.PEEK[]"; 17].join(" ");
    let (u, t) = c.cmd("b", &format!("FETCH 1 ({items})")).await;
    assert!(u.is_empty(), "{u:?}");
    assert_eq!(t, "b BAD At most 16 body sections in one FETCH");
    let items = ["BODY.PEEK[]"; 8].join(" ");
    let (u, t) = c.cmd("c", &format!("FETCH 1 ({items})")).await;
    assert!(u.is_empty(), "{}", u.len());
    assert_eq!(
        t,
        "c NO [LIMIT] FETCH response too large; fetch fewer messages or sections"
    );
    let u = c
        .ok("d", "FETCH 1 (BODY.PEEK[] BODY.PEEK[HEADER] RFC822.SIZE)")
        .await;
    assert_eq!(u.len(), 1);
    assert!(u[0].contains("BODY[] {20480}"), "{}", &u[0][..80]);
}

#[tokio::test(start_paused = true)]
async fn idle_time_does_not_count_towards_the_session_limit() {
    let mut f = Fixture::new();
    f.cfg.session_timeout = Duration::from_secs(60);
    f.cfg.imap_idle_timeout = Duration::from_secs(1800);
    let srv = f.server();
    let mut c = logged_in(&srv).await;
    c.ok("s", "SELECT INBOX").await;
    c.wait = Duration::from_secs(7200);
    c.send(b"a IDLE\r\n").await;
    assert_eq!(c.line().await, "+ idling");
    tokio::time::sleep(Duration::from_secs(300)).await;
    c.send(b"DONE\r\n").await;
    assert_eq!(c.line().await, "a OK IDLE terminated");
    c.ok("b", "NOOP").await;
}

#[tokio::test]
async fn read_only_tokens_cannot_change_the_mailbox() {
    let srv = Fixture::new().server();
    deliver(&srv, BOX, "one").await;
    let (mut c, _h) = connect(&srv).await;
    c.ok("l", &format!("LOGIN {BOX} ro-billing")).await;
    c.ok("a", "SELECT INBOX").await;
    let nope = "NO [NOPERM] The token does not allow changing this mailbox (mail:write)";
    let (_, t) = c.cmd("b", "STORE 1 +FLAGS (\\Deleted)").await;
    assert_eq!(t, format!("b {nope}"));
    c.ok("c", "STORE 1 +FLAGS (\\Seen)").await;
    let (_, t) = c.cmd("d", "EXPUNGE").await;
    assert_eq!(
        t,
        "d NO [NOPERM] The token does not allow expunging messages (mail:write or mail:delete)"
    );
    let (_, t) = c.cmd("e", "CREATE Archive").await;
    assert_eq!(t, format!("e {nope}"));
    let (_, t) = c.cmd("f", "COPY 1 INBOX").await;
    assert_eq!(t, format!("f {nope}"));
    let (_, t) = c.cmd("g", "MOVE 1 INBOX").await;
    assert_eq!(t, format!("g {nope}"));
    c.send(b"h APPEND INBOX {24}\r\n").await;
    assert_eq!(c.line().await, "+ Ready for literal data");
    c.send(b"From: a@example.com\r\n\r\nx\r\n").await;
    let (_, t) = c.until("h").await;
    assert_eq!(t, format!("h {nope}"));
    c.ok("i", "FETCH 1 (FLAGS BODY.PEEK[])").await;
    c.ok("j", "CLOSE").await;
    assert_eq!(
        srv.service
            .store
            .mailbox_entries(&inbox())
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn failed_logins_are_throttled_across_connections() {
    let srv = Fixture::new().server();
    for i in 0..5 {
        let (mut c, _h) = connect(&srv).await;
        let (_, t) = c.cmd(&format!("a{i}"), &format!("LOGIN {BOX} wrong")).await;
        assert!(t.contains("NO"), "{t}");
    }
    let (mut c, _h) = connect(&srv).await;
    let (_, t) = c.cmd("b", &format!("LOGIN {BOX} tok-billing")).await;
    assert_eq!(t, "b NO [AUTHENTICATIONFAILED] Authentication failed");
}

#[tokio::test]
async fn keywords_per_message_are_bounded() {
    let srv = Fixture::new().server();
    deliver(&srv, BOX, "one").await;
    let mut c = logged_in(&srv).await;
    c.ok("a", "SELECT INBOX").await;
    let many: Vec<String> = (0..33).map(|i| format!("k{i}")).collect();
    let (_, t) = c
        .cmd("b", &format!("STORE 1 +FLAGS ({})", many.join(" ")))
        .await;
    assert_eq!(t, "b NO [LIMIT] At most 32 keywords per message");
    let (_, t) = c
        .cmd("c", &format!("STORE 1 +FLAGS ({})", "x".repeat(65)))
        .await;
    assert_eq!(t, "c BAD Keywords are at most 64 bytes");
    c.ok("d", &format!("STORE 1 +FLAGS ({})", many[..30].join(" ")))
        .await;
    let (_, t) = c
        .cmd("e", &format!("STORE 1 +FLAGS ({})", many[30..].join(" ")))
        .await;
    assert_eq!(t, "e NO [LIMIT] At most 32 keywords per message");
    c.ok("f", &format!("STORE 1 FLAGS ({})", many[..32].join(" ")))
        .await;
}

#[tokio::test]
async fn list_patterns_are_capped() {
    let srv = Fixture::new().server();
    let mut c = logged_in(&srv).await;
    let long = "*".repeat(1100);
    let (_, t) = c.cmd("a", &format!("LIST \"\" \"{long}\"")).await;
    assert_eq!(t, "a BAD Pattern too long");
    let u = c.ok("b", "LIST \"\" \"*\"").await;
    assert_eq!(u.len(), 1, "{u:?}");
}
