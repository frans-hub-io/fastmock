use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::body::Body;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use rustls::RootCertStore;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower::ServiceExt;

use crate::config::TlsSettings;

pub const CLIENT_CERT_HEADER: &str = "x-fastmock-client-cert-sha256";

#[derive(Clone, Debug)]
pub struct ClientCert(pub String);

pub fn server_config(s: &TlsSettings) -> anyhow::Result<Arc<rustls::ServerConfig>> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&s.cert_file)
        .with_context(|| format!("reading {}", s.cert_file.display()))?
        .collect::<Result<_, _>>()?;
    if certs.is_empty() {
        anyhow::bail!("{} contains no certificates", s.cert_file.display());
    }
    let key = PrivateKeyDer::from_pem_file(&s.key_file)
        .with_context(|| format!("reading {}", s.key_file.display()))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?;
    let mut cfg = match &s.client_ca_file {
        Some(ca) => {
            let mut roots = RootCertStore::empty();
            for c in CertificateDer::pem_file_iter(ca)
                .with_context(|| format!("reading {}", ca.display()))?
            {
                roots.add(c?)?;
            }
            let v = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider);
            let v = if s.client_auth_required {
                v.build()?
            } else {
                v.allow_unauthenticated().build()?
            };
            builder
                .with_client_cert_verifier(v)
                .with_single_cert(certs, key)?
        }
        None => builder.with_no_client_auth().with_single_cert(certs, key)?,
    };
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

async fn connection<I>(
    io: I,
    router: Router,
    fingerprint: Option<String>,
    builder: Builder<TokioExecutor>,
    watcher: hyper_util::server::graceful::Watcher,
    peer: std::net::SocketAddr,
) where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let slot = crate::dataplane::FaultSlot::default();
    let armed = slot.clone();
    let svc = tower::service_fn(move |mut req: hyper::Request<Incoming>| {
        if let Some(fp) = &fingerprint {
            req.extensions_mut().insert(ClientCert(fp.clone()));
        }
        let h1 = req.version() < hyper::Version::HTTP_2;
        let armed = armed.clone();
        let fut = router.clone().oneshot(req.map(Body::new));
        async move {
            let resp = match fut.await {
                Ok(r) => r,
                Err(e) => match e {},
            };
            if h1 {
                armed.arm_from(&resp);
            } else if resp
                .extensions()
                .get::<crate::dataplane::AbortStream>()
                .is_some()
            {
                return Err(std::io::Error::other("fault: stream reset"));
            }
            Ok::<_, std::io::Error>(resp)
        }
    });
    let io = crate::dataplane::FaultIo::new(io, slot);
    let conn = builder
        .serve_connection_with_upgrades(io, TowerToHyperService::new(svc))
        .into_owned();
    if let Err(e) = watcher.watch(conn).await {
        tracing::debug!(%peer, error = %e, "connection closed with error");
    }
}

async fn stopped(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|v| *v).await.map(|_| ());
}

/// Default of `FASTMOCK_HTTP2_MAX_CONCURRENT_STREAMS`, the same as hyper's.
pub const DEFAULT_HTTP2_MAX_CONCURRENT_STREAMS: u32 = 200;

pub async fn serve(
    listener: TcpListener,
    router: Router,
    tls: Option<Arc<rustls::ServerConfig>>,
    shutdown: watch::Receiver<bool>,
    header_read_timeout: Duration,
    drain: Duration,
) {
    serve_with_streams(
        listener,
        router,
        tls,
        shutdown,
        header_read_timeout,
        drain,
        DEFAULT_HTTP2_MAX_CONCURRENT_STREAMS,
    )
    .await
}

/// [`serve`] with an explicit limit of concurrent HTTP/2 streams per connection.
pub async fn serve_with_streams(
    listener: TcpListener,
    router: Router,
    tls: Option<Arc<rustls::ServerConfig>>,
    mut shutdown: watch::Receiver<bool>,
    header_read_timeout: Duration,
    drain: Duration,
    max_concurrent_streams: u32,
) {
    let acceptor = tls.map(tokio_rustls::TlsAcceptor::from);
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    let mut builder = Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout);
    builder
        .http2()
        .timer(TokioTimer::new())
        .max_concurrent_streams(max_concurrent_streams);
    loop {
        let (stream, peer) = tokio::select! {
            _ = stopped(&mut shutdown) => break,
            r = listener.accept() => match r {
                Ok(x) => x,
                Err(e) => {
                    metrics::counter!("fastmock_accept_errors_total").increment(1);
                    tracing::warn!(error = %e, "accept failed; pausing");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            },
        };
        let router = router.clone();
        let builder = builder.clone();
        let watcher = graceful.watcher();
        match acceptor.clone() {
            None => {
                tokio::spawn(connection(
                    TokioIo::new(stream),
                    router,
                    None,
                    builder,
                    watcher,
                    peer,
                ));
            }
            Some(acceptor) => {
                tokio::spawn(async move {
                    let tls = match tokio::time::timeout(
                        Duration::from_secs(10),
                        acceptor.accept(stream),
                    )
                    .await
                    {
                        Ok(Ok(t)) => t,
                        Ok(Err(e)) => {
                            metrics::counter!("fastmock_tls_handshake_failures_total").increment(1);
                            tracing::debug!(%peer, error = %e, "TLS handshake failed");
                            return;
                        }
                        Err(_) => {
                            metrics::counter!("fastmock_tls_handshake_failures_total").increment(1);
                            return;
                        }
                    };
                    let fingerprint = tls
                        .get_ref()
                        .1
                        .peer_certificates()
                        .and_then(|c| c.first())
                        .map(|c| hex::encode(Sha256::digest(c.as_ref())));
                    connection(
                        TokioIo::new(tls),
                        router,
                        fingerprint,
                        builder,
                        watcher,
                        peer,
                    )
                    .await;
                });
            }
        }
    }
    drop(listener);
    if tokio::time::timeout(drain, graceful.shutdown())
        .await
        .is_err()
    {
        tracing::warn!("open connections did not finish within the drain period");
    }
}
