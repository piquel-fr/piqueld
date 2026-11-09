//! `cargo xtask dist`: builds the static release binaries that install.sh
//! (`apps/site/install.sh`) installs, as the archive
//! `target/dist/piqueld-<version>-x86_64-linux.tar.gz` and its `SHA256SUMS`.
//! The release workflow attaches both to the GitHub release; see
//! docs/dev/releasing.md.
//!
//! The binaries link musl and OpenSSL statically, so they run on any
//! `x86_64` Linux. Building them needs `musl-gcc`, from Debian and Ubuntu's
//! `musl-tools`, for the C code of OpenSSL, SQLite and ring.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result};

use crate::Workspace;
use crate::process::{checked_stdout, exit_code};

/// The Rust target the release binaries build for.
const TARGET: &str = "x86_64-unknown-linux-musl";

/// The release archive of one version, with what it holds: each file's path
/// in the workspace, and its name in the archive.
struct Archive {
    /// `target/dist`, which holds the archive and its checksums.
    dist: PathBuf,
    /// The archive's top-level directory, such as
    /// `piqueld-0.1.0-x86_64-linux`.
    name: String,
    files: [(PathBuf, &'static str); 4],
}

impl Archive {
    fn new(workspace: &Workspace, version: &str) -> Self {
        let root = workspace.root();
        let binaries = root.join("target").join(TARGET).join("release");
        Self {
            dist: root.join("target/dist"),
            name: format!("piqueld-{version}-x86_64-linux"),
            files: [
                (binaries.join("piqueld"), "piqueld"),
                (binaries.join("piquelctl"), "piquelctl"),
                (root.join("apps/piqueld/piqueld.service"), "piqueld.service"),
                (root.join("LICENSE"), "LICENSE"),
            ],
        }
    }

    /// Replaces `target/dist` with the archive and its `SHA256SUMS`.
    fn write(&self) -> Result<()> {
        if self.dist.exists() {
            fs::remove_dir_all(&self.dist)
                .with_context(|| format!("remove {}", self.dist.display()))?;
        }
        let staging = self.dist.join(&self.name);
        fs::create_dir_all(&staging).with_context(|| format!("create {}", staging.display()))?;
        for (from, name) in &self.files {
            // Keeps the binaries' executable mode.
            fs::copy(from, staging.join(name))
                .with_context(|| format!("copy {}", from.display()))?;
        }
        let archive = format!("{}.tar.gz", self.name);
        self.run(Command::new("tar").args([
            "--create",
            "--gzip",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "--file",
            &archive,
            &self.name,
        ]))?;
        fs::remove_dir_all(&staging).with_context(|| format!("remove {}", staging.display()))?;
        let sums = self.run(Command::new("sha256sum").arg(&archive))?;
        let path = self.dist.join("SHA256SUMS");
        fs::write(&path, sums + "\n").with_context(|| format!("write {}", path.display()))?;
        println!(
            "wrote {} and SHA256SUMS",
            self.dist.join(&archive).display()
        );
        Ok(())
    }

    /// Runs `command` in `target/dist`, failing unless it succeeds, and
    /// returns its trimmed output.
    fn run(&self, command: &mut Command) -> Result<String> {
        let name = command.get_program().to_string_lossy().into_owned();
        let output = command
            .current_dir(&self.dist)
            .output()
            .with_context(|| format!("run {name}"))?;
        checked_stdout(&name, output)
    }
}

/// Builds the binaries for [`TARGET`], with the embedded dashboard, then
/// writes their [`Archive`].
pub fn run(workspace: &Workspace) -> Result<ExitCode> {
    let status = workspace
        .cargo()
        .args(["build", "--release", "--locked", "--target", TARGET])
        .args(["--package", "piqueld", "--package", "piquelctl"])
        .args(["--bin", "piqueld", "--bin", "piquelctl"])
        // The daemon's features. `piqueld/<feature>` would name the feature of
        // its dev-dependency on itself, which this build leaves out.
        .args(["--features", "embedded-ui,vendored-openssl"])
        .status()
        .context("run cargo build")?;
    if !status.success() {
        return Ok(exit_code(status));
    }
    Archive::new(workspace, &workspace.version()?).write()?;
    Ok(ExitCode::SUCCESS)
}
