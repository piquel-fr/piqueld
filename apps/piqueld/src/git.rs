//! Isolated Git checkouts. No shared mutable repository or credential store.

use anyhow::{Context, bail};
use piqueld_core::manifest::{GitRepository, valid_git_commit, valid_repository_path};
use std::path::PathBuf;
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
        clone.args(["clone", "--no-checkout", "--single-branch"]);
        if repository.commit.is_none() {
            clone.args(["--branch", &repository.branch]);
        }
        clone.arg("--").arg(&repository.url).arg(&root);
        crate::command::LoggedCommand::run(&mut clone, "clone Git repository").await?;
        if let Some(commit) = &repository.commit {
            crate::command::LoggedCommand::run(
                Self::command()
                    .arg("-C")
                    .arg(&root)
                    .args(["fetch", "origin", commit]),
                "fetch pinned Git commit",
            )
            .await?;
        }
        let revision = repository.commit.as_deref().unwrap_or("HEAD");
        crate::command::LoggedCommand::run(
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
        // Inherit host credentials while excluding executable transport helpers, even
        // when host configuration enables them or rewrites a URL to use one.
        // Never hang waiting for a password prompt.
        command
            .env("GIT_ALLOW_PROTOCOL", "file:git:http:https:ssh")
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

    /// Pin a checkout and resolve its paths before building its local image.
    pub(crate) async fn prepare(
        repository: &GitRepository,
        build: &piqueld_core::manifest::Build,
        docker: &impl crate::docker::DockerApi,
    ) -> anyhow::Result<(String, piqueld_core::resource::Sha256Digest)> {
        let checkout = Self::clone(repository).await?;
        let piqueld_core::manifest::Build::Docker {
            dockerfile,
            context,
        } = build;
        let dockerfile = checkout.path(dockerfile).await?;
        let context = checkout.path(context).await?;
        let image = docker
            .build_image(&dockerfile, &context)
            .await
            .context("build Git source image")?;
        Ok((checkout.commit, image))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    async fn commit(root: &Path, contents: &str) -> String {
        tokio::fs::write(root.join("Dockerfile"), contents)
            .await
            .unwrap();
        crate::command::LoggedCommand::run(
            Checkout::command().arg("-C").arg(root).args(["add", "."]),
            "stage fixture",
        )
        .await
        .unwrap();
        crate::command::LoggedCommand::run(
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
    async fn checkout_rejects_executable_transports_even_when_host_allows_them() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("executed");
        let helper = format!("ext::touch {}", marker.display());
        for rewritten in [false, true] {
            let mut command = Checkout::command();
            command.args(["-c", "protocol.ext.allow=always"]);
            if rewritten {
                command.args([
                    "-c",
                    &format!("url.{helper}.insteadOf=https://example.com/repo"),
                ]);
            }
            command
                .args(["clone", "--"])
                .arg(if rewritten {
                    "https://example.com/repo"
                } else {
                    &helper
                })
                .arg(root.path().join("checkout"));
            let error = crate::command::LoggedCommand::run(&mut command, "clone fixture")
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("transport 'ext' not allowed"),
                "{error:#}"
            );
            assert!(!marker.exists());
        }
    }

    #[tokio::test]
    async fn checkout_pins_commits_and_confines_paths() {
        let repository = tempfile::tempdir().unwrap();
        crate::command::LoggedCommand::run(
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
        source.branch = "branch-no-longer-exists".into();
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
