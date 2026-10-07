//! `just boundary`: `piqueld-core` holds the shared contracts, so it must not
//! depend on the server, Docker, database, or browser frameworks.

use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};

use crate::Workspace;

const FORBIDDEN: [&str; 7] = [
    "axum",
    "axum-core",
    "bollard",
    "bollard-stubs",
    "leptos",
    "sqlx",
    "sqlx-core",
];

pub fn check(workspace: &Workspace) -> Result<ExitCode> {
    // All edges: the boundary covers build and development dependencies too.
    let output = Command::new("cargo")
        .args(["tree", "--locked", "--package", "piqueld-core"])
        .args(["--edges", "all", "--prefix", "none"])
        .current_dir(workspace.root())
        .output()
        .context("run cargo tree")?;
    ensure!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8(output.stdout)?;
    let forbidden: Vec<&str> = tree
        .lines()
        .filter(|line| FORBIDDEN.contains(&line.split(' ').next().unwrap_or_default()))
        .collect();
    if !forbidden.is_empty() {
        eprintln!("{}", forbidden.join("\n"));
        bail!("piqueld-core contains a forbidden runtime dependency");
    }
    println!("piqueld-core dependency boundary is intact");
    Ok(ExitCode::SUCCESS)
}
