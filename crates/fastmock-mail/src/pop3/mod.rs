//! The POP3 listener (phase C2, RFC 1939 with CAPA (RFC 2449), STLS (RFC 2595), AUTH PLAIN (RFC 5034)
//! and response codes (RFC 2449, RFC 3206)), written by hand. A session logs in to the INBOX of one
//! mailbox ([`login::MailboxLogin`]), takes a snapshot of its entries (the maildrop does not change during
//! the session), and on QUIT removes the messages marked with DELE from that INBOX only
//! ([`fastmock_core::mail::MailStore::mailbox_remove`]); the stored messages stay in the admin API.
//!
//! The server crate accepts connections (and applies the connection limits shared with SMTP and IMAP)
//! and hands each one to [`Pop3Server::serve`].

pub mod command;
pub mod login;
pub mod render;
mod session;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;

use crate::service::MailService;

pub use crate::smtp::SessionEnd;
pub use login::{
    LoginOk, MailboxLogin, MailboxTokenCheck, hash_password, hash_password_with, verify_password,
};

/// The reply to a POP3 connection over the connection limits, sent before closing it.
pub const BUSY_REPLY: &[u8] = b"-ERR [SYS/TEMP] Too many connections, try again later\r\n";
/// How long a failed login waits before its reply.
pub const FAILURE_DELAY: Duration = Duration::from_millis(500);
/// Most commands in one session.
pub const MAX_COMMANDS: u32 = 100_000;
/// Consecutive refused commands (unknown, wrong syntax or state) before the connection is closed.
pub const MAX_BAD_COMMANDS: u32 = 20;

/// What every POP3 session of a server shares.
pub struct Pop3Server {
    pub service: Arc<MailService>,
    /// With a certificate, STLS is offered and POP3S listeners can run.
    pub tls: Option<Arc<rustls::ServerConfig>>,
    pub login: MailboxLogin,
    /// Wait before answering a failed login ([`FAILURE_DELAY`]).
    pub failure_delay: Duration,
}

/// The outcome of a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pop3Report {
    pub end: SessionEnd,
    /// The mailbox the session logged in to.
    pub mailbox: Option<String>,
    /// Entries removed from the INBOX at QUIT.
    pub deleted: u32,
}

impl Pop3Server {
    pub fn new(
        service: Arc<MailService>,
        tls: Option<Arc<rustls::ServerConfig>>,
        tokens: Option<Arc<dyn MailboxTokenCheck>>,
    ) -> Self {
        Self {
            login: MailboxLogin::new(service.clone(), tokens),
            service,
            tls,
            failure_delay: FAILURE_DELAY,
        }
    }

    /// Runs one session to its end. `implicit_tls` (POP3S) does the TLS handshake first. While waiting
    /// for a command, a change of `shutdown` to `true` answers `-ERR [SYS/TEMP]` and closes without
    /// removing messages.
    pub async fn serve<S>(
        &self,
        stream: S,
        peer: SocketAddr,
        implicit_tls: bool,
        shutdown: Option<watch::Receiver<bool>>,
    ) -> Pop3Report
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut s = session::Session::new(self, Box::new(stream), peer, shutdown);
        let end = s.drive(implicit_tls).await;
        metrics::counter!("fastmock_mail_pop3_sessions_total", "result" => end.as_str())
            .increment(1);
        tracing::debug!(peer = %peer.ip(), result = end.as_str(), deleted = s.deleted, "POP3 session ended");
        Pop3Report {
            end,
            mailbox: s.mailbox(),
            deleted: s.deleted,
        }
    }
}

#[cfg(test)]
mod tests;
