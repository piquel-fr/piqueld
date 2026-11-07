//! Direct access to a daemon database, for tests that inspect or break it.

use sqlx::{
    ConnectOptions as _, SqliteConnection,
    sqlite::{SqliteConnectOptions, SqliteSynchronous},
};

/// Opens the SQLite database at `path`, creating it if missing. Like the
/// store of a test build, it never waits on the disk to sync a commit.
pub async fn connect(path: &std::path::Path) -> SqliteConnection {
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .synchronous(SqliteSynchronous::Off)
        .connect()
        .await
        .expect("database opens")
}
