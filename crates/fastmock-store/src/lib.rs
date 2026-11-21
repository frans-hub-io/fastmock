pub mod mail;
pub mod mail_events;
pub mod mail_login;
mod pem;
pub mod postgres;
pub mod valkey;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use fastmock_core::Stores;

pub use mail::{MailUsage, PgMailStore};
pub use mail_events::ValkeyMailEvents;
pub use mail_login::ValkeyLoginFailures;
pub use postgres::{AppliedMigration, PgStore, PgTls, SslMode, pg_connect_options};
pub use valkey::{ValkeyStore, ValkeyTls, valkey_client};

pub const DEFAULT_MIGRATION_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MigrateMode {
    #[default]
    Auto,
    Skip,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageOptions {
    pub database_tls: PgTls,
    pub migrate: MigrateMode,
    pub migration_timeout: Duration,
    pub valkey_username: Option<String>,
    pub valkey_tls: ValkeyTls,
}

impl Default for StorageOptions {
    fn default() -> Self {
        Self {
            database_tls: PgTls::default(),
            migrate: MigrateMode::Auto,
            migration_timeout: DEFAULT_MIGRATION_TIMEOUT,
            valkey_username: None,
            valkey_tls: ValkeyTls::default(),
        }
    }
}

pub async fn connect_postgres(
    database_url: &str,
    tls: &PgTls,
    max_connections: u32,
) -> anyhow::Result<PgStore> {
    let plan = pg_connect_options(database_url, tls).context("PostgreSQL settings")?;
    for w in &plan.warnings {
        tracing::warn!("PostgreSQL TLS: {w}");
    }
    tracing::info!(
        host = plan.options.get_host(),
        port = plan.options.get_port(),
        database = plan.options.get_database().unwrap_or_default(),
        sslmode = %plan.mode,
        "connecting to PostgreSQL"
    );
    tokio::time::timeout(
        Duration::from_secs(30),
        PgStore::connect_with(plan.options, max_connections),
    )
    .await
    .context("PostgreSQL did not answer within 30s")?
    .context("connecting to PostgreSQL")
}

pub async fn migrate(pg: &PgStore, timeout: Duration) -> anyhow::Result<Vec<AppliedMigration>> {
    tracing::info!(
        target_version = PgStore::expected_schema_version(),
        "applying database migrations"
    );
    let applied = tokio::time::timeout(timeout, pg.migrate())
        .await
        .with_context(|| format!("migrations did not finish within {timeout:?}"))?
        .context("applying database migrations")?;
    if applied.is_empty() {
        tracing::info!(
            version = PgStore::expected_schema_version(),
            "database schema is up to date, no migration applied"
        );
    }
    Ok(applied)
}

pub async fn prepare_schema(pg: &PgStore, options: &StorageOptions) -> anyhow::Result<()> {
    match options.migrate {
        MigrateMode::Auto => migrate(pg, options.migration_timeout).await.map(|_| ()),
        MigrateMode::Skip => {
            pg.check_schema()
                .await
                .context("checking the database schema (FASTMOCK_DATABASE_MIGRATE=skip)")?;
            tracing::info!(
                version = PgStore::expected_schema_version(),
                "database schema is up to date, migrations skipped"
            );
            Ok(())
        }
    }
}

pub async fn connect(
    database_url: &str,
    valkey_url: &str,
    valkey_prefix: &str,
    max_connections: u32,
    options: &StorageOptions,
) -> anyhow::Result<(Stores, PgStore)> {
    let pg = connect_postgres(database_url, &options.database_tls, max_connections).await?;
    prepare_schema(&pg, options).await?;
    let client = valkey_client(
        valkey_url,
        options.valkey_username.as_deref(),
        &options.valkey_tls,
    )
    .context("Valkey settings")?;
    let info = client.get_connection_info();
    tracing::info!(
        addr = %info.addr,
        tls = matches!(info.addr, redis::ConnectionAddr::TcpTls { .. }),
        acl_user = info.redis.username.as_deref().unwrap_or("default"),
        "connecting to Valkey"
    );
    let hot = tokio::time::timeout(
        Duration::from_secs(30),
        ValkeyStore::connect_with(client, valkey_prefix),
    )
    .await
    .context("Valkey did not answer within 30s")?
    .context("connecting to Valkey")?;
    tracing::info!("storage ready");
    let pg_arc = Arc::new(pg.clone());
    let stores = Stores {
        stubs: pg_arc.clone(),
        journal: pg_arc.clone(),
        outbox: pg_arc.clone(),
        hot: Arc::new(hot),
        data: pg_arc.clone(),
        audit: pg_arc,
    };
    Ok((stores, pg))
}
