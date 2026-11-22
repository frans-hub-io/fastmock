use fastmock_core::capture::conformance;
use fastmock_store::{PgCaptureStore, PgStore};
use sqlx::Connection;
use uuid::Uuid;

#[tokio::test]
async fn the_postgres_capture_store_conforms() {
    let Ok(admin_url) = std::env::var("FASTMOCK_TEST_DATABASE_URL") else {
        eprintln!("FASTMOCK_TEST_DATABASE_URL not set: skipped");
        return;
    };
    let db = format!("fm_capture_{}", Uuid::new_v4().simple());
    let mut admin = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(&format!("create database {db}"))
        .execute(&mut admin)
        .await
        .unwrap();
    let url = format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, db);
    let pg = PgStore::connect(&url, 2).await.unwrap();
    pg.migrate().await.unwrap();
    conformance::run(&PgCaptureStore::new(pg.pool().clone())).await;
    pg.pool().close().await;
    let _ = sqlx::query(&format!("drop database {db}"))
        .execute(&mut admin)
        .await;
}
