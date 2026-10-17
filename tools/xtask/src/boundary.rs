//! `just boundary`: `piqueld-core` holds the shared contracts, so it must not
//! depend on the server, Docker, database, or browser frameworks.

use std::process::ExitCode;

use anyhow::{Context, Result, bail};

use crate::Workspace;
use crate::process::checked_stdout;

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
    let output = workspace
        .cargo()
        .args(["tree", "--locked", "--package", "piqueld-core"])
        .args(["--edges", "all", "--prefix", "none"])
        .output()
        .context("run cargo tree")?;
    let tree = checked_stdout("cargo tree", output)?;
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
