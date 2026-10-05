//! The tar layout: writing archives and unpacking them into a staging directory.

use super::{
    BackupError, BackupManifest, DATABASE, INGRESS, INGRESS_STATE, MANIFEST, SECRET_KEY, TAILNET,
    parent_of, sync,
};
use std::{
    fs::{self, File},
    io::{self, BufReader, BufWriter, Write},
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;
use zeroize::Zeroizing;

/// Inputs gathered from the live data directory, written synchronously.
pub(super) struct Archive {
    pub(super) manifest: BackupManifest,
    pub(super) snapshot: NamedTempFile,
    pub(super) key: Option<Zeroizing<Vec<u8>>>,
    pub(super) data_dir: PathBuf,
    pub(super) output: PathBuf,
}

impl Archive {
    pub(super) fn write(self) -> Result<(), BackupError> {
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
pub(super) struct Entries<'a, W: Write> {
    pub(super) builder: &'a mut tar::Builder<W>,
    pub(super) mtime: u64,
    pub(super) output: &'a Path,
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

    pub(super) fn file(&mut self, name: &Path, data: &[u8]) -> Result<(), BackupError> {
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
pub(super) fn extract(archive: &Path, staging: &Path) -> Result<BackupManifest, BackupError> {
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
