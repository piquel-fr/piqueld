//! Moving restored state from its staging directory into the data directory.

use super::{BackupError, PUBLISHED, parent_of, sync};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};
use tempfile::TempDir;

/// Moves sealed state from `staging` into `data_dir`. The staging directory
/// marks the restore unfinished ([`super::Backups::ensure_restore_complete`]):
/// it is made durable before anything moves and removed only once everything
/// else, including `data_dir`'s own entry, is durable. If a step fails, moved
/// state is moved back so the restore can be retried; if that fails too, the
/// marker is kept.
pub(super) fn publish(staging: TempDir, data_dir: &Path) -> Result<(), BackupError> {
    seal(staging.path())?;
    sync(data_dir)?;
    let mut moved = Vec::new();
    let result = PUBLISHED
        .iter()
        .try_for_each(|&name| {
            let source = staging.path().join(name);
            if source.exists() {
                fs::rename(&source, data_dir.join(name))
                    .map_err(BackupError::io("move restored state into", data_dir))?;
                moved.push(name);
            }
            Ok(())
        })
        .and_then(|()| sync(data_dir))
        .and_then(|()| sync(parent_of(data_dir)));
    let Err(error) = result else {
        let marker = staging.path().to_path_buf();
        staging
            .close()
            .map_err(BackupError::io("remove restore staging directory", &marker))?;
        return sync(data_dir);
    };
    let mut undone = true;
    for name in moved.into_iter().rev() {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
