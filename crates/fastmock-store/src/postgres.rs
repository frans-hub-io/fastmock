use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use fastmock_core::{
    ApiToken, AuditEvent, AuditFilter, AuditStore, CallbackFilter, CallbackJob, CallbackStatus,
    DataStore, JournalEntry, JournalFilter, JournalStore, OutboxStore, SpaceFilter, StoreResult,
    Stub, StubStore, TokenStore,
};
use serde_json::Value;
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgListener, PgPoolOptions, PgRow, PgSslMode};
use sqlx::{ConnectOptions, PgPool, Postgres, QueryBuilder, Row};
use uuid::Uuid;

pub const STUBS_CHANNEL: &str = "fastmock_stubs";

fn no_nul(s: &str) -> String {
    if s.contains('\0') {
        s.replace('\0', "\u{FFFD}")
    } else {
        s.to_string()
    }
}

static MIGRATOR: LazyLock<Migrator> = LazyLock::new(|| {
    let mut m = sqlx::migrate!("./migrations");
    m.set_ignore_missing(true);
    m.set_locking(false);
    m
});

/// The advisory lock key sqlx uses for a database, so that older binaries, which let sqlx take the
/// lock, and this one never migrate at the same time.
fn migration_lock_id(database: &str) -> i64 {
    let mut crc: u32 = 0xffff_ffff;
    for b in database.bytes() {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    0x3d32_ad9e * i64::from(!crc)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SslMode {
    Disable,
    Allow,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl SslMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SslMode::Disable => "disable",
            SslMode::Allow => "allow",
            SslMode::Prefer => "prefer",
            SslMode::Require => "require",
            SslMode::VerifyCa => "verify-ca",
            SslMode::VerifyFull => "verify-full",
        }
    }

    pub fn verifies(self) -> bool {
        matches!(self, SslMode::VerifyCa | SslMode::VerifyFull)
    }
}

impl std::fmt::Display for SslMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SslMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(match s.trim() {
            "disable" => SslMode::Disable,
            "allow" => SslMode::Allow,
            "prefer" => SslMode::Prefer,
            "require" => SslMode::Require,
            "verify-ca" => SslMode::VerifyCa,
            "verify-full" => SslMode::VerifyFull,
            other => anyhow::bail!(
                "unknown SSL mode `{other}`: use disable, allow, prefer, require, verify-ca or verify-full"
            ),
        })
    }
}

impl From<SslMode> for PgSslMode {
    fn from(m: SslMode) -> Self {
        match m {
            SslMode::Disable => PgSslMode::Disable,
            SslMode::Allow => PgSslMode::Allow,
            SslMode::Prefer => PgSslMode::Prefer,
            SslMode::Require => PgSslMode::Require,
            SslMode::VerifyCa => PgSslMode::VerifyCa,
            SslMode::VerifyFull => PgSslMode::VerifyFull,
        }
    }
}

