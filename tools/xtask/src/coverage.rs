//! `just coverage`: which code the native workspace tests run, measured by
//! `cargo llvm-cov`, as an HTML report and a summary per crate. See
//! docs/dev/validation.md for what it leaves out.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::AddAssign;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::Workspace;
use crate::process::{checked_stdout, exit_code};

/// Runs the tests `just test` runs under coverage instrumentation, writes the
/// HTML report to target/llvm-cov/html, and prints each crate's totals.
/// `args` go on to `cargo nextest run`.
pub fn run(workspace: &Workspace, args: &[String]) -> Result<ExitCode> {
    let status = workspace
        .cargo()
        .args([
            "llvm-cov",
            "nextest",
            "--locked",
            "--workspace",
            "--all-features",
            "--html",
        ])
        .args(args)
        .status()
        .context("run cargo llvm-cov; `nix develop` provides it")?;
    if !status.success() {
        return Ok(exit_code(status));
    }
    let export: Export =
        workspace.cargo_json(&["llvm-cov", "report", "--json", "--summary-only"])?;
    let metadata: Metadata =
        workspace.cargo_json(&["metadata", "--locked", "--no-deps", "--format-version", "1"])?;

    let mut crates = BTreeMap::<&str, Summary>::new();
    let mut total = Summary::default();
    for data in &export.data {
        for file in &data.files {
            let package = metadata
                .package_of(&file.filename)
                .with_context(|| format!("{} is in no workspace crate", file.filename.display()))?;
            *crates.entry(package).or_default() += file.summary;
            total += file.summary;
        }
    }

    println!("\n{:<20} {:>24} {:>24}", "crate", "lines", "functions");
    for (name, summary) in crates.iter().chain([(&"total", &total)]) {
        println!("{name:<20} {:>24} {:>24}", summary.lines, summary.functions);
    }
    println!("\nHTML report: target/llvm-cov/html/index.html");
    Ok(ExitCode::SUCCESS)
}

impl Workspace {
    /// Runs a cargo subcommand that prints JSON and parses its output.
    fn cargo_json<T: DeserializeOwned>(&self, args: &[&str]) -> Result<T> {
        let name = format!("cargo {}", args.join(" "));
        let output = self
            .cargo()
            .args(args)
            .output()
            .with_context(|| format!("run {name}"))?;
        serde_json::from_str(&checked_stdout(&name, output)?)
            .with_context(|| format!("parse {name}"))
    }
}

/// `cargo metadata`, reduced to the workspace's crates.
#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    manifest_path: PathBuf,
}

impl Metadata {
    /// The crate whose directory holds `file`: the deepest, since a crate's
    /// directory may contain another's.
    fn package_of(&self, file: &Path) -> Option<&str> {
        self.packages
            .iter()
            .filter_map(|package| Some((package, package.manifest_path.parent()?)))
            .filter(|(_, dir)| file.starts_with(dir))
            .max_by_key(|(_, dir)| dir.components().count())
            .map(|(package, _)| package.name.as_str())
    }
}

/// `llvm-cov export --summary-only`, as `cargo llvm-cov report --json` prints it.
#[derive(Deserialize)]
struct Export {
    data: Vec<Data>,
}

#[derive(Deserialize)]
struct Data {
    files: Vec<File>,
}

#[derive(Deserialize)]
struct File {
    filename: PathBuf,
    summary: Summary,
}

#[derive(Clone, Copy, Default, Deserialize)]
struct Summary {
    lines: Count,
    functions: Count,
}

/// How many of `count` lines or functions the tests ran.
#[derive(Clone, Copy, Default, Deserialize)]
struct Count {
    count: u64,
    covered: u64,
}

impl AddAssign for Summary {
    fn add_assign(&mut self, other: Self) {
        self.lines += other.lines;
        self.functions += other.functions;
    }
}

impl AddAssign for Count {
    fn add_assign(&mut self, other: Self) {
        self.count += other.count;
        self.covered += other.covered;
    }
}

impl fmt::Display for Count {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // In tenths of a percent, so it needs no lossy float conversion.
        let permille = (self.covered * 1000).checked_div(self.count).unwrap_or(0);
        let percent = format!("{}.{}%", permille / 10, permille % 10);
        f.pad(&format!("{}/{} {percent:>6}", self.covered, self.count))
    }
}
