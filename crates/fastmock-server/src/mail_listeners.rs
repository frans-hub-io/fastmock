//! Binds the mail listeners (SMTP, SMTPS, POP3, POP3S) from `MailConfig`, applies the connection limits shared by all
//! mail listeners (`FASTMOCK_MAIL_MAX_CONNECTIONS`, `FASTMOCK_MAIL_MAX_CONNECTIONS_PER_IP`) and stops
//! with the server: no new connections after the shutdown signal, sessions waiting for a command get
//! `421`, the rest get a grace period.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use fastmock_mail::pop3::{BUSY_REPLY as POP3_BUSY_REPLY, MailboxTokenCheck, Pop3Server};
use fastmock_mail::smtp::{BUSY_REPLY, SmtpServer, TokenCheck};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::access::Permission;
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
/// like the AUTH user.
pub struct ManagedTokens {
    app: AppState,
}

#[async_trait]
impl TokenCheck for ManagedTokens {
    async fn allows(&self, user: &str, token: &str) -> bool {
        if !looks_managed(token) {
            return false;
        }
        let t = match self.app.tokens.lookup(token).await {
            Ok(Lookup::Valid(t)) => t,
            Ok(_) => return false,
            Err(e) => {
                tracing::warn!(error = %e, "checking a managed token for SMTP AUTH failed");
                return false;
            }
        };
        let Some(p) = principal_of(&self.app.auth, &t) else {
            return false;
        };
        let space = self.app.auth.access().space(user).map(|s| s.name.as_str());
        let ok = p.allowed(Permission::MailWrite, None)
            || space.is_some_and(|s| p.allowed(Permission::MailWrite, Some(s)));
        if ok {
            self.app.tokens.touch(t.id);
        }
        ok
    }
}

/// The SMTP server of an application (`None` without the mail module).
pub fn smtp_server(app: &AppState) -> anyhow::Result<Option<Arc<SmtpServer>>> {
    let Some(mail) = &app.mail else {
        return Ok(None);
    };
    let tls = mail
        .config
        .tls
        .as_ref()
        .map(fastmock_mail::tls::server_config)
        .transpose()
        .context("mail TLS certificate")?;
    let tokens: Arc<dyn TokenCheck> = Arc::new(ManagedTokens { app: app.clone() });
    Ok(Some(Arc::new(SmtpServer::new(
        mail.clone(),
        tls,
        Some(tokens),
        app.mail_chaos.clone(),
    ))))
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
}

/// Binds the SMTP, SMTPS, POP3 and POP3S listeners; `None` without the mail module.
pub async fn start(
    app: &AppState,
    shutdown: watch::Receiver<bool>,
) -> anyhow::Result<Option<MailListeners>> {
    let Some(server) = smtp_server(app)? else {
        return Ok(None);
    };
    let cfg = server.service.config.clone();
    let limits = ConnectionLimits::new(cfg.max_connections, cfg.max_connections_per_ip);
    let mut addrs = Vec::new();
    let mut tasks = JoinSet::new();
    for (var, addr, implicit_tls) in [
        ("FASTMOCK_MAIL_SMTP_LISTEN", cfg.listeners.smtp, false),
        ("FASTMOCK_MAIL_SMTPS_LISTEN", cfg.listeners.smtps, true),
    ] {
        let Some(addr) = addr else { continue };
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding {var} {addr}"))?;
        let local = listener.local_addr()?;
        tracing::info!(listener = var, addr = %local, tls = implicit_tls, "mail listener started");
        addrs.push((var, local));
        tasks.spawn(accept_loop(
            listener,
            server.clone(),
            implicit_tls,
            limits.clone(),
            shutdown.clone(),
        ));
    }
    start_pop3(app, &limits, &shutdown, &mut addrs, &mut tasks).await?;
    Ok(Some(MailListeners {
        addrs,
        limits,
        tasks,
    }))
}

async fn refuse(mut stream: tokio::net::TcpStream, implicit_tls: bool) {
    if !implicit_tls {
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            stream.write_all(BUSY_REPLY).await?;
            stream.shutdown().await
        })
        .await;
    }
}