impl From<PgSslMode> for SslMode {
    fn from(m: PgSslMode) -> Self {
        match m {
            PgSslMode::Disable => SslMode::Disable,
            PgSslMode::Allow => SslMode::Allow,
            PgSslMode::Prefer => SslMode::Prefer,
            PgSslMode::Require => SslMode::Require,
            PgSslMode::VerifyCa => SslMode::VerifyCa,
            PgSslMode::VerifyFull => SslMode::VerifyFull,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PgTls {
    pub mode: Option<SslMode>,
    pub root_cert: Option<PathBuf>,
    pub client_cert: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
}

pub struct PgConnectPlan {
    pub options: PgConnectOptions,
    pub mode: SslMode,
    pub warnings: Vec<String>,
}

enum CertSource {
    File(PathBuf),
    Inline,
}

fn cert_source(v: &str) -> CertSource {
    if v.starts_with("-----BEGIN") {
        CertSource::Inline
    } else {
        CertSource::File(PathBuf::from(v.strip_prefix("file: ").unwrap_or(v)))
    }
}

pub fn pg_connect_options(url: &str, tls: &PgTls) -> anyhow::Result<PgConnectPlan> {
    let mut options = PgConnectOptions::from_str(url)
        .map_err(|e| anyhow::anyhow!("not a valid PostgreSQL URL: {e}"))?;
    if let Some(m) = tls.mode {
        options = options.ssl_mode(m.into());
    }
    if let Some(p) = &tls.root_cert {
        options = options.ssl_root_cert(p);
    }
    if let Some(p) = &tls.client_cert {
        options = options.ssl_client_cert(p);
    }
    if let Some(p) = &tls.client_key {
        options = options.ssl_client_key(p);
    }
    let effective = options.to_url_lossy();
    let param = |name: &str| {
        effective
            .query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| cert_source(&v))
    };
    let (root, cert, key) = (param("sslrootcert"), param("sslcert"), param("sslkey"));
    match (&cert, &key) {
        (Some(_), None) => anyhow::bail!(
            "a client certificate is set (FASTMOCK_DATABASE_SSL_CERT or sslcert) without a key: set FASTMOCK_DATABASE_SSL_KEY or sslkey too"
        ),
        (None, Some(_)) => anyhow::bail!(
            "a client key is set (FASTMOCK_DATABASE_SSL_KEY or sslkey) without a certificate: set FASTMOCK_DATABASE_SSL_CERT or sslcert too"
        ),
        _ => {}
    }
    if let Some(CertSource::File(p)) = &root {
        crate::pem::read_certs(
            "PostgreSQL CA file (FASTMOCK_DATABASE_SSL_ROOT_CERT or sslrootcert)",
            p,
        )?;
    }
    if let Some(CertSource::File(p)) = &cert {
        crate::pem::read_certs(
            "PostgreSQL client certificate (FASTMOCK_DATABASE_SSL_CERT or sslcert)",
            p,
        )?;
    }
    if let Some(CertSource::File(p)) = &key {
        crate::pem::read_key(
            "PostgreSQL client key (FASTMOCK_DATABASE_SSL_KEY or sslkey)",
            p,
        )?;
    }
    let mode = SslMode::from(options.get_ssl_mode());
    let mut warnings = Vec::new();
    if mode.verifies() && root.is_none() {
        warnings.push(format!(
            "sslmode={mode} without a CA file: only the public CAs bundled with FastMock (webpki-roots) are trusted; set FASTMOCK_DATABASE_SSL_ROOT_CERT for a private CA"
        ));
    }
    if !mode.verifies() && root.is_some() {
        warnings.push(format!(
            "sslmode={mode} does not verify the server certificate, so the CA file is not used; set FASTMOCK_DATABASE_SSL_MODE=verify-full"
        ));
    }
    if matches!(mode, SslMode::Disable | SslMode::Allow) && cert.is_some() {
        warnings.push(format!(
            "sslmode={mode} never uses TLS, so the client certificate is not sent"
        ));
    }
    Ok(PgConnectPlan {
        options,
        mode,
        warnings,
    })
}

/// A migration applied by [`PgStore::migrate`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedMigration {
    pub version: i64,
    pub name: String,
    pub duration: Duration,
}

async fn applied_versions(conn: &mut sqlx::PgConnection) -> anyhow::Result<Vec<i64>> {
    let present: bool = sqlx::query_scalar("select to_regclass('_sqlx_migrations') is not null")
        .fetch_one(&mut *conn)
        .await?;
    if !present {
        return Ok(Vec::new());
    }
    Ok(
        sqlx::query_scalar("select version from _sqlx_migrations where success")
            .fetch_all(&mut *conn)
            .await?,
    )
}

#[derive(Clone)]
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    pub async fn connect(url: &str, max_connections: u32) -> anyhow::Result<Self> {
        let options = PgConnectOptions::from_str(url)?;
        Self::connect_with(options, max_connections).await
    }

