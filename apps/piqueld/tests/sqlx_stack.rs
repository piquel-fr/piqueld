//! Integrated `SQLx` `SQLite` driver evidence for ADR 0001.

use sqlx::{Connection, sqlite::SqliteConnection};

#[tokio::test]
async fn sqlx_owns_execution_and_compile_time_validation() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sqlx-validation-spike.db");
    let url = format!("sqlite://{}?mode=rwc", database_path.display());
    let mut connection = SqliteConnection::connect(&url).await.unwrap();

    // `query!` describes and type-checks the statement while this target is
    // compiled, then the same SQLx driver executes it against SQLite.
    let row = sqlx::query!(r#"SELECT CAST('integrated' AS TEXT) AS "value!: String""#)
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(row.value, "integrated");
}
