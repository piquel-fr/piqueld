//! Consistent single-file archives of daemon state.
//!
//! An archive is an uncompressed tar file containing `manifest.json` (always the
//! first entry), an `SQLite` online snapshot of `piqueld.db`, `secrets.key` when
//! present, the ingress gateway's `ingress/{data,config}` state, and the tailnet
//! node's `tailscale` state. Archives are written while the daemon runs and
//! restored only into an empty data directory. Database access lives in
//! [`DatabaseFile`]; this module owns the archive format and the filesystem.

mod archive;
mod publish;
mod rotation;
#[cfg(test)]
mod tests;

use crate::store::{DatabaseFile, SCHEMA_VERSION, now_ms};
use archive::{Archive, extract};
use publish::publish;
use rotation::Rotation;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io,
    num::NonZeroUsize,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;
use thiserror::Error;
use zeroize::Zeroizing;

/// Archive layout version written into every manifest.
const ARCHIVE_FORMAT: u32 = 1;
const MANIFEST: &str = "manifest.json";
const DATABASE: &str = "piqueld.db";
const SECRET_KEY: &str = "secrets.key";
const INGRESS: &str = "ingress";
/// Gateway state; `ingress/control` only holds a runtime socket.
const INGRESS_STATE: [&str; 2] = ["ingress/data", "ingress/config"];
/// Tailnet node identity, as laid out by `ServerConfig::tailscale_dir`.
const TAILNET: &str = "tailscale";
/// Top-level entries restore moves into the data directory.
const PUBLISHED: [&str; 4] = [DATABASE, SECRET_KEY, INGRESS, TAILNET];
/// Name prefix of the staging directory restore unpacks into. While one
/// exists, the data directory holds an unfinished restore.
const RESTORE_STAGING: &str = ".restore-";
/// Pre-migration archives kept in `<data_dir>/backups`.
const PRE_MIGRATION_KEEP: usize = 3;

