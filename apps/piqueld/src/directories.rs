//! Shared validation and process exclusivity for daemon directories.

use std::{
    fs::File,
    io,
    os::unix::fs::{MetadataExt, PermissionsExt},
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

#[derive(Clone, Copy)]
enum DirectoryKind {
    Data,
    Runtime,
}

impl DirectoryKind {
    fn name(self) -> &'static str {
        match self {
            Self::Data => "data",
            Self::Runtime => "runtime",
        }
    }

    async fn prepare(self, path: &Path) -> io::Result<()> {
        let name = self.name();
        if path.as_os_str().is_empty() || path == Path::new("/") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("the {name} directory must be a dedicated directory"),
            ));
        }

        let mut current = PathBuf::new();
        let mut components = path.components().peekable();
        let expected_uid = rustix::process::geteuid().as_raw();
        while let Some(component) = components.next() {
            match component {
                std::path::Component::Prefix(prefix) => current.push(prefix.as_os_str()),
                std::path::Component::RootDir => current.push(Path::new("/")),
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
                std::path::Component::Normal(name) => current.push(name),
            }

            let is_final = components.peek().is_none();
            match tokio::fs::symlink_metadata(&current).await {
                Ok(metadata) if metadata.is_dir() && !metadata.is_symlink() => {}
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "{name} directory component {} is not a real directory",
                            current.display()
                        ),
                    ));
                }
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound && matches!(self, Self::Data) =>
                {
                    tokio::fs::DirBuilder::new()
                        .mode(0o700)
                        .create(&current)
                        .await?;
                    let metadata = tokio::fs::symlink_metadata(&current).await?;
                    if !metadata.is_dir() || metadata.is_symlink() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "{name} directory component {} was replaced by a non-directory",
                                current.display()
                            ),
                        ));
                    }
                }
                Err(error) => return Err(error),
            }

            let metadata = tokio::fs::symlink_metadata(&current).await?;
            if is_final {
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
        }
        Ok(())
    }

    fn validate_final(
        self,
        path: &Path,
        metadata: &std::fs::Metadata,
        expected_uid: u32,
    ) -> io::Result<()> {
        let name = self.name();
        let mode = metadata.permissions().mode();
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

fn protected_ancestor(mode: u32, owner_uid: u32, daemon_uid: u32) -> bool {
    let trusted_owner = owner_uid == 0 || owner_uid == daemon_uid;
    let sticky = mode & 0o1000 != 0;
    trusted_owner && (sticky || mode & 0o022 == 0)
}

#[cfg(test)]
mod tests {
    use super::protected_ancestor;

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

/// Holds a directory's exclusive lock until the daemon exits.
///
/// Locking the directory inode avoids a removable lock file and also excludes
/// processes accessing the same directory through another path. The OS releases
/// the lock when the last handle closes, including after a crash.
#[derive(Debug)]
pub struct DirectoryLock {
    _directory: File,
}

impl DirectoryLock {
    /// Locks a previously validated state or runtime directory.
    ///
    /// # Errors
    /// Returns `WouldBlock` if another daemon owns the directory, or the source
    /// I/O error if opening or locking it fails.
    pub fn acquire(path: &Path) -> io::Result<Self> {
        let directory = File::open(path)?;
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
