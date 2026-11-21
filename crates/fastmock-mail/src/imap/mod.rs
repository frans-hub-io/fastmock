//! The IMAP listener (phase C1): an own tokio session loop; `imap-codec` parses commands behind
//! [`codec`], responses are written by [`response`]. Mailboxes live in [`crate::MailStore`]
//! (`mailbox_*`), IDLE follows [`crate::MailEvents`], logins go through [`login`].
//!
//! The server crate accepts connections (and applies the connection limits) and hands each one to
//! [`ImapServer::serve`].

pub mod codec;
pub mod command;
pub mod login;
pub mod mime;
pub mod response;
pub mod search;
mod session;

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;

use crate::service::MailService;
pub use crate::smtp::SessionEnd;
pub use login::{PasswordVerify, ReadTokenCheck, hash_password, verify_password};
pub use session::{AUTH_FAILURE_DELAY, IDLE_POLL, MAX_BAD_COMMANDS, MAX_COMMANDS, MAX_FOLDERS};

/// The reply to a plain IMAP connection over the connection limits, sent before closing it.
pub const BUSY_REPLY: &[u8] = b"* BYE Too many connections, try again later\r\n";

/// What every IMAP session of a server shares.
pub struct ImapServer {
    pub service: Arc<MailService>,
    /// With a certificate, STARTTLS is offered and IMAPS listeners can run.
    pub tls: Option<Arc<rustls::ServerConfig>>,
    pub tokens: Option<Arc<dyn ReadTokenCheck>>,
    /// Compares mailbox password hashes ([`verify_password`]: `pbkdf2-sha256$...` by default).
    pub verify_password: PasswordVerify,
}

impl ImapServer {
    pub fn new(
        service: Arc<MailService>,
        tls: Option<Arc<rustls::ServerConfig>>,
        tokens: Option<Arc<dyn ReadTokenCheck>>,
    ) -> Self {
        Self {
            service,
            tls,
            tokens,
            verify_password,
        }
    }

    /// Runs one session to its end. `implicit_tls` (IMAPS) does the TLS handshake first. While waiting
    /// for a command or in IDLE, a change of `shutdown` to `true` sends `* BYE` and closes.
    pub async fn serve<S>(
        &self,
        stream: S,
        peer: SocketAddr,
        implicit_tls: bool,
        shutdown: Option<watch::Receiver<bool>>,
    ) -> SessionEnd
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut s = session::Session::new(self, Box::new(stream), peer, shutdown);
        let end = s.drive(implicit_tls).await;
        metrics::counter!("fastmock_mail_imap_sessions_total", "result" => end.as_str())
            .increment(1);
        tracing::debug!(peer = %peer.ip(), result = end.as_str(), "IMAP session ended");
        end
    }
}

#[cfg(test)]
mod tests;
