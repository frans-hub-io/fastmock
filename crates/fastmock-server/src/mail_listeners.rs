//! Binds the mail listeners (SMTP, SMTPS, POP3, POP3S, IMAP, IMAPS) from `MailConfig`, applies the
//! connection limits shared by all mail listeners (`FASTMOCK_MAIL_MAX_CONNECTIONS`,
//! `FASTMOCK_MAIL_MAX_CONNECTIONS_PER_IP`) and stops with the server: no new connections after the
//! shutdown signal, sessions waiting for a command get the protocol's closing reply (SMTP `421`, POP3
//! `-ERR [SYS/TEMP]`, IMAP `* BYE`), the rest get a grace period. Every listener runs the same accept
//! loop ([`serve_connections`]).

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use fastmock_mail::imap::{self, ImapServer};
use fastmock_mail::mailbox_login::{MailboxAccess, MailboxTokenCheck};
use fastmock_mail::pop3::{self, Pop3Server};
use fastmock_mail::smtp::{self, SmtpServer, TokenCheck, WriteScope};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::access::Permission;
use crate::auth::Principal;
use crate::state::AppState;
use crate::tokens::{Lookup, looks_managed, principal_of};

/// How long open sessions may take to finish after the shutdown signal.
pub const DRAIN: Duration = Duration::from_secs(20);

/// Why a connection was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Busy {
    Global,
    PerIp,
}

/// Connections open over all mail listeners, in total and per client address.
pub struct ConnectionLimits {
    max: usize,
    max_per_ip: usize,
    state: Mutex<(usize, HashMap<IpAddr, usize>)>,
}

/// A held connection slot; dropping it frees the slot.
pub struct ConnectionSlot {
    limits: Arc<ConnectionLimits>,
    ip: IpAddr,
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        let mut s = self.limits.state.lock().unwrap_or_else(|e| e.into_inner());
        s.0 = s.0.saturating_sub(1);
        if let Some(n) = s.1.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                s.1.remove(&self.ip);
            }
        }
        metrics::gauge!("fastmock_mail_connections").set(s.0 as f64);
    }
}

impl ConnectionLimits {
    pub fn new(max: usize, max_per_ip: usize) -> Arc<Self> {
        Arc::new(Self {
            max,
            max_per_ip,
            state: Mutex::new((0, HashMap::new())),
        })
    }

    pub fn acquire(self: &Arc<Self>, ip: IpAddr) -> Result<ConnectionSlot, Busy> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.0 >= self.max {
            return Err(Busy::Global);
        }
        let n = s.1.entry(ip).or_insert(0);
        if *n >= self.max_per_ip {
            return Err(Busy::PerIp);
        }
        *n += 1;
        s.0 += 1;
        metrics::gauge!("fastmock_mail_connections").set(s.0 as f64);
        Ok(ConnectionSlot {
            limits: self.clone(),
            ip,
        })
    }

    /// Open connections.
    pub fn open(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).0
    }
}

/// AUTH with a managed token: valid, and allowed `mail:write` in the global area or in the space named
/// like the AUTH user; every recipient must route to an area where it holds `mail:write`. As an
/// IMAP/POP3 password: valid, and allowed `mail:read` in the mailbox's space or globally (a mailbox of
/// the global area needs a global grant); changes need `mail:write` (expunge also `mail:delete`).
pub struct ManagedTokens {
    app: AppState,
}

impl ManagedTokens {
    /// The principal of a valid managed token.
    async fn principal(&self, token: &str, what: &str) -> Option<(uuid::Uuid, Principal)> {
        if !looks_managed(token) {
            return None;
        }
        let t = match self.app.tokens.lookup(token).await {
            Ok(Lookup::Valid(t)) => t,
            Ok(_) => return None,
            Err(e) => {
                tracing::warn!(error = %e, "checking a managed token for {what} failed");
                return None;
            }
        };
        principal_of(&self.app.auth, &t).map(|p| (t.id, p))
    }
}

/// Where a token that passed SMTP AUTH holds `mail:write`.
struct PrincipalScope(Principal);

