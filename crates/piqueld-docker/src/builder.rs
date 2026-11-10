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
use piqueld_core::build_machine::{BuildRepository, RemoteBuild};
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// A build machine registered with Buildx, in a configuration directory of
/// piqueld's own: nothing depends on the daemon user's Docker configuration,
/// or changes it.
#[derive(Debug)]
pub struct RemoteBuilder {
    /// Buildx's configuration directory, `<data_dir>/buildx`.
    config: PathBuf,
    /// Where the build machine pushes images.
    pub(super) repository: BuildRepository,
}

impl RemoteBuilder {
    /// The builder's name in Buildx.
    const NAME: &str = "piqueld";

    /// Registers `remote`'s `BuildKit` daemon in `<data_dir>/buildx`, replacing
    /// what an earlier start registered. Buildx only records the endpoint, so
    /// the build machine need not be reachable.
    ///
    /// # Errors
    /// Returns why the directory could not be replaced or Buildx refused.
    pub async fn register(remote: &RemoteBuild, data_dir: &Path) -> anyhow::Result<Self> {
        let builder = Self {
            config: data_dir.join("buildx"),
            repository: remote.repository.clone(),
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

    #[test]
    fn builds_run_on_the_builder_for_the_engines_platform_and_push_by_digest() {
        let builder = RemoteBuilder {
            config: PathBuf::from("/var/lib/piqueld/buildx"),
            repository: BuildRepository::try_from("build.example.ts.net:5000/piqueld".to_owned())
                .unwrap(),
        };
        assert_eq!(
            builder.options("linux/arm64"),
            [
                "--builder=piqueld",
                "--platform=linux/arm64",
                "--output=type=image,name=build.example.ts.net:5000/piqueld,push=true,push-by-digest=true",
            ]
        );
    }
}
