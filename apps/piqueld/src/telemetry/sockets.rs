//! The private directories telemetry receivers listen in, one per receiving
//! service of a granted environment:
//! `<data_dir>/telemetry/<environment-id>/<service>/`. The controller
//! bind-mounts a service's at [`piqueld_core::resource::TELEMETRY_TARGET`];
//! the service runs as the daemon's user and creates [`SOCKET`] there, and
//! the daemon exports to it. A renamed receiver gets a directory of its own,
//! so it listens while the old one still runs.
use piqueld_core::resource::TelemetryMount;
use piqueld_core::{EnvironmentId, ResolvedApplication, ServiceName};
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};

/// The socket a receiver's collector listens on, inside its directory.
pub const SOCKET: &str = "otlp.sock";

/// Where every receiver's directory lives.
#[derive(Clone, Debug)]
pub struct Sockets {
    root: PathBuf,
}

impl Sockets {
    /// The receivers' directories under the daemon's private `data_dir`.
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join("telemetry"),
        }
    }

    /// Environment `id`'s directory, holding its receivers'.
    fn environment(&self, id: &EnvironmentId) -> PathBuf {
        self.root.join(id.as_str())
    }

    /// The directory environment `id`'s receiving `service` listens in.
    fn directory(&self, id: &EnvironmentId, service: &ServiceName) -> PathBuf {
        self.environment(id).join(service.as_str())
    }

    /// The socket environment `id`'s receiving `service` listens on.
    #[must_use]
    pub fn socket(&self, id: &EnvironmentId, service: &ServiceName) -> PathBuf {
        self.directory(id, service).join(SOCKET)
    }

    /// `target` with its receiving service's directory mounted in it while
    /// its environment is `granted` telemetry, and in no service otherwise.
    #[must_use]
    pub fn project(&self, target: ResolvedApplication, granted: bool) -> ResolvedApplication {
        let mount = target
            .telemetry_receiver()
            .filter(|_| granted)
            .map(|receiver| TelemetryMount {
                source: self
                    .directory(&target.id, &receiver.logical_name)
                    .display()
                    .to_string(),
                user: format!(
                    "{}:{}",
                    rustix::process::geteuid().as_raw(),
                    rustix::process::getegid().as_raw()
                ),
            });
        target.with_telemetry(mount.as_ref())
    }

    /// Creates `mount`'s directory with mode `0700`, if it is one of these
    /// and missing, as a service mounting it is created or updated. Even a
    /// mount planned before a revocation, landing after cleanup removed the
    /// directory, must start: a failed update pauses until it changes again.
    /// Failure is logged: the service then fails to start on the missing
    /// mount source, which its tasks report.
    pub fn prepare(&self, mount: &TelemetryMount) {
        let directory = Path::new(&mount.source);
        if directory.parent().and_then(Path::parent) != Some(self.root.as_path()) {
            return;
        }
        if let Err(error) = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
        {
            tracing::error!(?error, directory = %directory.display(), "telemetry socket directory could not be created");
        }
    }

    /// Whether environment `id` has a directory. Only the controller creates
    /// and removes them, so a missing one needs no cleanup.
    #[must_use]
    pub fn exists(&self, id: &EnvironmentId) -> bool {
        self.environment(id).exists()
    }

    /// Whether environment `id` has a directory other than `receiver`'s,
    /// its receiving service while granted, to remove once unmounted.
    #[must_use]
    pub fn stale(&self, id: &EnvironmentId, receiver: Option<&ServiceName>) -> bool {
        let kept = receiver.map(|service| self.directory(id, service));
        let environment = self.environment(id);
        match std::fs::read_dir(&environment) {
            Ok(entries) => {
                let mut entries = entries.flatten();
                kept.is_none() || entries.any(|entry| Some(entry.path()) != kept)
            }
            Err(_) => false,
        }
    }

    /// Removes environment `id`'s directories no service mounts, as
    /// `mounts` lists their sources, and its own once empty.
    pub fn remove_unmounted(&self, id: &EnvironmentId, mounts: &[TelemetryMount]) {
        let environment = self.environment(id);
        if let Ok(entries) = std::fs::read_dir(&environment) {
            for entry in entries.flatten() {
                let directory = entry.path();
                if !mounts
                    .iter()
                    .any(|mount| Path::new(&mount.source) == directory)
                {
                    Self::remove_directory(&directory);
                }
            }
        }
        // Fails, keeping it, while a mounted directory remains.
        let _ = std::fs::remove_dir(&environment);
    }

    /// Removes environment `id`'s directories and their sockets, if present.
    pub fn remove(&self, id: &EnvironmentId) {
        Self::remove_directory(&self.environment(id));
    }

    fn remove_directory(directory: &Path) {
        match std::fs::remove_dir_all(directory) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                tracing::warn!(?error, directory = %directory.display(), "telemetry socket directory could not be removed");
            }
            _ => {}
        }
    }

    /// The environments that have a directory.
    #[must_use]
    pub fn environments(&self) -> Vec<EnvironmentId> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| EnvironmentId::parse(entry.file_name().to_str()?.to_owned()).ok())
            .collect()
    }
}
