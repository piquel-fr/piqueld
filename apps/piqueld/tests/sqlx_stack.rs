//! `SQLx` validation-tooling half of the database ownership evidence in ADR 0001.

use sqlx::{Connection, Row, sqlite::SqliteConnection};

#[tokio::test]
async fn sqlx_validates_sql_against_a_disposable_database() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sqlx-validation-spike.db");
    let url = format!("sqlite://{}?mode=rwc", database_path.display());
    let mut connection = SqliteConnection::connect(&url).await.unwrap();

    sqlx::query("CREATE TABLE evidence (value TEXT NOT NULL)")
        .execute(&mut connection)
        .await
        .unwrap();
    sqlx::query("INSERT INTO evidence (value) VALUES (?)")
        .bind("checked")
        .execute(&mut connection)
        .await
        .unwrap();
    let value: String = sqlx::query("SELECT value FROM evidence")
        .fetch_one(&mut connection)
        .await
        .unwrap()
        .get(0);
    assert_eq!(value, "checked");
}
