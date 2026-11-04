//! `cargo xtask publish`: publishes the workspace's crates to crates.io, with
//! the prebuilt dashboard the published daemon embeds, and `cargo xtask
//! release`, which also tags the release and creates it on GitHub. See
//! docs/dev/releasing.md.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::Workspace;
use crate::process::{checked_stdout, exit_code};

/// A release of the workspace version, from `HEAD`, by `cargo xtask release`.
struct Release<'a> {
    workspace: &'a Workspace,
    /// The workspace version, such as `0.2.0`.
    version: String,
    /// Runs the checks and `cargo publish --dry-run`, and only prints what the
    /// other steps would do.
    dry_run: bool,
}

impl Release<'_> {
    /// Checks that `HEAD` is the latest `origin/main` and that the version
    /// isn't tagged on `origin` yet. Then publishes the crates, tags `HEAD`
    /// `v<version>`, pushes the tag, creates its GitHub release, with notes
    /// listing the pull requests merged since the previous release, and closes
    /// its minor version's GitHub milestone, such as `v0.2`.
    fn run(&self) -> Result<ExitCode> {
        self.check()?;
        let dry_run = self.dry_run.then(|| "--dry-run".to_owned());
        let status = publish(self.workspace, dry_run.as_slice())?;
        if !status.success() {
            return Ok(exit_code(status));
        }
        let tag = self.tag();
        let title = format!("piqueld {}", self.version);
        self.step("git", &["tag", "--annotate", &tag, "--message", &title])?;
        self.step("git", &["push", "origin", &tag])?;
        self.step(
            "gh",
            &[
                "release",
                "create",
                &tag,
                "--verify-tag",
                "--title",
                &title,
                "--generate-notes",
            ],
        )?;
        let milestone = self.milestone_title()?;
        match self.milestone(&milestone)? {
            Some(number) => self.step(
                "gh",
                &[
                    "api",
                    "--method",
                    "PATCH",
                    "--silent",
                    &format!("repos/{{owner}}/{{repo}}/milestones/{number}"),
                    "--field",
                    "state=closed",
                ],
            )?,
            // A patch release finds its minor version's milestone closed.
            None => println!("no open GitHub milestone named {milestone} to close"),
        }
        Ok(ExitCode::SUCCESS)
    }

    /// `v<version>`, the release's tag.
    fn tag(&self) -> String {
        format!("v{}", self.version)
    }

    /// `v<major>.<minor>`, the name of the release's milestone.
    fn milestone_title(&self) -> Result<String> {
        let mut parts = self.version.split('.');
        let (Some(major), Some(minor)) = (parts.next(), parts.next()) else {
            bail!("the version {} has no minor version", self.version);
        };
        Ok(format!("v{major}.{minor}"))
    }

    /// Fails unless `HEAD` is the latest `origin/main`, and the version isn't
    /// tagged on `origin` yet.
    fn check(&self) -> Result<()> {
        let root = self.workspace.root();
        Workspace::git(root, &["fetch", "origin", "main"])?;
        ensure!(
            Workspace::git(root, &["rev-parse", "HEAD"])?
                == Workspace::git(root, &["rev-parse", "FETCH_HEAD"])?,
            "release from the latest origin/main: run `git switch main && git pull`"
        );
        let tag = self.tag();
        let tagged = Workspace::git(
            root,
            &["ls-remote", "--tags", "origin", &format!("refs/tags/{tag}")],
        )?;
        ensure!(
            tagged.is_empty(),
            "{tag} is already released; set a new version in Cargo.toml first"
        );
        Ok(())
    }

    /// The number of the open GitHub milestone named `title`, if any.
    fn milestone(&self, title: &str) -> Result<Option<u64>> {
        let query = format!(".[] | select(.title == \"{title}\") | .number");
        let output = Command::new("gh")
            .args([
                "api",
                "--paginate",
                "repos/{owner}/{repo}/milestones",
                "--jq",
                &query,
            ])
            .current_dir(self.workspace.root())
            .output()
            .context("run gh api")?;
        let number = checked_stdout("gh api", output)?;
        Ok(if number.is_empty() {
            None
        } else {
            Some(
                number
                    .parse()
                    .with_context(|| format!("invalid milestone number {number}"))?,
            )
        })
    }

    /// Runs `program` with `args` from the workspace root, failing unless it
    /// succeeds, or only prints the command in a dry run.
    fn step(&self, program: &str, args: &[&str]) -> Result<()> {
        let command = format!("{program} {}", args.join(" "));
        if self.dry_run {
            println!("would run: {command}");
            return Ok(());
        }
        println!("running: {command}");
        let status = Command::new(program)
            .args(args)
            .current_dir(self.workspace.root())
            .status()
            .with_context(|| format!("run {program}"))?;
        ensure!(status.success(), "{command} failed ({status})");
        Ok(())
    }
}