/// Backup and restore failures, each naming the step or path involved.
#[derive(Debug, Error)]
pub enum BackupError {
    /// A filesystem step failed.
    #[error("could not {action} {}", path.display())]
    Io {
        /// Step that failed.
        action: &'static str,
        /// Path being accessed.
        path: PathBuf,
        /// Underlying failure.
        #[source]
        source: io::Error,
    },
    /// A database step failed.
    #[error("could not {action}")]
    Database {
        /// Step that failed.
        action: &'static str,
        /// Underlying failure.
        #[source]
        source: sqlx::Error,
    },
    /// The data directory holds no migrated database.
    #[error("the database has no schema yet; there is nothing to back up")]
    Uninitialized,
    /// The archive or database was written by a newer daemon.
    #[error(
        "schema version {found} is newer than this binary supports ({supported}); use a newer piqueld"
    )]
    NewerSchema {
        /// Schema version found.
        found: u64,
        /// Latest schema this binary supports.
        supported: u64,
    },
    /// The archive layout is from an unknown, newer format.
    #[error("archive format {0} is not supported; use a newer piqueld")]
    UnsupportedFormat(u32),
    /// Restore refuses to merge with existing state.
    #[error("restore target {} is not empty", .0.display())]
    NotEmpty(PathBuf),
    /// The archive does not start with a readable manifest.
    #[error("archive does not start with a valid {MANIFEST}")]
    Manifest(#[source] Option<serde_json::Error>),
    /// The archive contains a path or entry type that restore does not write.
    #[error("archive contains unexpected entry {}", .0.display())]
    UnexpectedEntry(PathBuf),
    /// The archive has no database.
    #[error("archive does not contain {DATABASE}")]
    MissingDatabase,
    /// A failed restore could not be undone; see the log for each failed undo.
    #[error(
        "restore failed and could not be undone; empty the data directory and run piqueld restore again (remaining state is in {})",
        staging.display()
    )]
    PartialRestore {
        /// Staging directory kept so the daemon refuses to start.
        staging: PathBuf,
        /// Failure that interrupted publication.
        #[source]
        source: Box<BackupError>,
    },
    /// A restore was interrupted before it finished.
    #[error(
        "an interrupted restore left {}; empty the data directory and run piqueld restore again",
        .0.display()
    )]
    InterruptedRestore(PathBuf),
    /// The database contradicts itself or its manifest.
    #[error("database is inconsistent: {0}")]
    Inconsistent(&'static str),
    /// The archived database is damaged.
    #[error("archived database failed its integrity check: {0}")]
    Integrity(String),
    /// The secret key was replaced while the database was being copied.
    #[error("{SECRET_KEY} changed while the backup was taken; retry the backup")]
    KeyChanged,
    /// A blocking archive task panicked or was cancelled.
    #[error("archive task failed")]
    Task(#[source] tokio::task::JoinError),
}

impl BackupError {
    fn io(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> Self {
        let path = path.to_path_buf();
        move |source| Self::Io {
            action,
            path,
            source,
        }
    }

    fn database(action: &'static str) -> impl FnOnce(sqlx::Error) -> Self {
        move |source| Self::Database { action, source }
    }
}

/// Identity of an archive, stored as its first entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BackupManifest {
    /// Archive layout version.
    pub format: u32,
    /// `PRAGMA user_version` of the archived database.
    pub schema_version: u64,
    /// Version of the daemon binary that wrote the archive.
    pub daemon_version: String,
    /// Control-plane instance identity of the archived database.
    pub instance_id: String,
    /// When the archive was written, in Unix milliseconds.
    pub created_at_ms: i64,
}

impl BackupManifest {
    /// Describes `database`, rejecting schemas it cannot represent and metadata
    /// that `Store::open` would reject.
    async fn read(database: &mut DatabaseFile) -> Result<Self, BackupError> {
        let schema_version = schema_version(database).await?;
        if schema_version == 0 {
            return Err(BackupError::Uninitialized);
        }
        let instance = database
            .instance()
            .await
            .map_err(BackupError::database("read the instance metadata"))?;
        if u64::try_from(instance.schema_version).ok() != Some(schema_version) {
            return Err(BackupError::Inconsistent(
                "recorded schema version differs from user_version",
            ));
        }
        Ok(Self {
            format: ARCHIVE_FORMAT,
            schema_version,
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
            instance_id: instance.instance_id,
            created_at_ms: now_ms(),
        })
    }

    /// Accepts only archives this binary can restore and later open.
    fn check(&self) -> Result<(), BackupError> {
        if self.format != ARCHIVE_FORMAT {
            return Err(BackupError::UnsupportedFormat(self.format));
        }
        check_schema(self.schema_version)
    }
}

/// Rejects schemas newer than this binary can migrate.
fn check_schema(found: u64) -> Result<(), BackupError> {
    if found > SCHEMA_VERSION {
        return Err(BackupError::NewerSchema {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(())
}

/// Reads `database`'s schema version, rejecting ones newer than this binary.
async fn schema_version(database: &mut DatabaseFile) -> Result<u64, BackupError> {
    let version = database
        .user_version()
        .await
        .map_err(BackupError::database("read the schema version"))?;
    let version =
        u64::try_from(version).map_err(|_| BackupError::Inconsistent("negative schema version"))?;
    check_schema(version)?;
    Ok(version)
}

async fn open(path: &Path) -> Result<DatabaseFile, BackupError> {
    DatabaseFile::open(path)
        .await
        .map_err(BackupError::database("open the database"))
}

async fn close(database: DatabaseFile) -> Result<(), BackupError> {
    database
        .close()
        .await
        .map_err(BackupError::database("close the database"))
}

/// Backup and restore of one daemon data directory.
pub struct Backups<'a> {
    data_dir: &'a Path,
}

impl<'a> Backups<'a> {
    /// Selects the data directory holding `piqueld.db`.
    #[must_use]
    pub fn new(data_dir: &'a Path) -> Self {
        Self { data_dir }
    }

    /// Archives the data directory to `output`, which must not exist yet, and
    /// records the completion time in the database. Safe while the daemon runs.
    ///
    /// # Errors
    /// Returns the failed step; a partial archive is never left at `output`.
    pub async fn create(&self, output: &Path) -> Result<BackupManifest, BackupError> {
        let mut database = open(&self.data_dir.join(DATABASE)).await?;
        let manifest = self.write(&mut database, output).await?;
        // Older schemas lack the column; the daemon records nothing until it migrates.
        if manifest.schema_version == SCHEMA_VERSION {
            database
                .record_backup(manifest.created_at_ms)
                .await
                .map_err(BackupError::database("record the backup time"))?;
        }
        close(database).await?;
        Ok(manifest)
    }

    /// Like [`Self::create`], writing `piqueld-<ms>.tar` into `directory` and
    /// deleting all but the newest `keep` such archives.
    ///
    /// # Errors
    /// Returns the failed step. Pruning runs only after a successful backup.
    pub async fn create_rotated(
        &self,
        directory: &Path,
        keep: NonZeroUsize,
    ) -> Result<(PathBuf, BackupManifest), BackupError> {
        create_private_dir(directory)?;
        let output = directory.join(format!("piqueld-{}.tar", now_ms()));
        let manifest = self.create(&output).await?;
        Rotation::Scheduled.prune(directory, keep.get())?;
        Ok((output, manifest))
    }

    /// Migrations are forward-only, so before the daemon opens a database that
    /// needs migrating, this writes `<data_dir>/backups/pre-<schema>-<ms>.tar`
    /// as the previous binary's rollback point, keeping the newest few. Call it
    /// with the data directory locked.
    ///
    /// # Errors
    /// Returns the failed step, or [`BackupError::NewerSchema`] for a database
    /// the daemon could not open either.
    pub async fn before_migration(&self) -> Result<Option<(PathBuf, BackupManifest)>, BackupError> {
        let path = self.data_dir.join(DATABASE);
        if !path
            .try_exists()
            .map_err(BackupError::io("inspect", &path))?
        {
            return Ok(None);
        }
        let mut database = open(&path).await?;
        let version = schema_version(&mut database).await?;
        let archive = if version == 0 || version == SCHEMA_VERSION {
            None
        } else {
            let directory = self.data_dir.join("backups");
            create_private_dir(&directory)?;
            let output = directory.join(format!("pre-{version}-{}.tar", now_ms()));
            let manifest = self.write(&mut database, &output).await?;
            Rotation::PreMigration.prune(&directory, PRE_MIGRATION_KEEP)?;
            Some((output, manifest))
        };
        close(database).await?;
        Ok(archive)
    }

    /// Restores `archive` into the data directory, which must be empty or absent.
    /// The database keeps its archived schema; the daemon migrates it on start.
    ///
    /// # Errors
    /// Rejects newer schemas, unexpected entries, damaged databases, and a
    /// running daemon. Failures before publication leave the data directory
    /// empty. Undoing a failed publication may itself fail
    /// ([`BackupError::PartialRestore`]), and removing or syncing the staging
    /// directory may fail after everything was published; both leave restored
    /// state behind. See [`Self::ensure_restore_complete`] for interruptions.
    pub async fn restore(&self, archive: &Path) -> Result<BackupManifest, BackupError> {
        let data_dir = self.data_dir;
        crate::prepare_data_dir(data_dir)
            .await
            .map_err(BackupError::io("prepare data directory", data_dir))?;
        let _lock = crate::DirectoryLock::acquire(data_dir)
            .map_err(BackupError::io("lock data directory", data_dir))?;
        let mut existing =
            fs::read_dir(data_dir).map_err(BackupError::io("read data directory", data_dir))?;
        if existing.next().is_some() {
            return Err(BackupError::NotEmpty(data_dir.to_path_buf()));
        }

        let staging = tempfile::Builder::new()
            .prefix(RESTORE_STAGING)
            .tempdir_in(data_dir)
            .map_err(BackupError::io("create staging directory in", data_dir))?;
        let manifest = {
            let archive = archive.to_path_buf();
            let staging = staging.path().to_path_buf();
            tokio::task::spawn_blocking(move || extract(&archive, &staging))
                .await
                .map_err(BackupError::Task)??
        };
        verify(&staging.path().join(DATABASE), &manifest).await?;
        publish(staging, data_dir)?;
        Ok(manifest)
    }

    /// Refuses a data directory holding a restore staging directory, which an
    /// interrupted or unrecoverable restore leaves behind. The daemon checks
    /// this before creating any state, so it never starts over partly
    /// restored state.
    ///
    /// # Errors
    /// Returns [`BackupError::InterruptedRestore`] or the I/O failure.
    pub fn ensure_restore_complete(&self) -> Result<(), BackupError> {
        let entries = fs::read_dir(self.data_dir)
            .map_err(BackupError::io("read data directory", self.data_dir))?;
        for entry in entries {
            let entry = entry.map_err(BackupError::io("read data directory", self.data_dir))?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(RESTORE_STAGING))
            {
                return Err(BackupError::InterruptedRestore(entry.path()));
            }
        }
        Ok(())
    }

    /// Snapshots the database and key, then archives them with ingress and
    /// tailnet state. The manifest is read from the snapshot itself, so it
    /// describes the archived database even if a migration commits meanwhile.
    async fn write(
        &self,
        database: &mut DatabaseFile,
        output: &Path,
    ) -> Result<BackupManifest, BackupError> {
        let parent = parent_of(output);
        let snapshot = NamedTempFile::new_in(parent)
            .map_err(BackupError::io("create database snapshot in", parent))?;
        let snapshot_path = snapshot.path().to_str().ok_or_else(|| BackupError::Io {
            action: "snapshot the database to",
            path: snapshot.path().to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "path is not UTF-8"),
        })?;

        // An unchanged key around the snapshot is the key the snapshot uses.
        let key_path = self.data_dir.join(SECRET_KEY);
        let key = read_key(&key_path)?;
        database
            .snapshot_into(snapshot_path)
            .await
            .map_err(BackupError::database("snapshot the database"))?;
        if read_key(&key_path)? != key {
            return Err(BackupError::KeyChanged);
        }
        let mut copy = open(snapshot.path()).await?;
        let manifest = BackupManifest::read(&mut copy).await?;
        close(copy).await?;

        let archive = Archive {
            manifest: manifest.clone(),
            snapshot,
            key,
            data_dir: self.data_dir.to_path_buf(),
            output: output.to_path_buf(),
        };
        tokio::task::spawn_blocking(move || archive.write())
            .await
            .map_err(BackupError::Task)??;
        Ok(manifest)
    }
}

/// Checks a restored database's integrity and that it matches its manifest.
async fn verify(path: &Path, manifest: &BackupManifest) -> Result<(), BackupError> {
    let mut database = open(path).await?;
    let integrity = database
        .integrity_check()
        .await
        .map_err(BackupError::database("check database integrity"))?;
    if integrity != ["ok"] {
        return Err(BackupError::Integrity(integrity.join("; ")));
    }
    let found = BackupManifest::read(&mut database).await?;
    close(database).await?;
    if found.schema_version != manifest.schema_version {
        return Err(BackupError::Inconsistent(
            "schema version differs from the manifest",
        ));
    }
    if found.instance_id != manifest.instance_id {
        return Err(BackupError::Inconsistent(
            "instance identity differs from the manifest",
        ));
    }
    Ok(())
}

/// Reads the secret key if present; it is created lazily with the first secret.
fn read_key(path: &Path) -> Result<Option<Zeroizing<Vec<u8>>>, BackupError> {
    match fs::read(path) {
        Ok(key) => Ok(Some(Zeroizing::new(key))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(BackupError::io("read", path)(error)),
    }
}

fn create_private_dir(path: &Path) -> Result<(), BackupError> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(BackupError::io("create backup directory", path))
}

/// Flushes a file, or a directory's entries, to disk.
fn sync(path: &Path) -> Result<(), BackupError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(BackupError::io("sync", path))
}

/// Directory containing `path`, treating a bare file name as the working directory.
fn parent_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}
