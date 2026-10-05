//! Database access for [`crate::backup`]. Backups read databases directly
//! rather than through [`Store`], since they run beside a live daemon, before
//! migrations, and on restored databases the daemon has not opened yet.

use super::{Store, StoreError, ensure_database_target};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{path::Path, time::Duration};

/// One database file opened without creating, migrating, or changing its
/// journal mode.
pub(crate) struct DatabaseFile(SqliteConnection);

/// Identity recorded in a database's `instance_metadata`.
pub(crate) struct InstanceRecord {
    pub(crate) instance_id: String,
    /// Schema version the daemon recorded; `Store::open` requires it to equal
    /// `PRAGMA user_version`.
    pub(crate) schema_version: i64,
}

impl DatabaseFile {
    /// Opens the existing database at `path`, refusing symlinks and special files.
    pub(crate) async fn open(path: &Path) -> sqlx::Result<Self> {
        ensure_database_target(path).map_err(sqlx::Error::Io)?;
        let options = SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        SqliteConnection::connect_with(&options).await.map(Self)
    }

    /// `PRAGMA user_version`; zero before the first migration.
    pub(crate) async fn user_version(&mut self) -> sqlx::Result<i64> {
        sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&mut self.0)
            .await
    }

    pub(crate) async fn instance(&mut self) -> sqlx::Result<InstanceRecord> {
        sqlx::query_as!(
            InstanceRecord,
            "SELECT instance_id,schema_version FROM instance_metadata WHERE singleton=1"
        )
        .fetch_one(&mut self.0)
        .await
    }

    /// Writes a consistent copy of the database to `path`, which must not exist.
    pub(crate) async fn snapshot_into(&mut self, path: &str) -> sqlx::Result<()> {
        sqlx::query("VACUUM INTO ?1")
            .bind(path)
            .execute(&mut self.0)
            .await
            .map(drop)
    }

    /// Problems reported by `PRAGMA integrity_check`; `["ok"]` when there are none.
    pub(crate) async fn integrity_check(&mut self) -> sqlx::Result<Vec<String>> {
        sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_all(&mut self.0)
            .await
    }

    /// Records the completion time of a successful `piqueld backup`. The database
    /// must be at the latest schema; older schemas lack the column.
    pub(crate) async fn record_backup(&mut self, at_ms: i64) -> sqlx::Result<()> {
        sqlx::query!(
            "UPDATE instance_metadata SET last_backup_at_ms=?1 WHERE singleton=1",
            at_ms
        )
        .execute(&mut self.0)
        .await
        .map(drop)
    }

    pub(crate) async fn close(self) -> sqlx::Result<()> {
        self.0.close().await
    }

    /// Creates a database migrated only to `version`, as an older daemon left it.
    #[cfg(test)]
    pub(crate) async fn create_at_version(path: &Path, version: usize) {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .unwrap();
        for (index, migration) in super::MIGRATIONS.iter().take(version).enumerate() {
            Store::apply_migration(&pool, index + 1, migration)
                .await
                .unwrap();
        }
        pool.close().await;
    }
}

impl Store {
    /// Completion time of the last successful `piqueld backup`, if any.
    ///
    /// # Errors
    /// Returns the underlying database error.
    pub async fn last_backup_at_ms(&self) -> Result<Option<i64>, StoreError> {
        sqlx::query_scalar!("SELECT last_backup_at_ms FROM instance_metadata WHERE singleton=1")
            .fetch_one(&self.pool)
            .await
            .map_err(StoreError::database)
    }
}