/// Runs [`Release::run`] for `cargo xtask release`, for the workspace version.
pub fn release(workspace: &Workspace, dry_run: bool) -> Result<ExitCode> {
    let package_id = daemon_package_id(workspace)?;
    let version = package_id
        .rsplit(['#', '@'])
        .next()
        .with_context(|| format!("no version in the package ID {package_id}"))?
        .to_owned();
    Release {
        workspace,
        version,
        dry_run,
    }
    .run()
}

/// Runs [`publish`] for `cargo xtask publish`.
pub fn run(workspace: &Workspace, args: &[String]) -> Result<ExitCode> {
    publish(workspace, args).map(exit_code)
}

/// Ships the dashboard in the daemon crate, then runs
/// `cargo publish --workspace` with `args`, such as `--dry-run`. Cargo
/// publishes the crates in dependency order and skips the unpublished ones.
///
/// Cargo counts the shipped dashboard, which is gitignored but packaged, as an
/// uncommitted change, so this checks that the working tree is clean itself,
/// unless `args` has `--allow-dirty`, and always passes `--allow-dirty`.
fn publish(workspace: &Workspace, args: &[String]) -> Result<ExitStatus> {
    let allow_dirty = args.iter().any(|arg| arg == "--allow-dirty");
    if !allow_dirty {
        let changes = Workspace::git(
            workspace.root(),
            &["status", "--porcelain", "--untracked-files=all"],
        )?;
        ensure!(
            changes.is_empty(),
            "the working tree has uncommitted changes; commit them, or pass --allow-dirty:\n{changes}"
        );
    }
    let _dashboard = ShippedDashboard::build(workspace)?;
    workspace
        .cargo()
        .args(["publish", "--locked", "--workspace"])
        .args((!allow_dirty).then_some("--allow-dirty"))
        .args(args)
        .status()
        .context("run cargo publish")
}

/// The daemon's package ID, such as `path+file:///…/apps/piqueld#0.1.0`.
fn daemon_package_id(workspace: &Workspace) -> Result<String> {
    let output = workspace
        .cargo()
        .args(["pkgid", "--package", "piqueld"])
        .output()
        .context("run cargo pkgid")?;
    checked_stdout("cargo pkgid", output)
}

/// The dashboard bundle in `apps/piqueld/dashboard`, which the daemon's build
/// script embeds when a packaged crate builds without `embedded-ui`. It is
/// removed when dropped.
struct ShippedDashboard(PathBuf);

impl ShippedDashboard {
    /// Builds the bundle with the daemon's build script and copies it from the
    /// script's `OUT_DIR/dashboard-dist`.
    fn build(workspace: &Workspace) -> Result<Self> {
        let daemon = workspace.root().join("apps/piqueld");
        let package_id = daemon_package_id(workspace)?;
        let mut cargo = workspace
            .cargo()
            .args(["check", "--locked", "--package", "piqueld"])
            .args(["--features", "embedded-ui"])
            .args(["--message-format", "json-render-diagnostics"])
            .stdout(Stdio::piped())
            .spawn()
            .context("run cargo check")?;
        let stdout = cargo.stdout.take().context("stdout is not piped")?;
        let mut out_dir = None;
        for line in BufReader::new(stdout).lines() {
            let message: Message = serde_json::from_str(&line.context("read cargo's output")?)
                .context("parse cargo's output")?;
            if message.reason == "build-script-executed" && message.package_id == package_id {
                out_dir = message.out_dir;
            }
        }
        let status = cargo.wait().context("wait for cargo check")?;
        ensure!(status.success(), "building the dashboard failed ({status})");
        let dist = out_dir
            .context("cargo didn't run the daemon's build script")?
            .join("dashboard-dist");

        let shipped = Self(daemon.join("dashboard"));
        if shipped.0.exists() {
            fs::remove_dir_all(&shipped.0)
                .with_context(|| format!("remove {}", shipped.0.display()))?;
        }
        copy_dir(&dist, &shipped.0)?;
        println!("shipping the dashboard in {}", shipped.0.display());
        Ok(shipped)
    }
}

impl Drop for ShippedDashboard {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!("warning: couldn't remove {}: {error}", self.0.display());
        }
    }
}

/// The fields of Cargo's JSON messages this reads.
#[derive(Deserialize)]
struct Message {
    reason: String,
    #[serde(default)]
    package_id: String,
    out_dir: Option<PathBuf>,
}

/// Copies the directory `from`, recursively, to `to`.
fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("create {}", to.display()))?;
    for entry in fs::read_dir(from).with_context(|| format!("read {}", from.display()))? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)
                .with_context(|| format!("copy {}", entry.path().display()))?;
        }
    }
    Ok(())
}
