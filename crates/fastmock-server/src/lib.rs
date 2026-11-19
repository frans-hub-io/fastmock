pub mod admin;
pub mod admin_core;
pub mod asyncmock;
pub mod auth;
pub mod cli;
pub mod config;
pub mod contract;
pub mod dataplane;
pub mod extras;
pub mod files;
pub mod grpc;
pub mod imports;
pub mod journal_api;
pub mod openapi_doc;
pub mod outbox;
pub mod proxy;
pub mod redact;
pub mod services;
pub mod snapshot;
pub mod state;
pub mod telemetry;
pub mod tls;
pub mod ui;
pub mod websocket;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::extract::DefaultBodyLimit;
use axum::{Extension, Router, middleware};
use chrono::Utc;
use fastmock_core::memory::memory_stores;
use fastmock_core::template::Templates;
use fastmock_core::{Engine, Plugin, PluginContext, PluginSettings, Stores};
use tokio::sync::watch;

use crate::config::{Config, StorageKind};
use crate::state::{AppState, PluginInfo, Secrets};

pub type PluginFactory = fn() -> Vec<Box<dyn Plugin>>;

static PLUGINS: std::sync::OnceLock<PluginFactory> = std::sync::OnceLock::new();

/// Registers the plugins a custom binary compiles in. Call it once, before [`serve`] or [`build`];
/// returns `false` when plugins were already registered.
pub fn register_plugins(factory: PluginFactory) -> bool {
    PLUGINS.set(factory).is_ok()
}

pub fn all_plugins() -> Vec<Box<dyn Plugin>> {
    PLUGINS.get().map(|f| f()).unwrap_or_default()
}

async fn shutdown_signal(tx: watch::Sender<bool>, ready: Arc<AtomicBool>, delay: Duration) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    ready.store(false, Ordering::SeqCst);
    tracing::info!(delay = ?delay, "shutdown requested; readiness withdrawn, draining");
    tokio::time::sleep(delay).await;
    let _ = tx.send(true);
}

async fn journal_writer(
    journal: Arc<dyn fastmock_core::JournalStore>,
    mut rx: tokio::sync::mpsc::Receiver<fastmock_core::JournalEntry>,
) {
    let mut batch = Vec::with_capacity(500);
    while rx.recv_many(&mut batch, 500).await > 0 {
        metrics::histogram!("fastmock_journal_batch_size").record(batch.len() as f64);
        if let Err(e) = journal.append_many(&batch).await {
            metrics::counter!("fastmock_journal_write_errors_total").increment(batch.len() as u64);
            tracing::error!(error = %e, entries = batch.len(), "journal write failed");
        }
        batch.clear();
    }
}

fn panic_response(_: Box<dyn std::any::Any + Send + 'static>) -> axum::response::Response {
    metrics::counter!("fastmock_panics_total").increment(1);
    tracing::error!("request handler panicked");
    axum::response::IntoResponse::into_response((
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(serde_json::json!({"error": "internal error"})),
    ))
}

