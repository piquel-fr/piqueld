//! Integrated `SQLx` `SQLite` migration evidence.

use sqlx::{Connection, Executor, sqlite::SqliteConnection};

const MIGRATIONS: &[&str] = &[
    include_str!("../../../migrations/0001_control_plane.sql"),
    include_str!("../../../migrations/0002_retention_indexes.sql"),
];

#[tokio::test]
async fn sqlx_applies_the_complete_plan_four_migration_stack() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sqlx-validation.db");
    let url = format!("sqlite://{}?mode=rwc", database_path.display());
    let mut connection = SqliteConnection::connect(&url).await.unwrap();

    for migration in MIGRATIONS {
        connection.execute(sqlx::raw_sql(migration)).await.unwrap();
    }

    let table_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_one(&mut connection)
    .await
    .unwrap();
    assert_eq!(table_count, 7);
}
