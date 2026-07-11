//! `SQLx` compile-time checking half of the database ownership evidence in ADR 0001.

use sqlx::{Connection, sqlite::SqliteConnection};

#[tokio::test]
async fn sqlx_validates_sql_against_a_disposable_database() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sqlx-validation-spike.db");
    let url = format!("sqlite://{}?mode=rwc", database_path.display());
    let mut connection = SqliteConnection::connect(&url).await.unwrap();

    // `query!` asks SQLx to describe and type-check this statement while this
    // test target is compiled. The workspace Cargo configuration supplies a
    // disposable in-memory validation database; production never opens it.
    let row = sqlx::query!(r#"SELECT CAST('checked' AS TEXT) AS "value!: String""#)
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(row.value, "checked");
}
