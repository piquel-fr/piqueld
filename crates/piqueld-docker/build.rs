//! Embeds the static health probe, which services' HTTP health checks run.

use std::{env, error::Error, path::PathBuf};

#[path = "build/probe.rs"]
mod probe;

/// Embeds the health probe.
fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .ok_or("Cargo did not provide CARGO_MANIFEST_DIR to the piqueld-docker build script")?,
    );
    probe::embed(&manifest_dir)
}

/// This build script's `OUT_DIR`, where everything it generates goes.
fn out_dir() -> Result<PathBuf, Box<dyn Error>> {
    Ok(PathBuf::from(env::var_os("OUT_DIR").ok_or(
        "Cargo did not provide OUT_DIR to the piqueld-docker build script",
    )?))
}
