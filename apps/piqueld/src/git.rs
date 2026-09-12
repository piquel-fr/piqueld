//! Isolated Git checkouts. No shared mutable repository or credential store.

use anyhow::{Context, bail};
use piqueld_core::manifest::{GitRepository, valid_git_commit, valid_repository_path};
use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::process::Command;

/// A private checkout pinned once, retained until preparation finishes.
pub(crate) struct Checkout {
    directory: tempfile::TempDir,
    pub(crate) commit: String,
}

impl Checkout {
    pub(crate) async fn clone(repository: &GitRepository) -> anyhow::Result<Self> {
        let mut errors = Vec::new();
        repository.validate("repository", &mut errors);
        if !errors.is_empty() {
            bail!("invalid Git repository configuration");
        }
        let directory = tempfile::tempdir().context("create Git checkout directory")?;
        let root = directory.path().join("repository");
        let mut clone = Self::command();
        clone
            .args([
                "clone",
                "--no-checkout",
                "--single-branch",
                "--branch",
                &repository.branch,
                "--",
                &repository.url,
            ])
            .arg(&root);
        Self::run(&mut clone, "clone Git repository").await?;
        if let Some(commit) = &repository.commit {
            Self::run(
                Self::command()
                    .arg("-C")
                    .arg(&root)
                    .args(["fetch", "origin", commit]),
                "fetch pinned Git commit",
            )
            .await?;
        }
        let revision = repository.commit.as_deref().unwrap_or("HEAD");
        Self::run(
            Self::command()
                .arg("-C")
                .arg(&root)
                .args(["checkout", "--detach", revision, "--"]),
            "checkout Git commit",
        )
        .await?;
        let output = Self::command()
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", "HEAD"])
            .kill_on_drop(true)
            .output()
            .await
            .context("resolve Git commit")?;
        let commit = String::from_utf8(output.stdout)
            .context("decode Git commit")?
            .trim()
            .to_owned();
        if !output.status.success() || !valid_git_commit(&commit) {
            bail!("Git did not resolve a full commit hash");
        }
        Ok(Self { directory, commit })
    }

    fn command() -> Command {
        let mut command = Command::new("git");
        // Inherit host credentials, but never hang waiting for a password prompt.
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never");
        command.args(["-c", "core.hooksPath=/dev/null"]);
        command.kill_on_drop(true);
        command
    }

    pub(crate) fn root(&self) -> PathBuf {
        self.directory.path().join("repository")
    }

    pub(crate) async fn path(&self, relative: &str) -> anyhow::Result<PathBuf> {
        if !valid_repository_path(relative) {
            bail!("path must remain within the repository root");
        }
        let root = tokio::fs::canonicalize(self.root())
            .await
            .context("locate checkout root")?;
        let path = tokio::fs::canonicalize(root.join(relative))
            .await
            .context("locate repository file")?;
        if !path.starts_with(&root) || path.starts_with(root.join(".git")) {
            bail!("repository path escapes the checkout");
        }
        Ok(path)
    }

    /// Execute without retaining unbounded build output in memory. Error tails
    /// stay in internal diagnostics, never the public API response.
    pub(crate) async fn run(command: &mut Command, operation: &'static str) -> anyhow::Result<()> {
        let mut log = tempfile::tempfile().context("create command log")?;
        let status = command
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log.try_clone()?)
            .kill_on_drop(true)
            .status()
            .await
            .with_context(|| operation)?;
        if !status.success() {
            log.seek(SeekFrom::End(-i64::try_from(
                log.metadata()?.len().min(8192),
            )?))?;
            let mut tail = String::new();
            log.take(8192)
                .read_to_string(&mut tail)
                .context("read command diagnostics")?;
            bail!("{operation} failed ({status}): {tail}");
        }
        Ok(())
    }

    pub(crate) async fn build(
        &self,
        socket: &Path,
        build: &piqueld_core::manifest::Build,
    ) -> anyhow::Result<piqueld_core::resource::Sha256Digest> {
        let piqueld_core::manifest::Build::Docker {
            dockerfile,
            context,
        } = build;
        let dockerfile = self.path(dockerfile).await?;
        let context = self.path(context).await?;
        if !dockerfile.is_file() || !context.is_dir() {
            bail!("Dockerfile must be a file and build context must be a directory");
        }
        let iidfile = self.directory.path().join("image-id");
        let mut command = Command::new("docker");
        command
            .arg("--host")
            .arg(format!("unix://{}", socket.display()))
            .args(["build", "--pull", "--file"])
            .arg(dockerfile)
            .arg("--iidfile")
            .arg(&iidfile)
            .arg(context);
        Self::run(&mut command, "build Docker image").await?;
        let id = tokio::fs::read_to_string(iidfile)
            .await
            .context("read built image ID")?;
        piqueld_core::resource::Sha256Digest::parse(id.trim()).context("validate built image ID")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn commit(root: &Path, contents: &str) -> String {
        tokio::fs::write(root.join("Dockerfile"), contents)
            .await
            .unwrap();
        Checkout::run(
            Checkout::command().arg("-C").arg(root).args(["add", "."]),
            "stage fixture",
        )
        .await
        .unwrap();
        Checkout::run(
            Checkout::command().arg("-C").arg(root).args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "fixture",
            ]),
            "commit fixture",
        )
        .await
        .unwrap();
        let output = Checkout::command()
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "HEAD"])
            .output()
            .await
            .unwrap();
        String::from_utf8(output.stdout).unwrap().trim().into()
    }

    #[tokio::test]
    async fn checkout_pins_commits_and_confines_paths() {
        let repository = tempfile::tempdir().unwrap();
        Checkout::run(
            Checkout::command()
                .args(["init", "--initial-branch=main"])
                .arg(repository.path()),
            "initialize fixture",
        )
        .await
        .unwrap();
        let first = commit(repository.path(), "FROM scratch\n").await;
        let mut source = GitRepository {
            url: repository.path().display().to_string(),
            branch: "main".into(),
            commit: None,
        };
        let checkout = Checkout::clone(&source).await.unwrap();
        let second = commit(repository.path(), "FROM scratch\nLABEL version=2\n").await;
        assert_eq!(checkout.commit, first);
        assert_eq!(Checkout::clone(&source).await.unwrap().commit, second);
        source.commit = Some(first.clone());
        assert_eq!(Checkout::clone(&source).await.unwrap().commit, first);
        assert!(checkout.path("../Dockerfile").await.is_err());
        assert!(checkout.path("missing").await.is_err());
        std::os::unix::fs::symlink(repository.path(), checkout.root().join("escape")).unwrap();
        assert!(checkout.path("escape/Dockerfile").await.is_err());
        assert_eq!(
            tokio::fs::read_to_string(checkout.path("./Dockerfile").await.unwrap())
                .await
                .unwrap(),
            "FROM scratch\n"
        );
    }
}
