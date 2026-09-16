//! Runtime-directory ownership and Unix API socket lifecycle.

use anyhow::{Context, Result, bail};
use std::{
    io,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::net::{UnixListener, UnixStream};

use crate::directories::{DirectoryLock, validate_runtime_dir};

pub(crate) const SOCKET_NAME: &str = "piqueld.sock";

/// A validated runtime directory exclusively held for a daemon's lifetime.
/// Keep this guard alive while serving the Unix API.
/// The directory is dedicated to piqueld; its advisory lock coordinates
/// cooperating daemons, not other processes running as the same user or root.
#[derive(Debug)]
pub struct RuntimeDir {
    path: PathBuf,
    _lock: DirectoryLock,
}

impl RuntimeDir {
    /// Validates and locks a runtime directory prepared by the service manager.
    ///
    /// # Errors
    /// Rejects missing or unsafe directories and competing daemon instances.
    pub async fn acquire(path: &Path) -> Result<Self> {
        validate_runtime_dir(path).await.with_context(|| {
            format!(
                "invalid runtime directory {}; prepare it before starting the daemon",
                path.display()
            )
        })?;
        let lock = DirectoryLock::acquire(path)
            .with_context(|| format!("failed to lock runtime directory {}", path.display()))?;
        Ok(Self {
            path: path.to_owned(),
            _lock: lock,
        })
    }

    /// Binds the API socket with access for the daemon's effective group.
    ///
    /// # Errors
    /// Refuses observed live listeners and non-sockets. A refused connection
    /// permits stale-socket recovery under the directory lock; other probe
    /// errors leave the path untouched.
    pub async fn bind_api(&self) -> Result<UnixListener> {
        let path = self.path.join(SOCKET_NAME);
        Self::bind_at(&path)
            .await
            .with_context(|| format!("failed to bind Unix API socket {}", path.display()))
    }

    async fn bind_at(path: &Path) -> Result<UnixListener> {
        match tokio::fs::symlink_metadata(path).await {
            Ok(metadata) if metadata.file_type().is_socket() => {
                match tokio::time::timeout(Duration::from_secs(1), UnixStream::connect(path)).await
                {
                    Ok(Err(error)) if error.kind() == io::ErrorKind::ConnectionRefused => {
                        // Writers must honor the runtime-directory lock. A separate
                        // inode check before unlink would still race with an
                        // uncooperative process sharing the daemon's identity.
                        tokio::fs::remove_file(path)
                            .await
                            .context("failed to remove stale socket")?;
                    }
                    Ok(Ok(_)) => bail!("Unix API socket already has an active listener"),
                    Ok(Err(error)) => {
                        return Err(error)
                            .context("could not establish whether existing socket is stale");
                    }
                    Err(error) => {
                        return Err(error)
                            .context("timed out checking existing socket; leaving it untouched");
                    }
                }
            }
            Ok(_) => bail!("refusing to replace non-socket Unix API path"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("failed to inspect Unix API path"),
        }
        // Bind synchronously while the umask is restrictive, then restore it
        // before yielding. Other threads can only receive stricter permissions.
        let previous_umask =
            rustix::process::umask(rustix::fs::Mode::RWXG | rustix::fs::Mode::RWXO);
        let bound = UnixListener::bind(path);
        rustix::process::umask(previous_umask);
        let listener = bound.context("failed to create Unix listener")?;
        // A setgid directory can otherwise assign a different group, including
        // in a private development directory where its group is unrestricted.
        rustix::fs::chown(path, None, Some(rustix::process::getegid()))
            .context("failed to set Unix socket group")?;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
            .await
            .context("failed to set Unix socket permissions")?;
        tracing::info!(socket = %path.display(), "Unix API socket bound");
        Ok(listener)
    }
}
