//! One SMTP session: the tokio state machine over any byte stream, with STARTTLS, AUTH, DATA/BDAT,
//! limits, timeouts, chaos and the capture through [`crate::MailService::accept`].

use std::net::SocketAddr;
use std::time::Duration;

use chrono::Utc;
use fastmock_core::mail::{Envelope, MailSource, TranscriptDirection, TranscriptLine};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;
use tokio::time::Instant;

use super::codec::{BodyType, Command, CommandError, Mechanism};
use super::sasl::{self, Credentials, SaslError};
use super::{SmtpServer, WriteScope};
use crate::chaos::{ChaosEffect, ChaosEvent, ChaosStage};
use crate::config::MailConfig;
use crate::mailbox_login::verify_password;
use crate::service::{AcceptError, Incoming};
use std::sync::Arc;

/// Most commands in one session.
pub const MAX_COMMANDS: u32 = 10_000;
/// Consecutive refused commands before the connection is closed.
pub const MAX_BAD_COMMANDS: u32 = 20;
/// Most transcript lines kept for a message.
pub const MAX_TRANSCRIPT_LINES: usize = 500;
/// Longest transcript line.
pub const MAX_TRANSCRIPT_TEXT: usize = 512;
const READ_CHUNK: usize = 16 * 1024;

pub(crate) trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

type Io = Box<dyn Stream>;

/// How a session ended (the `result` label of `fastmock_mail_smtp_sessions_total`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEnd {
    /// The client sent QUIT.
    Quit,
    /// The client closed the connection.
    Disconnected,
    /// Idle, DATA or session timeout.
    Timeout,
    /// Refused: too many errors, failed AUTH attempts or commands, bytes after STARTTLS.
    Rejected,
    /// A chaos rule closed the connection.
    Chaos,
    /// The server is shutting down.
    Shutdown,
    /// Reading, writing or the TLS handshake failed.
    Error,
}

impl SessionEnd {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionEnd::Quit => "quit",
            SessionEnd::Disconnected => "disconnected",
            SessionEnd::Timeout => "timeout",
            SessionEnd::Rejected => "rejected",
            SessionEnd::Chaos => "chaos",
            SessionEnd::Shutdown => "shutdown",
            SessionEnd::Error => "error",
        }
    }
}

#[derive(Debug)]
enum Close {
    Quit,
    Eof,
    IdleTimeout,
    DataTimeout,
    SessionTimeout,
    Shutdown,
    /// A reply that closes was already queued.
    Rejected,
    Chaos,
    Io,
}

impl Close {
    fn end(&self) -> SessionEnd {
        match self {
            Close::Quit => SessionEnd::Quit,
            Close::Eof => SessionEnd::Disconnected,
            Close::IdleTimeout | Close::DataTimeout | Close::SessionTimeout => SessionEnd::Timeout,
            Close::Shutdown => SessionEnd::Shutdown,
            Close::Rejected => SessionEnd::Rejected,
            Close::Chaos => SessionEnd::Chaos,
            Close::Io => SessionEnd::Error,
        }
    }
}

type Step<T = ()> = Result<T, Close>;

fn rejected(reason: &'static str) {
    metrics::counter!("fastmock_mail_rejected_total", "reason" => reason).increment(1);
}

/// Lines of the session for the transcript of a message.
#[derive(Default)]
struct Transcript {
    lines: Vec<TranscriptLine>,
    truncated: bool,
}

impl Transcript {
    fn push(&mut self, direction: TranscriptDirection, text: &str) {
        if self.lines.len() >= MAX_TRANSCRIPT_LINES {
            self.truncated = true;
            return;
        }
        let mut t: String = text
            .chars()
            .filter(|c| *c != '\r' && *c != '\n')
            .map(|c| if c.is_control() { '?' } else { c })
            .collect();
        if t.len() > MAX_TRANSCRIPT_TEXT {
            let mut cut = MAX_TRANSCRIPT_TEXT;
            while !t.is_char_boundary(cut) {
                cut -= 1;
            }
            t.truncate(cut);
            t.push('…');
        }
        self.lines.push(TranscriptLine {
            at: Utc::now(),
            direction,
            text: t,
        });
    }

    fn snapshot(&self) -> Vec<TranscriptLine> {
        let mut v = self.lines.clone();
        if self.truncated
            && let Some(last) = v.last()
        {
            v.push(TranscriptLine {
                at: last.at,
                direction: TranscriptDirection::Server,
                text: "(transcript truncated)".into(),
            });
        }
        v
    }

