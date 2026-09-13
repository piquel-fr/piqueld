//! Validation and process exclusivity for the daemon's state directory.

use std::{
    fs::File,
    io,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

/// Prepares the daemon's single state directory.
///
/// The directory holds the Unix API socket, the embedded database, and future
/// user data. Missing components are created with mode `0700`. Existing
/// components are never modified but must be real (non-symlink) directories,
/// every ancestor must be protected from replacement by untrusted owners, and
/// the final directory must grant no access to group or other users: anyone
/// able to write there could replace the daemon socket and intercept operator
/// connections.
///
/// # Errors
///
/// Returns an [`std::io::Error`] when the directory cannot be inspected or
/// prepared, contains a leading current-directory or any parent component,
/// or violates the privacy requirements.
pub async fn prepare_data_dir(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() || path == Path::new("/") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the data directory must be a dedicated private directory",
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
                    "the data directory cannot contain a leading current-directory component",
                ));
            }
            std::path::Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the data directory cannot contain a parent component",
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
                        "data directory component {} is not a real directory",
                        current.display()
                    ),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                tokio::fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&current)
                    .await?;
                let metadata = tokio::fs::symlink_metadata(&current).await?;
                if !metadata.is_dir() || metadata.is_symlink() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "data directory component {} was replaced by a non-directory",
                            current.display()
                        ),
                    ));
                }
            }
            Err(error) => return Err(error),
        }

        let metadata = tokio::fs::symlink_metadata(&current).await?;
        if is_final {
            let mode = metadata.permissions().mode();
            if mode & 0o077 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "data directory {} must be private (mode {:o} grants group or other access)",
                        current.display(),
                        mode & 0o777
                    ),
                ));
            }
            if metadata.uid() != expected_uid {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "data directory {} must be owned by uid {expected_uid}",
                        current.display()
                    ),
                ));
            }
        } else {
            let mode = metadata.permissions().mode();
            if !protected_ancestor(mode, metadata.uid(), expected_uid) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "data directory ancestor {} is not protected from replacement by other users",
                        current.display()
                    ),
                ));
            }
        }
    }
    Ok(())
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

/// Holds the data directory's exclusive lock until the daemon exits.
///
/// Locking the directory inode avoids a removable lock file and also excludes
/// processes accessing the same directory through another path. The OS releases
/// the lock when the last handle closes, including after a crash.
#[derive(Debug)]
pub struct DataDirLock {
    _directory: File,
}

impl DataDirLock {
    /// Locks a directory previously checked by [`crate::prepare_data_dir`].
    ///
    /// # Errors
    /// Returns `WouldBlock` if another daemon owns the directory, or the source
    /// I/O error if opening or locking it fails.
    pub fn acquire(path: &Path) -> io::Result<Self> {
        let directory = File::open(path)?;
        directory.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => {
                io::Error::new(io::ErrorKind::WouldBlock, "data directory already in use")
            }
            std::fs::TryLockError::Error(source) => source,
        })?;
        Ok(Self {
            _directory: directory,
        })
    }
}
