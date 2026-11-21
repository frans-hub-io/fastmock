//! One POP3 session: the AUTHORIZATION, TRANSACTION and UPDATE states over any byte stream, with STLS,
//! USER/PASS, AUTH PLAIN, limits and timeouts.

use std::net::SocketAddr;
use std::time::Duration;

use fastmock_core::mail::{FlagOp, MailboxKey};
use fastmock_core::model::SpaceFilter;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;
use tokio::time::Instant;
use uuid::Uuid;

use super::command::{self, Command};
use super::{MAX_BAD_COMMANDS, MAX_COMMANDS, Pop3Server, SessionEnd, render};
use crate::config::MailConfig;
use crate::smtp::sasl::{self, SaslError};

const READ_CHUNK: usize = 16 * 1024;
const WRITE_CHUNK: usize = 64 * 1024;

pub(crate) trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

type Io = Box<dyn Stream>;

#[derive(Debug)]
enum Close {
    Quit,
    Eof,
    IdleTimeout,
    SessionTimeout,
    Shutdown,
    /// A reply that closes was already queued (or none is due).
    Rejected,
    Io,
}

impl Close {
    fn end(&self) -> SessionEnd {
        match self {
            Close::Quit => SessionEnd::Quit,
            Close::Eof => SessionEnd::Disconnected,
            Close::IdleTimeout | Close::SessionTimeout => SessionEnd::Timeout,
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

enum Line {
    Ok(Vec<u8>),
    TooLong,
}

struct Msg {
    uid: u32,
    id: Uuid,
    size: u64,
    deleted: bool,
    retrieved: bool,
}

/// The INBOX as it was at login.
struct Maildrop {
    key: MailboxKey,
    uid_validity: u32,
    msgs: Vec<Msg>,
}

impl Maildrop {
    fn live(&self) -> impl Iterator<Item = (usize, &Msg)> {
        self.msgs
            .iter()
            .enumerate()
            .filter(|(_, m)| !m.deleted)
            .map(|(i, m)| (i + 1, m))
    }

    fn stat(&self) -> (usize, u64) {
        self.live()
            .fold((0, 0), |(n, s), (_, m)| (n + 1, s + m.size))
    }

    /// The message with a number, or the `-ERR` text.
    fn get(&self, n: u32) -> Result<&Msg, String> {
        let len = self.msgs.len();
        match (n as usize).checked_sub(1).and_then(|i| self.msgs.get(i)) {
            None => Err(format!("No such message, only {len} messages in maildrop")),
            Some(m) if m.deleted => Err(format!("Message {n} already deleted")),
            Some(m) => Ok(m),
        }
    }

    fn unique_id(&self, m: &Msg) -> String {
        format!("{}.{}", self.uid_validity, m.uid)
    }
}

enum State {
    Authorization { user: Option<String> },
    Transaction(Box<Maildrop>),
}

pub(crate) struct Session<'a> {
    srv: &'a Pop3Server,
    cfg: &'a MailConfig,
    io: Option<Io>,
    rbuf: Vec<u8>,
    wbuf: Vec<u8>,
    peer: SocketAddr,
    deadline: Instant,
    shutdown: Option<watch::Receiver<bool>>,
    tls: bool,
    state: State,
    auth_failures: u32,
    bad_commands: u32,
    commands: u32,
    pub(crate) deleted: u32,
}

impl<'a> Session<'a> {
    pub(crate) fn new(
        srv: &'a Pop3Server,
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
            state: State::Authorization { user: None },
            auth_failures: 0,
            bad_commands: 0,
            commands: 0,
            deleted: 0,
        }
    }

    pub(crate) fn mailbox(&self) -> Option<String> {
        match &self.state {
            State::Transaction(d) => Some(d.key.address.clone()),
            State::Authorization { .. } => None,
        }
    }

    fn line(&mut self, text: &str) {
        self.wbuf.extend_from_slice(text.as_bytes());
        self.wbuf.extend_from_slice(b"\r\n");
    }

    fn ok(&mut self, text: &str) {
        if text.is_empty() {
            self.line("+OK");
        } else {
            self.line(&format!("+OK {text}"));
        }
    }

    fn err(&mut self, text: &str) {
        self.line(&format!("-ERR {text}"));
    }

    async fn flush(&mut self) -> Step {
        if self.wbuf.is_empty() {
            return Ok(());
        }
        let io = self.io.as_mut().ok_or(Close::Io)?;
        let wait = self.cfg.idle_timeout;
        let mut buf = std::mem::take(&mut self.wbuf);
        for chunk in buf.chunks(WRITE_CHUNK) {
            match tokio::time::timeout(wait, io.write_all(chunk)).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return Err(Close::Io),
                Err(_) => return Err(Close::IdleTimeout),
            }
        }
        match tokio::time::timeout(wait, io.flush()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(Close::Io),
            Err(_) => return Err(Close::IdleTimeout),
        }
        if buf.capacity() <= WRITE_CHUNK {
            buf.clear();
            self.wbuf = buf;
        }
        Ok(())
    }