    pub async fn connect_with(
        options: PgConnectOptions,
        max_connections: u32,
    ) -> anyhow::Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections.max(2))
            .acquire_timeout(Duration::from_secs(5))
            .idle_timeout(Duration::from_secs(300))
            .connect_with(options)
            .await
            .map_err(|e| {
                let msg = e.to_string();
                match crate::pem::tls_hint(&msg, "FASTMOCK_DATABASE_SSL_ROOT_CERT (or sslrootcert)")
                {
                    Some(hint) => anyhow::anyhow!("{msg} ({hint})"),
                    None => anyhow::Error::from(e),
                }
            })?;
        Ok(Self { pool })
    }

    /// Applies pending migrations under the migration lock and returns the ones this call applied, each
    /// logged with its version, name and duration. The lock is polled rather than waited for: a session
    /// blocked on it would hold a snapshot that a concurrent index build waits for.
    pub async fn migrate(&self) -> anyhow::Result<Vec<AppliedMigration>> {
        let mut conn = self.pool.acquire().await?;
        let database: String = sqlx::query_scalar("select current_database()")
            .fetch_one(&mut *conn)
            .await?;
        let key = migration_lock_id(&database);
        while !sqlx::query_scalar::<_, bool>("select pg_try_advisory_lock($1)")
            .bind(key)
            .fetch_one(&mut *conn)
            .await?
        {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        let result = async {
            let before = applied_versions(&mut conn).await?;
            MIGRATOR.run_direct(&mut *conn).await?;
            let rows: Vec<(i64, String, i64)> = sqlx::query_as(
                "select version, description, execution_time from _sqlx_migrations where success order by version",
            )
            .fetch_all(&mut *conn)
            .await?;
            Ok::<_, anyhow::Error>(
                rows.into_iter()
                    .filter(|(v, _, _)| !before.contains(v))
                    .map(|(version, name, nanos)| AppliedMigration {
                        version,
                        name,
                        duration: Duration::from_nanos(nanos.max(0) as u64),
                    })
                    .collect::<Vec<_>>(),
            )
        }
        .await;
        let unlocked = sqlx::query("select pg_advisory_unlock($1)")
            .bind(key)
            .execute(&mut *conn)
            .await;
        let applied = result?;
        unlocked?;
        for m in &applied {
            tracing::info!(
                version = m.version,
                name = %m.name,
                duration_ms = m.duration.as_millis() as u64,
                "applied database migration"
            );
        }
        Ok(applied)
    }

    pub fn expected_schema_version() -> i64 {
        MIGRATOR
            .iter()
            .filter(|m| !m.migration_type.is_down_migration())
            .map(|m| m.version)
            .max()
            .unwrap_or(0)
    }

    pub async fn check_schema(&self) -> anyhow::Result<()> {
        let present: bool =
            sqlx::query_scalar("select to_regclass('_sqlx_migrations') is not null")
                .fetch_one(&self.pool)
                .await?;
        if !present {
            anyhow::bail!(
                "the database has no FastMock schema (no _sqlx_migrations table): run `fastmock migrate` first, or set FASTMOCK_DATABASE_MIGRATE=auto"
            );
        }
        let rows: Vec<(i64, Vec<u8>, bool)> =
            sqlx::query_as("select version, checksum, success from _sqlx_migrations")
                .fetch_all(&self.pool)
                .await?;
        let applied: HashMap<i64, (Vec<u8>, bool)> =
            rows.into_iter().map(|(v, c, s)| (v, (c, s))).collect();
        let mut missing = Vec::new();
        for m in MIGRATOR
            .iter()
            .filter(|m| !m.migration_type.is_down_migration())
        {
            match applied.get(&m.version) {
                None => missing.push(format!("{} ({})", m.version, m.description)),
                Some((_, false)) => anyhow::bail!(
                    "migration {} ({}) is marked as failed in _sqlx_migrations: repair the database, then run `fastmock migrate`",
                    m.version,
                    m.description
                ),
                Some((checksum, true)) if checksum.as_slice() != &*m.checksum => anyhow::bail!(
                    "migration {} ({}) in the database differs from the one built into this FastMock version: applied migrations must never be edited",
                    m.version,
                    m.description
                ),
                Some(_) => {}
            }
        }
        if !missing.is_empty() {
            anyhow::bail!(
                "the database schema is older than this FastMock version expects (version {}): migration(s) {} not applied; run `fastmock migrate` from this version before starting it with FASTMOCK_DATABASE_MIGRATE=skip",
                Self::expected_schema_version(),
                missing.join(", ")
            );
        }
        Ok(())
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn listener(&self) -> anyhow::Result<PgListener> {
        let mut l = PgListener::connect_with(&self.pool).await?;
        l.listen(STUBS_CHANNEL).await?;
        Ok(l)
    }

    async fn bump(tx: &mut sqlx::Transaction<'_, Postgres>) -> StoreResult<()> {
        sqlx::query("update fm_revision set revision = revision + 1 where id = 1")
            .execute(&mut **tx)
            .await?;
        sqlx::query("select pg_notify($1, '')")
            .bind(STUBS_CHANNEL)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    pub async fn ping(&self) -> StoreResult<()> {
        sqlx::query("select 1").execute(&self.pool).await?;
        Ok(())
    }
}

fn push_spaces<'a>(qb: &mut QueryBuilder<'a, Postgres>, f: &'a SpaceFilter) {
    match f {
        SpaceFilter::Any => {}
        SpaceFilter::Global => {
            qb.push(" and space is null");
        }
        SpaceFilter::In(list) => {
            qb.push(" and space = any(").push_bind(list).push(")");
        }
    }
}

/// The space comes from its column: an older binary that rewrites the definition keeps the column.
fn row_to_stub(r: &PgRow) -> StoreResult<Stub> {
    let def: Value = r.try_get("definition")?;
    let mut s: Stub = serde_json::from_value(def)?;
    s.updated_at = Some(r.try_get("updated_at")?);
    s.space = r.try_get("space")?;
    Ok(s)
}

#[async_trait]
impl StubStore for PgStore {
    async fn list(&self) -> StoreResult<Vec<Stub>> {
        let rows = sqlx::query(
            "select definition, updated_at, space from fm_stubs order by updated_at desc, id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_stub).collect()
    }

    async fn get(&self, id: Uuid) -> StoreResult<Option<Stub>> {
        let row = sqlx::query("select definition, updated_at, space from fm_stubs where id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(row_to_stub).transpose()
    }

    async fn upsert(&self, stub: &Stub) -> StoreResult<()> {
        let mut s = stub.clone();
        s.updated_at = None;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "insert into fm_stubs (id, definition, priority, tags, space, updated_at) values ($1, $2, $3, $4, $5, clock_timestamp())
             on conflict (id) do update set definition = excluded.definition, priority = excluded.priority, tags = excluded.tags, space = excluded.space, updated_at = clock_timestamp()",
        )
        .bind(s.id)
        .bind(serde_json::to_value(&s)?)
        .bind(s.priority)
        .bind(&s.tags)
        .bind(&s.space)
        .execute(&mut *tx)
        .await?;
        Self::bump(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn insert(&self, stub: &Stub) -> StoreResult<bool> {
        let mut s = stub.clone();
        s.updated_at = None;
        let mut tx = self.pool.begin().await?;
        let n = sqlx::query(
            "insert into fm_stubs (id, definition, priority, tags, space, updated_at) values ($1, $2, $3, $4, $5, clock_timestamp()) on conflict (id) do nothing",
        )
        .bind(s.id)
        .bind(serde_json::to_value(&s)?)
        .bind(s.priority)
        .bind(&s.tags)
        .bind(&s.space)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n == 1 {
            Self::bump(&mut tx).await?;
        }
        tx.commit().await?;
        Ok(n == 1)
    }

    async fn delete(&self, id: Uuid) -> StoreResult<bool> {
        let mut tx = self.pool.begin().await?;
        let n = sqlx::query("delete from fm_stubs where id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n > 0 {
            Self::bump(&mut tx).await?;
        }
        tx.commit().await?;
        Ok(n > 0)
    }

    async fn delete_by_tag(&self, tag: &str, spaces: &SpaceFilter) -> StoreResult<u64> {
        let mut tx = self.pool.begin().await?;
        let mut qb = QueryBuilder::<Postgres>::new("delete from fm_stubs where ");
        qb.push_bind(tag).push(" = any(tags)");
        push_spaces(&mut qb, spaces);
        let n = qb.build().execute(&mut *tx).await?.rows_affected();
        if n > 0 {
            Self::bump(&mut tx).await?;
        }
        tx.commit().await?;
        Ok(n)
    }

    async fn replace_tag(
        &self,
        tag: Option<&str>,
        spaces: &SpaceFilter,
        stubs: &[Stub],
    ) -> StoreResult<u64> {
        let mut tx = self.pool.begin().await?;
        let removed = match tag {
            Some(t) => {
                let mut qb = QueryBuilder::<Postgres>::new("delete from fm_stubs where ");
                qb.push_bind(t).push(" = any(tags)");
                push_spaces(&mut qb, spaces);
                qb.build().execute(&mut *tx).await?.rows_affected()
            }
            None => 0,
        };
        for chunk in stubs.chunks(500) {
            let mut qb = QueryBuilder::<Postgres>::new(
                "insert into fm_stubs (id, definition, priority, tags, space, updated_at) ",
            );
            let mut rows = Vec::with_capacity(chunk.len());
            for s in chunk {
                let mut s = s.clone();
                s.updated_at = None;
                rows.push((
                    s.id,
                    serde_json::to_value(&s)?,
                    s.priority,
                    s.tags.clone(),
                    s.space.clone(),
                ));
            }
            qb.push_values(rows, |mut b, (id, def, prio, tags, space)| {
                b.push_bind(id)
                    .push_bind(def)
                    .push_bind(prio)
                    .push_bind(tags)
                    .push_bind(space)
                    .push("clock_timestamp()");
            });
            qb.push(" on conflict (id) do update set definition = excluded.definition, priority = excluded.priority, tags = excluded.tags, space = excluded.space, updated_at = clock_timestamp()");
            qb.build().execute(&mut *tx).await?;
        }
        Self::bump(&mut tx).await?;
        tx.commit().await?;
        Ok(removed)
    }

    async fn clear(&self, spaces: &SpaceFilter) -> StoreResult<u64> {
        let mut tx = self.pool.begin().await?;
        let mut qb = QueryBuilder::<Postgres>::new("delete from fm_stubs where true");
        push_spaces(&mut qb, spaces);
        let n = qb.build().execute(&mut *tx).await?.rows_affected();
        Self::bump(&mut tx).await?;
        tx.commit().await?;
        Ok(n)
    }

    async fn revision(&self) -> StoreResult<i64> {
        Ok(
            sqlx::query_scalar("select revision from fm_revision where id = 1")
                .fetch_one(&self.pool)
                .await?,
        )
    }
}

fn journal_where<'a>(qb: &mut QueryBuilder<'a, Postgres>, f: &'a JournalFilter) {
    qb.push(" where true");
    if let Some(v) = &f.source {
        qb.push(" and source = ").push_bind(v);
    }
    if let Some(v) = &f.method {
        qb.push(" and upper(method) = upper(")
            .push_bind(v)
            .push(")");
    }
    if let Some(v) = &f.path_prefix {
        qb.push(" and starts_with(path, ").push_bind(v).push(")");
    }
    if let Some(v) = &f.correlation {
        qb.push(" and correlation = ").push_bind(v);
    }
    if let Some(v) = &f.body_contains {
        qb.push(" and strpos(body, ").push_bind(v).push(") > 0");
    }
    if let Some(v) = f.matched {
        qb.push(" and matched = ").push_bind(v);
    }
    if let Some(v) = f.stub_id {
        qb.push(" and stub_id = ").push_bind(v);
    }
    if let Some(v) = f.since {
        qb.push(" and at >= ").push_bind(v);
    }
    push_spaces(qb, &f.spaces);
}

/// Columns read back for journal entries. Queries never select `*`: a replica of an earlier release keeps
/// its prepared statements while a newer one adds columns, and PostgreSQL rejects a cached plan whose
/// result type changed.
const JOURNAL_COLUMNS: &str = "id, at, source, method, path, query, headers, body, status, response_body, stub_id, correlation, matched, duration_ms, trace_id, space";

/// Columns read back for callback jobs; see [`JOURNAL_COLUMNS`].
const OUTBOX_COLUMNS: &str = "id, source, correlation, url, method, headers, body, due_at, attempts, max_attempts, status, last_error, last_status, created_at, sent_at, traceparent, space";

fn row_to_entry(r: &PgRow) -> StoreResult<JournalEntry> {
    let headers: Value = r.try_get("headers")?;
    Ok(JournalEntry {
        id: r.try_get("id")?,
        at: r.try_get("at")?,
        source: r.try_get("source")?,
        method: r.try_get("method")?,
        path: r.try_get("path")?,
        query: r.try_get("query")?,
        headers: serde_json::from_value::<BTreeMap<String, String>>(headers)?,
        body: r.try_get("body")?,
        status: r.try_get::<i32, _>("status")? as u16,
        response_body: r.try_get("response_body")?,
        stub_id: r.try_get("stub_id")?,
        correlation: r.try_get("correlation")?,
        matched: r.try_get("matched")?,
        duration_ms: r.try_get::<i64, _>("duration_ms")? as u64,
        trace_id: r.try_get("trace_id")?,
        space: r.try_get("space")?,
    })
}

#[async_trait]
impl JournalStore for PgStore {
    async fn append_many(&self, entries: &[JournalEntry]) -> StoreResult<()> {
        for chunk in entries.chunks(500) {
            let mut qb = QueryBuilder::<Postgres>::new(
                "insert into fm_journal (at, source, method, path, query, headers, body, status, response_body, stub_id, correlation, matched, duration_ms, trace_id, space) ",
            );
            let mut rows = Vec::with_capacity(chunk.len());
            for e in chunk {
                rows.push((e, serde_json::to_value(&e.headers)?));
            }
            qb.push_values(rows, |mut b, (e, headers)| {
                b.push_bind(e.at)
                    .push_bind(e.source.clone())
                    .push_bind(e.method.clone())
                    .push_bind(no_nul(&e.path))
                    .push_bind(no_nul(&e.query))
                    .push_bind(headers)
                    .push_bind(no_nul(&e.body))
                    .push_bind(e.status as i32)
                    .push_bind(no_nul(&e.response_body))
                    .push_bind(e.stub_id)
                    .push_bind(e.correlation.as_deref().map(no_nul))
                    .push_bind(e.matched)
                    .push_bind(e.duration_ms as i64)
                    .push_bind(e.trace_id.clone())
                    .push_bind(e.space.clone());
            });
            qb.build().execute(&self.pool).await?;
        }
        Ok(())
    }

    async fn append(&self, e: &JournalEntry) -> StoreResult<i64> {
        let id: i64 = sqlx::query_scalar(
            "insert into fm_journal (at, source, method, path, query, headers, body, status, response_body, stub_id, correlation, matched, duration_ms, trace_id, space)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) returning id",
        )
        .bind(e.at)
        .bind(&e.source)
        .bind(&e.method)
        .bind(no_nul(&e.path))
        .bind(no_nul(&e.query))
        .bind(serde_json::to_value(&e.headers)?)
        .bind(no_nul(&e.body))
        .bind(e.status as i32)
        .bind(no_nul(&e.response_body))
        .bind(e.stub_id)
        .bind(e.correlation.as_deref().map(no_nul))
        .bind(e.matched)
        .bind(e.duration_ms as i64)
        .bind(&e.trace_id)
        .bind(&e.space)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    async fn query(&self, f: &JournalFilter) -> StoreResult<Vec<JournalEntry>> {
        let mut qb = QueryBuilder::new(format!("select {JOURNAL_COLUMNS} from fm_journal"));
        journal_where(&mut qb, f);
        qb.push(" order by id desc limit ")
            .push_bind(f.limit.unwrap_or(100).clamp(1, 10_000));
        let rows = qb.build().fetch_all(&self.pool).await?;
        rows.iter().map(row_to_entry).collect()
    }

    async fn count(&self, f: &JournalFilter) -> StoreResult<i64> {
        let mut qb = QueryBuilder::new("select count(*) from fm_journal");
        journal_where(&mut qb, f);
        Ok(qb.build_query_scalar().fetch_one(&self.pool).await?)
    }

    async fn clear(&self, spaces: &SpaceFilter) -> StoreResult<u64> {
        let mut qb = QueryBuilder::<Postgres>::new("delete from fm_journal where true");
        push_spaces(&mut qb, spaces);
        Ok(qb.build().execute(&self.pool).await?.rows_affected())
    }

    async fn purge_older_than(&self, at: DateTime<Utc>) -> StoreResult<u64> {
        Ok(sqlx::query("delete from fm_journal where at < $1")
            .bind(at)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }
    async fn get(&self, id: i64) -> StoreResult<Option<JournalEntry>> {
        let row = sqlx::query(&format!(
            "select {JOURNAL_COLUMNS} from fm_journal where id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(row_to_entry).transpose()
    }

    async fn query_page(&self, f: &JournalFilter, offset: i64) -> StoreResult<Vec<JournalEntry>> {
        let mut qb = QueryBuilder::new(format!("select {JOURNAL_COLUMNS} from fm_journal"));
        journal_where(&mut qb, f);
        qb.push(" order by id desc limit ")
            .push_bind(f.limit.unwrap_or(100).clamp(1, 10_000))
            .push(" offset ")
            .push_bind(offset.max(0));
        let rows = qb.build().fetch_all(&self.pool).await?;
        rows.iter().map(row_to_entry).collect()
    }

    async fn delete_ids(&self, ids: &[i64]) -> StoreResult<u64> {
        let mut n = 0;
        for chunk in ids.chunks(5_000) {
            n += sqlx::query("delete from fm_journal where id = any($1)")
                .bind(chunk)
                .execute(&self.pool)
                .await?
                .rows_affected();
        }
        Ok(n)
    }

    async fn stub_hits(&self, since: Option<DateTime<Utc>>) -> StoreResult<Vec<Uuid>> {
        Ok(sqlx::query_scalar(
            "select distinct stub_id from fm_journal where stub_id is not null and ($1::timestamptz is null or at >= $1)",
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await?)
    }
}

fn row_to_job(r: &PgRow) -> StoreResult<CallbackJob> {
    let headers: Value = r.try_get("headers")?;
    let status: String = r.try_get("status")?;
    Ok(CallbackJob {
        id: r.try_get("id")?,
        source: r.try_get("source")?,
        correlation: r.try_get("correlation")?,
        url: r.try_get("url")?,
        method: r.try_get("method")?,
        headers: serde_json::from_value(headers)?,
        body: r.try_get("body")?,
        due_at: r.try_get("due_at")?,
        attempts: r.try_get::<i32, _>("attempts")? as u32,
        max_attempts: r.try_get::<i32, _>("max_attempts")? as u32,
        status: CallbackStatus::parse(&status)
            .ok_or_else(|| anyhow::anyhow!("unknown status {status}"))?,
        last_error: r.try_get("last_error")?,
        last_status: r
            .try_get::<Option<i32>, _>("last_status")?
            .map(|s| s as u16),
        created_at: r.try_get("created_at")?,
        sent_at: r.try_get("sent_at")?,
        traceparent: r.try_get("traceparent")?,
        space: r.try_get("space")?,
    })
}

#[async_trait]
impl OutboxStore for PgStore {
    async fn enqueue(&self, j: &CallbackJob) -> StoreResult<()> {
        sqlx::query(
            "insert into fm_outbox (id, source, correlation, url, method, headers, body, due_at, attempts, max_attempts, status, last_error, last_status, created_at, sent_at, traceparent, space)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)",
        )
        .bind(j.id)
        .bind(&j.source)
        .bind(j.correlation.as_deref().map(no_nul))
        .bind(no_nul(&j.url))
        .bind(&j.method)
        .bind(serde_json::to_value(j.headers.iter().map(|(k, v)| (no_nul(k), no_nul(v))).collect::<BTreeMap<_, _>>())?)
        .bind(no_nul(&j.body))
        .bind(j.due_at)
        .bind(j.attempts as i32)
        .bind(j.max_attempts as i32)
        .bind(j.status.as_str())
        .bind(&j.last_error)
        .bind(j.last_status.map(|s| s as i32))
        .bind(j.created_at)
        .bind(j.sent_at)
        .bind(&j.traceparent)
        .bind(&j.space)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn claim_due(
        &self,
        now: DateTime<Utc>,
        limit: i64,
        lease: Duration,
    ) -> StoreResult<Vec<CallbackJob>> {
        let lease_until = now + chrono::Duration::from_std(lease)?;
        sqlx::query(
            "update fm_outbox set status = 'failed', last_error = 'lease expired after the last attempt', lease_until = null
             where status = 'pending' and attempts >= max_attempts and lease_until is not null and lease_until <= $1",
        )
        .bind(now)
        .execute(&self.pool)
        .await?;
        let rows = sqlx::query(&format!(
            "with due as (
                select id as due_id from fm_outbox
                where status = 'pending' and attempts < max_attempts and due_at <= $1 and (lease_until is null or lease_until <= $1)
                order by due_at
                limit $2
                for update skip locked
            )
            update fm_outbox o set lease_until = $3, attempts = o.attempts + 1
            from due where o.id = due.due_id
            returning {OUTBOX_COLUMNS}"
        ))
        .bind(now)
        .bind(limit.max(0))
        .bind(lease_until)
        .fetch_all(&self.pool)
        .await?;
        let mut jobs: Vec<CallbackJob> = rows.iter().map(row_to_job).collect::<StoreResult<_>>()?;
        jobs.sort_by_key(|j| j.due_at);
        Ok(jobs)
    }

    async fn mark_sent(&self, id: Uuid, attempt: u32, status: u16) -> StoreResult<bool> {
        let n = sqlx::query(
            "update fm_outbox set status = 'sent', last_status = $3, last_error = null, sent_at = now(), lease_until = null
             where id = $1 and status = 'pending' and attempts = $2",
        )
        .bind(id)
        .bind(attempt as i32)
        .bind(status as i32)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n == 1)
    }

    async fn mark_attempt_failed(
        &self,
        id: Uuid,
        attempt: u32,
        error: &str,
        status: Option<u16>,
        retry_at: Option<DateTime<Utc>>,
    ) -> StoreResult<bool> {
        let n = match retry_at {
            Some(at) => sqlx::query(
                "update fm_outbox set last_error = $3, last_status = $4, due_at = $5, lease_until = null
                 where id = $1 and status = 'pending' and attempts = $2",
            )
            .bind(id)
            .bind(attempt as i32)
            .bind(error)
            .bind(status.map(|s| s as i32))
            .bind(at)
            .execute(&self.pool)
            .await?
            .rows_affected(),
            None => sqlx::query(
                "update fm_outbox set status = 'failed', last_error = $3, last_status = $4, lease_until = null
                 where id = $1 and status = 'pending' and attempts = $2",
            )
            .bind(id)
            .bind(attempt as i32)
            .bind(error)
            .bind(status.map(|s| s as i32))
            .execute(&self.pool)
            .await?
            .rows_affected(),
        };
        Ok(n == 1)
    }

    async fn list(&self, f: &CallbackFilter) -> StoreResult<Vec<CallbackJob>> {
        let mut qb =
            QueryBuilder::new(format!("select {OUTBOX_COLUMNS} from fm_outbox where true"));
        if let Some(v) = &f.source {
            qb.push(" and source = ").push_bind(v);
        }
        if let Some(v) = &f.correlation {
            qb.push(" and correlation = ").push_bind(v);
        }
        if let Some(v) = f.status {
            qb.push(" and status = ").push_bind(v.as_str());
        }
        push_spaces(&mut qb, &f.spaces);
        qb.push(" order by created_at desc, id desc limit ")
            .push_bind(f.limit.unwrap_or(100).clamp(1, 10_000));
        let rows = qb.build().fetch_all(&self.pool).await?;
        rows.iter().map(row_to_job).collect()
    }

    async fn pending_count(&self) -> StoreResult<i64> {
        Ok(
            sqlx::query_scalar("select count(*) from fm_outbox where status = 'pending'")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    async fn clear(&self, spaces: &SpaceFilter) -> StoreResult<u64> {
        let mut qb = QueryBuilder::<Postgres>::new("delete from fm_outbox where true");
        push_spaces(&mut qb, spaces);
        Ok(qb.build().execute(&self.pool).await?.rows_affected())
    }

    async fn purge_finished_older_than(&self, at: DateTime<Utc>) -> StoreResult<u64> {
        Ok(
            sqlx::query("delete from fm_outbox where status <> 'pending' and created_at < $1")
                .bind(at)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }
}

#[async_trait]
impl DataStore for PgStore {
    async fn get(&self, ns: &str, key: &str) -> StoreResult<Option<Value>> {
        Ok(
            sqlx::query_scalar("select value from fm_data where namespace = $1 and key = $2")
                .bind(ns)
                .bind(key)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn put(&self, ns: &str, key: &str, value: &Value) -> StoreResult<()> {
        sqlx::query(
            "insert into fm_data (namespace, key, value, updated_at) values ($1, $2, $3, now())
             on conflict (namespace, key) do update set value = excluded.value, updated_at = now()",
        )
        .bind(ns)
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete(&self, ns: &str, key: &str) -> StoreResult<bool> {
        Ok(
            sqlx::query("delete from fm_data where namespace = $1 and key = $2")
                .bind(ns)
                .bind(key)
                .execute(&self.pool)
                .await?
                .rows_affected()
                > 0,
        )
    }

    async fn list(&self, ns: &str, limit: i64) -> StoreResult<Vec<(String, Value)>> {
        let rows = sqlx::query(
            "select key, value from fm_data where namespace = $1 order by key limit $2",
        )
        .bind(ns)
        .bind(limit.clamp(1, 100_000))
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| Ok((r.try_get("key")?, r.try_get("value")?)))
            .collect()
    }

    async fn list_without(
        &self,
        ns: &str,
        limit: i64,
        omit_field: &str,
    ) -> StoreResult<Vec<(String, Value)>> {
        let rows = sqlx::query(
            "select key, value - $3 as value from fm_data where namespace = $1 order by key limit $2",
        )
        .bind(ns)
        .bind(limit.clamp(1, 100_000))
        .bind(omit_field)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| Ok((r.try_get("key")?, r.try_get("value")?)))
            .collect()
    }

    async fn clear(&self, ns: &str) -> StoreResult<u64> {
        Ok(sqlx::query("delete from fm_data where namespace = $1")
            .bind(ns)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    async fn ping(&self) -> StoreResult<()> {
        PgStore::ping(self).await
    }
}

fn row_to_audit(r: &PgRow) -> StoreResult<AuditEvent> {
    Ok(AuditEvent {
        id: r.try_get("id")?,
        at: r.try_get("at")?,
        principal: r.try_get("principal")?,
        role: r.try_get("role")?,
        principal_kind: r.try_get("principal_kind")?,
        space: r.try_get("space")?,
        action: r.try_get("action")?,
        detail: r.try_get("detail")?,
    })
}

#[async_trait]
impl AuditStore for PgStore {
    async fn record(&self, e: &AuditEvent) -> StoreResult<()> {
        sqlx::query("insert into fm_audit (at, principal, role, principal_kind, space, action, detail) values ($1, $2, $3, $4, $5, $6, $7)")
            .bind(e.at)
            .bind(&e.principal)
            .bind(&e.role)
            .bind(&e.principal_kind)
            .bind(&e.space)
            .bind(&e.action)
            .bind(&e.detail)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn list(&self, limit: i64) -> StoreResult<Vec<AuditEvent>> {
        let rows = sqlx::query("select id, at, principal, role, principal_kind, space, action, detail from fm_audit order by id desc limit $1")
            .bind(limit.clamp(1, 10_000))
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(row_to_audit).collect()
    }

    async fn query(&self, f: &AuditFilter) -> StoreResult<Vec<AuditEvent>> {
        let mut qb = QueryBuilder::<Postgres>::new(
            "select id, at, principal, role, principal_kind, space, action, detail from fm_audit where true",
        );
        match (f.action_prefix(), &f.action) {
            (Some(p), _) => {
                qb.push(" and starts_with(action, ")
                    .push_bind(p.to_string())
                    .push(")");
            }
            (None, Some(a)) => {
                qb.push(" and action = ").push_bind(a.clone());
            }
            (None, None) => {}
        }
        if let Some(p) = &f.principal {
            qb.push(" and principal = ").push_bind(p.clone());
        }
        if let Some(s) = f.since {
            qb.push(" and at >= ").push_bind(s);
        }
        qb.push(" order by id desc limit ")
            .push_bind(f.limit.unwrap_or(100).clamp(1, 10_000));
        let rows = qb.build().fetch_all(&self.pool).await?;
        rows.iter().map(row_to_audit).collect()
    }

    async fn purge_older_than(&self, at: DateTime<Utc>) -> StoreResult<u64> {
        Ok(sqlx::query("delete from fm_audit where at < $1")
            .bind(at)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }
}

const TOKEN_COLUMNS: &str = "id, name, sha256, role, spaces, global, description, created_by, created_at, expires_at, last_used_at, revoked_at, revoked, parent_id";

fn row_to_token(r: &PgRow) -> StoreResult<ApiToken> {
    Ok(ApiToken {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        sha256: r.try_get("sha256")?,
        role: r.try_get("role")?,
        spaces: r.try_get("spaces")?,
        global: r.try_get("global")?,
        description: r.try_get("description")?,
        created_by: r.try_get("created_by")?,
        created_at: r.try_get("created_at")?,
        expires_at: r.try_get("expires_at")?,
        last_used_at: r.try_get("last_used_at")?,
        revoked_at: r.try_get("revoked_at")?,
        revoked: r.try_get("revoked")?,
        parent_id: r.try_get("parent_id")?,
    })
}

#[async_trait]
impl TokenStore for PgStore {
    async fn create(&self, t: &ApiToken, now: DateTime<Utc>) -> StoreResult<bool> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("update fm_api_tokens set revoked_at = expires_at where name = $1 and revoked_at is null and expires_at <= $2")
            .bind(&t.name)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        let inserted = sqlx::query(&format!(
            "insert into fm_api_tokens ({TOKEN_COLUMNS}) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) on conflict (name) where revoked_at is null do nothing"
        ))
        .bind(t.id)
        .bind(no_nul(&t.name))
        .bind(&t.sha256)
        .bind(&t.role)
        .bind(&t.spaces)
        .bind(t.global)
        .bind(t.description.as_deref().map(no_nul))
        .bind(no_nul(&t.created_by))
        .bind(t.created_at)
        .bind(t.expires_at)
        .bind(t.last_used_at)
        .bind(t.revoked_at)
        .bind(t.revoked)
        .bind(t.parent_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        tx.commit().await?;
        Ok(inserted)
    }

    async fn get(&self, id: Uuid) -> StoreResult<Option<ApiToken>> {
        let r = sqlx::query(&format!(
            "select {TOKEN_COLUMNS} from fm_api_tokens where id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        r.as_ref().map(row_to_token).transpose()
    }

    async fn by_hash(&self, sha256: &str) -> StoreResult<Option<ApiToken>> {
        let r = sqlx::query(&format!(
            "select {TOKEN_COLUMNS} from fm_api_tokens where sha256 = $1"
        ))
        .bind(sha256)
        .fetch_optional(&self.pool)
        .await?;
        r.as_ref().map(row_to_token).transpose()
    }

    async fn list(&self) -> StoreResult<Vec<ApiToken>> {
        let rows = sqlx::query(&format!(
            "select {TOKEN_COLUMNS} from fm_api_tokens order by created_at desc, id desc limit 10000"
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_token).collect()
    }

    async fn revoke(&self, id: Uuid, at: DateTime<Utc>) -> StoreResult<bool> {
        Ok(sqlx::query(
            "update fm_api_tokens set revoked = true, revoked_at = $2 where id = $1 and revoked_at is null and expires_at > $2",
        )
        .bind(id)
        .bind(at)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    async fn has_any(&self) -> StoreResult<bool> {
        Ok(
            sqlx::query_scalar("select exists (select 1 from fm_api_tokens)")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    async fn touch(&self, id: Uuid, at: DateTime<Utc>) -> StoreResult<()> {
        sqlx::query("update fm_api_tokens set last_used_at = $2 where id = $1 and (last_used_at is null or last_used_at <= $2 - interval '1 minute')")
            .bind(id)
            .bind(at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn count_active(&self, now: DateTime<Utc>) -> StoreResult<i64> {
        Ok(sqlx::query_scalar(
            "select count(*) from fm_api_tokens where not revoked and expires_at > $1",
        )
        .bind(now)
        .fetch_one(&self.pool)
        .await?)
    }

    async fn purge_inactive_before(&self, at: DateTime<Utc>) -> StoreResult<u64> {
        Ok(
            sqlx::query("delete from fm_api_tokens where revoked_at < $1 or expires_at < $1")
                .bind(at)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn migration_lock_matches_sqlx() {
        assert_eq!(super::migration_lock_id("postgres"), 368907709894077238);
        assert_eq!(super::migration_lock_id("fastmock"), 3340988666452667578);
    }
}
