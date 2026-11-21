//! One IMAP session: framing of lines and literals, the state machine (not authenticated,
//! authenticated, selected), mailbox commands over [`crate::MailStore`] and IDLE over
//! [`crate::MailEvents`].

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use fastmock_core::mail::{
    Envelope, FlagOp, INBOX, MailEventKind, MailMessage, MailSource, MailSubscriptionError,
    MailboxDelivery, MailboxEntry, MailboxKey, folder_name,
};
use fastmock_core::model::SpaceFilter;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;
use tokio::time::Instant;

use super::codec::{self, guess_tag};
use super::command::{Cmd, FetchItem, Request, SearchKey, SeqNum, SeqSet, StatusItem, StoreOp};
use super::mime::MessageView;
use super::response::{flag_list, internal_date, literal, nstring, string};
use super::search::{Candidate, SearchDoc, matches};
use super::{ImapServer, SessionEnd};
use crate::config::MailConfig;
use crate::mailbox_login::LoginOk;
use crate::service::{AcceptError, Destination, Incoming, MailService};

/// Most commands in one session.
pub const MAX_COMMANDS: u32 = 10_000;
/// Consecutive commands answered with BAD before the connection is closed.
pub const MAX_BAD_COMMANDS: u32 = 20;
/// Folders a mailbox may have.
pub const MAX_FOLDERS: usize = 100;
/// Wait before answering a failed login.
pub const AUTH_FAILURE_DELAY: Duration = Duration::from_millis(300);
/// How often IDLE re-reads the mailbox when the events cannot be lost.
pub const IDLE_POLL: Duration = Duration::from_secs(5);
/// Literals in one command.
const MAX_LITERALS: usize = 64;
/// Longest folder name in bytes.
const MAX_FOLDER_NAME: usize = 255;
const READ_CHUNK: usize = 16 * 1024;
const FLUSH_AT: usize = 256 * 1024;
const SYSTEM_FLAGS: &str = "(\\Answered \\Flagged \\Deleted \\Seen \\Draft)";

pub(crate) trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

type Io = Box<dyn Stream>;

#[derive(Debug)]
enum Close {
    Logout,
    Eof,
    IdleTimeout,
    LiteralTimeout,
    SessionTimeout,
    IdleExpired,
    Shutdown,
    /// A `* BYE` that closes was already queued.
    Rejected,
    Io,
}

impl Close {
    fn end(&self) -> SessionEnd {
        match self {
            Close::Logout => SessionEnd::Quit,
            Close::Eof => SessionEnd::Disconnected,
            Close::IdleTimeout
            | Close::LiteralTimeout
            | Close::SessionTimeout
            | Close::IdleExpired => SessionEnd::Timeout,
            Close::Shutdown => SessionEnd::Shutdown,
            Close::Rejected => SessionEnd::Rejected,
            Close::Io => SessionEnd::Error,
        }
    }
}

type Step<T = ()> = Result<T, Close>;

fn rejected(reason: &'static str) {
    metrics::counter!("fastmock_mail_rejected_total", "reason" => reason).increment(1);
}

/// A command message as framed from the stream, or a command already answered while framing.
enum Framed {
    Message(Vec<u8>),
    Answered,
}

/// A message of the selected folder as the client knows it; sequence numbers are positions.
#[derive(Clone, Debug)]
struct Known {
    uid: u32,
    flags: Vec<String>,
}

struct Selected {
    key: MailboxKey,
    read_only: bool,
    known: Vec<Known>,
}

/// The literal a line announces: `{n}` (synchronizing) or `{n+}` at its end.
fn literal_announcement(line: &[u8]) -> Option<(u32, bool)> {
    let l = line.strip_suffix(b"\n").unwrap_or(line);
    let l = l.strip_suffix(b"\r").unwrap_or(l);
    let l = l.strip_suffix(b"}")?;
    let open = l.iter().rposition(|b| *b == b'{')?;
    let inner = &l[open + 1..];
    let (digits, sync) = match inner.strip_suffix(b"+") {
        Some(d) => (d, false),
        None => (inner, true),
    };
    if digits.is_empty() || digits.len() > 10 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let n: u64 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    Some((u32::try_from(n).ok()?, sync))
}

/// The command name after the tag, upper-cased.
fn command_name(msg: &[u8]) -> String {
    msg.split(|b| *b == b' ')
        .nth(1)
        .map(|n| {
            String::from_utf8_lossy(n)
                .trim_end_matches(['\r', '\n'])
                .to_ascii_uppercase()
        })
        .unwrap_or_default()
}

/// Matches a LIST pattern: `*` is anything, `%` anything but the delimiter `/`.
pub(crate) fn list_match(pattern: &[u8], name: &[u8]) -> bool {
    let (p, n) = (pattern, name);
    let mut dp = vec![vec![false; n.len() + 1]; p.len() + 1];
    dp[p.len()][n.len()] = true;
    for i in (0..p.len()).rev() {
        for j in (0..=n.len()).rev() {
            dp[i][j] = match p[i] {
                b'*' => dp[i + 1][j] || (j < n.len() && dp[i][j + 1]),
                b'%' => dp[i + 1][j] || (j < n.len() && n[j] != b'/' && dp[i][j + 1]),
                c => j < n.len() && n[j] == c && dp[i + 1][j + 1],
            };
        }
    }
    dp[0][0]
}

/// `1:3,5` from ascending numbers.
pub(crate) fn compress(nums: &[u32]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < nums.len() {
        let start = nums[i];
        let mut end = start;
        while i + 1 < nums.len() && nums[i + 1] == end + 1 {
            end += 1;
            i += 1;
        }
        out.push(if start == end {
            start.to_string()
        } else {
            format!("{start}:{end}")
        });
        i += 1;
    }
    out.join(",")
}

/// A folder name a client may create.
fn valid_folder(name: &str) -> Option<String> {
    let name = name.trim_end_matches('/');
    let ok = !name.is_empty()
        && name.len() <= MAX_FOLDER_NAME
        && !name.chars().any(|c| c.is_control() || c == '*' || c == '%')
        && !name.starts_with('/')
        && !name.contains("//");
    ok.then(|| folder_name(name))
}

pub(crate) struct Session<'a> {
    srv: &'a ImapServer,
    cfg: &'a MailConfig,
    io: Option<Io>,
    rbuf: Vec<u8>,
    wbuf: Vec<u8>,
    peer: SocketAddr,
    deadline: Instant,
    shutdown: Option<watch::Receiver<bool>>,
    tls: bool,
    utf8: bool,
    user: Option<LoginOk>,
    selected: Option<Selected>,
    auth_failures: u32,
    commands: u32,
    bad_commands: u32,
}

impl<'a> Session<'a> {
    pub(crate) fn new(
        srv: &'a ImapServer,
        io: Io,
        peer: SocketAddr,
        shutdown: Option<watch::Receiver<bool>>,
    ) -> Self {
        let cfg = &*srv.service.config;
        Self {
            srv,
            cfg,
            io: Some(io),
            rbuf: Vec::with_capacity(1024),
            wbuf: Vec::with_capacity(1024),
            peer,
            deadline: Instant::now() + cfg.session_timeout,
            shutdown,
            tls: false,
            utf8: false,
            user: None,
            selected: None,
            auth_failures: 0,
            commands: 0,
            bad_commands: 0,
        }
    }

