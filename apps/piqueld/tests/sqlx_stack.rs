//! Integrated `SQLx` `SQLite` driver evidence for ADR 0001.

use sqlx::{Connection, Executor, sqlite::SqliteConnection};

#[tokio::test]
async fn sqlx_owns_execution_and_compile_time_validation() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sqlx-validation-spike.db");
    let url = format!("sqlite://{}?mode=rwc", database_path.display());
    let mut connection = SqliteConnection::connect(&url).await.unwrap();
    connection
        .execute(sqlx::raw_sql(include_str!(
            "../../../migrations/0001_control_plane.sql"
        )))
        .await
        .unwrap();
    connection
        .execute(sqlx::raw_sql(include_str!(
            "../../../migrations/0002_retention_indexes.sql"
        )))
        .await
        .unwrap();

    // These are production repository query shapes. `query!` checks them from
    // committed offline metadata at compile time; this separate test artifact
    // then executes them only against a disposable migrated SQLx database.
    let application_columns = sqlx::query!(
        "SELECT id,name,desired_json,resolved_json,generation,spec_hash,delete_intent,created_at_ms,updated_at_ms FROM applications WHERE id=?1",
        "application-validation"
    )
    .fetch_optional(&mut connection)
    .await
    .unwrap();
    assert!(application_columns.is_none());

    let pending_operations = sqlx::query!(
        "SELECT o.id,o.application_id,o.generation,o.kind,o.state,o.error_code,o.error_message,o.created_at_ms,o.updated_at_ms,o.started_at_ms,o.finished_at_ms FROM operations o WHERE o.state IN ('pending','recovery') AND NOT EXISTS (SELECT 1 FROM operations older WHERE older.application_id=o.application_id AND older.state IN ('pending','recovery','running') AND (older.generation < o.generation OR (older.generation=o.generation AND (older.created_at_ms < o.created_at_ms OR (older.created_at_ms=o.created_at_ms AND older.id < o.id))))) ORDER BY o.created_at_ms,o.id LIMIT ?1",
        16_i64
    )
    .fetch_all(&mut connection)
    .await
    .unwrap();
    assert!(pending_operations.is_empty());

    let generation_cas = sqlx::query!(
        "UPDATE applications SET generation=?1,updated_at_ms=?2 WHERE id=?3 AND generation=?4 AND delete_intent=0",
        2_i64,
        2_i64,
        "application-validation",
        1_i64
    )
    .execute(&mut connection)
    .await
    .unwrap();
    assert_eq!(generation_cas.rows_affected(), 0);

    // Complex state-machine and cross-row CAS statements are checked here too;
    // they are deliberately not executed because this disposable database has no
    // operation tree. Keeping the text identical to production makes SQL/schema
    // drift a compile-time failure under checked-in offline metadata.
    let _ = sqlx::query!(
        "UPDATE operation_steps SET state=?1,error_code=?2,error_message=?3,updated_at_ms=?4,started_at_ms=COALESCE(started_at_ms,?5),finished_at_ms=?6,attempt=attempt+?7 WHERE id=?8 AND state=?9 AND EXISTS (SELECT 1 FROM operations WHERE id=operation_steps.operation_id AND state='running') AND (?1 != 'running' OR (NOT EXISTS (SELECT 1 FROM operation_steps active WHERE active.operation_id=operation_steps.operation_id AND active.state='running' AND active.id != operation_steps.id) AND NOT EXISTS (SELECT 1 FROM operation_steps earlier WHERE earlier.operation_id=operation_steps.operation_id AND earlier.position < operation_steps.position AND earlier.state NOT IN ('succeeded','skipped'))))",
        "running",
        None::<String>,
        None::<String>,
        1_i64,
        Some(1_i64),
        None::<i64>,
        1_i64,
        "step-validation",
        "pending"
    );
    let _ = sqlx::query!(
        "UPDATE operations SET state=?1,error_code=?2,error_message=?3,updated_at_ms=?4,started_at_ms=COALESCE(started_at_ms,?5),finished_at_ms=?4 WHERE id=?6 AND state=?7",
        "failed",
        Some("execution_failed"),
        Some("operation execution failed"),
        1_i64,
        Some(1_i64),
        "operation-validation",
        "running"
    );
    let _ = sqlx::query!(
        "INSERT OR IGNORE INTO builds(id,operation_id,application_id,service_name,state,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,'pending',?5,?5)",
        "build-validation",
        "operation-validation",
        "application-validation",
        "web",
        1_i64
    );
}
