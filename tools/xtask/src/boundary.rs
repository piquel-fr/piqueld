//! `just boundary`: checks the workspace's dependency boundaries.
//! `piqueld-core` holds the shared contracts, so it must not depend on the
//! server, Docker, database, or browser frameworks. The daemon's own crates
//! build in parallel only while they don't depend on each other.

use std::process::ExitCode;

use anyhow::{Context, Result, bail};

use crate::Workspace;
use crate::process::checked_stdout;

/// A package that must not depend on any of `forbidden`, through the
/// dependency kinds of `edges`.
struct Boundary {
    package: &'static str,
    /// `cargo tree --edges` kinds.
    edges: &'static str,
    forbidden: &'static [&'static str],
    /// Why, as the error says when the boundary is crossed.
    reason: &'static str,
}

const BOUNDARIES: [Boundary; 3] = [
    Boundary {
        package: "piqueld-core",
        // The client and dashboard build it, so its build and development
        // dependencies count too.
        edges: "all",
        forbidden: &[
            "axum",
            "axum-core",
            "bollard",
            "bollard-stubs",
            "leptos",
            "sqlx",
            "sqlx-core",
        ],
        reason: "holds the shared contracts, without server, Docker, database, or browser frameworks",
    },
    Boundary {
        package: "piqueld-config",
        edges: "normal,build",
        forbidden: &["bollard", "leptos", "sqlx", "piqueld"],
        reason: "builds before the daemon's Docker, store, and server crates",
    },
    Boundary {
        package: "piqueld-store",
        edges: "normal,build",
        forbidden: &["axum", "bollard", "leptos", "piqueld"],
        reason: "builds alongside the daemon's Docker crate, without a server, Docker, or browser framework",
    },
];

pub fn check(workspace: &Workspace) -> Result<ExitCode> {
    let mut crossed = false;
    for boundary in &BOUNDARIES {
        let output = workspace
            .cargo()
            .args(["tree", "--locked", "--package", boundary.package])
            .args(["--edges", boundary.edges, "--prefix", "none"])
            .output()
            .context("run cargo tree")?;
        let tree = checked_stdout("cargo tree", output)?;
        let forbidden: Vec<&str> = tree
            .lines()
            .filter(|line| {
                boundary
                    .forbidden
                    .contains(&line.split(' ').next().unwrap_or_default())
            })
            .collect();
        if forbidden.is_empty() {
            println!("{} dependency boundary is intact", boundary.package);
        } else {
            eprintln!(
                "{} {}, but depends on:\n{}",
                boundary.package,
                boundary.reason,
                forbidden.join("\n")
            );
            crossed = true;
        }
    }
    if crossed {
        bail!("a dependency boundary is crossed");
    }
    Ok(ExitCode::SUCCESS)
}
