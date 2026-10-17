//! Direct access to a daemon database, for tests that inspect or break it.

use sqlx::{Connection, SqliteConnection};

/// Opens the SQLite database at `path`, creating it if missing.
pub async fn connect(path: &std::path::Path) -> SqliteConnection {
    SqliteConnection::connect(&format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .expect("database opens")
}
