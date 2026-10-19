//! Shared validation and process exclusivity for daemon directories, and
//! writing and removal of the files the daemon manages in them.

use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io,
    os::{
        fd::{AsFd, BorrowedFd},
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

/// Prepares private state, creating missing components with mode `0700`.
/// Existing permissions are never changed.
///
/// # Errors
/// Returns an error for unsafe ownership, permissions, symlinks, or I/O failures.
pub async fn prepare_data_dir(path: &Path) -> io::Result<()> {
    DirectoryKind::Data.prepare(path).await
}

/// Validates an existing runtime directory without creating or modifying it.
///
/// # Errors
/// Returns an error for missing directories, unsafe ownership, permissions,
/// symlinks, or I/O failures.
pub(crate) async fn validate_runtime_dir(path: &Path) -> io::Result<()> {
    DirectoryKind::Runtime.prepare(path).await
}

/// Removes the file at `path`; one that does not exist counts as removed.
///
/// # Errors
/// Returns any other I/O error.
pub(crate) async fn remove_file(path: &Path) -> io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// A file only the daemon may read, replaced atomically and durably. Its
/// directory may be writable by a container running as the daemon's user (the
/// gateway's Caddy), so no operation follows a symlink planted there.
pub(crate) struct PrivateFile(PathBuf);

impl PrivateFile {
    /// The private file at `path`.
    pub(crate) fn at(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    /// Replaces the file with `bytes`, through a temporary file that `O_EXCL`
    /// creates with mode `0600` and an unpredictable name, so never through a
    /// symlink. The temporary is synced and renamed over the file, then the
    /// directory is synced. Temporaries a crash left behind are removed
    /// first, so no secret outlives its file.
    ///
    /// # Errors
    /// Returns the I/O error, with the file's path as context.
    pub(crate) async fn write(self, bytes: impl Into<Vec<u8>>) -> anyhow::Result<()> {
        let bytes = bytes.into();
        self.run("write", move |file| {
            use std::io::Write;
            file.remove_temporaries()?;
            let mut temporary = tempfile::Builder::new()
                .prefix(&file.temporary_prefix())
                .tempfile_in(file.directory())?;
            temporary.write_all(&bytes)?;
            temporary.as_file().sync_all()?;
            temporary.persist(&file.0)?;
            file.sync_directory()
        })
        .await
    }

    /// Removes the file and any temporaries left beside it, durably. A missing
    /// file counts as removed.
    ///
    /// # Errors
    /// Returns the I/O error, with the file's path as context.
    pub(crate) async fn remove(self) -> anyhow::Result<()> {
        self.run("remove", |file| {
            let removed =
                file.remove_temporaries()? | Self::removed(std::fs::remove_file(&file.0))?;
            if removed {
                file.sync_directory()?;
            }
            Ok(())
        })
        .await
    }

    /// Runs the blocking `operation` off the async runtime, naming the file
    /// and `action` in its error.
    async fn run(
        self,
        action: &'static str,
        operation: impl FnOnce(&Self) -> io::Result<()> + Send + 'static,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        tokio::task::spawn_blocking(move || {
            operation(&self).with_context(|| format!("{action} {}", self.0.display()))
        })
        .await?
    }

    /// Removes temporaries of this file left by an interrupted write,
    /// including the fixed `.tmp` name earlier versions used. Returns whether
    /// any existed.
    fn remove_temporaries(&self) -> io::Result<bool> {
        let entries = match std::fs::read_dir(self.directory()) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            entries => entries?,
        };
        let prefix = self.temporary_prefix();
        let mut removed = Self::removed(std::fs::remove_file(self.0.with_extension("tmp")))?;
        for entry in entries {
            let entry = entry?;
            if entry
                .file_name()
                .as_encoded_bytes()
                .starts_with(prefix.as_encoded_bytes())
            {
                removed |= Self::removed(std::fs::remove_file(entry.path()))?;
            }
        }
        Ok(removed)
    }

    /// Whether a removal deleted something; a missing file was not.
    fn removed(result: io::Result<()>) -> io::Result<bool> {
        match result {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Start of every temporary file name: `.<name>.tmp-`.
    fn temporary_prefix(&self) -> OsString {
        let mut prefix = OsString::from(".");
        prefix.push(self.0.file_name().unwrap_or_default());
        prefix.push(".tmp-");
        prefix
    }

    /// The directory holding the file.
    fn directory(&self) -> &Path {
        self.0.parent().unwrap_or_else(|| Path::new("."))
    }

    /// Makes renames and removals in the file's directory durable.
    fn sync_directory(&self) -> io::Result<()> {
        File::open(self.directory())?.sync_all()
    }
}

/// Which daemon directory is being checked; each has its own creation and
/// permission policy.
#[derive(Clone, Copy)]
enum DirectoryKind {
    /// Private persistent state; missing components are created.
    Data,
    /// Service-manager-provided socket directory; must already exist and may
    /// grant group access to the daemon's group.
    Runtime,
}

impl DirectoryKind {
    /// Human-readable label used in error messages.
    fn name(self) -> &'static str {
        match self {
            Self::Data => "data",
            Self::Runtime => "runtime",
        }
    }

    /// Walks `path` one component at a time from the root, so no ancestor can be
    /// swapped for a symlink or attacker-controlled directory. Each component
    /// is opened below its parent's handle and checked through its own, so
    /// every check describes the directory the walk continues from.
    ///
    /// 1. Rejects empty and `/` paths, a leading `.`, and any `..` component.
    /// 2. Requires each component to be a real directory (never a symlink),
    ///    creating missing ones with mode `0700` for [`Self::Data`] only.
    /// 3. Requires each ancestor to be owned by root or the daemon and not
    ///    writable by others unless sticky.
    /// 4. Checks the final directory with `validate_final`.
    async fn prepare(self, path: &Path) -> io::Result<()> {
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || self.walk(&path)).await?
    }

    /// Blocking implementation of [`Self::prepare`].
    fn walk(self, path: &Path) -> io::Result<()> {
        let name = self.name();
        if path.as_os_str().is_empty() || path == Path::new("/") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("the {name} directory must be a dedicated directory"),
            ));
        }

        let mut current = PathBuf::new();
        // Relative paths start from the working directory.
        let mut parent: Option<File> = None;
        let mut components = path.components().peekable();
        let expected_uid = rustix::process::geteuid().as_raw();
        while let Some(component) = components.next() {
            match component {
                std::path::Component::Prefix(_)
                | std::path::Component::RootDir
                | std::path::Component::Normal(_) => current.push(component),
                std::path::Component::CurDir => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "the {name} directory cannot contain a leading current-directory component"
                        ),
                    ));
                }
                std::path::Component::ParentDir => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("the {name} directory cannot contain a parent component"),
                    ));
                }
            }

            let at = parent.as_ref().map_or(rustix::fs::CWD, AsFd::as_fd);
            let directory = self.open(at, component.as_os_str(), &current)?;
            let metadata = directory.metadata()?;
            if components.peek().is_none() {
                self.validate_final(&current, &metadata, expected_uid)?;
            } else {
                let mode = metadata.permissions().mode();
                if !protected_ancestor(mode, metadata.uid(), expected_uid) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "{name} directory ancestor {} is not protected from replacement by other users",
                            current.display()
                        ),
                    ));
                }
            }
            parent = Some(directory);
        }
        Ok(())
    }

    /// Opens the directory `component` below `parent` as a path-only handle,
    /// never through a symlink, so even directories the daemon may only
    /// traverse can be checked. A missing [`Self::Data`] component is created
    /// with mode `0700`. `current` names it in errors.
    fn open(self, parent: BorrowedFd<'_>, component: &OsStr, current: &Path) -> io::Result<File> {
        use rustix::{
            fs::{Mode, OFlags},
            io::Errno,
        };
        let open = || {
            rustix::fs::openat(
                parent,
                component,
                OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
        };
        let opened = match open() {
            Err(Errno::NOENT) if matches!(self, Self::Data) => {
                rustix::fs::mkdirat(parent, component, Mode::RWXU)?;
                open()
            }
            opened => opened,
        };
        match opened {
            Ok(directory) => Ok(directory.into()),
            Err(Errno::LOOP | Errno::NOTDIR) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} directory component {} is not a real directory",
                    self.name(),
                    current.display()
                ),
            )),
            Err(error) => Err(error.into()),
        }
    }

    /// Checks the target directory itself: owned by the effective UID with full
    /// owner access. Data directories must grant nothing to group or other; runtime
    /// directories may grant group read/execute only when owned by the daemon's
    /// effective group.
    fn validate_final(
        self,
        path: &Path,
        metadata: &std::fs::Metadata,
        expected_uid: u32,
    ) -> io::Result<()> {
        let name = self.name();
        let mode = metadata.permissions().mode();
        if mode & 0o700 != 0o700 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{name} directory {} must grant owner read, write, and execute access",
                    path.display()
                ),
            ));
        }
        if metadata.uid() != expected_uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{name} directory {} must be owned by uid {expected_uid}",
                    path.display()
                ),
            ));
        }
        match self {
            Self::Data if mode & 0o077 != 0 => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "data directory {} must be private (mode {:o} grants group or other access)",
                        path.display(),
                        mode & 0o777
                    ),
                ));
            }
            Self::Runtime => {
                if mode & 0o027 != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "runtime directory {} must grant no group write or other access (mode {:o})",
                            path.display(),
                            mode & 0o777
                        ),
                    ));
                }
                let daemon_group = rustix::process::getegid().as_raw();
                if mode & 0o050 != 0 && metadata.gid() != daemon_group {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "runtime directory {} must be owned by gid {daemon_group} when granting group access",
                            path.display()
                        ),
                    ));
                }
            }
            Self::Data => {}
        }
        Ok(())
    }
}

