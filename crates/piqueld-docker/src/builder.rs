//! A build machine's `BuildKit` daemon, reached through Docker Buildx.
//!
//! Buildx's `remote` driver sends a build's context to the daemon, which
//! builds the image and pushes it to a registry by digest, without a tag.
//! The engine then pulls it from there like any registry image:
//!
//! ```text
//! docker buildx build --builder=piqueld --platform=linux/arm64
//!     --output=type=image,name=build.example.ts.net:5000/piqueld,push=true,push-by-digest=true …
//! build.example.ts.net:5000/piqueld@sha256:<64 hex>
//! ```
use anyhow::Context;
use piqueld_core::build_machine::{BuildRepository, BuilderEndpoint, RemoteBuild, WhenUnavailable};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

/// A build machine whose `BuildKit` daemon did not accept a connection
/// before a build.
#[derive(Debug, thiserror::Error)]
#[error("the build machine's BuildKit daemon at {address} is unreachable")]
pub struct BuilderUnavailable {
    /// The `host:port` tried.
    address: String,
    /// Why connecting failed.
    #[source]
    source: std::io::Error,
}

/// A build machine registered with Buildx, in a configuration directory of
/// piqueld's own: nothing depends on the daemon user's Docker configuration,
/// or changes it.
#[derive(Debug)]
pub struct RemoteBuilder {
    /// Buildx's configuration directory, `<data_dir>/buildx`.
    config: PathBuf,
    /// Where its `BuildKit` daemon listens.
    endpoint: BuilderEndpoint,
    /// Where the build machine pushes images.
    pub(super) repository: BuildRepository,
    /// What a build does while the build machine can't be reached.
    pub(super) unavailable: WhenUnavailable,
    /// The latest check of its `BuildKit` daemon; see [`Self::probe`].
    last_check: parking_lot::Mutex<Option<piqueld_core::api::BuilderCheck>>,
}

impl RemoteBuilder {
    /// The builder's name in Buildx.
    const NAME: &str = "piqueld";
    /// Longest a connection to the build machine may take before it counts
    /// as unreachable. Buildx itself waits about 20 seconds.
    const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

    /// Registers `remote`'s `BuildKit` daemon in `<data_dir>/buildx`, replacing
    /// what an earlier start registered. Buildx only records the endpoint, so
    /// the build machine need not be reachable.
    ///
    /// # Errors
    /// Returns why the directory could not be replaced or Buildx refused.
    pub async fn register(remote: &RemoteBuild, data_dir: &Path) -> anyhow::Result<Self> {
        let builder = Self {
            config: data_dir.join("buildx"),
            endpoint: remote.builder.clone(),
            repository: remote.repository.clone(),
            unavailable: remote.unavailable,
            last_check: parking_lot::Mutex::default(),
        };
        match tokio::fs::remove_dir_all(&builder.config).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error).context("remove the earlier Buildx configuration");
            }
            _ => {}
        }
        let mut create = builder.docker();
        create
            .args(["buildx", "create", "--driver=remote", "--name", Self::NAME])
            .arg(remote.builder.to_string());
        crate::command::LoggedCommand::run_recorded(&mut create, "register the builder", None)
            .await?;
        Ok(builder)
    }

    /// Checks that the build machine's `BuildKit` daemon accepts a
    /// connection within [`Self::PROBE_TIMEOUT`], and keeps the result for
    /// [`Self::status`]. Its registry is the engine's to reach, not the
    /// daemon's, and is on the same machine.
    ///
    /// # Errors
    /// Returns why it could not be reached.
    pub async fn probe(&self) -> Result<(), BuilderUnavailable> {
        let address = self.endpoint.address();
        let result =
            tokio::time::timeout(Self::PROBE_TIMEOUT, tokio::net::TcpStream::connect(address))
                .await
                .map_err(std::io::Error::from)
                .flatten()
                .map(drop)
                .map_err(|source| BuilderUnavailable {
                    address: address.to_owned(),
                    source,
                });
        *self.last_check.lock() = Some(piqueld_core::api::BuilderCheck {
            reachable: result.is_ok(),
            checked_at_ms: crate::now_ns() / 1_000_000,
            // Why the connection failed; the rest is the status's.
            message: result.as_ref().err().map(|error| error.source.to_string()),
        });
        result
    }

    /// Where the build machine is, and its latest check.
    #[must_use]
    pub fn status(&self) -> piqueld_core::api::BuilderStatus {
        piqueld_core::api::BuilderStatus {
            builder: self.endpoint.to_string(),
            repository: self.repository.to_string(),
            last_check: self.last_check.lock().clone(),
        }
    }

    /// A `docker` command that reads this builder's Buildx configuration.
    pub(super) fn docker(&self) -> Command {
        let mut command = Command::new("docker");
        command.env("BUILDX_CONFIG", &self.config);
        command
    }

    /// The `docker buildx build` options that run a build on the build
    /// machine for `platform`, the engine's, and push the image by digest.
    pub(super) fn options(&self, platform: &str) -> [String; 3] {
        [
            format!("--builder={}", Self::NAME),
            format!("--platform={platform}"),
            format!(
                "--output=type=image,name={},push=true,push-by-digest=true",
                self.repository
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A builder whose `BuildKit` daemon listens at `daemon`.
    fn builder(daemon: std::net::SocketAddr) -> RemoteBuilder {
        RemoteBuilder {
            config: PathBuf::from("/var/lib/piqueld/buildx"),
            endpoint: BuilderEndpoint::try_from(format!("tcp://{daemon}")).unwrap(),
            repository: BuildRepository::try_from("build.example.ts.net:5000/piqueld".to_owned())
                .unwrap(),
            unavailable: WhenUnavailable::Fail,
            last_check: parking_lot::Mutex::default(),
        }
    }

    #[test]
    fn builds_run_on_the_builder_for_the_engines_platform_and_push_by_digest() {
        assert_eq!(
            builder("127.0.0.1:1234".parse().unwrap()).options("linux/arm64"),
            [
                "--builder=piqueld",
                "--platform=linux/arm64",
                "--output=type=image,name=build.example.ts.net:5000/piqueld,push=true,push-by-digest=true",
            ]
        );
    }

    #[tokio::test]
    async fn a_build_machine_is_reachable_once_its_daemon_accepts_connections() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let reachable = builder(listener.local_addr().unwrap());
        assert_eq!(reachable.status().last_check, None);
        reachable.probe().await.unwrap();
        assert!(reachable.status().last_check.unwrap().reachable);
        // A port nothing listens on: bound, then released.
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        let unreachable = builder(closed);
        let error = unreachable.probe().await.unwrap_err().to_string();
        assert_eq!(
            error,
            format!("the build machine's BuildKit daemon at {closed} is unreachable")
        );
        let check = unreachable.status().last_check.unwrap();
        assert!(!check.reachable);
        assert!(
            check
                .message
                .is_some_and(|message| !error.contains(&message))
        );
    }
}
