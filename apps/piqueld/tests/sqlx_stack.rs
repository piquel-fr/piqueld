//! Integrated `SQLx` `SQLite` migration evidence.

use piqueld::store::{SCHEMA_VERSION, SqliteStore};
use sqlx::{Connection, sqlite::SqliteConnection};

#[tokio::test]
async fn sqlx_applies_the_single_fresh_plan_migration() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sqlx-validation.db");
    let store = SqliteStore::open(&database_path).await.unwrap();
    let instance_id = store.instance_id().to_owned();
    drop(store);

    let reopened = SqliteStore::open(&database_path).await.unwrap();
    assert_eq!(reopened.instance_id(), instance_id);

    let url = format!("sqlite://{}?mode=rwc", database_path.display());
    let mut connection = SqliteConnection::connect(&url).await.unwrap();

    let table_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_one(&mut connection)
    .await
    .unwrap();
    assert_eq!(table_count, 8);

    let schema_version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(schema_version, SCHEMA_VERSION.cast_signed());
}
