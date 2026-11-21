//! The SMTP listener (phase B1): an own tokio session state machine on top of `smtp-proto`, kept behind
//! the internal [`codec::SmtpCodec`] trait. A session ends in [`crate::MailService::accept`] with
//! [`crate::Incoming`] (`source: smtp`, the envelope, the raw message and the masked transcript);
//! limits, timeouts and AUTH settings come from [`crate::MailConfig`], TLS from
//! [`crate::tls::server_config`], chaos from [`crate::chaos::MailChaos`].
//!
//! The server crate accepts connections (and applies the connection limits) and hands each one to
//! [`SmtpServer::serve`].

pub mod codec;
pub mod sasl;
mod session;

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;

use crate::chaos::MailChaos;
use crate::service::MailService;

pub use session::{
    MAX_BAD_COMMANDS, MAX_COMMANDS, MAX_TRANSCRIPT_LINES, MAX_TRANSCRIPT_TEXT, SessionEnd,
    fail_code,
};

/// The reply to a plain SMTP connection over the connection limits, sent before closing it.
pub const BUSY_REPLY: &[u8] = b"421 4.7.0 Too many connections, try again later\r\n";

/// Checks a managed FastMock token given as the AUTH password while
/// `FASTMOCK_MAIL_SMTP_REQUIRE_AUTH` is on; the server implements it.
#[async_trait]
pub trait TokenCheck: Send + Sync {
    /// Whether `token` may send mail as `user`.
    async fn allows(&self, user: &str, token: &str) -> bool;
}

/// What every SMTP session of a server shares.
pub struct SmtpServer {
    pub service: Arc<MailService>,
    /// With a certificate, STARTTLS is offered and SMTPS listeners can run.
    pub tls: Option<Arc<rustls::ServerConfig>>,
    pub tokens: Option<Arc<dyn TokenCheck>>,
    pub chaos: Option<Arc<MailChaos>>,
    codec: Arc<dyn codec::SmtpCodec>,
}

/// The outcome of a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionReport {
    pub end: SessionEnd,
    /// Messages accepted in the session.
    pub messages: u32,
}

impl SmtpServer {
    pub fn new(
        service: Arc<MailService>,
        tls: Option<Arc<rustls::ServerConfig>>,
        tokens: Option<Arc<dyn TokenCheck>>,
        chaos: Option<Arc<MailChaos>>,
    ) -> Self {
        Self {
            service,
            tls,
            tokens,
            chaos,
            codec: Arc::new(codec::ProtoCodec),
        }
    }

    /// Runs one session to its end. `implicit_tls` (SMTPS) does the TLS handshake first. While waiting
    /// for a command, a change of `shutdown` to `true` answers `421` and closes.
    pub async fn serve<S>(
        &self,
        stream: S,
        peer: SocketAddr,
        implicit_tls: bool,
        shutdown: Option<watch::Receiver<bool>>,
    ) -> SessionReport
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut s = session::Session::new(self, session::boxed(stream), peer, shutdown);
        let end = s.drive(implicit_tls).await;
        metrics::counter!("fastmock_mail_smtp_sessions_total", "result" => end.as_str())
            .increment(1);
        tracing::debug!(peer = %peer.ip(), result = end.as_str(), messages = s.messages, "SMTP session ended");
        SessionReport {
            end,
            messages: s.messages,
        }
    }
}

#[cfg(test)]
mod tests;