impl WriteScope for PrincipalScope {
    fn can_write(&self, area: Option<&str>) -> bool {
        self.0.allowed(Permission::MailWrite, area)
    }
}

#[async_trait]
impl TokenCheck for ManagedTokens {
    async fn allows(&self, user: &str, token: &str) -> Option<Arc<dyn WriteScope>> {
        let (id, p) = self.principal(token, "SMTP AUTH").await?;
        let space = self.app.auth.access().space(user).map(|s| s.name.as_str());
        let ok = p.allowed(Permission::MailWrite, None)
            || space.is_some_and(|s| p.allowed(Permission::MailWrite, Some(s)));
        if !ok {
            return None;
        }
        self.app.tokens.touch(id);
        Some(Arc::new(PrincipalScope(p)))
    }
}

#[async_trait]
impl MailboxTokenCheck for ManagedTokens {
    async fn access(&self, area: Option<&str>, token: &str) -> Option<MailboxAccess> {
        let (id, p) = self.principal(token, "a mailbox login").await?;
        if !p.allowed(Permission::MailRead, area) {
            return None;
        }
        self.app.tokens.touch(id);
        Some(MailboxAccess {
            write: p.allowed(Permission::MailWrite, area),
            delete: p.allowed(Permission::MailDelete, area),
        })
    }
}

/// The running mail listeners.
pub struct MailListeners {
    /// Variable name and bound address of each listener.
    pub addrs: Vec<(&'static str, SocketAddr)>,
    pub limits: Arc<ConnectionLimits>,
    tasks: JoinSet<()>,
}

impl MailListeners {
    /// The bound address of a listener by its variable name, e.g. `FASTMOCK_MAIL_SMTP_LISTEN`.
    pub fn addr(&self, var: &str) -> Option<SocketAddr> {
        self.addrs.iter().find(|(n, _)| *n == var).map(|(_, a)| *a)
    }

    /// Waits until every listener and session has stopped.
    pub async fn wait(mut self) {
        while self.tasks.join_next().await.is_some() {}
    }

