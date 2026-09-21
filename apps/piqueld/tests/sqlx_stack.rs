//! Integrated `SQLx` `SQLite` migration evidence.

use piqueld::store::{SCHEMA_VERSION, Store};
use sqlx::{Connection, sqlite::SqliteConnection};

#[tokio::test]
async fn sqlx_applies_migrations_and_preserves_instance_identity() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sqlx-validation.db");
    let store = Store::open(&database_path).await.unwrap();
    let instance_id = store.instance_id().to_owned();
    drop(store);

    let reopened = Store::open(&database_path).await.unwrap();
    assert_eq!(reopened.instance_id(), instance_id);

    let url = format!("sqlite://{}?mode=rwc", database_path.display());
    let mut connection = SqliteConnection::connect(&url).await.unwrap();

    let table_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_one(&mut connection)
    .await
    .unwrap();
    assert_eq!(table_count, 18);

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
              ('op-second','app-legacy','apply','succeeded',2,2,3,3);
        INSERT INTO applications(id,name,desired_json,generation,delete_intent,deleted_at_ms,created_at_ms,updated_at_ms)
        VALUES('app-deleted','deleted','{}',1,1,3,1,3);
        INSERT INTO operations(id,application_id,kind,state,generation,created_at_ms,updated_at_ms,finished_at_ms)
        VALUES('op-delete','app-deleted','delete','succeeded',1,1,3,3);
        INSERT INTO events(application_id,kind,created_at_ms) VALUES('app-deleted','deleted',3),('app-legacy','created',1);
        INSERT INTO request_receipts VALUES('delete-key','fingerprint','{"Operation":{"application_id":"app-deleted"}}',999999);"#,
    ).execute(&mut connection).await.unwrap();
    let manifest = piqueld_core::parse_toml(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='legacy'\n[spec]",
    )
    .unwrap()
    .normalize(piqueld_core::ApplicationId::parse("app-legacy").unwrap());
    let manifest_json = manifest.canonical_json().unwrap();
    sqlx::query("UPDATE applications SET desired_json=?1 WHERE id='app-legacy'")
        .bind(&manifest_json)
        .execute(&mut connection)
        .await
        .unwrap();
    sqlx::query("UPDATE operations SET state='failed',error_code='runtime_unavailable',error_message='runtime unavailable' WHERE id='op-second'").execute(&mut connection).await.unwrap();
    connection.close().await.unwrap();

    let store = Store::open(&database_path).await.unwrap();
    assert_eq!(store.instance_id(), "instance-legacy");
    assert_eq!(
        store.deployment_manifest("op-second").await.unwrap(),
        manifest
    );
    let mut connection = SqliteConnection::connect(&url).await.unwrap();
    let snapshots: Vec<(String, String, i64, Option<i64>)> =
        sqlx::query_as("SELECT id,manifest_json,generation,succeeded_at_ms FROM deployments")
            .fetch_all(&mut connection)
            .await
            .unwrap();
    assert_eq!(
        snapshots,
        vec![("op-second".into(), manifest_json, 2, None)]
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
    for query in [
        "SELECT COUNT(*) FROM applications WHERE id='app-deleted'",
        "SELECT COUNT(*) FROM operations WHERE application_id='app-deleted'",
        "SELECT COUNT(*) FROM events WHERE application_id='app-deleted'",
        "SELECT COUNT(*) FROM request_receipts WHERE request_id='delete-key'",
    ] {
        let count: i64 = sqlx::query_scalar(query)
            .fetch_one(&mut connection)
            .await
            .unwrap();
        assert_eq!(count, 0, "{query}");
    }
    let retained: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE application_id='app-legacy'")
            .fetch_one(&mut connection)
            .await
            .unwrap();
    assert_eq!(retained, 1);
}

#[tokio::test]
async fn structured_logs_expire_old_output_without_losing_build_records() {
    let mut connection = SqliteConnection::connect("sqlite::memory:").await.unwrap();
    for migration in [
        include_str!("../../../migrations/0001_control_plane.sql"),
        include_str!("../../../migrations/0002_deployments.sql"),
        include_str!("../../../migrations/0003_deployment_inputs.sql"),
        include_str!("../../../migrations/0004_build_records.sql"),
    ] {
        sqlx::raw_sql(migration)
            .execute(&mut connection)
            .await
            .unwrap();
    }
    sqlx::raw_sql(
        "INSERT INTO applications(id,name,desired_json,generation,created_at_ms,updated_at_ms)
         VALUES('app-build','build','{}',1,1,1);
         INSERT INTO builds(id,application_id,operation_id,service,source_json,state,started_at_ms,log_bytes)
         VALUES(1,'app-build','op-build','web','{}','succeeded',1,3);
         INSERT INTO build_log_chunks(build_id,offset,data) VALUES(1,0,x'616263');",
    ).execute(&mut connection).await.unwrap();
    sqlx::raw_sql(include_str!(
        "../../../migrations/0005_structured_build_logs.sql"
    ))
    .execute(&mut connection)
    .await
    .unwrap();
    let record: (String, String, i64, i64) =
        sqlx::query_as("SELECT service,state,log_bytes,log_expired FROM builds WHERE id=1")
            .fetch_one(&mut connection)
            .await
            .unwrap();
    assert_eq!(record, ("web".into(), "succeeded".into(), 0, 1));
    let chunks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM build_log_chunks")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(chunks, 0);
}