pub async fn build(
    cfg: Config,
    prometheus: metrics_exporter_prometheus::PrometheusHandle,
) -> anyhow::Result<(AppState, Router, Router, Arc<outbox::EgressPolicy>)> {
    let cfg = Arc::new(cfg);
    let (stores, pg): (Stores, Option<fastmock_store::PgStore>) = match &cfg.storage {
        StorageKind::Memory => (memory_stores(100_000), None),
        StorageKind::Postgres {
            database_url,
            valkey_url,
            valkey_prefix,
            max_connections,
        } => {
            let (s, pg) =
                fastmock_store::connect(database_url, valkey_url, valkey_prefix, *max_connections)
                    .await
                    .context("connecting to storage")?;
            (s, Some(pg))
        }
    };
    let secrets: Arc<dyn fastmock_core::SecretProvider> =
        Arc::new(Secrets::new(cfg.secrets_dir.clone()));
    let templates = Arc::new(Templates::new());
    let stub_secrets: Arc<dyn fastmock_core::SecretProvider> = Arc::new(state::StubSecrets {
        inner: secrets.clone(),
        allowed: cfg.stub_secrets.clone(),
    });
    let engine = Arc::new(Engine::new(stores.clone(), templates.clone(), stub_secrets));
    engine.reload().await?;
    let auth = Arc::new(auth::Authenticator::new(
        &cfg.admin_tokens,
        cfg.admin_tokens_file.as_deref(),
        cfg.auth_disabled,
        cfg.admin_allowed_origins.clone(),
    )?);
    let redactor = Arc::new(redact::Redactor::new(
        &cfg.redact_headers,
        &cfg.redact_json_keys,
    ));
    let policy = Arc::new(outbox::EgressPolicy {
        allowed_hosts: cfg.callback_allowed_hosts.clone(),
        denied: cfg.callback_denied_cidrs.clone(),
    });
    let outbound_roots = outbox::load_outbound_ca(cfg.outbound_ca_file.as_deref())?;
    let http = outbox::http_client(policy.clone(), cfg.callback_timeout, &outbound_roots)?;

    let mut infos = Vec::new();
    let mut data_routes = Vec::new();
    let mut admin_routes = Vec::new();
    for p in all_plugins() {
        let enabled = cfg
            .plugins
            .as_ref()
            .is_none_or(|l| l.iter().any(|n| n == p.name() || n == "all"));
        if !enabled {
            continue;
        }
        let mut settings = PluginSettings::from_env(p.name());
        if let Some(o) = cfg.plugin_overrides.get(p.name()) {
            settings = settings.merged(o);
        }
        let ctx = PluginContext {
            name: p.name().to_string(),
            stores: stores.clone(),
            settings,
            templates: templates.clone(),
            secrets: secrets.clone(),
        };
        let router = p
            .router(ctx.clone())
            .with_context(|| format!("plugin {}", p.name()))?;
        let admin = p.admin_router(ctx);
        let mount = format!("/{}", p.name());
        infos.push(PluginInfo {
            name: p.name().into(),
            description: p.description().into(),
            mount: mount.clone(),
            admin: admin.is_some(),
        });
        data_routes.push((mount, router));
        if let Some(a) = admin {
            admin_routes.push((p.name().to_string(), a));
        }
    }
    let (journal_tx, journal_rx) = tokio::sync::mpsc::channel(cfg.journal_queue);
    tokio::spawn(journal_writer(stores.journal.clone(), journal_rx));
    let app = AppState {
        cfg: cfg.clone(),
        stores,
        engine,
        auth,
        redactor,
        plugins: Arc::new(infos),
        pg,
        started: Instant::now(),
        ready: Arc::new(AtomicBool::new(false)),
        http,
        policy: policy.clone(),
        chaos: Arc::new(extras::ChaosCache::default()),
        kafka: Arc::new(extras::KafkaHub::new(
            cfg.kafka_bootstrap.clone(),
            cfg.kafka_schema_registry.clone(),
            cfg.kafka_allowed_topics.clone(),
        )?),
        grpc: Arc::new(grpc::Registry::default()),
        body_budget: Arc::new(tokio::sync::Semaphore::new(
            (cfg.max_inflight_body_bytes / 1024).max(1),
        )),
        journal_tx,
        scans: Arc::new(tokio::sync::Semaphore::new(cfg.journal_scan_concurrency)),
    };
    if let Err(e) = app.grpc.reload(&app).await {
        tracing::warn!(error = %e, "loading gRPC descriptors failed");
    }
    let mut data: Router<AppState> = Router::new();
    for (mount, r) in data_routes {
        data = data.nest_service(&mount, r);
    }
    let data = data
        .fallback(dataplane::stub_handler)
        .with_state(app.clone())
        .layer(middleware::from_fn_with_state(
            app.clone(),
            dataplane::data_plane_middleware,
        ))
        .layer(DefaultBodyLimit::disable())
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(
            panic_response,
        ));
    let admin = admin::router(app.clone(), admin_routes)
        .layer(Extension(prometheus))
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(
            panic_response,
        ));
    Ok((app, data, admin, policy))
}