    /// Binds `addr` (when set) and runs [`serve_connections`] on it.
    async fn bind<F, Fut>(
        &mut self,
        var: &'static str,
        addr: Option<SocketAddr>,
        implicit_tls: bool,
        busy: &'static [u8],
        shutdown: &watch::Receiver<bool>,
        session: F,
    ) -> anyhow::Result<()>
    where
        F: Fn(TcpStream, SocketAddr, watch::Receiver<bool>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let Some(addr) = addr else {
            return Ok(());
        };
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding {var} {addr}"))?;
        let local = listener.local_addr()?;
        tracing::info!(listener = var, addr = %local, tls = implicit_tls, "mail listener started");
        self.addrs.push((var, local));
        self.tasks.spawn(serve_connections(
            listener,
            implicit_tls,
            self.limits.clone(),
            shutdown.clone(),
            busy,
            session,
        ));
        Ok(())
    }
}

/// Binds the SMTP, SMTPS, POP3, POP3S, IMAP and IMAPS listeners that are configured; `None` without the
/// mail module.
pub async fn start(
    app: &AppState,
    shutdown: watch::Receiver<bool>,
) -> anyhow::Result<Option<MailListeners>> {
    let Some(mail) = &app.mail else {
        return Ok(None);
    };
    let cfg = mail.config.clone();
    let tls = cfg
        .tls
        .as_ref()
        .map(fastmock_mail::tls::server_config)
        .transpose()
        .context("mail TLS certificate")?;
    let tokens = Arc::new(ManagedTokens { app: app.clone() });
    let smtp_server = Arc::new(SmtpServer::new(
        mail.clone(),
        tls.clone(),
        Some(tokens.clone() as Arc<dyn TokenCheck>),
        app.mail_chaos.clone(),
    ));
    let pop3_server = Arc::new(Pop3Server::new(
        mail.clone(),
        tls.clone(),
        Some(tokens.clone() as Arc<dyn MailboxTokenCheck>),
    ));
    let imap_server = Arc::new(ImapServer::new(
        mail.clone(),
        tls,
        Some(tokens as Arc<dyn MailboxTokenCheck>),
    ));
    let mut l = MailListeners {
        addrs: Vec::new(),
        limits: ConnectionLimits::new(cfg.max_connections, cfg.max_connections_per_ip),
        tasks: JoinSet::new(),
    };
    let ls = &cfg.listeners;
    for (var, addr, implicit_tls) in [
        ("FASTMOCK_MAIL_SMTP_LISTEN", ls.smtp, false),
        ("FASTMOCK_MAIL_SMTPS_LISTEN", ls.smtps, true),
    ] {
        let server = smtp_server.clone();
        l.bind(
            var,
            addr,
            implicit_tls,
            smtp::BUSY_REPLY,
            &shutdown,
            move |s, peer, rx| {
                let server = server.clone();
                async move {
                    server.serve(s, peer, implicit_tls, Some(rx)).await;
                }
            },
        )
        .await?;
    }
    for (var, addr, implicit_tls) in [
        ("FASTMOCK_MAIL_POP3_LISTEN", ls.pop3, false),
        ("FASTMOCK_MAIL_POP3S_LISTEN", ls.pop3s, true),
    ] {
        let server = pop3_server.clone();
        l.bind(
            var,
            addr,
            implicit_tls,
            pop3::BUSY_REPLY,
            &shutdown,
            move |s, peer, rx| {
                let server = server.clone();
                async move {
                    server.serve(s, peer, implicit_tls, Some(rx)).await;
                }
            },
        )
        .await?;
    }
    for (var, addr, implicit_tls) in [
        ("FASTMOCK_MAIL_IMAP_LISTEN", ls.imap, false),
        ("FASTMOCK_MAIL_IMAPS_LISTEN", ls.imaps, true),
    ] {
        let server = imap_server.clone();
        l.bind(
            var,
            addr,
            implicit_tls,
            imap::BUSY_REPLY,
            &shutdown,
            move |s, peer, rx| {
                let server = server.clone();
                async move {
                    server.serve(s, peer, implicit_tls, Some(rx)).await;
                }
            },
        )
        .await?;
    }
    Ok(Some(l))
}

/// Accepts connections within the limits and runs `session` for each until shutdown, then gives the
/// open sessions [`DRAIN`] to finish and aborts the rest. Refused plain connections get `busy` before
/// closing; refused implicit-TLS connections are closed without a reply.
pub async fn serve_connections<F, Fut>(
    listener: TcpListener,
    implicit_tls: bool,
    limits: Arc<ConnectionLimits>,
    mut shutdown: watch::Receiver<bool>,
    busy: &'static [u8],
    session: F,
) where
    F: Fn(TcpStream, SocketAddr, watch::Receiver<bool>) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut sessions = JoinSet::new();
    loop {
        let accepted = tokio::select! {
            _ = shutdown.changed() => break,
            r = listener.accept() => r,
            Some(_) = sessions.join_next(), if !sessions.is_empty() => continue,
        };
        let (mut stream, peer) = match accepted {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(error = %e, "accepting a mail connection failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let slot = match limits.acquire(peer.ip()) {
            Ok(s) => s,
            Err(b) => {
                let reason = match b {
                    Busy::Global => "connections",
                    Busy::PerIp => "connections_per_ip",
                };
                metrics::counter!("fastmock_mail_rejected_total", "reason" => reason).increment(1);
                tracing::warn!(peer = %peer.ip(), reason, "mail connection refused: limit reached");
                if !implicit_tls {
                    sessions.spawn(async move {
                        let _ = tokio::time::timeout(Duration::from_secs(2), async {
                            stream.write_all(busy).await?;
                            stream.shutdown().await
                        })
                        .await;
                    });
                }
                continue;
            }
        };
        let fut = session(stream, peer, shutdown.clone());
        sessions.spawn(async move {
            let _slot = slot;
            fut.await;
        });
    }
    drop(listener);
    if tokio::time::timeout(DRAIN, async {
        while sessions.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tracing::warn!(
            open = sessions.len(),
            "mail sessions still open after the grace period; closing them"
        );
        sessions.abort_all();
    }
}
