//! Official-SDK half of the database ownership evidence in ADR 0001.

use libsql::Builder;

#[tokio::test]
async fn official_sdk_owns_the_embedded_database() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("libsql-spike.db");
    let database = Builder::new_local(&database_path).build().await.unwrap();
    let connection = database.connect().unwrap();

    connection
        .execute("CREATE TABLE evidence (value TEXT NOT NULL)", ())
        .await
        .unwrap();
    connection
        .execute("INSERT INTO evidence (value) VALUES ('embedded')", ())
        .await
        .unwrap();
    let mut rows = connection
        .query("SELECT value FROM evidence", ())
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    let value: String = row.get(0).unwrap();
    assert_eq!(value, "embedded");
}