pub async fn maintenance(app: AppState, mut shutdown: watch::Receiver<bool>) {
    let mut last_purge = Instant::now() - Duration::from_secs(3600);
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = tokio::time::sleep(app.cfg.stub_poll) => {}
        }
        match app.engine.refresh_if_changed().await {
            Ok(true) => tracing::info!(stubs = app.engine.snapshot().len(), "stubs reloaded"),
            Ok(false) => {}
            Err(e) => {
                metrics::counter!("fastmock_store_errors_total", "store" => "stubs").increment(1);
                tracing::error!(error = %e, "stub refresh failed");
            }
        }
        metrics::gauge!("fastmock_stubs_loaded").set(app.engine.snapshot().len() as f64);
        if let Err(e) = app.grpc.refresh_if_changed(&app).await {
            tracing::warn!(error = %e, "gRPC descriptor refresh failed");
        }
        if let Ok(n) = app.stores.outbox.pending_count().await {
            metrics::gauge!("fastmock_callbacks_pending").set(n as f64);
        }
        if last_purge.elapsed() >= Duration::from_secs(60) {
            last_purge = Instant::now();
            let cutoff = Utc::now()
                - chrono::Duration::from_std(app.cfg.journal_retention).unwrap_or_default();
            match app.stores.journal.purge_older_than(cutoff).await {
                Ok(n) if n > 0 => tracing::info!(deleted = n, "journal retention applied"),
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "journal retention failed"),
            }
            let cutoff = Utc::now()
                - chrono::Duration::from_std(app.cfg.callback_retention).unwrap_or_default();
            if let Err(e) = app.stores.outbox.purge_finished_older_than(cutoff).await {
                tracing::error!(error = %e, "callback retention failed");
            }
            let cutoff = Utc::now() - chrono::Duration::days(90);
            if let Err(e) = app.stores.audit.purge_older_than(cutoff).await {
                tracing::error!(error = %e, "audit retention failed");
            }
        }
    }
}

pub async fn listen_notifications(app: AppState, mut shutdown: watch::Receiver<bool>) {
    let Some(pg) = app.pg.clone() else {
        return;
    };
    loop {
        let mut listener = match pg.listener().await {
            Ok(l) => {
                if let Err(e) = app.engine.refresh_if_changed().await {
                    tracing::warn!(error = %e, "stub refresh after LISTEN failed");
                }
                l
            }
            Err(e) => {
                tracing::warn!(error = %e, "LISTEN failed; relying on polling");
                tokio::select! { _ = shutdown.changed() => return, _ = tokio::time::sleep(Duration::from_secs(10)) => continue }
            }
        };
        loop {
            tokio::select! {
                _ = shutdown.changed() => return,
                n = listener.recv() => match n {
                    Ok(_) => {
                        if let Err(e) = app.engine.refresh_if_changed().await {
                            tracing::error!(error = %e, "stub refresh after notification failed");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "notification stream broken; reconnecting");
                        break;
                    }
                }
            }
        }
    }
}

pub async fn serve() -> anyhow::Result<()> {
    let cfg = Config::from_env()?;
    let telemetry = telemetry::init(cfg.log_format)?;
    if cfg.auth_disabled {
        tracing::warn!(
            "admin authentication is DISABLED (FASTMOCK_AUTH_DISABLED=true): use only for local development"
        );
    }
    let listen = cfg.listen;
    let admin_listen = cfg.admin_listen;
    let (app, data, admin, policy) = build(cfg, telemetry.prometheus.clone()).await?;
    let (tx, rx) = watch::channel(false);
    let worker = tokio::spawn(outbox::run(app.clone(), policy, rx.clone()));
    let maint = tokio::spawn(maintenance(app.clone(), rx.clone()));
    let notify = tokio::spawn(listen_notifications(app.clone(), rx.clone()));
    let async_publisher = tokio::spawn(asyncmock::run(app.clone(), rx.clone()));
    tokio::spawn(shutdown_signal(
        tx,
        app.ready.clone(),
        app.cfg.shutdown_delay,
    ));
    let data_listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    let admin_listener = tokio::net::TcpListener::bind(admin_listen)
        .await
        .with_context(|| format!("binding {admin_listen}"))?;
    tracing::info!(%listen, %admin_listen, plugins = ?app.plugins.iter().map(|p| &p.name).collect::<Vec<_>>(), otel = telemetry.otel_enabled, "FastMock started");
    app.ready.store(true, Ordering::SeqCst);
    let tls_cfg = app.cfg.tls.as_ref().map(tls::server_config).transpose()?;
    if tls_cfg.is_some() {
        tracing::info!("data port serves HTTPS (HTTP/2 and HTTP/1.1 via ALPN)");
    }
    let hrt = app.cfg.header_read_timeout;
    let drain = Duration::from_secs(20);
    tokio::join!(
        tls::serve(data_listener, data, tls_cfg, rx.clone(), hrt, drain),
        tls::serve(admin_listener, admin, None, rx.clone(), hrt, drain),
    );
    let _ = tokio::time::timeout(Duration::from_secs(15), async {
        let _ = worker.await;
        let _ = maint.await;
        let _ = notify.await;
    })
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(5), async_publisher).await;
    telemetry.shutdown();
    tracing::info!("FastMock stopped");
    Ok(())
}