/// Whether other users are unable to rename or replace entries in an ancestor.
///
/// The owner must be root or the daemon, and the directory must either deny
/// group/other write or be sticky (like `/tmp`).
fn protected_ancestor(mode: u32, owner_uid: u32, daemon_uid: u32) -> bool {
    let trusted_owner = owner_uid == 0 || owner_uid == daemon_uid;
    let sticky = mode & 0o1000 != 0;
    trusted_owner && (sticky || mode & 0o022 == 0)
}

/// Holds a directory's exclusive lock until the daemon exits.
///
/// Locking the directory inode avoids a removable lock file and also excludes
/// processes accessing the same directory through another path. The OS releases
/// the lock when the last handle closes, including after a crash.
#[derive(Debug)]
pub struct DirectoryLock {
    /// Open handle carrying the lock; dropping it releases the lock.
    _directory: File,
}

impl DirectoryLock {
    /// Locks a previously validated state or runtime directory, refusing a
    /// symlink swapped in since.
    ///
    /// # Errors
    /// Returns `WouldBlock` if another daemon owns the directory, or the source
    /// I/O error if opening or locking it fails.
    pub fn acquire(path: &Path) -> io::Result<Self> {
        use rustix::fs::{Mode, OFlags};
        let directory = File::from(rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        directory.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => {
                io::Error::new(io::ErrorKind::WouldBlock, "directory already in use")
            }
            std::fs::TryLockError::Error(source) => source,
        })?;
        Ok(Self {
            _directory: directory,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{DirectoryKind, PrivateFile, protected_ancestor};
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn private_writes_never_follow_a_planted_temporary() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("autosave.json");
        let victim = directory.path().join("piqueld.db");
        std::fs::write(&victim, "database").unwrap();
        // The fixed temporary name earlier versions wrote through.
        std::os::unix::fs::symlink(&victim, path.with_extension("tmp")).unwrap();

        PrivateFile::at(&path).write("secret").await.unwrap();

        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "database");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "secret");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn removing_a_private_file_removes_temporaries_a_crash_left() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credentials.json");
        let leftovers = [
            directory.path().join(".credentials.json.tmp-a1B2c3"),
            path.with_extension("tmp"),
        ];
        for file in leftovers
            .iter()
            .chain([&path, &directory.path().join("config.json")])
        {
            std::fs::write(file, "secret").unwrap();
        }

