//! Consistent single-file archives of daemon state.
//!
//! An archive is an uncompressed tar file containing `manifest.json` (always the
//! first entry), an `SQLite` online snapshot of `piqueld.db`, `secrets.key` when
//! present, the ingress gateway's `ingress/{data,config}` state, and the tailnet
//! node's `tailscale` state. Archives are written while the daemon runs and
//! restored only into an empty data directory.

use super::{SCHEMA_VERSION, Store, StoreError, ensure_database_target, now_ms};
use serde::{Deserialize, Serialize};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{
    fs::{self, File},
    io::{self, BufReader, BufWriter, Write},
    num::NonZeroUsize,
    os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tempfile::{NamedTempFile, TempDir};
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
/// Top-level restored state, in publication order: the database moves last, so
/// a data directory without it never looks like a completed restore.
const PUBLISHED: [&str; 4] = [SECRET_KEY, INGRESS, TAILNET, DATABASE];
/// Name prefix of the staging directory restore unpacks into.
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
    /// A restore was interrupted before its database was moved into place.
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
    /// Describes the database behind `connection`, rejecting schemas it cannot
    /// represent and metadata that `Store::open` would reject.
    async fn read(connection: &mut SqliteConnection) -> Result<Self, BackupError> {
        let schema_version = schema_version(connection).await?;
        if schema_version == 0 {
            return Err(BackupError::Uninitialized);
        }
        let metadata = sqlx::query!(
            "SELECT instance_id,schema_version FROM instance_metadata WHERE singleton=1"
        )
        .fetch_one(&mut *connection)
        .await
        .map_err(BackupError::database("read the instance metadata"))?;
        if u64::try_from(metadata.schema_version).ok() != Some(schema_version) {
            return Err(BackupError::Inconsistent(
                "recorded schema version differs from user_version",
            ));
        }
        Ok(Self {
            format: ARCHIVE_FORMAT,
            schema_version,
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
            instance_id: metadata.instance_id,
            created_at_ms: now_ms(),
        })
    }

    /// Accepts only archives this binary can restore and later open.
    fn check(&self) -> Result<(), BackupError> {
        if self.format != ARCHIVE_FORMAT {
            return Err(BackupError::UnsupportedFormat(self.format));
        }
        if self.schema_version > SCHEMA_VERSION {
            return Err(BackupError::NewerSchema {
                found: self.schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        Ok(())
    }
}

/// Reads and bounds `PRAGMA user_version`.
async fn schema_version(connection: &mut SqliteConnection) -> Result<u64, BackupError> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut *connection)
        .await
        .map_err(BackupError::database("read the schema version"))?;
    let version =
        u64::try_from(version).map_err(|_| BackupError::Inconsistent("negative schema version"))?;
    if version > SCHEMA_VERSION {
        return Err(BackupError::NewerSchema {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(version)
}

/// Opens an existing database without creating, migrating, or changing its journal mode.
async fn connect(path: &Path) -> Result<SqliteConnection, BackupError> {
    ensure_database_target(path).map_err(BackupError::io("use database", path))?;
    let options = SqliteConnectOptions::new()
        .filename(path)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
    SqliteConnection::connect_with(&options)
        .await
        .map_err(BackupError::database("open the database"))
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
        let mut connection = connect(&self.data_dir.join(DATABASE)).await?;
        let manifest = self.write(&mut connection, output).await?;
        // Older schemas lack the column; the daemon records nothing until it migrates.
        if manifest.schema_version == SCHEMA_VERSION {
            sqlx::query!(
                "UPDATE instance_metadata SET last_backup_at_ms=?1 WHERE singleton=1",
                manifest.created_at_ms
            )
            .execute(&mut connection)
            .await
            .map_err(BackupError::database("record the backup time"))?;
        }
        connection
            .close()
            .await
            .map_err(BackupError::database("close the database"))?;
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

    /// Writes `<data_dir>/backups/pre-<schema>-<ms>.tar` before migrations run,
    /// keeping the newest few.
    pub(super) async fn before_migration(
        &self,
        connection: &mut SqliteConnection,
    ) -> Result<PathBuf, BackupError> {
        let directory = self.data_dir.join("backups");
        create_private_dir(&directory)?;
        let output = directory.join(format!(
            "pre-{}-{}.tar",
            schema_version(connection).await?,
            now_ms()
        ));
        self.write(connection, &output).await?;
        Rotation::PreMigration.prune(&directory, PRE_MIGRATION_KEEP)?;
        Ok(output)
    }

    /// Restores `archive` into the data directory, which must be empty or absent.
    /// The database keeps its archived schema; the daemon migrates it on start.
    ///
    /// # Errors
    /// Rejects newer schemas, unexpected entries, damaged databases, and a
    /// running daemon. Nothing is left in the data directory on failure, unless
    /// undoing a partial publication fails ([`BackupError::PartialRestore`]);
    /// see [`Self::ensure_restore_complete`] for interruptions.
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

    /// Refuses a data directory left by an interrupted restore: one holding a
    /// restore staging directory but no database. The daemon checks this before
    /// creating any state, so it never starts an empty instance over partly
    /// restored state.
    ///
    /// # Errors
    /// Returns [`BackupError::InterruptedRestore`] or the I/O failure.
    pub fn ensure_restore_complete(&self) -> Result<(), BackupError> {
        if self.data_dir.join(DATABASE).exists() {
            return Ok(());
        }
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
        connection: &mut SqliteConnection,
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
        sqlx::query("VACUUM INTO ?1")
            .bind(snapshot_path)
            .execute(&mut *connection)
            .await
            .map_err(BackupError::database("snapshot the database"))?;
        if read_key(&key_path)? != key {
            return Err(BackupError::KeyChanged);
        }
        let mut copy = connect(snapshot.path()).await?;
        let manifest = BackupManifest::read(&mut copy).await?;
        copy.close()
            .await
            .map_err(BackupError::database("close the database snapshot"))?;

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

/// Inputs gathered from the live data directory, written synchronously.
struct Archive {
    manifest: BackupManifest,
    snapshot: NamedTempFile,
    key: Option<Zeroizing<Vec<u8>>>,
    data_dir: PathBuf,
    output: PathBuf,
}

impl Archive {
    fn write(self) -> Result<(), BackupError> {
        let parent = parent_of(&self.output);
        let file =
            NamedTempFile::new_in(parent).map_err(BackupError::io("create archive in", parent))?;
        let mut builder = tar::Builder::new(BufWriter::new(file.as_file()));
        let manifest = serde_json::to_vec_pretty(&self.manifest).expect("manifest serializes");
        let mtime = u64::try_from(self.manifest.created_at_ms / 1000).unwrap_or_default();
        let mut entries = Entries {
            builder: &mut builder,
            mtime,
            output: &self.output,
        };

        entries.file(Path::new(MANIFEST), &manifest)?;
        let database = fs::read(self.snapshot.path()).map_err(BackupError::io(
            "read database snapshot",
            self.snapshot.path(),
        ))?;
        entries.file(Path::new(DATABASE), &database)?;
        if let Some(key) = &self.key {
            entries.file(Path::new(SECRET_KEY), key)?;
        }
        if INGRESS_STATE
            .iter()
            .any(|name| self.data_dir.join(name).exists())
        {
            entries.directory(Path::new(INGRESS))?;
            for name in INGRESS_STATE {
                entries.tree(&self.data_dir.join(name), Path::new(name))?;
            }
        }
        entries.tree(&self.data_dir.join(TAILNET), Path::new(TAILNET))?;

        let mut writer = builder
            .into_inner()
            .map_err(BackupError::io("finish archive", &self.output))?;
        writer
            .flush()
            .map_err(BackupError::io("write archive", &self.output))?;
        drop(writer);
        file.as_file()
            .sync_all()
            .map_err(BackupError::io("sync archive", &self.output))?;
        file.persist_noclobber(&self.output)
            .map_err(|error| BackupError::io("create archive", &self.output)(error.error))?;
        sync(parent)
    }
}

/// Appends entries with private modes and no host ownership.
struct Entries<'a, W: Write> {
    builder: &'a mut tar::Builder<W>,
    mtime: u64,
    output: &'a Path,
}

impl<W: Write> Entries<'_, W> {
    fn append(
        &mut self,
        kind: tar::EntryType,
        mode: u32,
        name: &Path,
        data: &[u8],
    ) -> Result<(), BackupError> {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_mode(mode);
        header.set_mtime(self.mtime);
        header.set_size(data.len() as u64);
        self.builder
            .append_data(&mut header, name, data)
            .map_err(BackupError::io("write archive", self.output))
    }

    fn file(&mut self, name: &Path, data: &[u8]) -> Result<(), BackupError> {
        self.append(tar::EntryType::Regular, 0o600, name, data)
    }

    fn directory(&mut self, name: &Path) -> Result<(), BackupError> {
        self.append(tar::EntryType::Directory, 0o700, name, &[])
    }

    /// Appends a directory tree in name order. Files are read whole, since the
    /// gateway or tailscaled may rewrite them while the archive is being
    /// written. Runtime sockets are skipped; their owners recreate them.
    fn tree(&mut self, source: &Path, name: &Path) -> Result<(), BackupError> {
        let metadata = match fs::symlink_metadata(source) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(BackupError::io("inspect", source)(error)),
        };
        if metadata.is_file() {
            let data = fs::read(source).map_err(BackupError::io("read", source))?;
            return self.file(name, &data);
        }
        if metadata.file_type().is_socket() {
            return Ok(());
        }
        if !metadata.is_dir() {
            tracing::warn!(path = %source.display(), "skipping special file in backup");
            return Ok(());
        }
        self.directory(name)?;
        let mut children = fs::read_dir(source)
            .and_then(|entries| {
                entries
                    .map(|entry| entry.map(|entry| entry.file_name()))
                    .collect::<Result<Vec<_>, _>>()
            })
            .map_err(BackupError::io("read directory", source))?;
        children.sort();
        for child in children {
            self.tree(&source.join(&child), &name.join(&child))?;
        }
        Ok(())
    }
}

/// Unpacks `archive` into `staging`, returning its checked manifest.
fn extract(archive: &Path, staging: &Path) -> Result<BackupManifest, BackupError> {
    let file = File::open(archive).map_err(BackupError::io("open archive", archive))?;
    let mut archive_reader = tar::Archive::new(BufReader::new(file));
    let mut entries = archive_reader
        .entries()
        .map_err(BackupError::io("read archive", archive))?;
    let first = entries
        .next()
        .ok_or(BackupError::Manifest(None))?
        .map_err(BackupError::io("read archive", archive))?;
    if first.path().ok().as_deref() != Some(Path::new(MANIFEST)) {
        return Err(BackupError::Manifest(None));
    }
    let manifest: BackupManifest =
        serde_json::from_reader(first).map_err(|error| BackupError::Manifest(Some(error)))?;
    manifest.check()?;

    for entry in entries {
        let mut entry = entry.map_err(BackupError::io("read archive", archive))?;
        let path = entry
            .path()
            .map_err(BackupError::io("read archive", archive))?
            .into_owned();
        let in_tree =
            INGRESS_STATE.iter().any(|state| path.starts_with(state)) || path.starts_with(TAILNET);
        let allowed = match entry.header().entry_type() {
            tar::EntryType::Regular => {
                in_tree || path == Path::new(DATABASE) || path == Path::new(SECRET_KEY)
            }
            tar::EntryType::Directory => in_tree || path == Path::new(INGRESS),
            _ => false,
        };
        if !allowed
            || !entry
                .unpack_in(staging)
                .map_err(BackupError::io("unpack archive into", staging))?
        {
            return Err(BackupError::UnexpectedEntry(path));
        }
    }
    if !staging.join(DATABASE).is_file() {
        return Err(BackupError::MissingDatabase);
    }
    Ok(manifest)
}

/// Moves sealed state from `staging` into `data_dir` in [`PUBLISHED`] order,
/// making everything else durable before the database marks the restore
/// complete. If a step fails, moved state is moved back so the restore can be
/// retried; if that fails too, `staging` is kept so the daemon refuses to start.
fn publish(staging: TempDir, data_dir: &Path) -> Result<(), BackupError> {
    seal(staging.path())?;
    // The staging directory is the interruption marker; persist it first.
    sync(data_dir)?;
    let mut moved = Vec::new();
    let result = PUBLISHED.iter().try_for_each(|&name| {
        let source = staging.path().join(name);
        if !source.exists() {
            return Ok(());
        }
        if name == DATABASE {
            sync(data_dir)?;
        }
        fs::rename(&source, data_dir.join(name))
            .map_err(BackupError::io("move restored state into", data_dir))?;
        moved.push(name);
        Ok(())
    });
    let Err(error) = result.and_then(|()| sync(data_dir)) else {
        return Ok(());
    };
    let mut undone = true;
    for name in moved {
        if let Err(undo) = fs::rename(data_dir.join(name), staging.path().join(name)) {
            tracing::error!(%undo, name, "could not undo a partial restore");
            undone = false;
        }
    }
    if undone {
        return Err(error);
    }
    Err(BackupError::PartialRestore {
        staging: staging.keep(),
        source: Box::new(error),
    })
}

/// Makes restored state private and durable before it is published. Modes are
/// fixed rather than taken from the archive, including for parents that tar
/// created implicitly, since the daemon rejects a non-private key or state
/// directory. Extraction admits only regular files and directories.
fn seal(path: &Path) -> Result<(), BackupError> {
    let metadata = fs::symlink_metadata(path).map_err(BackupError::io("inspect", path))?;
    let mode = if metadata.is_dir() {
        for child in fs::read_dir(path).map_err(BackupError::io("read directory", path))? {
            seal(
                &child
                    .map_err(BackupError::io("read directory", path))?
                    .path(),
            )?;
        }
        0o700
    } else {
        0o600
    };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(BackupError::io("set permissions of", path))?;
    sync(path)
}

/// Checks a restored database's integrity and that it matches its manifest.
async fn verify(path: &Path, manifest: &BackupManifest) -> Result<(), BackupError> {
    let mut connection = connect(path).await?;
    let integrity: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_all(&mut connection)
        .await
        .map_err(BackupError::database("check database integrity"))?;
    if integrity != ["ok"] {
        return Err(BackupError::Integrity(integrity.join("; ")));
    }
    let found = BackupManifest::read(&mut connection).await?;
    connection
        .close()
        .await
        .map_err(BackupError::database("close the database"))?;
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

/// Archive families that rotation deletes from. Only exact generated names
/// match, so archives named by hand in the same directory are never deleted.
#[derive(Clone, Copy)]
enum Rotation {
    /// `piqueld-<ms>.tar`, written by `piqueld backup --directory`.
    Scheduled,
    /// `pre-<schema>-<ms>.tar`, written before migrations.
    PreMigration,
}

impl Rotation {
    /// Creation time encoded in `name`, if it belongs to this family.
    fn created_at(self, name: &str) -> Option<i64> {
        let stem = name.strip_suffix(".tar")?;
        let created = match self {
            Self::Scheduled => stem.strip_prefix("piqueld-")?,
            Self::PreMigration => {
                let (schema, created) = stem.strip_prefix("pre-")?.split_once('-')?;
                decimal(schema)?;
                created
            }
        };
        decimal(created)
    }

    /// Deletes all but the newest `keep` archives of this family in `directory`.
    fn prune(self, directory: &Path, keep: usize) -> Result<(), BackupError> {
        let mut archives = Vec::new();
        for entry in
            fs::read_dir(directory).map_err(BackupError::io("read directory", directory))?
        {
            let name = entry
                .map_err(BackupError::io("read directory", directory))?
                .file_name();
            if let Some(created) = name.to_str().and_then(|name| self.created_at(name)) {
                archives.push((created, name));
            }
        }
        archives.sort_unstable_by(|left, right| right.cmp(left));
        for (_, name) in archives.into_iter().skip(keep) {
            let path = directory.join(name);
            fs::remove_file(&path).map_err(BackupError::io("delete old backup", &path))?;
        }
        Ok(())
    }
}

/// Parses an unsigned decimal with no sign or other characters.
fn decimal(text: &str) -> Option<i64> {
    if text.bytes().all(|byte| byte.is_ascii_digit()) {
        text.parse().ok()
    } else {
        None
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
pub(super) fn parent_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    fn archive_with(manifest: &BackupManifest, extra: &[(&str, &[u8])]) -> NamedTempFile {
        let file = NamedTempFile::new().unwrap();
        let mut builder = tar::Builder::new(file.as_file());
        let mut entries = Entries {
            builder: &mut builder,
            mtime: 0,
            output: file.path(),
        };
        entries
            .file(Path::new(MANIFEST), &serde_json::to_vec(manifest).unwrap())
            .unwrap();
        for (name, data) in extra {
            entries.file(Path::new(name), data).unwrap();
        }
        builder.into_inner().unwrap();
        file
    }

    fn manifest(schema_version: u64) -> BackupManifest {
        BackupManifest {
            format: ARCHIVE_FORMAT,
            schema_version,
            daemon_version: "0.0.0".into(),
            instance_id: "instance-test".into(),
            created_at_ms: 1,
        }
    }

    #[tokio::test]
    async fn backup_restores_database_key_ingress_and_tailnet_state() {
        let source = tempfile::tempdir().unwrap();
        let store = Store::open(source.path().join(DATABASE)).await.unwrap();
        fs::write(source.path().join(SECRET_KEY), [7; 32]).unwrap();
        fs::create_dir_all(source.path().join("ingress/data/caddy")).unwrap();
        fs::write(source.path().join("ingress/data/caddy/cert.pem"), b"cert").unwrap();
        fs::create_dir_all(source.path().join("ingress/control")).unwrap();
        fs::write(source.path().join("ingress/control/runtime"), b"skip").unwrap();
        fs::create_dir_all(source.path().join(TAILNET)).unwrap();
        fs::write(source.path().join("tailscale/tailscaled.state"), b"node").unwrap();
        let _socket =
            std::os::unix::net::UnixListener::bind(source.path().join("tailscale/tailscaled.sock"))
                .unwrap();

        let output = source.path().join("backup.tar");
        let written = Backups::new(source.path()).create(&output).await.unwrap();
        assert_eq!(written.schema_version, SCHEMA_VERSION);
        assert_eq!(
            store.last_backup_at_ms().await.unwrap(),
            Some(written.created_at_ms)
        );
        assert!(matches!(
            Backups::new(source.path()).create(&output).await,
            Err(BackupError::Io { .. })
        ));

        let target = tempfile::tempdir().unwrap();
        let data_dir = target.path().join("restored");
        let restored = Backups::new(&data_dir).restore(&output).await.unwrap();
        assert_eq!(restored, written);
        assert_eq!(fs::read(data_dir.join(SECRET_KEY)).unwrap(), [7; 32]);
        assert_eq!(
            fs::read(data_dir.join("ingress/data/caddy/cert.pem")).unwrap(),
            b"cert"
        );
        assert!(!data_dir.join("ingress/control").exists());
        assert_eq!(
            fs::read(data_dir.join("tailscale/tailscaled.state")).unwrap(),
            b"node"
        );
        assert!(!data_dir.join("tailscale/tailscaled.sock").exists());
        let reopened = Store::open(data_dir.join(DATABASE)).await.unwrap();
        assert_eq!(reopened.instance_id(), store.instance_id());

        assert!(matches!(
            Backups::new(&data_dir).restore(&output).await,
            Err(BackupError::NotEmpty(_))
        ));
    }

    #[tokio::test]
    async fn migration_writes_a_restorable_backup_of_the_previous_schema() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(DATABASE);
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .unwrap();
        let previous = super::super::MIGRATIONS.len() - 1;
        for (index, migration) in super::super::MIGRATIONS.iter().take(previous).enumerate() {
            Store::apply_migration(&pool, index + 1, migration)
                .await
                .unwrap();
        }
        pool.close().await;

        Store::open(&path).await.unwrap();
        let archives: Vec<_> = fs::read_dir(directory.path().join("backups"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(archives.len(), 1);
        let restored = Backups::new(&directory.path().join("rollback"))
            .restore(&archives[0])
            .await
            .unwrap();
        assert_eq!(restored.schema_version, SCHEMA_VERSION - 1);
    }

    #[tokio::test]
    async fn restore_rejects_newer_schemas_and_unexpected_entries() {
        let newer = archive_with(&manifest(SCHEMA_VERSION + 1), &[(DATABASE, b"")]);
        let unexpected = archive_with(&manifest(1), &[("authorized_keys", b"")]);
        for (archive, expected) in [(newer, "newer"), (unexpected, "unexpected")] {
            let target = tempfile::tempdir().unwrap();
            let data_dir = target.path().join("data");
            let error = Backups::new(&data_dir)
                .restore(archive.path())
                .await
                .unwrap_err();
            match (expected, error) {
                ("newer", BackupError::NewerSchema { .. })
                | ("unexpected", BackupError::UnexpectedEntry(_)) => {}
                (_, error) => panic!("unexpected {expected} error: {error}"),
            }
            assert_eq!(fs::read_dir(&data_dir).unwrap().count(), 0);
        }
    }

    #[test]
    fn prune_keeps_the_newest_archives_with_exact_generated_names() {
        let directory = tempfile::tempdir().unwrap();
        for name in [
            "pre-9-100.tar",
            "pre-10-300.tar",
            "pre-10-200.tar",
            "pre-10-50.tar",
            "pre-manual-1.tar",
            "piqueld-1.tar",
        ] {
            fs::write(directory.path().join(name), b"").unwrap();
        }
        Rotation::PreMigration.prune(directory.path(), 2).unwrap();
        let mut remaining: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        remaining.sort();
        assert_eq!(
            remaining,
            [
                "piqueld-1.tar",
                "pre-10-200.tar",
                "pre-10-300.tar",
                "pre-manual-1.tar"
            ]
        );
        assert_eq!(Rotation::Scheduled.created_at("piqueld-5.tar"), Some(5));
        assert_eq!(
            Rotation::Scheduled.created_at("piqueld-before-upgrade-5.tar"),
            None
        );
    }

    #[test]
    fn seal_makes_implicit_parents_and_files_private() {
        let staging = tempfile::tempdir().unwrap();
        let parent = staging.path().join("ingress/data/caddy");
        fs::create_dir_all(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(parent.join("cert.pem"), b"cert").unwrap();
        fs::set_permissions(parent.join("cert.pem"), fs::Permissions::from_mode(0o644)).unwrap();
        seal(staging.path()).unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&parent), 0o700);
        assert_eq!(mode(&parent.join("cert.pem")), 0o600);
    }

    #[test]
    fn interrupted_restores_are_refused_until_the_database_is_in_place() {
        let data_dir = tempfile::tempdir().unwrap();
        let backups = Backups::new(data_dir.path());
        backups.ensure_restore_complete().unwrap();
        fs::create_dir(data_dir.path().join(".restore-abc")).unwrap();
        assert!(matches!(
            backups.ensure_restore_complete(),
            Err(BackupError::InterruptedRestore(_))
        ));
        fs::write(data_dir.path().join(DATABASE), b"").unwrap();
        backups.ensure_restore_complete().unwrap();
    }
}
