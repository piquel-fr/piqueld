//! Integrated `SQLx` `SQLite` migration evidence.

#[path = "support/sqlite.rs"]
mod sqlite;

use piqueld::store::{SCHEMA_VERSION, Store};
use sqlx::{Connection, sqlite::SqliteConnection};

/// The first migrations, which upgrade tests apply by hand to build a
/// database of an older schema.
const MIGRATIONS: [&str; 10] = [
    include_str!("../../../migrations/0001_control_plane.sql"),
    include_str!("../../../migrations/0002_deployments.sql"),
    include_str!("../../../migrations/0003_deployment_inputs.sql"),
    include_str!("../../../migrations/0004_build_records.sql"),
    include_str!("../../../migrations/0005_structured_build_logs.sql"),
    include_str!("../../../migrations/0006_observability.sql"),
    include_str!("../../../migrations/0007_authentication.sql"),
    include_str!("../../../migrations/0008_application_secrets.sql"),
    include_str!("../../../migrations/0009_ingress.sql"),
    include_str!("../../../migrations/0010_history_pagination.sql"),
];

/// Applies the first `count` [`MIGRATIONS`], then `sql`.
async fn migrate(connection: &mut SqliteConnection, count: usize, sql: &str) {
    for migration in &MIGRATIONS[..count] {
        sqlx::raw_sql(migration)
            .execute(&mut *connection)
            .await
            .unwrap();
    }
    sqlx::raw_sql(sql).execute(connection).await.unwrap();
}

/// The single integer `query` returns.
async fn scalar(connection: &mut SqliteConnection, query: &str) -> i64 {
    sqlx::query_scalar(query)
        .fetch_one(connection)
        .await
        .unwrap()
}

#[tokio::test]
async fn sqlx_applies_migrations_and_preserves_instance_identity() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sqlx-validation.db");
    let store = Store::open(&database_path).await.unwrap();
    let instance_id = store.instance_id().to_owned();
    drop(store);

    let reopened = Store::open(&database_path).await.unwrap();
    assert_eq!(reopened.instance_id(), &instance_id);

    let mut connection = sqlite::connect(&database_path).await;
    let table_count = scalar(
        &mut connection,
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )
    .await;
    assert_eq!(table_count, 50);
    let schema_version = scalar(&mut connection, "PRAGMA user_version").await;
    assert_eq!(schema_version, SCHEMA_VERSION.cast_signed());
}

#[tokio::test]
async fn upgrades_legacy_configuration_to_latest_recoverable_deployment() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("legacy.db");
    let mut connection = sqlite::connect(&database_path).await;
    migrate(
        &mut connection,
        1,
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
        INSERT INTO request_receipts VALUES('delete-key','fingerprint','{"Operation":{"application_id":"app-deleted"}}',999999);
        UPDATE operations SET state='failed',error_code='runtime_unavailable',error_message='runtime unavailable' WHERE id='op-second';"#,
    )
    .await;
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
    connection.close().await.unwrap();

    let store = Store::open(&database_path).await.unwrap();
    assert_eq!(store.instance_id().as_str(), "instance-legacy");
    let deployments = store
        .deployments(
            &piqueld_core::EnvironmentId::parse("app-legacy").unwrap(),
            None,
            10,
        )
        .await
        .unwrap();
    let [deployment] = deployments.items.as_slice() else {
        panic!("one recoverable deployment")
    };
    assert_eq!(deployment.operation.id, "op-second");
    assert_eq!(
        deployment.template,
        piqueld_core::manifest::ApplicationTemplate::from(&manifest)
    );
    assert_eq!(deployment.application.as_ref(), Some(&manifest));
    let mut connection = sqlite::connect(&database_path).await;
    let snapshots: Vec<(String, String, i64, Option<i64>)> =
        sqlx::query_as("SELECT id,manifest_json,generation,succeeded_at_ms FROM deployments")
            .fetch_all(&mut connection)
            .await
            .unwrap();
    assert_eq!(
        snapshots,
        vec![("op-second".into(), manifest_json, 2, None)]
    );
    let state: String = sqlx::query_scalar("SELECT state FROM environment_status")
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
        "SELECT COUNT(*) FROM environments WHERE id='app-deleted'",
        "SELECT COUNT(*) FROM operations WHERE environment_id='app-deleted'",
        "SELECT COUNT(*) FROM events WHERE environment_id='app-deleted'",
        "SELECT COUNT(*) FROM request_receipts WHERE request_id='delete-key'",
    ] {
        assert_eq!(scalar(&mut connection, query).await, 0, "{query}");
    }
    let retained = scalar(
        &mut connection,
        "SELECT COUNT(*) FROM events WHERE environment_id='app-legacy'",
    )
    .await;
    assert_eq!(retained, 1);
}

