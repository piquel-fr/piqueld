//! Process exclusivity for the daemon's already-prepared state directory.

use std::{fs::File, io, path::Path};

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
