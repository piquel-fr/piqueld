//! Integrated `SQLx` `SQLite` migration evidence.

use piqueld::store::{SCHEMA_VERSION, SqliteStore};
use sqlx::{Connection, sqlite::SqliteConnection};

#[tokio::test]
async fn sqlx_applies_migrations_and_preserves_instance_identity() {
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

#[tokio::test]
async fn upgrades_legacy_configuration_to_latest_recoverable_deployment() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("legacy.db");
    let url = format!("sqlite://{}?mode=rwc", database_path.display());
    let mut connection = SqliteConnection::connect(&url).await.unwrap();
    sqlx::raw_sql(include_str!("../../../migrations/0001_control_plane.sql"))
        .execute(&mut connection)
        .await
        .unwrap();
    sqlx::raw_sql(
        r#"PRAGMA user_version=1;
        INSERT INTO instance_metadata VALUES(1,'instance-legacy',1,1);
        INSERT INTO applications(id,name,desired_json,generation,created_at_ms,updated_at_ms)
        VALUES('app-legacy','legacy','{"preserved":"configuration"}',2,1,2);
        INSERT INTO application_status VALUES('app-legacy','ready',NULL,NULL,2);
        INSERT INTO operations(id,application_id,kind,state,generation,created_at_ms,updated_at_ms,finished_at_ms)
        VALUES('op-first','app-legacy','apply','succeeded',1,1,1,1),
              ('op-second','app-legacy','apply','succeeded',2,2,3,3);"#,
    ).execute(&mut connection).await.unwrap();
    connection.close().await.unwrap();

    let store = SqliteStore::open(&database_path).await.unwrap();
    assert_eq!(store.instance_id(), "instance-legacy");
    let mut connection = SqliteConnection::connect(&url).await.unwrap();
    let snapshots: Vec<(String, String, i64, Option<i64>)> =
        sqlx::query_as("SELECT id,manifest_json,generation,succeeded_at_ms FROM deployments")
            .fetch_all(&mut connection)
            .await
            .unwrap();
    assert_eq!(
        snapshots,
        vec![(
            "op-second".into(),
            r#"{"preserved":"configuration"}"#.into(),
            2,
            Some(3)
        )]
    );
    let state: String = sqlx::query_scalar("SELECT state FROM application_status")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(state, "ready");
    let violations = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&mut connection)
        .await
        .unwrap();
    assert!(violations.is_empty());
}
