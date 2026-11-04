//! `cargo xtask publish`: publishes the workspace's crates to crates.io, with
//! the prebuilt dashboard the published daemon embeds, and `cargo xtask
//! release`, which also tags the release and creates it on GitHub. See
//! docs/dev/releasing.md.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::Workspace;
use crate::process::{checked_stdout, exit_code};

/// Releases the workspace version from `HEAD`, which must be the latest
/// `origin/main`, unless the version is already tagged on `origin`: publishes
/// the crates, then tags `HEAD` `v<version>`, pushes the tag, and creates its
/// GitHub release, with notes listing the pull requests merged since the
/// previous release.
pub fn release(workspace: &Workspace) -> Result<ExitCode> {
    let root = workspace.root();
    let package_id = daemon_package_id(workspace)?;
    let version = package_id
        .rsplit(['#', '@'])
        .next()
        .with_context(|| format!("no version in the package ID {package_id}"))?;
    let tag = format!("v{version}");
    let title = format!("piqueld {version}");

    Workspace::git(root, &["fetch", "origin", "main"])?;
    ensure!(
        Workspace::git(root, &["rev-parse", "HEAD"])?
            == Workspace::git(root, &["rev-parse", "FETCH_HEAD"])?,
        "release from the latest origin/main: run `git switch main && git pull`"
    );
    let tagged = Workspace::git(
        root,
        &["ls-remote", "--tags", "origin", &format!("refs/tags/{tag}")],
    )?;
    ensure!(
        tagged.is_empty(),
        "{tag} is already released; set a new version in Cargo.toml first"
    );

    let status = publish(workspace, &[])?;
    if !status.success() {
        return Ok(exit_code(status));
    }
    Workspace::git(root, &["tag", "--annotate", &tag, "--message", &title])?;
    Workspace::git(root, &["push", "origin", &tag])?;
    println!("tagged and pushed {tag}");
    let status = Command::new("gh")
        .args(["release", "create", &tag, "--verify-tag", "--title", &title])
        .arg("--generate-notes")
        .current_dir(root)
        .status()
        .context("run gh release create")?;
    Ok(exit_code(status))
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