        PrivateFile::at(&path).remove().await.unwrap();

        let remaining: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(remaining, ["config.json"]);
    }

    #[tokio::test]
    async fn unusable_owner_permissions_are_rejected_without_modification() {
        let cwd = std::env::current_dir().unwrap();
        for kind in [DirectoryKind::Data, DirectoryKind::Runtime] {
            for mode in [0o000, 0o300, 0o500, 0o600] {
                let directory = tempfile::tempdir_in(&cwd).unwrap();
                let path = directory.path();
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
                // Validate only the owned fixture tree, including in Nix's user namespace.
                let result = kind.prepare(path.strip_prefix(&cwd).unwrap()).await;
                let actual_mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
                // Restore access before assertions so the fixture can always be cleaned up.
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();

                let error = result.unwrap_err();
                assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
                assert!(
                    error
                        .to_string()
                        .contains("must grant owner read, write, and execute access")
                );
                assert_eq!(actual_mode, mode);
            }
        }
    }

    #[test]
    fn sticky_ancestors_are_safe_only_when_owned_by_a_trusted_user() {
        let daemon_uid = 1_000;

        assert!(protected_ancestor(0o41_777, 0, daemon_uid));
        assert!(protected_ancestor(0o41_700, daemon_uid, daemon_uid));
        assert!(!protected_ancestor(0o41_777, 2_000, daemon_uid));
        assert!(!protected_ancestor(0o40_777, 0, daemon_uid));
        assert!(!protected_ancestor(0o40_555, 2_000, daemon_uid));
    }
}