    // ----- output -----

    fn line(&mut self, s: &str) {
        self.wbuf.extend_from_slice(s.as_bytes());
        self.wbuf.extend_from_slice(b"\r\n");
    }

    fn tagged(&mut self, tag: &str, status: &str, text: &str) {
        self.line(&format!("{tag} {status} {text}"));
    }

    async fn flush(&mut self) -> Step {
        if self.wbuf.is_empty() {
            return Ok(());
        }
        let io = self.io.as_mut().ok_or(Close::Io)?;
        let buf = std::mem::take(&mut self.wbuf);
        match tokio::time::timeout(self.cfg.idle_timeout, async {
            io.write_all(&buf).await?;
            io.flush().await
        })
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(Close::Io),
            Err(_) => Err(Close::IdleTimeout),
        }
    }

    async fn flush_if_large(&mut self) -> Step {
        if self.wbuf.len() >= FLUSH_AT {
            self.flush().await?;
        }
        Ok(())
    }

    // ----- input -----

    /// Reads more bytes after sending the queued output. `interruptible`: waiting for a command (idle
    /// timeout, shutdown); otherwise inside a command (literal timeout).
    async fn fill(&mut self, interruptible: bool) -> Step {
        self.flush().await?;
        let now = Instant::now();
        if now >= self.deadline {
            return Err(Close::SessionTimeout);
        }
        let limit = if interruptible {
            self.cfg.idle_timeout
        } else {
            self.cfg.data_timeout
        };
        let wait = limit.min(self.deadline - now);
        self.rbuf.reserve(READ_CHUNK);
        let io = self.io.as_mut().ok_or(Close::Io)?;
        let shutdown = self.shutdown.as_mut().filter(|_| interruptible);
        let read = tokio::time::timeout(wait, io.read_buf(&mut self.rbuf));
        let res = match shutdown {
            Some(s) => {
                if *s.borrow() {
                    return Err(Close::Shutdown);
                }
                tokio::select! {
                    r = read => r,
                    _ = s.changed() => return Err(Close::Shutdown),
                }
            }
            None => read.await,
        };
        match res {
            Err(_) if wait < limit => Err(Close::SessionTimeout),
            Err(_) if interruptible => Err(Close::IdleTimeout),
            Err(_) => Err(Close::LiteralTimeout),
            Ok(Ok(0)) => Err(Close::Eof),
            Ok(Ok(_)) => Ok(()),
            Ok(Err(_)) => Err(Close::Io),
        }
    }

    /// One line including its LF; closes with BYE when it is longer than the line limit.
    async fn next_line(&mut self, interruptible: bool) -> Step<Vec<u8>> {
        let max = self.cfg.max_line_bytes;
        loop {
            if let Some(pos) = self.rbuf.iter().position(|b| *b == b'\n') {
                if pos + 1 > max {
                    return Err(self.bye_rejected("line_too_long", "Line too long"));
                }
                return Ok(self.rbuf.drain(..=pos).collect());
            }
            if self.rbuf.len() > max {
                return Err(self.bye_rejected("line_too_long", "Line too long"));
            }
            self.fill(interruptible).await?;
        }
    }

    async fn read_exact(&mut self, n: usize) -> Step<Vec<u8>> {
        while self.rbuf.len() < n {
            self.fill(false).await?;
        }
        Ok(self.rbuf.drain(..n).collect())
    }

    fn bye_rejected(&mut self, reason: &'static str, text: &str) -> Close {
        rejected(reason);
        self.line(&format!("* BYE {text}"));
        Close::Rejected
    }

    /// Reads one command with its literals. Synchronizing literals get a continuation request; a
    /// literal over the limit (the message size for APPEND, the line size otherwise) is refused.
    async fn read_command(&mut self) -> Step<Framed> {
        let mut msg = self.next_line(true).await?;
        let mut literals = 0;
        while let Some((len, sync)) = literal_announcement(&msg) {
            literals += 1;
            let append = command_name(&msg) == "APPEND";
            let limit = if append {
                self.cfg.max_message_bytes
            } else {
                self.cfg.max_line_bytes
            };
            let len = len as usize;
            if len > limit || literals > MAX_LITERALS {
                if !sync {
                    return Err(self.bye_rejected("literal_too_large", "Literal too large"));
                }
                rejected("literal_too_large");
                let tag = guess_tag(&msg).unwrap_or_else(|| "*".into());
                if append {
                    self.tagged(&tag, "NO", "[TOOBIG] Message too large");
                } else {
                    self.tagged(&tag, "BAD", "Literal too large");
                }
                self.bad_command()?;
                return Ok(Framed::Answered);
            }
            if sync {
                self.line("+ Ready for literal data");
            }
            let data = self.read_exact(len).await?;
            msg.extend_from_slice(&data);
            let rest = self.next_line(false).await?;
            msg.extend_from_slice(&rest);
        }
        Ok(Framed::Message(msg))
    }

    // ----- state -----

    fn login_allowed(&self) -> bool {
        self.tls || self.srv.tls.is_none() || self.cfg.allow_plaintext_auth
    }

    fn capabilities(&self) -> String {
        let mut c = vec!["IMAP4rev1".to_string()];
        if self.user.is_none() {
            if self.srv.tls.is_some() && !self.tls {
                c.push("STARTTLS".into());
            }
            if self.login_allowed() {
                c.push("AUTH=PLAIN".into());
                c.push("SASL-IR".into());
            } else {
                c.push("LOGINDISABLED".into());
            }
        }
        for s in [
            "ID",
            "ENABLE",
            "IDLE",
            "NAMESPACE",
            "UNSELECT",
            "UIDPLUS",
            "MOVE",
            "UTF8=ACCEPT",
        ] {
            c.push(s.into());
        }
        c.push(format!("APPENDLIMIT={}", self.cfg.max_message_bytes));
        c.join(" ")
    }

    fn bad_command(&mut self) -> Step {
        self.bad_commands += 1;
        if self.bad_commands >= MAX_BAD_COMMANDS {
            return Err(self.bye_rejected("too_many_errors", "Too many errors"));
        }
        Ok(())
    }

    pub(crate) async fn drive(&mut self, implicit_tls: bool) -> SessionEnd {
        let r = self.run(implicit_tls).await;
        self.close(r).await
    }

    async fn run(&mut self, implicit_tls: bool) -> Step {
        if implicit_tls {
            self.handshake().await?;
        }
        let caps = self.capabilities();
        self.line(&format!(
            "* OK [CAPABILITY {caps}] FastMock IMAP4rev1 ready"
        ));
        loop {
            let framed = self.read_command().await?;
            self.commands += 1;
            if self.commands > MAX_COMMANDS {
                return Err(self.bye_rejected("too_many_commands", "Too many commands"));
            }
            let msg = match framed {
                Framed::Message(m) => m,
                Framed::Answered => continue,
            };
            match codec::parse(&msg) {
                Ok(req) => {
                    let good = self.command(req).await?;
                    if good {
                        self.bad_commands = 0;
                    } else {
                        self.bad_command()?;
                    }
                }
                Err(e) => {
                    let tag = e.tag.unwrap_or_else(|| "*".into());
                    self.tagged(&tag, "BAD", e.reason);
                    self.bad_command()?;
                }
            }
        }
    }

    /// Handles a command; returns false for BAD answers (they count towards [`MAX_BAD_COMMANDS`]).
    async fn command(&mut self, req: Request) -> Step<bool> {
        let Request { tag, cmd } = req;
        let authed = self.user.is_some();
        let selected = self.selected.is_some();
        match &cmd {
            Cmd::Capability | Cmd::Noop | Cmd::Logout | Cmd::Id => {}
            Cmd::StartTls | Cmd::Login { .. } | Cmd::Authenticate { .. } if authed => {
                self.tagged(&tag, "BAD", "Already logged in");
                return Ok(false);
            }
            Cmd::StartTls | Cmd::Login { .. } | Cmd::Authenticate { .. } => {}
            _ if !authed => {
                self.tagged(&tag, "BAD", "Log in first");
                return Ok(false);
            }
            Cmd::Check
            | Cmd::Close
            | Cmd::Unselect
            | Cmd::Expunge(_)
            | Cmd::Search { .. }
            | Cmd::Fetch { .. }
            | Cmd::Store { .. }
            | Cmd::Copy { .. }
                if !selected =>
            {
                self.tagged(&tag, "BAD", "Select a mailbox first");
                return Ok(false);
            }
            _ => {}
        }
        match cmd {
            Cmd::Capability => {
                let caps = self.capabilities();
                self.line(&format!("* CAPABILITY {caps}"));
                self.tagged(&tag, "OK", "CAPABILITY completed");
            }
            Cmd::Noop | Cmd::Check => {
                self.sync(true).await;
                self.tagged(&tag, "OK", &format!("{} completed", cmd.name()));
            }
            Cmd::Logout => {
                self.line("* BYE FastMock logging out");
                self.tagged(&tag, "OK", "LOGOUT completed");
                return Err(Close::Logout);
            }
            Cmd::Id => {
                self.line("* ID (\"name\" \"FastMock\")");
                self.tagged(&tag, "OK", "ID completed");
            }
            Cmd::StartTls => return self.starttls(&tag).await,
            Cmd::Login { user, password } => {
                if !self.login_allowed() {
                    rejected("auth_before_tls");
                    self.tagged(
                        &tag,
                        "NO",
                        "[PRIVACYREQUIRED] LOGIN is disabled before STARTTLS",
                    );
                    return Ok(true);
                }
                return self.login(&tag, &user, &password.0).await;
            }
            Cmd::Authenticate { mechanism, initial } => {
                return self
                    .authenticate(&tag, &mechanism, initial.map(|s| s.0))
                    .await;
            }
            Cmd::Enable(caps) => {
                let mut enabled = Vec::new();
                for c in caps {
                    if c == "UTF8=ACCEPT" {
                        self.utf8 = true;
                        enabled.push(c);
                    }
                }
                self.line(format!("* ENABLED {}", enabled.join(" ")).trim_end());
                self.tagged(&tag, "OK", "ENABLE completed");
            }
            Cmd::Namespace => {
                self.line("* NAMESPACE ((\"\" \"/\")) NIL NIL");
                self.tagged(&tag, "OK", "NAMESPACE completed");
            }
            Cmd::Select { mailbox, read_only } => self.select(&tag, &mailbox, read_only).await,
            Cmd::Create(name) => self.create(&tag, &name).await,
            Cmd::Delete(name) => self.delete(&tag, &name).await,
            Cmd::Rename => self.tagged(&tag, "NO", "RENAME is not supported"),
            Cmd::Subscribe(_) | Cmd::Unsubscribe(_) => {
                self.tagged(&tag, "OK", &format!("{} completed", cmd.name()));
            }
            Cmd::List {
                reference,
                pattern,
                lsub,
            } => self.list(&tag, &reference, &pattern, lsub).await,
            Cmd::Status { mailbox, items } => self.status(&tag, &mailbox, &items).await,
            Cmd::Append {
                mailbox,
                flags,
                message,
            } => self.append(&tag, &mailbox, flags, message).await,
            Cmd::Close => {
                if let Some(s) = self.selected.take()
                    && !s.read_only
                    && let Err(e) = self.srv.service.store.mailbox_expunge(&s.key, None).await
                {
                    tracing::warn!(error = %e, "IMAP CLOSE expunge failed");
                }
                self.tagged(&tag, "OK", "CLOSE completed");
            }
            Cmd::Unselect => {
                self.selected = None;
                self.tagged(&tag, "OK", "UNSELECT completed");
            }
            Cmd::Expunge(uids) => self.expunge(&tag, uids).await,
            Cmd::Search { charset, key, uid } => {
                return self.search(&tag, charset.as_deref(), &key, uid).await;
            }
            Cmd::Fetch { set, items, uid } => return self.fetch(&tag, &set, &items, uid).await,
            Cmd::Store {
                set,
                op,
                silent,
                flags,
                uid,
            } => return self.store_flags(&tag, &set, op, silent, flags, uid).await,
            Cmd::Copy {
                set,
                mailbox,
                uid,
                remove,
            } => return self.copy(&tag, &set, &mailbox, uid, remove).await,
            Cmd::Idle => return self.idle(&tag).await,
            Cmd::Unsupported(name) => {
                self.tagged(&tag, "BAD", &format!("{name} is not supported"));
                return Ok(false);
            }
        }
        Ok(true)
    }

    // ----- not authenticated -----

    async fn handshake(&mut self) -> Step {
        let Some(cfg) = self.srv.tls.clone() else {
            return Err(Close::Io);
        };
        let io = self.io.take().ok_or(Close::Io)?;
        let acceptor = tokio_rustls::TlsAcceptor::from(cfg);
        let wait = self
            .cfg
            .idle_timeout
            .min(self.deadline.saturating_duration_since(Instant::now()));
        match tokio::time::timeout(wait, acceptor.accept(io)).await {
            Ok(Ok(s)) => {
                self.io = Some(Box::new(s));
                self.tls = true;
                Ok(())
            }
            Ok(Err(e)) => {
                rejected("tls_handshake");
                tracing::debug!(error = %e, peer = %self.peer, "IMAP TLS handshake failed");
                Err(Close::Io)
            }
            Err(_) => Err(Close::Io),
        }
    }

    async fn starttls(&mut self, tag: &str) -> Step<bool> {
        if self.srv.tls.is_none() {
            self.tagged(tag, "BAD", "STARTTLS is not offered");
            return Ok(false);
        }
        if self.tls {
            self.tagged(tag, "BAD", "TLS is already active");
            return Ok(false);
        }
        self.tagged(tag, "OK", "Begin TLS negotiation now");
        self.flush().await?;
        if !self.rbuf.is_empty() {
            rejected("starttls_pipelined");
            tracing::warn!(peer = %self.peer, "IMAP client sent bytes after STARTTLS; closing");
            return Err(Close::Rejected);
        }
        self.handshake().await?;
        self.utf8 = false;
        Ok(true)
    }

    async fn authenticate(
        &mut self,
        tag: &str,
        mechanism: &str,
        initial: Option<Vec<u8>>,
    ) -> Step<bool> {
        if mechanism != "PLAIN" {
            self.tagged(tag, "NO", "Unsupported authentication mechanism");
            return Ok(true);
        }
        if !self.login_allowed() {
            rejected("auth_before_tls");
            self.tagged(
                tag,
                "NO",
                "[PRIVACYREQUIRED] Authentication is disabled before STARTTLS",
            );
            return Ok(true);
        }
        let data = match initial {
            Some(d) => d,
            None => {
                self.line("+ ");
                let line = self.next_line(false).await?;
                let text = String::from_utf8_lossy(&line);
                let text = text.trim_end_matches(['\r', '\n']);
                if text == "*" {
                    self.tagged(tag, "BAD", "Authentication cancelled");
                    return Ok(false);
                }
                match B64.decode(text) {
                    Ok(d) => d,
                    Err(_) => {
                        self.tagged(tag, "BAD", "Invalid base64");
                        return Ok(false);
                    }
                }
            }
        };
        let mut parts = data.split(|b| *b == 0);
        let (authz, user, pass) = (parts.next(), parts.next(), parts.next());
        let (Some(authz), Some(user), Some(pass), None) = (authz, user, pass, parts.next()) else {
            return self.login_failed(tag).await;
        };
        if !authz.is_empty() && authz != user {
            return self.login_failed(tag).await;
        }
        self.login(tag, user, pass).await
    }

    async fn login(&mut self, tag: &str, user: &[u8], password: &[u8]) -> Step<bool> {
        let svc = &self.srv.service;
        let found = match (std::str::from_utf8(user), std::str::from_utf8(password)) {
            (Ok(user), Ok(password)) => self.srv.login.check(user, password).await,
            _ => None,
        };
        let Some(u) = found else {
            return self.login_failed(tag).await;
        };
        if let Err(e) = svc
            .store
            .mailbox_create(&MailboxKey::inbox(u.area.as_deref(), &u.address))
            .await
        {
            tracing::warn!(error = %e, "creating an IMAP INBOX failed");
        }
        self.user = Some(u);
        let caps = self.capabilities();
        self.tagged(tag, "OK", &format!("[CAPABILITY {caps}] Logged in"));
        Ok(true)
    }

    async fn login_failed(&mut self, tag: &str) -> Step<bool> {
        self.auth_failures += 1;
        rejected("auth_failed");
        if self.auth_failures >= self.cfg.max_auth_failures {
            self.tagged(tag, "NO", "[AUTHENTICATIONFAILED] Authentication failed");
            return Err(self.bye_rejected(
                "too_many_auth_failures",
                "Too many failed authentication attempts",
            ));
        }
        self.flush().await?;
        tokio::time::sleep(AUTH_FAILURE_DELAY).await;
        self.tagged(tag, "NO", "[AUTHENTICATIONFAILED] Authentication failed");
        Ok(true)
    }

    // ----- authenticated -----

    fn user(&self) -> &LoginOk {
        self.user.as_ref().expect("checked by command()")
    }

    fn key(&self, folder: &str) -> MailboxKey {
        let u = self.user();
        MailboxKey {
            space: u.area.clone(),
            address: u.address.clone(),
            folder: folder_name(folder.trim_end_matches('/')),
        }
    }

    fn name_bytes(&self, name: &str) -> Vec<u8> {
        let mut o = Vec::new();
        string(&mut o, name.as_bytes(), self.utf8);
        o
    }

    fn store_failed(&mut self, tag: &str, e: anyhow::Error) {
        tracing::warn!(error = %e, "IMAP mailbox operation failed");
        self.tagged(tag, "NO", "[SERVERBUG] The mailbox store failed; try again");
    }

    async fn select(&mut self, tag: &str, name: &str, read_only: bool) {
        self.selected = None;
        let key = self.key(name);
        let store = self.srv.service.store.clone();
        let info = match store.mailbox_info(&key).await {
            Ok(Some(i)) => i,
            Ok(None) => {
                self.tagged(tag, "NO", "[NONEXISTENT] No such mailbox");
                return;
            }
            Err(e) => return self.store_failed(tag, e),
        };
        let entries = match store.mailbox_entries(&key).await {
            Ok(e) => e,
            Err(e) => return self.store_failed(tag, e),
        };
        self.line(&format!("* FLAGS {SYSTEM_FLAGS}"));
        if read_only {
            self.line("* OK [PERMANENTFLAGS ()] Read-only mailbox");
        } else {
            self.line(
                "* OK [PERMANENTFLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft \\*)] Flags permitted",
            );
        }
        self.line(&format!("* {} EXISTS", entries.len()));
        self.line("* 0 RECENT");
        if let Some(i) = entries.iter().position(|e| !e.has_flag("\\Seen")) {
            self.line(&format!("* OK [UNSEEN {}] First unseen", i + 1));
        }
        self.line(&format!(
            "* OK [UIDVALIDITY {}] UIDs valid",
            info.uid_validity
        ));
        self.line(&format!(
            "* OK [UIDNEXT {}] Predicted next UID",
            info.uid_next
        ));
        self.selected = Some(Selected {
            key,
            read_only,
            known: entries
                .into_iter()
                .map(|e| Known {
                    uid: e.uid,
                    flags: e.flags,
                })
                .collect(),
        });
        let (code, cmd) = if read_only {
            ("READ-ONLY", "EXAMINE")
        } else {
            ("READ-WRITE", "SELECT")
        };
        self.tagged(tag, "OK", &format!("[{code}] {cmd} completed"));
    }

    async fn create(&mut self, tag: &str, name: &str) {
        let Some(folder) = valid_folder(name) else {
            self.tagged(tag, "NO", "Invalid mailbox name");
            return;
        };
        if folder == INBOX {
            self.tagged(tag, "NO", "[ALREADYEXISTS] INBOX always exists");
            return;
        }
        let store = self.srv.service.store.clone();
        let u = self.user().clone();
        match store.mailbox_folders(u.area.as_deref(), &u.address).await {
            Ok(f) if f.len() >= MAX_FOLDERS => {
                self.tagged(tag, "NO", "[LIMIT] Too many mailboxes");
                return;
            }
            Ok(_) => {}
            Err(e) => return self.store_failed(tag, e),
        }
        match store.mailbox_create(&self.key(&folder)).await {
            Ok(true) => self.tagged(tag, "OK", "CREATE completed"),
            Ok(false) => self.tagged(tag, "NO", "[ALREADYEXISTS] Mailbox already exists"),
            Err(e) => self.store_failed(tag, e),
        }
    }

    async fn delete(&mut self, tag: &str, name: &str) {
        let key = self.key(name);
        if key.folder == INBOX {
            self.tagged(tag, "NO", "INBOX cannot be deleted");
            return;
        }
        match self.srv.service.store.mailbox_delete(&key).await {
            Ok(true) => {
                if self.selected.as_ref().is_some_and(|s| s.key == key) {
                    self.selected = None;
                }
                self.tagged(tag, "OK", "DELETE completed");
            }
            Ok(false) => self.tagged(tag, "NO", "[NONEXISTENT] No such mailbox"),
            Err(e) => self.store_failed(tag, e),
        }
    }

    async fn list(&mut self, tag: &str, reference: &str, pattern: &str, lsub: bool) {
        let cmd = if lsub { "LSUB" } else { "LIST" };
        if pattern.is_empty() && !lsub {
            self.line("* LIST (\\Noselect) \"/\" \"\"");
            self.tagged(tag, "OK", "LIST completed");
            return;
        }
        let u = self.user().clone();
        let folders = match self
            .srv
            .service
            .store
            .mailbox_folders(u.area.as_deref(), &u.address)
            .await
        {
            Ok(f) => f,
            Err(e) => return self.store_failed(tag, e),
        };
        let full = format!("{reference}{pattern}");
        for f in folders {
            let name = f.folder;
            let hit = list_match(full.as_bytes(), name.as_bytes())
                || (name == INBOX && list_match(full.to_ascii_uppercase().as_bytes(), b"INBOX"));
            if hit {
                let mut l = format!("* {cmd} () \"/\" ").into_bytes();
                l.extend_from_slice(&self.name_bytes(&name));
                self.wbuf.extend_from_slice(&l);
                self.wbuf.extend_from_slice(b"\r\n");
            }
        }
        self.tagged(tag, "OK", &format!("{cmd} completed"));
    }

    async fn status(&mut self, tag: &str, name: &str, items: &[StatusItem]) {
        let key = self.key(name);
        let store = self.srv.service.store.clone();
        let info = match store.mailbox_info(&key).await {
            Ok(Some(i)) => i,
            Ok(None) => {
                self.tagged(tag, "NO", "[NONEXISTENT] No such mailbox");
                return;
            }
            Err(e) => return self.store_failed(tag, e),
        };
        let needs_entries = items
            .iter()
            .any(|i| matches!(i, StatusItem::Deleted | StatusItem::DeletedStorage));
        let entries = if needs_entries {
            match store.mailbox_entries(&key).await {
                Ok(e) => e,
                Err(e) => return self.store_failed(tag, e),
            }
        } else {
            vec![]
        };
        let deleted = entries.iter().filter(|e| e.has_flag("\\Deleted"));
        let mut parts = Vec::new();
        for i in items {
            parts.push(match i {
                StatusItem::Messages => format!("MESSAGES {}", info.exists),
                StatusItem::Recent => "RECENT 0".into(),
                StatusItem::UidNext => format!("UIDNEXT {}", info.uid_next),
                StatusItem::UidValidity => format!("UIDVALIDITY {}", info.uid_validity),
                StatusItem::Unseen => format!("UNSEEN {}", info.unseen),
                StatusItem::Deleted => format!("DELETED {}", deleted.clone().count()),
                StatusItem::DeletedStorage => format!(
                    "DELETED-STORAGE {}",
                    deleted.clone().map(|e| e.size).sum::<u64>()
                ),
            });
        }
        let mut l = b"* STATUS ".to_vec();
        l.extend_from_slice(&self.name_bytes(&key.folder));
        l.extend_from_slice(format!(" ({})\r\n", parts.join(" ")).as_bytes());
        self.wbuf.extend_from_slice(&l);
        self.tagged(tag, "OK", "STATUS completed");
    }

    async fn append(&mut self, tag: &str, name: &str, flags: Vec<String>, message: Vec<u8>) {
        let key = self.key(name);
        let svc = self.srv.service.clone();
        let info = match svc.store.mailbox_info(&key).await {
            Ok(Some(i)) => i,
            Ok(None) => {
                self.tagged(tag, "NO", "[TRYCREATE] No such mailbox");
                return;
            }
            Err(e) => return self.store_failed(tag, e),
        };
        if message.is_empty() {
            self.tagged(tag, "NO", "Empty message");
            return;
        }
        let u = self.user().clone();
        let flags: Vec<String> = flags
            .into_iter()
            .filter(|f| !f.eq_ignore_ascii_case("\\Recent"))
            .collect();
        let mut inc = Incoming::new(
            MailSource::Inject,
            Envelope {
                mail_from: String::new(),
                rcpt_to: vec![u.address.clone()],
                client_ip: Some(self.peer.ip().to_string()),
                tls: self.tls,
                ..Default::default()
            },
            message,
        );
        inc.destination = Destination::Area(u.area.clone());
        inc.mailboxes = Some(vec![MailboxDelivery {
            address: u.address.clone(),
            folder: key.folder.clone(),
            flags,
        }]);
        let stored = match svc.accept(inc).await {
            Ok(s) => s,
            Err(AcceptError::TooLarge(_)) => {
                rejected("size");
                self.tagged(tag, "NO", "[TOOBIG] Message too large");
                return;
            }
            Err(AcceptError::Store(e)) => return self.store_failed(tag, e),
        };
        let id = stored.first().map(|e| e.id);
        let uid = match svc.store.mailbox_entries(&key).await {
            Ok(entries) => entries
                .iter()
                .find(|e| Some(e.message_id) == id)
                .map(|e| e.uid),
            Err(_) => None,
        };
        if self.selected.as_ref().is_some_and(|s| s.key == key) {
            self.sync(false).await;
        }
        match uid {
            Some(uid) => self.tagged(
                tag,
                "OK",
                &format!("[APPENDUID {} {uid}] APPEND completed", info.uid_validity),
            ),
            None => self.tagged(tag, "OK", "APPEND completed"),
        }
    }

    // ----- selected -----

    /// Reads the selected folder and reports changes: EXPUNGE (when allowed), EXISTS and FLAGS.
    /// Returns the current entries by UID.
    async fn sync(&mut self, expunges: bool) -> HashMap<u32, MailboxEntry> {
        let Some(sel) = self.selected.as_mut() else {
            return HashMap::new();
        };
        let entries = match self.srv.service.store.mailbox_entries(&sel.key).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "reading the IMAP mailbox failed");
                return HashMap::new();
            }
        };
        let current: HashMap<u32, MailboxEntry> =
            entries.iter().map(|e| (e.uid, e.clone())).collect();
        let mut out: Vec<String> = Vec::new();
        if expunges {
            for i in (0..sel.known.len()).rev() {
                if !current.contains_key(&sel.known[i].uid) {
                    sel.known.remove(i);
                    out.push(format!("* {} EXPUNGE", i + 1));
                }
            }
        }
        let last = sel.known.iter().map(|k| k.uid).max().unwrap_or(0);
        let mut added = false;
        for e in &entries {
            if e.uid > last {
                sel.known.push(Known {
                    uid: e.uid,
                    flags: e.flags.clone(),
                });
                added = true;
            }
        }
        if added {
            out.push(format!("* {} EXISTS", sel.known.len()));
        }
        for (i, k) in sel.known.iter_mut().enumerate() {
            if let Some(e) = current.get(&k.uid)
                && e.flags != k.flags
            {
                k.flags = e.flags.clone();
                out.push(format!("* {} FETCH (FLAGS {})", i + 1, flag_list(&k.flags)));
            }
        }
        for l in out {
            self.line(&l);
        }
        current
    }

    /// `(sequence number, uid)` of the known messages in `set`.
    fn targets(&self, set: &SeqSet, uid: bool) -> Vec<(u32, u32)> {
        let Some(sel) = &self.selected else {
            return vec![];
        };
        let max_seq = sel.known.len() as u32;
        let max_uid = sel.known.last().map_or(0, |k| k.uid);
        sel.known
            .iter()
            .enumerate()
            .map(|(i, k)| (i as u32 + 1, k.uid))
            .filter(|(seq, u)| {
                if uid {
                    set.contains(*u, max_uid)
                } else {
                    set.contains(*seq, max_seq)
                }
            })
            .collect()
    }

    /// Whether a message sequence set without `*` names numbers beyond the mailbox (`BAD` for non-UID
    /// commands).
    fn invalid_seq(&self, set: &SeqSet, uid: bool) -> bool {
        let n = self.selected.as_ref().map_or(0, |s| s.known.len() as u32);
        let star = set
            .0
            .iter()
            .any(|(a, b)| *a == SeqNum::Star || *b == SeqNum::Star);
        !uid && !star
            && set
                .0
                .iter()
                .any(|(a, b)| [a, b].iter().any(|x| matches!(x, SeqNum::Num(v) if *v > n)))
    }

    fn read_only(&self) -> bool {
        self.selected.as_ref().is_some_and(|s| s.read_only)
    }

    fn selected_key(&self) -> MailboxKey {
        self.selected
            .as_ref()
            .map(|s| s.key.clone())
            .expect("checked by command()")
    }

    async fn expunge(&mut self, tag: &str, uids: Option<SeqSet>) {
        if self.read_only() {
            self.tagged(tag, "NO", "Mailbox is read-only");
            return;
        }
        let key = self.selected_key();
        let only: Option<Vec<u32>> = uids.map(|set| {
            self.targets(&set, true)
                .into_iter()
                .map(|(_, u)| u)
                .collect()
        });
        if let Err(e) = self
            .srv
            .service
            .store
            .mailbox_expunge(&key, only.as_deref())
            .await
        {
            return self.store_failed(tag, e);
        }
        self.sync(true).await;
        self.tagged(tag, "OK", "EXPUNGE completed");
    }

    async fn search(
        &mut self,
        tag: &str,
        charset: Option<&str>,
        key: &SearchKey,
        uid: bool,
    ) -> Step<bool> {
        if let Some(c) = charset
            && !c.eq_ignore_ascii_case("UTF-8")
            && !c.eq_ignore_ascii_case("US-ASCII")
        {
            self.tagged(
                tag,
                "NO",
                "[BADCHARSET (US-ASCII UTF-8)] Unsupported charset",
            );
            return Ok(true);
        }
        let current = self.sync(uid).await;
        let known: Vec<(u32, u32)> = self
            .selected
            .as_ref()
            .map(|s| {
                s.known
                    .iter()
                    .enumerate()
                    .map(|(i, k)| (i as u32 + 1, k.uid))
                    .collect()
            })
            .unwrap_or_default();
        let max_seq = known.len() as u32;
        let max_uid = known.last().map_or(0, |k| k.1);
        let limits = self.cfg.parse_limits();
        let svc = self.srv.service.clone();
        let space = self.user().area.clone();
        let mut hits = Vec::new();
        for (seq, u) in known {
            let Some(entry) = current.get(&u) else {
                continue;
            };
            let doc = if key.needs_message() {
                match load(&svc, space.as_deref(), entry).await {
                    Some(m) => Some(SearchDoc::build(&m.raw, &limits)),
                    None => continue,
                }
            } else {
                None
            };
            let c = Candidate {
                seq,
                entry,
                doc: doc.as_ref(),
            };
            if matches(key, &c, max_seq, max_uid) {
                hits.push(if uid { u } else { seq });
            }
        }
        let list: Vec<String> = hits.iter().map(u32::to_string).collect();
        self.line(format!("* SEARCH {}", list.join(" ")).trim_end());
        self.tagged(
            tag,
            "OK",
            &format!("{}SEARCH completed", if uid { "UID " } else { "" }),
        );
        Ok(true)
    }

    async fn fetch(
        &mut self,
        tag: &str,
        set: &SeqSet,
        items: &[FetchItem],
        uid: bool,
    ) -> Step<bool> {
        if self.invalid_seq(set, uid) {
            self.tagged(tag, "BAD", "Invalid message sequence number");
            return Ok(false);
        }
        let mut current = self.sync(uid).await;
        let targets = self.targets(set, uid);
        let key = self.selected_key();
        let mut seen_set: Vec<u32> = Vec::new();
        if items.iter().any(FetchItem::sets_seen) && !self.read_only() {
            let unseen: Vec<u32> = targets
                .iter()
                .filter(|(_, u)| current.get(u).is_some_and(|e| !e.has_flag("\\Seen")))
                .map(|(_, u)| *u)
                .collect();
            if !unseen.is_empty() {
                match self
                    .srv
                    .service
                    .store
                    .mailbox_store_flags(&key, &unseen, FlagOp::Add, &["\\Seen".to_string()])
                    .await
                {
                    Ok(changed) => {
                        for e in changed {
                            seen_set.push(e.uid);
                            self.remember_flags(e.uid, &e.flags);
                            current.insert(e.uid, e);
                        }
                    }
                    Err(e) => {
                        self.store_failed(tag, e);
                        return Ok(true);
                    }
                }
            }
        }
        let needs_message = items.iter().any(FetchItem::needs_message);
        let svc = self.srv.service.clone();
        let space = self.user().area.clone();
        let limits = self.cfg.parse_limits();
        let utf8 = self.utf8;
        for (seq, u) in targets {
            let Some(entry) = current.get(&u) else {
                continue;
            };
            let msg = if needs_message {
                match load(&svc, space.as_deref(), entry).await {
                    Some(m) => Some(m),
                    None => continue,
                }
            } else {
                None
            };
            let view = msg.as_ref().map(|m| MessageView::parse(&m.raw, &limits));
            let mut parts: Vec<Vec<u8>> = Vec::new();
            if uid && !items.contains(&FetchItem::Uid) {
                parts.push(format!("UID {u}").into_bytes());
            }
            for item in items {
                parts.push(render_item(item, entry, view.as_ref(), utf8));
            }
            if seen_set.contains(&u) && !items.contains(&FetchItem::Flags) {
                parts.push(format!("FLAGS {}", flag_list(&entry.flags)).into_bytes());
            }
            self.wbuf
                .extend_from_slice(format!("* {seq} FETCH (").as_bytes());
            for (i, p) in parts.iter().enumerate() {
                if i > 0 {
                    self.wbuf.push(b' ');
                }
                self.wbuf.extend_from_slice(p);
            }
            self.wbuf.extend_from_slice(b")\r\n");
            self.flush_if_large().await?;
        }
        self.tagged(
            tag,
            "OK",
            &format!("{}FETCH completed", if uid { "UID " } else { "" }),
        );
        Ok(true)
    }

    fn remember_flags(&mut self, uid: u32, flags: &[String]) {
        if let Some(k) = self
            .selected
            .as_mut()
            .and_then(|s| s.known.iter_mut().find(|k| k.uid == uid))
        {
            k.flags = flags.to_vec();
        }
    }

    async fn store_flags(
        &mut self,
        tag: &str,
        set: &SeqSet,
        op: StoreOp,
        silent: bool,
        flags: Vec<String>,
        uid: bool,
    ) -> Step<bool> {
        if self.read_only() {
            self.tagged(tag, "NO", "Mailbox is read-only");
            return Ok(true);
        }
        if self.invalid_seq(set, uid) {
            self.tagged(tag, "BAD", "Invalid message sequence number");
            return Ok(false);
        }
        self.sync(uid).await;
        let targets = self.targets(set, uid);
        let seq_of: BTreeMap<u32, u32> = targets.iter().map(|(s, u)| (*u, *s)).collect();
        let uids: Vec<u32> = targets.iter().map(|(_, u)| *u).collect();
        let flags: Vec<String> = flags
            .into_iter()
            .filter(|f| !f.eq_ignore_ascii_case("\\Recent"))
            .collect();
        let op = match op {
            StoreOp::Add => FlagOp::Add,
            StoreOp::Remove => FlagOp::Remove,
            StoreOp::Replace => FlagOp::Replace,
        };
        let key = self.selected_key();
        let changed = if uids.is_empty() {
            vec![]
        } else {
            match self
                .srv
                .service
                .store
                .mailbox_store_flags(&key, &uids, op, &flags)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    self.store_failed(tag, e);
                    return Ok(true);
                }
            }
        };
        for e in changed {
            self.remember_flags(e.uid, &e.flags);
            if !silent && let Some(seq) = seq_of.get(&e.uid) {
                let uid_part = if uid {
                    format!(" UID {}", e.uid)
                } else {
                    String::new()
                };
                self.line(&format!(
                    "* {seq} FETCH (FLAGS {}{uid_part})",
                    flag_list(&e.flags)
                ));
            }
        }
        self.tagged(
            tag,
            "OK",
            &format!("{}STORE completed", if uid { "UID " } else { "" }),
        );
        Ok(true)
    }

    async fn copy(
        &mut self,
        tag: &str,
        set: &SeqSet,
        target: &str,
        uid: bool,
        remove: bool,
    ) -> Step<bool> {
        let cmd = format!(
            "{}{}",
            if uid { "UID " } else { "" },
            if remove { "MOVE" } else { "COPY" }
        );
        if remove && self.read_only() {
            self.tagged(tag, "NO", "Mailbox is read-only");
            return Ok(true);
        }
        if self.invalid_seq(set, uid) {
            self.tagged(tag, "BAD", "Invalid message sequence number");
            return Ok(false);
        }
        let key = self.selected_key();
        let target_key = self.key(target);
        if remove && target_key == key {
            self.tagged(tag, "NO", "Cannot move messages into the same mailbox");
            return Ok(true);
        }
        let current = self.sync(uid).await;
        let uids: Vec<u32> = self
            .targets(set, uid)
            .into_iter()
            .map(|(_, u)| u)
            .filter(|u| current.contains_key(u))
            .collect();
        let store = self.srv.service.store.clone();
        let info = match store.mailbox_info(&target_key).await {
            Ok(Some(i)) => i,
            Ok(None) => {
                self.tagged(tag, "NO", "[TRYCREATE] No such mailbox");
                return Ok(true);
            }
            Err(e) => {
                self.store_failed(tag, e);
                return Ok(true);
            }
        };
        if uids.is_empty() {
            self.tagged(tag, "OK", &format!("{cmd} completed, no messages"));
            return Ok(true);
        }
        let pairs = match store.mailbox_copy(&key, &uids, &target_key.folder).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                self.tagged(tag, "NO", "[TRYCREATE] No such mailbox");
                return Ok(true);
            }
            Err(e) => {
                self.store_failed(tag, e);
                return Ok(true);
            }
        };
        let src: Vec<u32> = pairs.iter().map(|p| p.0).collect();
        let dst: Vec<u32> = pairs.iter().map(|p| p.1).collect();
        let code = (!pairs.is_empty()).then(|| {
            format!(
                "[COPYUID {} {} {}] ",
                info.uid_validity,
                compress(&src),
                compress(&dst)
            )
        });
        if remove {
            if let Some(c) = &code {
                self.line(&format!("* OK {c}Moved"));
            }
            if let Err(e) = store.mailbox_remove(&key, &src).await {
                self.store_failed(tag, e);
                return Ok(true);
            }
            self.sync(true).await;
            self.tagged(tag, "OK", &format!("{cmd} completed"));
        } else {
            if target_key == key {
                self.sync(false).await;
            }
            self.tagged(
                tag,
                "OK",
                &format!("{}{cmd} completed", code.unwrap_or_default()),
            );
        }
        Ok(true)
    }

    async fn idle(&mut self, tag: &str) -> Step<bool> {
        let Some(u) = self.user.clone() else {
            return Ok(false);
        };
        let events = self.srv.service.events.clone();
        let mut sub = Some(events.subscribe(SpaceFilter::area(u.area.as_deref())));
        let tick = events.poll_interval().unwrap_or(IDLE_POLL);
        let now = Instant::now();
        let session_left = self.deadline.saturating_duration_since(now);
        let idle_until = now + self.cfg.imap_idle_timeout.min(session_left);
        self.line("+ idling");
        self.sync(true).await;
        loop {
            self.flush().await?;
            if let Some(pos) = self.rbuf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.rbuf.drain(..=pos).collect();
                let text = String::from_utf8_lossy(&line);
                if text.trim().eq_ignore_ascii_case("DONE") {
                    self.tagged(tag, "OK", "IDLE terminated");
                    return Ok(true);
                }
                self.tagged(tag, "BAD", "Expected DONE");
                return Ok(false);
            }
            if self.rbuf.len() > self.cfg.max_line_bytes {
                return Err(self.bye_rejected("line_too_long", "Line too long"));
            }
            if Instant::now() >= idle_until {
                rejected("timeout");
                return Err(Close::IdleExpired);
            }
            self.rbuf.reserve(READ_CHUNK);
            let io = self.io.as_mut().ok_or(Close::Io)?;
            let shutdown = self.shutdown.clone();
            let wake = tokio::select! {
                r = io.read_buf(&mut self.rbuf) => match r {
                    Ok(0) => return Err(Close::Eof),
                    Ok(_) => false,
                    Err(_) => return Err(Close::Io),
                },
                ev = async {
                    match sub.as_mut() {
                        Some(s) => s.recv().await,
                        None => std::future::pending().await,
                    }
                } => match ev {
                    Ok(e) => e.kind == MailEventKind::Deleted || e.mailboxes.contains(&u.address),
                    Err(MailSubscriptionError::Lagged(_)) => true,
                    Err(MailSubscriptionError::Closed) => {
                        sub = None;
                        false
                    }
                },
                _ = tokio::time::sleep(tick) => true,
                _ = tokio::time::sleep_until(idle_until) => false,
                _ = async {
                    match shutdown {
                        Some(mut s) => {
                            if !*s.borrow() {
                                let _ = s.changed().await;
                            }
                        }
                        None => std::future::pending().await,
                    }
                } => return Err(Close::Shutdown),
            };
            if wake {
                self.sync(true).await;
            }
        }
    }

    /// Sends what the end of the session needs and closes.
    async fn close(&mut self, r: Step) -> SessionEnd {
        let close = match r {
            Ok(()) => Close::Logout,
            Err(c) => c,
        };
        match close {
            Close::IdleTimeout => {
                rejected("timeout");
                self.line("* BYE Autologout; idle for too long");
            }
            Close::LiteralTimeout => {
                rejected("timeout");
                self.line("* BYE Timeout waiting for data");
            }
            Close::SessionTimeout => {
                rejected("timeout");
                self.line("* BYE Session time limit reached");
            }
            Close::IdleExpired => self.line("* BYE IDLE time limit reached"),
            Close::Shutdown => self.line("* BYE Server shutting down"),
            _ => {}
        }
        if !matches!(close, Close::Io | Close::Eof)
            && let Some(io) = self.io.as_mut()
        {
            let buf = std::mem::take(&mut self.wbuf);
            let _ = tokio::time::timeout(Duration::from_secs(2), async {
                io.write_all(&buf).await?;
                io.flush().await?;
                io.shutdown().await
            })
            .await;
        }
        close.end()
    }
}

