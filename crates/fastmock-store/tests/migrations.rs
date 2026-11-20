use std::collections::BTreeMap;
use std::time::Duration;

use chrono::Utc;
use fastmock_core::{
    AuditStore, CallbackFilter, CallbackJob, JournalEntry, JournalFilter, JournalStore,
    OutboxStore, StubStore, TokenStore,
};
use fastmock_store::PgStore;
use sqlx::Connection;

async fn fresh_database() -> Option<String> {
    let Ok(admin_url) = std::env::var("FASTMOCK_TEST_DATABASE_URL") else {
        eprintln!("FASTMOCK_TEST_DATABASE_URL not set: skipped");
        return None;
    };
    let db = format!("fm_{}", uuid::Uuid::new_v4().simple());
    let mut admin = sqlx::PgConnection::connect(&admin_url)
        .await
        .expect("admin connection");
    sqlx::query(&format!("create database {db}"))
        .execute(&mut admin)
        .await
        .unwrap();
    Some(format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db))
}

#[tokio::test]
async fn migrate_reports_each_applied_migration() {
    let Some(url) = fresh_database().await else {
        return;
    };
    let pg = PgStore::connect(&url, 2).await.unwrap();
    let applied = pg.migrate().await.unwrap();
    let versions: Vec<i64> = applied.iter().map(|m| m.version).collect();
    assert_eq!(versions.first(), Some(&1), "{applied:?}");
    assert_eq!(
        versions.last(),
        Some(&PgStore::expected_schema_version()),
        "{applied:?}"
    );
    assert!(
        applied.iter().any(|m| m.version == 6 && m.name == "spaces"),
        "{applied:?}"
    );
    sqlx::query("delete from _sqlx_migrations where version = $1")
        .bind(PgStore::expected_schema_version())
        .execute(pg.pool())
        .await
        .unwrap();
    let again = pg.migrate().await.unwrap();
    assert_eq!(
        again.iter().map(|m| m.version).collect::<Vec<_>>(),
        vec![PgStore::expected_schema_version()],
        "only the missing migration runs again"
    );
    assert!(pg.migrate().await.unwrap().is_empty());
}

#[tokio::test]
async fn concurrent_replicas_migrate_once() {
    let Some(url) = fresh_database().await else {
        return;
    };
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let url = url.clone();
        tasks.push(tokio::spawn(async move {
            let pg = PgStore::connect(&url, 2).await.unwrap();
            fastmock_store::migrate(&pg, Duration::from_secs(60)).await
        }));
    }
    for t in tasks {
        t.await.unwrap().expect("every replica migrates or waits");
    }
    let pg = PgStore::connect(&url, 2).await.unwrap();
    assert!(
        pg.migrate().await.unwrap().is_empty(),
        "nothing is left to apply"
    );
    let (failed, max): (i64, i64) = sqlx::query_as(
        "select count(*) filter (where not success), max(version) from _sqlx_migrations",
    )
    .fetch_one(pg.pool())
    .await
    .unwrap();
    assert_eq!(max, PgStore::expected_schema_version());
    assert_eq!(failed, 0);
    pg.check_schema().await.unwrap();
}

#[tokio::test]
async fn skip_mode_checks_the_schema_version() {
    let Some(url) = fresh_database().await else {
        return;
    };
    let pg = PgStore::connect(&url, 2).await.unwrap();
    let e = format!("{:#}", pg.check_schema().await.unwrap_err());
    assert!(e.contains("fastmock migrate"), "{e}");
    assert!(e.contains("no FastMock schema"), "{e}");

    pg.migrate().await.unwrap();
    pg.check_schema().await.unwrap();

    let latest = PgStore::expected_schema_version();
    sqlx::query("delete from _sqlx_migrations where version = $1")
        .bind(latest)
        .execute(pg.pool())
        .await
        .unwrap();
    let e = format!("{:#}", pg.check_schema().await.unwrap_err());
    assert!(e.contains("older than this FastMock version"), "{e}");
    assert!(e.contains(&latest.to_string()), "{e}");
    pg.migrate().await.unwrap();
    pg.check_schema().await.unwrap();

    sqlx::query(
        "insert into _sqlx_migrations (version, description, success, checksum, execution_time) values (999999, 'from a newer release', true, '\\x00', 0)",
    )
    .execute(pg.pool())
    .await
    .unwrap();
    pg.check_schema()
        .await
        .expect("a newer schema is accepted by an older binary");
    pg.migrate()
        .await
        .expect("an older binary starts against a newer schema");

    sqlx::query("update _sqlx_migrations set checksum = '\\x00' where version = $1")
        .bind(latest)
        .execute(pg.pool())
        .await
        .unwrap();
    let e = format!("{:#}", pg.check_schema().await.unwrap_err());
    assert!(e.contains("never be edited"), "{e}");
}