    /// Reads more bytes into the buffer after sending the queued replies.
    async fn fill(&mut self) -> Step {
        self.flush().await?;
        let idle = self.cfg.idle_timeout;
        let now = Instant::now();
        if now >= self.deadline {
            return Err(Close::SessionTimeout);
        }
        let wait = idle.min(self.deadline - now);
        self.rbuf.reserve(READ_CHUNK);
        let io = self.io.as_mut().ok_or(Close::Io)?;
        let read = tokio::time::timeout(wait, io.read_buf(&mut self.rbuf));
        let res = match self.shutdown.as_mut() {
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
            Err(_) if wait < idle => Err(Close::SessionTimeout),
            Err(_) => Err(Close::IdleTimeout),
            Ok(Ok(0)) => Err(Close::Eof),
            Ok(Ok(_)) => Ok(()),
            Ok(Err(_)) => Err(Close::Io),
        }
    }

    async fn next_line(&mut self) -> Step<Line> {
        let max = self.cfg.max_line_bytes;
        let mut too_long = false;
        let mut scanned = 0;
        loop {
            if let Some(pos) = self.rbuf[scanned..].iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.rbuf.drain(..=scanned + pos).collect();
                if too_long || line.len() > max {
                    return Ok(Line::TooLong);
                }
                return Ok(Line::Ok(line));
            }
            if self.rbuf.len() > max {
                too_long = true;
                self.rbuf.clear();
            }
            scanned = self.rbuf.len();
            self.fill().await?;
        }
    }

    fn auth_offered(&self) -> bool {
        self.tls || self.srv.tls.is_none() || self.cfg.allow_plaintext_auth
    }

    pub(crate) async fn drive(&mut self, implicit_tls: bool) -> SessionEnd {
        let r = self.run(implicit_tls).await;
        self.close(r).await
    }

    async fn run(&mut self, implicit_tls: bool) -> Step {
        if implicit_tls {
            self.handshake().await?;
        }
        let host = self.cfg.hostname.clone();
        self.ok(&format!("{host} FastMock POP3 server ready"));
        loop {
            let line = self.next_line().await?;
            self.commands += 1;
            if self.commands > MAX_COMMANDS {
                rejected("too_many_commands");
                self.err("Too many commands; closing connection");
                return Err(Close::Rejected);
            }
            let mark = self.wbuf.len();
            let good = match line {
                Line::TooLong => {
                    self.err("Line too long");
                    false
                }
                Line::Ok(l) => match command::parse(&l) {
                    Ok(cmd) => self.command(cmd).await?,
                    Err(e) => {
                        self.err(&e.text());
                        false
                    }
                },
            };
            if good {
                self.bad_commands = 0;
            } else {
                self.bad_commands += 1;
                if self.bad_commands >= MAX_BAD_COMMANDS {
                    rejected("too_many_errors");
                    self.wbuf.truncate(mark);
                    self.err("Too many errors; closing connection");
                    return Err(Close::Rejected);
                }
            }
            if self.wbuf.len() >= WRITE_CHUNK {
                self.flush().await?;
            }
        }
    }

    /// Handles a command; returns whether it was accepted (refusals count towards
    /// [`MAX_BAD_COMMANDS`]).
    async fn command(&mut self, cmd: Command) -> Step<bool> {
        match cmd {
            Command::Capa => {
                self.capa();
                return Ok(true);
            }
            Command::Noop => {
                self.ok("");
                return Ok(true);
            }
            Command::Quit => return self.quit().await,
            _ => {}
        }
        if matches!(self.state, State::Authorization { .. }) {
            return match cmd {
                Command::User(u) => Ok(self.user(u)),
                Command::Pass(p) => self.pass(p).await,
                Command::Auth { mechanism, initial } => self.auth(mechanism, initial).await,
                Command::Stls => self.stls().await,
                Command::Apop => {
                    self.err("APOP is not supported; use USER/PASS or AUTH PLAIN");
                    Ok(false)
                }
                other => {
                    self.err(&format!("{} is not allowed before login", other.name()));
                    Ok(false)
                }
            };
        }
        let State::Transaction(md) = &mut self.state else {
            return Ok(false);
        };
        match cmd {
            Command::Stat => {
                let (n, size) = md.stat();
                self.ok(&format!("{n} {size}"));
            }
            Command::List(None) => {
                let (n, size) = md.stat();
                let mut out = format!("+OK {n} messages ({size} octets)\r\n");
                for (i, m) in md.live() {
                    out.push_str(&format!("{i} {}\r\n", m.size));
                }
                out.push_str(".\r\n");
                self.wbuf.extend_from_slice(out.as_bytes());
            }
            Command::List(Some(n)) => match md.get(n) {
                Ok(m) => {
                    let size = m.size;
                    self.ok(&format!("{n} {size}"));
                }
                Err(e) => self.err(&e),
            },
            Command::Uidl(None) => {
                let mut out = String::from("+OK unique-id listing follows\r\n");
                for (i, m) in md.live() {
                    out.push_str(&format!("{i} {}\r\n", md.unique_id(m)));
                }
                out.push_str(".\r\n");
                self.wbuf.extend_from_slice(out.as_bytes());
            }
            Command::Uidl(Some(n)) => match md.get(n).map(|m| md.unique_id(m)) {
                Ok(u) => self.ok(&format!("{n} {u}")),
                Err(e) => self.err(&e),
            },
            Command::Dele(n) => match md.get(n) {
                Ok(_) => {
                    md.msgs[n as usize - 1].deleted = true;
                    self.ok(&format!("Message {n} deleted"));
                }
                Err(e) => self.err(&e),
            },
            Command::Rset => {
                for m in &mut md.msgs {
                    m.deleted = false;
                }
                let (n, size) = md.stat();
                self.ok(&format!("Maildrop has {n} messages ({size} octets)"));
            }
            Command::Retr(n) => self.send_message(n, None).await,
            Command::Top(n, lines) => self.send_message(n, Some(lines)).await,
            Command::Stls => {
                self.err("STLS is only allowed before login");
                return Ok(false);
            }
            other => {
                self.err(&format!("{} is not allowed after login", other.name()));
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn capa(&mut self) {
        let mut caps = vec!["TOP", "UIDL", "RESP-CODES", "AUTH-RESP-CODE", "PIPELINING"];
        if matches!(self.state, State::Authorization { .. }) {
            if self.auth_offered() {
                caps.push("USER");
                caps.push("SASL PLAIN");
            }
            if self.srv.tls.is_some() && !self.tls {
                caps.push("STLS");
            }
        }
        caps.push("IMPLEMENTATION FastMock");
        self.ok("Capability list follows");
        for c in caps {
            self.line(c);
        }
        self.line(".");
    }

    /// Refuses USER, PASS and AUTH before TLS when TLS is available (unless plaintext is allowed).
    fn plaintext_refused(&mut self) -> bool {
        if self.auth_offered() {
            return false;
        }
        rejected("auth_before_tls");
        self.err("[AUTH] Plaintext authentication is disabled on this connection; use STLS first");
        true
    }

    fn user(&mut self, user: String) -> bool {
        if self.plaintext_refused() {
            return false;
        }
        self.state = State::Authorization { user: Some(user) };
        self.ok("Send PASS");
        true
    }

    async fn pass(&mut self, password: String) -> Step<bool> {
        if self.plaintext_refused() {
            return Ok(false);
        }
        let user = match &mut self.state {
            State::Authorization { user } => user.take(),
            State::Transaction(_) => None,
        };
        let Some(user) = user else {
            self.err("Send USER first");
            return Ok(false);
        };
        self.login(Some((user, password))).await
    }

    async fn auth(&mut self, mechanism: Option<String>, initial: Option<String>) -> Step<bool> {
        if self.plaintext_refused() {
            return Ok(false);
        }
        self.state = State::Authorization { user: None };
        let Some(mechanism) = mechanism else {
            self.ok("Supported mechanisms follow");
            self.line("PLAIN");
            self.line(".");
            return Ok(true);
        };
        if mechanism != "PLAIN" {
            self.err("Unsupported authentication mechanism");
            return Ok(false);
        }
        let response = match initial {
            Some(i) => i,
            None => {
                self.line("+ ");
                match self.next_line().await? {
                    Line::Ok(l) => String::from_utf8_lossy(command::strip_eol(&l)).into_owned(),
                    Line::TooLong => String::new(),
                }
            }
        };
        let creds = match sasl::decode(&response) {
            Err(SaslError::Cancelled) => {
                self.err("Authentication cancelled");
                return Ok(false);
            }
            Err(SaslError::Malformed) => None,
            Ok(bytes) => {
                let authz = bytes.split(|b| *b == 0).next().unwrap_or_default().to_vec();
                sasl::plain(&bytes).ok().and_then(|c| {
                    let same = authz.is_empty()
                        || std::str::from_utf8(&authz).is_ok_and(|a| {
                            crate::address::mailbox_of(a) == crate::address::mailbox_of(&c.user)
                        });
                    same.then_some(c)
                })
            }
        };
        let creds = creds.and_then(|c| c.password.map(|p| (c.user, p)));
        self.login(creds).await
    }

    async fn login(&mut self, creds: Option<(String, String)>) -> Step<bool> {
        let ok = match &creds {
            Some((user, password)) => self.srv.login.check(user, password).await,
            None => None,
        };
        let Some(ok) = ok else {
            self.auth_failures += 1;
            rejected("auth_failed");
            self.flush().await?;
            let left = self.deadline.saturating_duration_since(Instant::now());
            tokio::time::sleep(self.srv.failure_delay.min(left)).await;
            if self.auth_failures >= self.cfg.max_auth_failures {
                self.err("[AUTH] Too many failed login attempts; closing connection");
                return Err(Close::Rejected);
            }
            self.err("[AUTH] Invalid credentials");
            return Ok(false);
        };
        let key = MailboxKey::inbox(ok.area.as_deref(), &ok.address);
        let store = &self.srv.service.store;
        let opened = async {
            let info = store.mailbox_info(&key).await?;
            let entries = store.mailbox_entries(&key).await?;
            anyhow::Ok((info, entries))
        }
        .await;
        let (info, entries) = match opened {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(error = %e, "opening a POP3 maildrop failed");
                self.err("[SYS/TEMP] The mailbox is unavailable, try again later");
                return Ok(true);
            }
        };
        let md = Maildrop {
            key,
            uid_validity: info.map_or(0, |i| i.uid_validity),
            msgs: entries
                .into_iter()
                .map(|e| Msg {
                    uid: e.uid,
                    id: e.message_id,
                    size: e.size,
                    deleted: false,
                    retrieved: false,
                })
                .collect(),
        };
        let (n, size) = md.stat();
        tracing::debug!(
            peer = %self.peer.ip(),
            mailbox = %ok.address,
            space = ok.area.as_deref().unwrap_or("-"),
            token = ok.by_token,
            "POP3 login"
        );
        self.state = State::Transaction(Box::new(md));
        self.ok(&format!("Logged in, {n} messages ({size} octets)"));
        Ok(true)
    }

    async fn send_message(&mut self, n: u32, top: Option<u32>) {
        let State::Transaction(md) = &mut self.state else {
            return;
        };
        let (id, size) = match md.get(n) {
            Ok(m) => (m.id, m.size),
            Err(e) => {
                self.err(&e);
                return;
            }
        };
        let area = SpaceFilter::area(md.key.space.as_deref());
        if top.is_none() {
            md.msgs[n as usize - 1].retrieved = true;
        }
        match self.srv.service.store.get(id, &area).await {
            Ok(Some(m)) => {
                match top {
                    None => self.ok(&format!("{size} octets")),
                    Some(_) => self.ok("Top of message follows"),
                }
                render::message(&m.raw, top, &mut self.wbuf);
            }
            Ok(None) => self.err(&format!("Message {n} is no longer available")),
            Err(e) => {
                tracing::warn!(error = %e, "reading a message for POP3 failed");
                self.err("[SYS/TEMP] Reading the message failed, try again later");
            }
        }
    }

    async fn quit(&mut self) -> Step<bool> {
        let host = self.cfg.hostname.clone();
        let State::Transaction(md) = &self.state else {
            self.ok(&format!("{host} FastMock POP3 server signing off"));
            return Err(Close::Quit);
        };
        let store = &self.srv.service.store;
        let gone: Vec<u32> = md
            .msgs
            .iter()
            .filter(|m| m.deleted)
            .map(|m| m.uid)
            .collect();
        let seen: Vec<u32> = md
            .msgs
            .iter()
            .filter(|m| m.retrieved && !m.deleted)
            .map(|m| m.uid)
            .collect();
        if !seen.is_empty()
            && let Err(e) = store
                .mailbox_store_flags(&md.key, &seen, FlagOp::Add, &["\\Seen".to_string()])
                .await
        {
            tracing::warn!(error = %e, "marking POP3 messages seen failed");
        }
        let removed = if gone.is_empty() {
            Ok(vec![])
        } else {
            store.mailbox_remove(&md.key, &gone).await
        };
        match removed {
            Ok(r) => {
                self.deleted = r.len() as u32;
                let left = md.msgs.len() - gone.len();
                self.ok(&format!(
                    "{host} FastMock POP3 server signing off ({} messages deleted, {left} left)",
                    gone.len()
                ));
            }
            Err(e) => {
                tracing::warn!(error = %e, "removing POP3 messages failed");
                self.err("[SYS/TEMP] Some deleted messages were not removed");
            }
        }
        Err(Close::Quit)
    }

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
                tracing::debug!(error = %e, peer = %self.peer, "POP3 TLS handshake failed");
                Err(Close::Io)
            }
            Err(_) => Err(Close::Io),
        }
    }

    async fn stls(&mut self) -> Step<bool> {
        if self.srv.tls.is_none() {
            self.err("STLS is not offered");
            return Ok(false);
        }
        if self.tls {
            self.err("TLS is already active");
            return Ok(false);
        }
        self.ok("Begin TLS negotiation");
        self.flush().await?;
        if !self.rbuf.is_empty() {
            rejected("starttls_pipelined");
            tracing::warn!(peer = %self.peer, "POP3 client sent bytes after STLS; closing");
            return Err(Close::Rejected);
        }
        self.handshake().await?;
        self.state = State::Authorization { user: None };
        Ok(true)
    }

    /// Sends what the end of the session needs and closes. Only QUIT in the TRANSACTION state removes
    /// messages; every other end leaves the mailbox as it was.
    async fn close(&mut self, r: Step) -> SessionEnd {
        let close = match r {
            Ok(()) => Close::Quit,
            Err(c) => c,
        };
        match close {
            Close::IdleTimeout => {
                rejected("timeout");
                self.err("Timeout waiting for the client; closing connection");
            }
            Close::SessionTimeout => {
                rejected("timeout");
                self.err("Session time limit reached; closing connection");
            }
            Close::Shutdown => {
                self.err("[SYS/TEMP] Server shutting down; closing connection");
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maildrop_numbers() {
        let msg = |uid, size| Msg {
            uid,
            id: Uuid::nil(),
            size,
            deleted: false,
            retrieved: false,
        };
        let mut d = Maildrop {
            key: MailboxKey::inbox(None, "a@example.test"),
            uid_validity: 7,
            msgs: vec![msg(3, 10), msg(5, 20)],
        };
        assert_eq!(d.stat(), (2, 30));
        assert_eq!(d.get(2).map(|m| m.uid), Ok(5));
        assert_eq!(
            d.get(0).err().unwrap(),
            "No such message, only 2 messages in maildrop"
        );
        assert!(d.get(3).is_err());
        d.msgs[0].deleted = true;
        assert_eq!(d.get(1).err().unwrap(), "Message 1 already deleted");
        assert_eq!(d.stat(), (1, 20));
        assert_eq!(d.live().map(|(i, _)| i).collect::<Vec<_>>(), vec![2]);
        assert_eq!(d.unique_id(&d.msgs[1]), "7.5");
    }
}
