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

/// The `fastmock.mail.session` span of one SMTP, IMAP or POP3 session: protocol, client address and,
/// once known, the result, the logged-in area (`fastmock.space`) and the messages accepted.
pub fn session_span(protocol: &'static str, peer: SocketAddr, implicit_tls: bool) -> tracing::Span {
    tracing::info_span!(
        "fastmock.mail.session",
        otel.name = %format!("{protocol} session"),
        otel.kind = "server",
        network.protocol.name = protocol,
        client.address = %peer.ip(),
        fastmock.mail.implicit_tls = implicit_tls,
        fastmock.space = tracing::field::Empty,
        fastmock.mail.messages = tracing::field::Empty,
        fastmock.mail.result = tracing::field::Empty,
    )
}

/// The reply to a plain SMTP connection over the connection limits, sent before closing it.
pub const BUSY_REPLY: &[u8] = b"421 4.7.0 Too many connections, try again later\r\n";

/// The areas a managed token that passed AUTH may deliver to: where it holds `mail:write`.
pub trait WriteScope: Send + Sync {
    /// Whether the token holds `mail:write` in `area` (a space; `None` is the global area). A grant in
    /// the global area covers every space.
    fn can_write(&self, area: Option<&str>) -> bool;
}

/// Checks a managed FastMock token given as the AUTH password while
/// `FASTMOCK_MAIL_SMTP_REQUIRE_AUTH` is on; the server implements it.
#[async_trait]
pub trait TokenCheck: Send + Sync {
    /// `Some` when `token` may send mail as `user`, with the areas it may deliver to; every RCPT is
    /// checked against them.
    async fn allows(&self, user: &str, token: &str) -> Option<Arc<dyn WriteScope>>;
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
        use tracing::Instrument as _;
        let span = session_span("smtp", peer, implicit_tls);
        let mut s = session::Session::new(self, session::boxed(stream), peer, shutdown);
        let end = s.drive(implicit_tls).instrument(span.clone()).await;
        span.record("fastmock.mail.result", end.as_str());
        span.record("fastmock.mail.messages", s.messages);
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