    fn truncate(&mut self, len: usize) {
        self.lines.truncate(len);
        self.truncated = false;
    }
}

struct Txn {
    mail_from: String,
    smtputf8: bool,
    rcpt: Vec<String>,
    /// BDAT chunks received so far; `Some` once BDAT started.
    chunks: Option<Vec<u8>>,
    /// Transcript length before the MAIL command.
    transcript_start: usize,
}

enum Line {
    Ok(Vec<u8>),
    TooLong,
}

pub(crate) struct Session<'a> {
    srv: &'a SmtpServer,
    cfg: &'a MailConfig,
    io: Option<Io>,
    rbuf: Vec<u8>,
    wbuf: Vec<u8>,
    peer: SocketAddr,
    deadline: Instant,
    shutdown: Option<watch::Receiver<bool>>,
    tls: bool,
    helo: Option<String>,
    esmtp: bool,
    auth_user: Option<String>,
    /// The write scope of a managed token that passed AUTH.
    token_scope: Option<Arc<dyn WriteScope>>,
    auth_failures: u32,
    txn: Option<Txn>,
    transcript: Transcript,
    bad_commands: u32,
    commands: u32,
    pub(crate) messages: u32,
}

fn is_ascii(s: &str) -> bool {
    s.is_ascii()
}

fn host_text(h: &str) -> String {
    h.chars()
        .filter(|c| c.is_ascii_graphic())
        .take(255)
        .collect()
}

/// The code of a `fail+<code>@...` recipient: 4xx or 5xx.
pub fn fail_code(address: &str) -> Option<u16> {
    let (local, _) = address.rsplit_once('@')?;
    let (tag, code) = local.split_once('+')?;
    if !tag.eq_ignore_ascii_case("fail") || code.len() != 3 {
        return None;
    }
    let c: u16 = code.parse().ok()?;
    (400..=599).contains(&c).then_some(c)
}

