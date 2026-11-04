//! `cargo xtask publish`: publishes the workspace's crates to crates.io, with
//! the prebuilt dashboard the published daemon embeds. See
//! docs/dev/releasing.md.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::Workspace;
use crate::process::{checked_stdout, exit_code};

/// Ships the dashboard in the daemon crate, then runs
/// `cargo publish --workspace` with `args`, such as `--dry-run`. Cargo
/// publishes the crates in dependency order and skips the unpublished ones.
///
/// Cargo counts the shipped dashboard, which is gitignored but packaged, as an
/// uncommitted change, so this checks that the working tree is clean itself,
/// unless `args` has `--allow-dirty`, and always passes `--allow-dirty`.
pub fn run(workspace: &Workspace, args: &[String]) -> Result<ExitCode> {
    let allow_dirty = args.iter().any(|arg| arg == "--allow-dirty");
    if !allow_dirty {
        let changes = Workspace::git(workspace.root(), &["status", "--porcelain"])?;
        ensure!(
            changes.is_empty(),
            "the working tree has uncommitted changes; commit them, or pass --allow-dirty:\n{changes}"
        );
    }
    let _dashboard = ShippedDashboard::build(workspace)?;
    let status = workspace
        .cargo()
        .args(["publish", "--locked", "--workspace"])
        .args((!allow_dirty).then_some("--allow-dirty"))
        .args(args)
        .status()
        .context("run cargo publish")?;
    Ok(exit_code(status))
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
        let output = workspace
            .cargo()
            .args(["pkgid", "--package", "piqueld"])
            .output()
            .context("run cargo pkgid")?;
        let package_id = checked_stdout("cargo pkgid", output)?;
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
