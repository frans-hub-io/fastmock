pub mod postgres;
pub mod valkey;

use std::sync::Arc;

use fastmock_core::Stores;

pub use postgres::PgStore;
pub use valkey::ValkeyStore;

pub async fn connect(
    database_url: &str,
    valkey_url: &str,
    valkey_prefix: &str,
    max_connections: u32,
) -> anyhow::Result<(Stores, PgStore)> {
    use anyhow::Context;
    use std::time::Duration;
    tracing::info!("connecting to PostgreSQL");
    let pg = tokio::time::timeout(
        Duration::from_secs(30),
        PgStore::connect(database_url, max_connections),
    )
    .await
    .context("PostgreSQL did not answer within 30s")?
    .context("connecting to PostgreSQL")?;
    tracing::info!("applying database migrations");
    tokio::time::timeout(Duration::from_secs(120), pg.migrate())
        .await
        .context("migrations did not finish within 120s")??;
    tracing::info!("connecting to Valkey");
    let hot = tokio::time::timeout(
        Duration::from_secs(30),
        ValkeyStore::connect(valkey_url, valkey_prefix),
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
