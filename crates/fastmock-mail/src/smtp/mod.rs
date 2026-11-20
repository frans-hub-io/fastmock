//! The SMTP listener (phase B1): an own tokio session state machine on top of `smtp-proto`, kept behind
//! an internal parser trait. A session ends in [`crate::MailService::accept`] with
//! [`crate::Incoming`] (`source: smtp`, the envelope, the raw message and the masked transcript);
//! limits, timeouts and AUTH settings come from [`crate::MailConfig`], TLS from
//! [`crate::tls::server_config`].
