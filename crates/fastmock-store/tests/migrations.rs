use std::time::Duration;

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