/// The stored message of a mailbox entry.
async fn load(svc: &MailService, space: Option<&str>, entry: &MailboxEntry) -> Option<MailMessage> {
    match svc
        .store
        .get(entry.message_id, &SpaceFilter::area(space))
        .await
    {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "reading a message for IMAP failed");
            None
        }
    }
}

/// One FETCH data item.
fn render_item(
    item: &FetchItem,
    entry: &MailboxEntry,
    view: Option<&MessageView<'_>>,
    utf8: bool,
) -> Vec<u8> {
    let mut o = Vec::new();
    match item {
        FetchItem::Flags => {
            o.extend_from_slice(format!("FLAGS {}", flag_list(&entry.flags)).as_bytes())
        }
        FetchItem::Uid => o.extend_from_slice(format!("UID {}", entry.uid).as_bytes()),
        FetchItem::InternalDate => o.extend_from_slice(
            format!("INTERNALDATE {}", internal_date(entry.received_at)).as_bytes(),
        ),
        FetchItem::Rfc822Size => {
            o.extend_from_slice(format!("RFC822.SIZE {}", entry.size).as_bytes())
        }
        FetchItem::Envelope => {
            o.extend_from_slice(b"ENVELOPE ");
            if let Some(v) = view {
                v.envelope(&mut o, utf8);
            }
        }
        FetchItem::BodyStructure | FetchItem::Body => {
            let ext = *item == FetchItem::BodyStructure;
            o.extend_from_slice(if ext { b"BODYSTRUCTURE " } else { b"BODY " });
            if let Some(v) = view {
                v.body_structure(&mut o, ext, utf8);
            }
        }
        FetchItem::BodySection {
            section, partial, ..
        } => {
            o.extend_from_slice(format!("BODY[{}]", section.spec()).as_bytes());
            let data = view.and_then(|v| v.section(section));
            match (data, partial) {
                (Some(d), Some((origin, len))) => {
                    let start = (*origin as usize).min(d.len());
                    let end = start.saturating_add(*len as usize).min(d.len());
                    o.extend_from_slice(format!("<{origin}> ").as_bytes());
                    literal(&mut o, &d[start..end]);
                }
                (Some(d), None) => {
                    o.push(b' ');
                    literal(&mut o, &d);
                }
                (None, Some((origin, _))) => {
                    o.extend_from_slice(format!("<{origin}> ").as_bytes());
                    nstring(&mut o, None, utf8);
                }
                (None, None) => {
                    o.push(b' ');
                    nstring(&mut o, None, utf8);
                }
            }
        }
        FetchItem::Rfc822 => {
            o.extend_from_slice(b"RFC822 ");
            literal(&mut o, view.map_or(&[][..], |v| v.raw()));
        }
        FetchItem::Rfc822Header => {
            o.extend_from_slice(b"RFC822.HEADER ");
            literal(&mut o, view.map_or(&[][..], |v| v.header()));
        }
        FetchItem::Rfc822Text => {
            o.extend_from_slice(b"RFC822.TEXT ");
            literal(&mut o, view.map_or(&[][..], |v| v.text()));
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_announcements() {
        assert_eq!(literal_announcement(b"a LOGIN {5}\r\n"), Some((5, true)));
        assert_eq!(literal_announcement(b"a LOGIN {5+}\r\n"), Some((5, false)));
        assert_eq!(literal_announcement(b"a LOGIN {5}\n"), Some((5, true)));
        assert_eq!(literal_announcement(b"a LOGIN {}\r\n"), None);
        assert_eq!(literal_announcement(b"a LOGIN {x}\r\n"), None);
        assert_eq!(literal_announcement(b"a LOGIN {99999999999}\r\n"), None);
        assert_eq!(literal_announcement(b"a NOOP\r\n"), None);
        assert_eq!(command_name(b"a1 append INBOX {3}\r\n"), "APPEND");
        assert_eq!(command_name(b"a1\r\n"), "");
    }

    #[test]
    fn list_patterns() {
        assert!(list_match(b"*", b"INBOX"));
        assert!(list_match(b"*", b"a/b/c"));
        assert!(list_match(b"%", b"Sent"));
        assert!(!list_match(b"%", b"a/b"));
        assert!(list_match(b"a/%", b"a/b"));
        assert!(!list_match(b"a/%", b"a/b/c"));
        assert!(list_match(b"S*t", b"Sent"));
        assert!(!list_match(b"Sent", b"sent"));
        assert!(list_match(b"", b""));
        let long = vec![b'*'; 2000];
        assert!(list_match(&long, b"abc"));
    }

    #[test]
    fn uid_sets_and_folder_names() {
        assert_eq!(compress(&[1, 2, 3, 5, 7, 8]), "1:3,5,7:8");
        assert_eq!(compress(&[4]), "4");
        assert_eq!(compress(&[]), "");
        assert_eq!(valid_folder("Sent/"), Some("Sent".into()));
        assert_eq!(valid_folder("inbox"), Some("INBOX".into()));
        assert_eq!(valid_folder("a/b"), Some("a/b".into()));
        assert_eq!(valid_folder(""), None);
        assert_eq!(valid_folder("a*"), None);
        assert_eq!(valid_folder("/a"), None);
        assert_eq!(valid_folder("a//b"), None);
        assert_eq!(valid_folder("a\u{7}"), None);
        assert_eq!(valid_folder(&"x".repeat(300)), None);
    }
}