impl<'a> Session<'a> {
    pub(crate) fn new(
        srv: &'a SmtpServer,
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
            helo: None,
            esmtp: false,
            auth_user: None,
            token_scope: None,
            auth_failures: 0,
            txn: None,
            transcript: Transcript::default(),
            bad_commands: 0,
            commands: 0,
            messages: 0,
        }
    }

    fn reply(&mut self, code: u16, enhanced: &str, text: &str) {
        let line = if enhanced.is_empty() {
            format!("{code} {text}")
        } else {
            format!("{code} {enhanced} {text}")
        };
        self.raw_line(&line);
    }

    fn raw_line(&mut self, line: &str) {
        self.transcript.push(TranscriptDirection::Server, line);
        self.wbuf.extend_from_slice(line.as_bytes());
        self.wbuf.extend_from_slice(b"\r\n");
    }

    fn client_line(&mut self, text: &str) {
        self.transcript.push(TranscriptDirection::Client, text);
    }

    async fn flush(&mut self) -> Step {
        if self.wbuf.is_empty() {
            return Ok(());
        }
        let io = self.io.as_mut().ok_or(Close::Io)?;
        let wait = self.cfg.idle_timeout;
        let buf = std::mem::take(&mut self.wbuf);
        match tokio::time::timeout(wait, async {
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

    /// Reads more bytes into the buffer after sending the queued replies.
    async fn fill(&mut self, idle: Duration, interruptible: bool) -> Step {
        self.flush().await?;
        let now = Instant::now();
        if now >= self.deadline {
            return Err(Close::SessionTimeout);
        }
        let session_left = self.deadline - now;
        let wait = idle.min(session_left);
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
            Err(_) if wait < idle => Err(Close::SessionTimeout),
            Err(_) => Err(if interruptible {
                Close::IdleTimeout
            } else {
                Close::DataTimeout
            }),
            Ok(Ok(0)) => Err(Close::Eof),
            Ok(Ok(_)) => Ok(()),
            Ok(Err(_)) => Err(Close::Io),
        }
    }

    async fn next_line(&mut self) -> Step<Line> {
        let max = self.cfg.max_line_bytes;
        let mut too_long = false;
        loop {
            if let Some(pos) = self.rbuf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.rbuf.drain(..=pos).collect();
                if too_long || line.len() > max {
                    return Ok(Line::TooLong);
                }
                return Ok(Line::Ok(line));
            }
            if self.rbuf.len() > max {
                too_long = true;
                self.rbuf.clear();
            }
            let idle = self.cfg.idle_timeout;
            self.fill(idle, true).await?;
        }
    }

    fn auth_offered(&self) -> bool {
        self.tls || self.srv.tls.is_none() || self.cfg.allow_plaintext_auth
    }

    fn user_area(&self, user: Option<&str>) -> Option<String> {
        user.filter(|u| self.srv.service.spaces.contains(*u))
            .map(String::from)
    }

    /// Runs the chaos rules of a stage. `Ok(Some(..))` is a reply to send instead of the normal one.
    async fn chaos(
        &mut self,
        areas: Vec<Option<String>>,
        event: ChaosEvent<'_>,
    ) -> Step<Option<(u16, String, String)>> {
        let Some(chaos) = self.srv.chaos.clone() else {
            return Ok(None);
        };
        let Some(d) = chaos.evaluate_areas(&areas, &event).await else {
            return Ok(None);
        };
        if let Some(delay) = d.delay {
            self.flush().await?;
            let left = self.deadline.saturating_duration_since(Instant::now());
            tokio::time::sleep(delay.min(left)).await;
        }
        match d.effect {
            ChaosEffect::Continue | ChaosEffect::ProviderError(_) => Ok(None),
            ChaosEffect::Disconnect => {
                rejected("chaos");
                Err(Close::Chaos)
            }
            ChaosEffect::Reply {
                code,
                enhanced,
                text,
            } => Ok(Some((code, enhanced, text))),
        }
    }

    /// Sends a chaos reply; 421 closes the connection.
    fn chaos_reply(&mut self, (code, enhanced, text): (u16, String, String)) -> Step {
        rejected("chaos");
        self.reply(code, &enhanced, &text);
        if code == 421 {
            return Err(Close::Chaos);
        }
        Ok(())
    }

    /// Runs the session and closes it.
    pub(crate) async fn drive(&mut self, implicit_tls: bool) -> SessionEnd {
        let r = self.run(implicit_tls).await;
        self.close(r).await
    }

    async fn run(&mut self, implicit_tls: bool) -> Step {
        if implicit_tls {
            match self
                .chaos(vec![None], ChaosEvent::new(ChaosStage::Tls))
                .await?
            {
                Some(_) => {
                    rejected("chaos");
                    return Err(Close::Chaos);
                }
                None => self.handshake().await?,
            }
        }
        if let Some(r) = self
            .chaos(vec![None], ChaosEvent::new(ChaosStage::Connect))
            .await?
        {
            rejected("chaos");
            let (code, enhanced, text) = r;
            self.reply(code, &enhanced, &text);
            return Err(Close::Chaos);
        }
        let host = self.cfg.hostname.clone();
        self.reply(220, "", &format!("{host} ESMTP FastMock"));
        loop {
            let line = self.next_line().await?;
            self.commands += 1;
            if self.commands > MAX_COMMANDS {
                self.client_line("(command)");
                rejected("too_many_commands");
                self.reply(421, "4.7.0", "Too many commands; closing connection");
                return Err(Close::Rejected);
            }
            let line = match line {
                Line::Ok(l) => l,
                Line::TooLong => {
                    self.client_line("(line too long)");
                    self.reply(500, "5.5.2", "Line too long");
                    self.bad_command()?;
                    continue;
                }
            };
            let parsed = self.srv.codec.command(&line);
            match &parsed {
                Ok(Command::Auth { mechanism, initial }) => {
                    let name = match mechanism {
                        Mechanism::Other => line
                            .split(|b| *b == b' ')
                            .nth(1)
                            .map(|m| String::from_utf8_lossy(m).trim().to_ascii_uppercase())
                            .unwrap_or_default(),
                        m => m.as_str().to_string(),
                    };
                    let masked = if initial.is_some() { " ****" } else { "" };
                    self.client_line(&format!("AUTH {name}{masked}"));
                }
                _ => {
                    let text = String::from_utf8_lossy(&line);
                    self.client_line(text.trim_end_matches(['\r', '\n']));
                }
            }
            match parsed {
                Ok(cmd) => {
                    let good = self.command(cmd).await?;
                    if good {
                        self.bad_commands = 0;
                    } else {
                        self.bad_command()?;
                    }
                }
                Err(e) => {
                    self.command_error(e);
                    self.bad_command()?;
                }
            }
        }
    }

    fn bad_command(&mut self) -> Step {
        self.bad_commands += 1;
        if self.bad_commands >= MAX_BAD_COMMANDS {
            rejected("too_many_errors");
            self.reply(421, "4.7.0", "Too many errors; closing connection");
            return Err(Close::Rejected);
        }
        Ok(())
    }

    fn command_error(&mut self, e: CommandError) {
        match e {
            CommandError::Unknown => self.reply(500, "5.5.1", "Command not recognized"),
            CommandError::BadSender => self.reply(501, "5.1.7", "Bad sender address syntax"),
            CommandError::BadRecipient => self.reply(501, "5.1.3", "Bad recipient address syntax"),
            CommandError::Syntax(s) => self.reply(501, "5.5.4", &format!("Syntax: {s}")),
            CommandError::InvalidParameter(p) => {
                self.reply(501, "5.5.4", &format!("Invalid parameter {p}"))
            }
            CommandError::UnsupportedParameter(p) => {
                self.reply(504, "5.5.4", &format!("Unsupported parameter {p}"))
            }
        }
    }

    /// Handles a command; returns whether it was accepted (refusals count towards
    /// [`MAX_BAD_COMMANDS`]).
    async fn command(&mut self, cmd: Command) -> Step<bool> {
        match cmd {
            Command::Ehlo { host } => self.ehlo(host, true).await,
            Command::Helo { host } => self.ehlo(host, false).await,
            Command::Mail {
                address,
                size,
                smtputf8,
                body,
            } => self.mail(address, size, smtputf8, body).await,
            Command::Rcpt { address } => self.rcpt(address).await,
            Command::Data => self.data().await,
            Command::Bdat { size, last } => self.bdat(size, last).await,
            Command::Auth { mechanism, initial } => self.auth(mechanism, initial).await,
            Command::StartTls => self.starttls().await,
            Command::Rset => {
                if let Some(t) = self.txn.take() {
                    self.transcript.truncate(t.transcript_start);
                }
                self.reply(250, "2.0.0", "OK");
                Ok(true)
            }
            Command::Noop => {
                self.reply(250, "2.0.0", "OK");
                Ok(true)
            }
            Command::Vrfy => {
                self.reply(
                    252,
                    "2.5.0",
                    "Cannot VRFY user, but will accept the message",
                );
                Ok(true)
            }
            Command::Quit => {
                self.reply(221, "2.0.0", "Bye");
                Err(Close::Quit)
            }
            Command::NotImplemented(name) => {
                self.reply(502, "5.5.1", &format!("{name} is not implemented"));
                Ok(false)
            }
        }
    }

    async fn ehlo(&mut self, host: String, esmtp: bool) -> Step<bool> {
        if let Some(r) = self
            .chaos(vec![None], ChaosEvent::new(ChaosStage::Ehlo))
            .await?
        {
            self.chaos_reply(r)?;
            return Ok(true);
        }
        self.txn = None;
        self.helo = Some(host_text(&host));
        self.esmtp = esmtp;
        let name = self.cfg.hostname.clone();
        if !esmtp {
            self.reply(250, "", &name);
            return Ok(true);
        }
        let mut caps = vec![
            format!("{name} greets {}", host_text(&host)),
            format!("SIZE {}", self.cfg.max_message_bytes),
            "8BITMIME".into(),
            "SMTPUTF8".into(),
            "PIPELINING".into(),
            "CHUNKING".into(),
            "ENHANCEDSTATUSCODES".into(),
        ];
        if self.srv.tls.is_some() && !self.tls {
            caps.push("STARTTLS".into());
        }
        if self.auth_offered() {
            caps.push(if self.cfg.smtp_require_auth {
                "AUTH PLAIN LOGIN".into()
            } else {
                "AUTH PLAIN LOGIN CRAM-MD5".into()
            });
        }
        let n = caps.len();
        for (i, c) in caps.iter().enumerate() {
            let sep = if i + 1 == n { ' ' } else { '-' };
            self.raw_line(&format!("250{sep}{c}"));
        }
        Ok(true)
    }

    async fn mail(
        &mut self,
        address: String,
        size: Option<usize>,
        smtputf8: bool,
        body: BodyType,
    ) -> Step<bool> {
        if self.helo.is_none() {
            self.reply(503, "5.5.1", "Send HELO or EHLO first");
            return Ok(false);
        }
        if self.txn.is_some() {
            self.reply(503, "5.5.1", "Sender already given");
            return Ok(false);
        }
        if self.cfg.smtp_require_auth && self.auth_user.is_none() {
            rejected("auth_required");
            self.reply(530, "5.7.0", "Authentication required");
            return Ok(false);
        }
        if body == BodyType::BinaryMime {
            self.reply(555, "5.5.4", "BODY=BINARYMIME is not supported");
            return Ok(false);
        }
        if !smtputf8 && !is_ascii(&address) {
            rejected("smtputf8");
            self.reply(553, "5.6.7", "Non-ASCII address needs SMTPUTF8");
            return Ok(false);
        }
        if size.is_some_and(|s| s > self.cfg.max_message_bytes) {
            rejected("size");
            self.reply(
                552,
                "5.3.4",
                "Message size exceeds fixed maximum message size",
            );
            return Ok(false);
        }
        let area = self.user_area(self.auth_user.as_deref());
        let user = self.auth_user.clone();
        let ev = ChaosEvent {
            mail_from: Some(&address),
            auth_user: user.as_deref(),
            ..ChaosEvent::new(ChaosStage::Mail)
        };
        if let Some(r) = self.chaos(vec![area], ev).await? {
            self.chaos_reply(r)?;
            return Ok(true);
        }
        self.txn = Some(Txn {
            mail_from: address,
            smtputf8,
            rcpt: vec![],
            chunks: None,
            transcript_start: self.transcript.lines.len().saturating_sub(1),
        });
        self.reply(250, "2.1.0", "Sender OK");
        Ok(true)
    }

    async fn rcpt(&mut self, address: String) -> Step<bool> {
        let max = self.cfg.max_recipients;
        let Some((bdat, count, smtputf8, mail_from)) = self.txn.as_ref().map(|t| {
            (
                t.chunks.is_some(),
                t.rcpt.len(),
                t.smtputf8,
                t.mail_from.clone(),
            )
        }) else {
            self.reply(503, "5.5.1", "Send MAIL first");
            return Ok(false);
        };
        if bdat {
            self.reply(503, "5.5.1", "RCPT is not allowed during BDAT");
            return Ok(false);
        }
        if count >= max {
            rejected("recipients");
            self.reply(452, "4.5.3", "Too many recipients");
            return Ok(true);
        }
        if !smtputf8 && !is_ascii(&address) {
            rejected("smtputf8");
            self.reply(553, "5.6.7", "Non-ASCII address needs SMTPUTF8");
            return Ok(false);
        }
        if let Some(code) = fail_code(&address) {
            rejected("fail_address");
            self.reply(
                code,
                &format!("{}.0.0", code / 100),
                &format!("Rejected by the fail+{code} address"),
            );
            if code == 421 {
                return Err(Close::Chaos);
            }
            return Ok(true);
        }
        let user = self.auth_user.clone();
        let areas: Vec<Option<String>> = self
            .srv
            .service
            .routes(std::slice::from_ref(&address), user.as_deref())
            .into_iter()
            .map(|r| r.space)
            .collect();
        if let Some(scope) = &self.token_scope
            && areas.iter().any(|a| !scope.can_write(a.as_deref()))
        {
            rejected("rcpt_scope");
            self.reply(550, "5.7.1", "Not authorized to deliver to this recipient");
            return Ok(true);
        }
        let one = [address.clone()];
        let ev = ChaosEvent {
            mail_from: Some(&mail_from),
            rcpt_to: &one,
            auth_user: user.as_deref(),
            ..ChaosEvent::new(ChaosStage::Rcpt)
        };
        if let Some(r) = self.chaos(areas, ev).await? {
            self.chaos_reply(r)?;
            return Ok(true);
        }
        if let Some(t) = self.txn.as_mut() {
            t.rcpt.push(address);
        }
        self.reply(250, "2.1.5", "Recipient OK");
        Ok(true)
    }

    async fn data(&mut self) -> Step<bool> {
        let Some((bdat, no_rcpt)) = self
            .txn
            .as_ref()
            .map(|t| (t.chunks.is_some(), t.rcpt.is_empty()))
        else {
            self.reply(503, "5.5.1", "Send MAIL first");
            return Ok(false);
        };
        if bdat {
            self.reply(503, "5.5.1", "DATA cannot be mixed with BDAT");
            return Ok(false);
        }
        if no_rcpt {
            self.reply(554, "5.5.1", "No valid recipients");
            return Ok(false);
        }
        self.reply(354, "", "Start mail input; end with <CRLF>.<CRLF>");
        let max = self.cfg.max_message_bytes;
        let mut dec = self.srv.codec.body_decoder();
        let mut body = Vec::new();
        let mut dropped = 0usize;
        loop {
            if !self.rbuf.is_empty() {
                let (n, done) = dec.feed(&self.rbuf, &mut body);
                self.rbuf.drain(..n);
                if body.len() > max {
                    dropped += body.len();
                    body.clear();
                }
                if done {
                    break;
                }
            }
            let t = self.cfg.data_timeout;
            self.fill(t, false).await?;
        }
        let size = dropped + body.len();
        self.client_line(&format!("(message: {size} bytes)"));
        if dropped > 0 {
            rejected("size");
            self.txn = None;
            self.reply(
                552,
                "5.3.4",
                "Message size exceeds fixed maximum message size",
            );
            return Ok(true);
        }
        self.finish(body).await?;
        Ok(true)
    }

    async fn bdat(&mut self, size: usize, last: bool) -> Step<bool> {
        let max = self.cfg.max_message_bytes;
        if size > max {
            rejected("size");
            self.txn = None;
            self.reply(
                552,
                "5.3.4",
                "Message size exceeds fixed maximum message size",
            );
            return Err(Close::Rejected);
        }
        let refusal = match &self.txn {
            None => Some((503, "5.5.1", "Send MAIL first")),
            Some(t) if t.rcpt.is_empty() => Some((554, "5.5.1", "No valid recipients")),
            _ => None,
        };
        let mut chunks = self
            .txn
            .as_mut()
            .and_then(|t| t.chunks.take())
            .unwrap_or_default();
        let keep = refusal.is_none() && chunks.len().saturating_add(size) <= max;
        let mut left = size;
        while left > 0 {
            if self.rbuf.is_empty() {
                let t = self.cfg.data_timeout;
                self.fill(t, false).await?;
            }
            let take = left.min(self.rbuf.len());
            if keep {
                chunks.extend_from_slice(&self.rbuf[..take]);
            }
            self.rbuf.drain(..take);
            left -= take;
        }
        if let Some((code, enh, text)) = refusal {
            self.reply(code, enh, text);
            return Ok(false);
        }
        if !keep {
            rejected("size");
            self.txn = None;
            self.reply(
                552,
                "5.3.4",
                "Message size exceeds fixed maximum message size",
            );
            return Ok(true);
        }
        if !last {
            if let Some(t) = self.txn.as_mut() {
                t.chunks = Some(chunks);
            }
            self.reply(250, "2.0.0", &format!("{size} octets received"));
            return Ok(true);
        }
        self.client_line(&format!("(message: {} bytes)", chunks.len()));
        self.finish(chunks).await?;
        Ok(true)
    }

    async fn finish(&mut self, raw: Vec<u8>) -> Step {
        let Some(txn) = self.txn.take() else {
            return Ok(());
        };
        let user = self.auth_user.clone();
        let svc = self.srv.service.clone();
        let areas: Vec<Option<String>> = svc
            .routes(&txn.rcpt, user.as_deref())
            .into_iter()
            .map(|r| r.space)
            .collect();
        let ev = ChaosEvent {
            mail_from: Some(&txn.mail_from),
            rcpt_to: &txn.rcpt,
            auth_user: user.as_deref(),
            ..ChaosEvent::new(ChaosStage::Data)
        };
        if let Some(r) = self.chaos(areas, ev).await? {
            self.transcript.truncate(txn.transcript_start);
            return self.chaos_reply(r);
        }
        let mut inc = Incoming::new(
            MailSource::Smtp,
            Envelope {
                mail_from: txn.mail_from,
                rcpt_to: txn.rcpt,
                helo: self.helo.clone(),
                client_ip: Some(self.peer.ip().to_string()),
                tls: self.tls,
                auth_user: user,
            },
            raw,
        );
        inc.transcript = self.transcript.snapshot();
        self.transcript.truncate(txn.transcript_start);
        match svc.accept(inc).await {
            Ok(entries) => {
                self.messages += 1;
                let id = entries
                    .first()
                    .map(|e| e.id.to_string())
                    .unwrap_or_default();
                self.reply(250, "2.0.0", &format!("OK: queued as {id}"));
            }
            Err(AcceptError::TooLarge(_)) => {
                rejected("size");
                self.reply(
                    552,
                    "5.3.4",
                    "Message size exceeds fixed maximum message size",
                );
            }
            Err(AcceptError::Store(e)) => {
                tracing::error!(error = %e, "storing an SMTP message failed");
                self.reply(451, "4.3.0", "Temporary failure storing the message");
            }
        }
        Ok(())
    }

    async fn sasl_line(&mut self) -> Step<Result<Vec<u8>, SaslError>> {
        let line = self.next_line().await?;
        self.client_line("****");
        Ok(match line {
            Line::TooLong => Err(SaslError::Malformed),
            Line::Ok(l) => sasl::decode(String::from_utf8_lossy(&l).trim()),
        })
    }

    async fn challenge(&mut self, text: &str) -> Step<Result<Vec<u8>, SaslError>> {
        self.raw_line(&format!("334 {text}"));
        self.sasl_line().await
    }

    async fn read_credentials(
        &mut self,
        mechanism: Mechanism,
        initial: Option<String>,
    ) -> Step<Result<Credentials, SaslError>> {
        let initial = initial.map(|i| sasl::decode(&i));
        Ok(match mechanism {
            Mechanism::Plain => {
                let msg = match initial {
                    Some(m) => m,
                    None => self.challenge("").await?,
                };
                msg.and_then(|m| sasl::plain(&m))
            }
            Mechanism::Login => {
                let user = match initial {
                    Some(u) => u,
                    None => self.challenge(&sasl::encode(b"Username:")).await?,
                };
                let user = match user.and_then(sasl::login_field) {
                    Ok(u) if !u.is_empty() => u,
                    Ok(_) => return Ok(Err(SaslError::Malformed)),
                    Err(e) => return Ok(Err(e)),
                };
                let pass = self.challenge(&sasl::encode(b"Password:")).await?;
                pass.and_then(sasl::login_field).map(|p| Credentials {
                    user,
                    password: Some(p),
                })
            }
            Mechanism::CramMd5 | Mechanism::Other => {
                let ch = sasl::cram_challenge(&self.cfg.hostname);
                let resp = self.challenge(&sasl::encode(ch.as_bytes())).await?;
                resp.and_then(sasl::cram_response)
            }
        })
    }

    /// Checks AUTH credentials: `None` when refused, else the token's write scope (`None` inside for a
    /// configured user or when AUTH is not required).
    async fn verify(srv: &SmtpServer, c: &Credentials) -> Option<Option<Arc<dyn WriteScope>>> {
        let cfg = &*srv.service.config;
        if !cfg.smtp_require_auth {
            return Some(None);
        }
        let pass = c.password.as_ref()?;
        if let Some(u) = cfg.smtp_users.iter().find(|u| u.name == c.user) {
            let ok = if u.is_pbkdf2() {
                let (stored, pw) = (u.hash.clone(), pass.clone());
                srv.service
                    .auth
                    .hash(move || verify_password(&stored, &pw))
                    .await
                    .unwrap_or(false)
            } else {
                let hash = hex::encode(Sha256::digest(pass.as_bytes()));
                bool::from(u.hash.as_bytes().ct_eq(hash.as_bytes()))
            };
            if ok {
                return Some(None);
            }
        }
        match &srv.tokens {
            Some(t) => t.allows(&c.user, pass).await.map(Some),
            None => None,
        }
    }

    async fn auth(&mut self, mechanism: Mechanism, initial: Option<String>) -> Step<bool> {
        if !self.esmtp {
            self.reply(503, "5.5.1", "Send EHLO first");
            return Ok(false);
        }
        if self.auth_user.is_some() {
            self.reply(503, "5.5.1", "Already authenticated");
            return Ok(false);
        }
        if self.txn.is_some() {
            self.reply(
                503,
                "5.5.1",
                "AUTH is not allowed during a mail transaction",
            );
            return Ok(false);
        }
        if !self.auth_offered() {
            rejected("auth_before_tls");
            self.reply(
                538,
                "5.7.11",
                "Encryption required for requested authentication mechanism",
            );
            return Ok(false);
        }
        let cram_ok = !self.cfg.smtp_require_auth;
        if mechanism == Mechanism::Other || (mechanism == Mechanism::CramMd5 && !cram_ok) {
            self.reply(504, "5.5.4", "Unrecognized authentication type");
            return Ok(false);
        }
        let creds = match self.read_credentials(mechanism, initial).await? {
            Ok(c) => Some(c),
            Err(SaslError::Cancelled) => {
                self.reply(501, "5.0.0", "Authentication cancelled");
                return Ok(false);
            }
            Err(SaslError::Malformed) => None,
        };
        let guard = self.srv.service.auth.clone();
        let ip = self.peer.ip();
        let throttled = self.cfg.smtp_require_auth;
        let verified = match &creds {
            Some(c) if throttled && guard.blocked(ip, &c.user).await => None,
            Some(c) => Self::verify(self.srv, c).await,
            None => None,
        };
        if throttled && let Some(c) = &creds {
            match verified {
                Some(_) => guard.succeeded(&c.user).await,
                None => guard.failed(ip, &c.user).await,
            }
        }
        let Some((creds, scope)) = creds.zip(verified) else {
            self.auth_failures += 1;
            rejected("auth_failed");
            if self.auth_failures >= self.cfg.max_auth_failures {
                self.reply(
                    421,
                    "4.7.0",
                    "Too many failed authentication attempts; closing connection",
                );
                return Err(Close::Rejected);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            self.reply(535, "5.7.8", "Authentication credentials invalid");
            return Ok(false);
        };
        let area = self.user_area(Some(&creds.user));
        if let Some(a) = &area {
            tracing::Span::current().record("fastmock.space", a.as_str());
        }
        let ev = ChaosEvent {
            auth_user: Some(&creds.user),
            ..ChaosEvent::new(ChaosStage::Auth)
        };
        if let Some(r) = self.chaos(vec![area], ev).await? {
            self.chaos_reply(r)?;
            return Ok(true);
        }
        self.auth_user = Some(creds.user);
        self.token_scope = scope;
        self.reply(235, "2.7.0", "Authentication successful");
        Ok(true)
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
                tracing::debug!(error = %e, peer = %self.peer, "SMTP TLS handshake failed");
                Err(Close::Io)
            }
            Err(_) => Err(Close::Io),
        }
    }

    async fn starttls(&mut self) -> Step<bool> {
        if self.srv.tls.is_none() {
            self.reply(502, "5.5.1", "STARTTLS is not offered");
            return Ok(false);
        }
        if self.tls {
            self.reply(503, "5.5.1", "TLS is already active");
            return Ok(false);
        }
        if let Some(r) = self
            .chaos(vec![None], ChaosEvent::new(ChaosStage::Tls))
            .await?
        {
            self.chaos_reply(r)?;
            return Ok(true);
        }
        self.reply(220, "2.0.0", "Ready to start TLS");
        self.flush().await?;
        if !self.rbuf.is_empty() {
            rejected("starttls_pipelined");
            tracing::warn!(peer = %self.peer, "SMTP client sent bytes after STARTTLS; closing");
            return Err(Close::Rejected);
        }
        self.handshake().await?;
        self.helo = None;
        self.esmtp = false;
        self.auth_user = None;
        self.token_scope = None;
        self.txn = None;
        Ok(true)
    }

    /// Sends what the end of the session needs and closes.
    async fn close(&mut self, r: Step) -> SessionEnd {
        let close = match r {
            Ok(()) => Close::Quit,
            Err(c) => c,
        };
        match close {
            Close::IdleTimeout | Close::DataTimeout => {
                rejected("timeout");
                self.reply(
                    421,
                    "4.4.2",
                    "Timeout waiting for the client; closing connection",
                );
            }
            Close::SessionTimeout => {
                rejected("timeout");
                self.reply(
                    421,
                    "4.4.2",
                    "Session time limit reached; closing connection",
                );
            }
            Close::Shutdown => {
                self.reply(421, "4.3.2", "Service shutting down; closing connection");
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

pub(crate) fn boxed<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(s: S) -> Io {
    Box::new(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fail_addresses() {
        assert_eq!(fail_code("fail+451@example.test"), Some(451));
        assert_eq!(fail_code("FAIL+550@x"), Some(550));
        assert_eq!(fail_code("fail+250@x"), None);
        assert_eq!(fail_code("fail+600@x"), None);
        assert_eq!(fail_code("fail+45@x"), None);
        assert_eq!(fail_code("fail+4510@x"), None);
        assert_eq!(fail_code("nofail+451@x"), None);
        assert_eq!(fail_code("fail+451"), None);
    }

    #[test]
    fn transcript_caps() {
        let mut t = Transcript::default();
        t.push(TranscriptDirection::Client, "a\r\nb\x07");
        assert_eq!(t.lines[0].text, "ab?");
        t.push(TranscriptDirection::Client, &"é".repeat(400));
        assert!(t.lines[1].text.len() <= MAX_TRANSCRIPT_TEXT + 3);
        for _ in 0..MAX_TRANSCRIPT_LINES {
            t.push(TranscriptDirection::Server, "x");
        }
        let s = t.snapshot();
        assert_eq!(s.len(), MAX_TRANSCRIPT_LINES + 1);
        assert_eq!(s.last().unwrap().text, "(transcript truncated)");
        t.truncate(1);
        assert_eq!(t.snapshot().len(), 1);
    }
}