/// Indexes on tables that already hold data are built with `CREATE INDEX CONCURRENTLY` in migrations of
/// their own that run outside a transaction, so writes of running replicas never wait for them.
#[test]
fn indexes_on_existing_tables_are_built_concurrently() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
    let mut files: Vec<(i64, String, String)> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            let version: i64 = name.split('_').next().unwrap().parse().unwrap();
            (version, name, std::fs::read_to_string(&p).unwrap())
        })
        .collect();
    files.sort();
    let mut created: Vec<String> = Vec::new();
    for (version, name, sql) in &files {
        let lower = sql.to_ascii_lowercase();
        let statements: Vec<&str> = lower
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.lines().all(|l| l.trim_start().starts_with("--")))
            .collect();
        let new_tables: Vec<String> = statements
            .iter()
            .filter_map(|s| {
                s.strip_prefix("create table if not exists ")
                    .or_else(|| s.strip_prefix("create table "))
                    .map(|r| r.split_whitespace().next().unwrap().to_string())
            })
            .collect();
        for s in &statements {
            let body: String = s
                .lines()
                .filter(|l| !l.trim_start().starts_with("--"))
                .collect::<Vec<_>>()
                .join(" ");
            let Some(on) = body.split(" on ").nth(1) else {
                continue;
            };
            if !body.contains("create index") && !body.contains("create unique index") {
                continue;
            }
            let table = on.split_whitespace().next().unwrap().to_string();
            if *version < 6 || new_tables.contains(&table) || !created.contains(&table) {
                continue;
            }
            assert!(
                body.contains("concurrently") && body.contains("if not exists"),
                "{name}: index on {table} must use CREATE INDEX CONCURRENTLY IF NOT EXISTS"
            );
            assert!(
                lower.trim_start().starts_with("-- no-transaction"),
                "{name}: concurrent index builds need a no-transaction migration"
            );
            assert_eq!(
                statements.len(),
                1,
                "{name}: one statement per no-transaction migration"
            );
        }
        created.extend(new_tables);
    }
    assert!(created.contains(&"fm_journal".to_string()));
}

fn journal_entry() -> JournalEntry {
    JournalEntry {
        id: 0,
        at: Utc::now(),
        source: "http".into(),
        method: "GET".into(),
        path: "/x".into(),
        query: "".into(),
        headers: BTreeMap::new(),
        body: "".into(),
        status: 200,
        response_body: "ok".into(),
        stub_id: None,
        correlation: None,
        matched: true,
        duration_ms: 1,
        trace_id: None,
        space: None,
    }
}

async fn run_queries(pg: &PgStore) {
    let job = CallbackJob::new(
        "sms",
        "http://example.com/cb".into(),
        "POST".into(),
        BTreeMap::new(),
        "{}".into(),
        Utc::now() - chrono::Duration::seconds(1),
        3,
    );
    pg.enqueue(&job).await.unwrap();
    let claimed = pg
        .claim_due(Utc::now(), 10, Duration::from_secs(30))
        .await
        .expect("claim_due");
    assert!(claimed.iter().any(|j| j.id == job.id));
    OutboxStore::list(pg, &CallbackFilter::default())
        .await
        .expect("outbox list");
    let id = pg.append(&journal_entry()).await.unwrap();
    JournalStore::query(pg, &JournalFilter::default())
        .await
        .expect("journal query");
    pg.query_page(&JournalFilter::default(), 0)
        .await
        .expect("journal page");
    JournalStore::get(pg, id)
        .await
        .expect("journal get")
        .unwrap();
    StubStore::list(pg).await.expect("stub list");
    AuditStore::list(pg, 10).await.expect("audit list");
    TokenStore::list(pg).await.expect("token list");
}

/// Replicas of the previous release keep running while a newer one applies additive migrations. Their
/// prepared statements must survive new columns, so no query reads `*`.
#[tokio::test]
async fn running_replicas_survive_added_columns() {
    let Some(url) = fresh_database().await else {
        return;
    };
    let pg = PgStore::connect(&url, 2).await.unwrap();
    pg.migrate().await.unwrap();
    for _ in 0..3 {
        run_queries(&pg).await;
    }
    let mut other = sqlx::PgConnection::connect(&url).await.unwrap();
    for table in [
        "fm_stubs",
        "fm_journal",
        "fm_outbox",
        "fm_data",
        "fm_audit",
        "fm_api_tokens",
    ] {
        sqlx::query(&format!("alter table {table} add column added_later text"))
            .execute(&mut other)
            .await
            .unwrap();
    }
    for _ in 0..3 {
        run_queries(&pg).await;
    }
}
