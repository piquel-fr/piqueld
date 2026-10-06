//! Development tasks for the piqueld workspace, run with `cargo xtask`: this
//! worktree's development instance (`just dev`, see docs/development.md), the
//! isolated Docker integration tests, the Playwright browser suite, and the
//! `piqueld-core` dependency boundary.

mod boundary;
mod dev;
mod docker;
mod docker_test;
mod playwright;
mod process;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "Development tasks for the piqueld workspace")]
struct Cli {
    #[command(subcommand)]
    task: Task,
}

#[derive(Subcommand)]
enum Task {
    /// Manage this worktree's isolated development instance; runs it in the
    /// foreground without a command.
    Dev {
        #[command(subcommand)]
        command: Option<dev::Command>,
    },
    /// Run the Docker integration tests against a throwaway Docker-in-Docker
    /// engine.
    DockerTest,
    /// Run the Playwright browser suite with Chromium in its pinned container.
    #[command(disable_help_flag = true)]
    Playwright {
        /// Arguments for `playwright test`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Check that `piqueld-core` depends on no server, Docker, or browser
    /// framework.
    Boundary,
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
    let workspace = Workspace::current()?;
    match Cli::parse().task {
        Task::Dev { command } => command.unwrap_or(dev::Command::Run).run(workspace).await,
        Task::DockerTest => docker_test::run(&workspace).await,
        Task::Playwright { args } => playwright::run(&workspace, &args).await,
        Task::Boundary => boundary::check(&workspace),
    }
}

/// The checkout `cargo xtask` runs in, which may be one of several worktrees.
struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn current() -> Result<Self> {
        let root = Self::git(Path::new("."), &["rev-parse", "--show-toplevel"])?;
        Ok(Self { root: root.into() })
    }

    fn root(&self) -> &Path {
        &self.root
    }

    /// Every checkout of the repository, the main one first.
    fn worktrees(&self) -> Result<Vec<PathBuf>> {
        let list = Self::git(&self.root, &["worktree", "list", "--porcelain"])?;
        Ok(list
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .map(PathBuf::from)
            .collect())
    }

    /// Runs git in `dir` and returns its trimmed output.
    fn git(dir: &Path, args: &[&str]) -> Result<String> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .context("run git")?;
        ensure!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(String::from_utf8(output.stdout)
            .context("git printed invalid UTF-8")?
            .trim()
            .to_owned())
    }
}