#[tokio::test]
async fn observability_upgrade_preserves_populated_history_and_restorable_backup() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("schema-five.db");
    let backup = directory.path().join("pre-upgrade.db");
    let mut connection = sqlite::connect(&path).await;
    migrate(
        &mut connection,
        5,
        "PRAGMA user_version=5;
         INSERT INTO instance_metadata VALUES(1,'instance-upgrade',5,1);
         INSERT INTO applications(id,name,desired_json,generation,created_at_ms,updated_at_ms)
         VALUES('app-upgrade','upgrade','{}',1,1,1);
         INSERT INTO operations(id,application_id,kind,state,generation,created_at_ms,updated_at_ms,finished_at_ms)
         VALUES('op-upgrade','app-upgrade','apply','failed',1,1,2,2);
         INSERT INTO events(application_id,operation_id,kind,message,created_at_ms)
         VALUES('app-upgrade','op-upgrade','operation_failed','Retained failure',2);",
    )
    .await;
    sqlx::query("VACUUM INTO ?")
        .bind(backup.to_str().unwrap())
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    let store = Store::open(&path).await.unwrap();
    assert_eq!(store.instance_id().as_str(), "instance-upgrade");
    let events = store.events(None, None, 100).await.unwrap().items;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].message.as_deref(), Some("Retained failure"));
    assert_eq!(
        events[0].scope,
        piqueld_core::observability::EventScope::Application
    );
    assert!(events[0].diagnostic.is_none());
    let mut connection = sqlite::connect(&path).await;
    let cursor = scalar(&mut connection, "SELECT event_id FROM notification_cursor").await;
    assert_eq!(
        cursor, events[0].id,
        "pre-upgrade failures are not replayed"
    );
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&mut connection)
            .await
            .unwrap()
            .is_empty()
    );
    connection.close().await.unwrap();

    // Restore to a separate empty path: the old schema and its data remain usable.
    let restored_path = directory.path().join("restored.db");
    std::fs::copy(&backup, &restored_path).unwrap();
    let mut restored = sqlite::connect(&restored_path).await;
    assert_eq!(scalar(&mut restored, "PRAGMA user_version").await, 5);
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(&mut restored)
        .await
        .unwrap();
    assert_eq!(integrity, "ok");
    let history: String = sqlx::query_scalar("SELECT message FROM events")
        .fetch_one(&mut restored)
        .await
        .unwrap();
    assert_eq!(history, "Retained failure");
}

#[tokio::test]
async fn structured_logs_expire_old_output_without_losing_build_records() {
    let mut connection = SqliteConnection::connect("sqlite::memory:").await.unwrap();
    migrate(
        &mut connection,
        4,
        "INSERT INTO applications(id,name,desired_json,generation,created_at_ms,updated_at_ms)
         VALUES('app-build','build','{}',1,1,1);
         INSERT INTO builds(id,application_id,operation_id,service,source_json,state,started_at_ms,log_bytes)
         VALUES(1,'app-build','op-build','web','{}','succeeded',1,3);
         INSERT INTO build_log_chunks(build_id,offset,data) VALUES(1,0,x'616263');",
    )
    .await;
    sqlx::raw_sql(MIGRATIONS[4])
        .execute(&mut connection)
        .await
        .unwrap();
    let record: (String, String, i64, i64) =
        sqlx::query_as("SELECT service,state,log_bytes,log_expired FROM builds WHERE id=1")
            .fetch_one(&mut connection)
            .await
            .unwrap();
    assert_eq!(record, ("web".into(), "succeeded".into(), 0, 1));
    assert_eq!(
        scalar(&mut connection, "SELECT COUNT(*) FROM build_log_chunks").await,
        0
    );
}

/// Upgrading to authorization keeps every existing account unrestricted.
#[tokio::test]
async fn authorization_upgrade_makes_existing_accounts_administrators() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("schema-ten.db");
    let mut connection = sqlite::connect(&path).await;
    migrate(
        &mut connection,
        10,
        "PRAGMA user_version=10;
         INSERT INTO instance_metadata VALUES(1,'instance-upgrade',10,1);
         INSERT INTO auth_users VALUES('alice','alice','',1),('bob','bob','',1);
         INSERT INTO auth_invitations VALUES('invite','alice','hash',9999999999);",
    )
    .await;
    connection.close().await.unwrap();

    Store::open(&path).await.unwrap();
    let mut connection = sqlite::connect(&path).await;
    let grants: Vec<(Option<String>, String, Option<String>)> = sqlx::query_as(
        "SELECT user_id,permission,application_id FROM auth_grants ORDER BY user_id",
    )
    .fetch_all(&mut connection)
    .await
    .unwrap();
    assert_eq!(
        grants,
        vec![
            (Some("alice".into()), "admin".into(), None),
            (Some("bob".into()), "admin".into(), None),
        ]
    );
}