async fn accept_loop(
    listener: TcpListener,
    server: Arc<SmtpServer>,
    implicit_tls: bool,
    limits: Arc<ConnectionLimits>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut sessions = JoinSet::new();
    loop {
        let accepted = tokio::select! {
            _ = shutdown.changed() => break,
            r = listener.accept() => r,
            Some(_) = sessions.join_next(), if !sessions.is_empty() => continue,
        };
        let (stream, peer) = match accepted {
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
                sessions.spawn(refuse(stream, implicit_tls));
                continue;
            }
        };
        let server = server.clone();
        let rx = shutdown.clone();
        sessions.spawn(async move {
            let _slot = slot;
            server.serve(stream, peer, implicit_tls, Some(rx)).await;
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

/// POP3 and IMAP logins with a managed token: valid, and allowed `mail:read` in the mailbox's space or
/// globally.
#[async_trait]
impl MailboxTokenCheck for ManagedTokens {
    async fn can_read(&self, area: Option<&str>, token: &str) -> bool {
        if !looks_managed(token) {
            return false;
        }
        let t = match self.app.tokens.lookup(token).await {
            Ok(Lookup::Valid(t)) => t,
            Ok(_) => return false,
            Err(e) => {
                tracing::warn!(error = %e, "checking a managed token for a mailbox login failed");
                return false;
            }
        };
        let Some(p) = principal_of(&self.app.auth, &t) else {
            return false;
        };
        let ok = p.allowed(Permission::MailRead, area);
        if ok {
            self.app.tokens.touch(t.id);
        }
        ok
    }
}

/// The POP3 server of an application (`None` without the mail module).
pub fn pop3_server(app: &AppState) -> anyhow::Result<Option<Arc<Pop3Server>>> {
    let Some(mail) = &app.mail else {
        return Ok(None);
    };
    let tls = mail
        .config
        .tls
        .as_ref()
        .map(fastmock_mail::tls::server_config)
        .transpose()
        .context("mail TLS certificate")?;
    let tokens: Arc<dyn MailboxTokenCheck> = Arc::new(ManagedTokens { app: app.clone() });
    Ok(Some(Arc::new(Pop3Server::new(
        mail.clone(),
        tls,
        Some(tokens),
    ))))
}

/// Binds `FASTMOCK_MAIL_POP3_LISTEN` and `FASTMOCK_MAIL_POP3S_LISTEN` when set; their connections count
/// towards the limits shared by all mail listeners.
async fn start_pop3(
    app: &AppState,
    limits: &Arc<ConnectionLimits>,
    shutdown: &watch::Receiver<bool>,
    addrs: &mut Vec<(&'static str, SocketAddr)>,
    tasks: &mut JoinSet<()>,
) -> anyhow::Result<()> {
    let Some(mail) = &app.mail else {
        return Ok(());
    };
    let l = &mail.config.listeners;
    let wanted: Vec<_> = [
        ("FASTMOCK_MAIL_POP3_LISTEN", l.pop3, false),
        ("FASTMOCK_MAIL_POP3S_LISTEN", l.pop3s, true),
    ]
    .into_iter()
    .filter_map(|(v, a, t)| a.map(|a| (v, a, t)))
    .collect();
    if wanted.is_empty() {
        return Ok(());
    }
    let Some(server) = pop3_server(app)? else {
        return Ok(());
    };
    for (var, addr, implicit_tls) in wanted {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding {var} {addr}"))?;
        let local = listener.local_addr()?;
        tracing::info!(listener = var, addr = %local, tls = implicit_tls, "mail listener started");
        addrs.push((var, local));
        tasks.spawn(pop3_accept_loop(
            listener,
            server.clone(),
            implicit_tls,
            limits.clone(),
            shutdown.clone(),
        ));
    }
    Ok(())
}

async fn pop3_refuse(mut stream: tokio::net::TcpStream, implicit_tls: bool) {
    if !implicit_tls {
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            stream.write_all(POP3_BUSY_REPLY).await?;
            stream.shutdown().await
        })
        .await;
    }
}

async fn pop3_accept_loop(
    listener: TcpListener,
    server: Arc<Pop3Server>,
    implicit_tls: bool,
    limits: Arc<ConnectionLimits>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut sessions = JoinSet::new();
    loop {
        let accepted = tokio::select! {
            _ = shutdown.changed() => break,
            r = listener.accept() => r,
            Some(_) = sessions.join_next(), if !sessions.is_empty() => continue,
        };
        let (stream, peer) = match accepted {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(error = %e, "accepting a POP3 connection failed");
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
                sessions.spawn(pop3_refuse(stream, implicit_tls));
                continue;
            }
        };
        let server = server.clone();
        let rx = shutdown.clone();
        sessions.spawn(async move {
            let _slot = slot;
            server.serve(stream, peer, implicit_tls, Some(rx)).await;
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
            "POP3 sessions still open after the grace period; closing them"
        );
        sessions.abort_all();
    }
}
