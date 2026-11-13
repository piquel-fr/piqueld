//! Embeds the dashboard bundle into the daemon binary: built under
//! `--features embedded-ui`, or the prebuilt one a published crate ships.

use std::{
    env,
    error::Error,
    io,
    path::{Path, PathBuf},
};

#[path = "build/dashboard.rs"]
mod dashboard;

/// Embeds any dashboard bundle.
fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .ok_or("Cargo did not provide CARGO_MANIFEST_DIR to the piqueld build script")?,
    );
    dashboard::embed(&manifest_dir)
}

/// This build script's `OUT_DIR`, where everything it generates goes.
fn out_dir() -> Result<PathBuf, Box<dyn Error>> {
    Ok(PathBuf::from(env::var_os("OUT_DIR").ok_or(
        "Cargo did not provide OUT_DIR to the piqueld build script",
    )?))
}

/// `path` as UTF-8, as the generated `include_*!` calls that name it need.
fn utf8(path: &Path) -> io::Result<&str> {
    path.to_str()
        .ok_or_else(|| io::Error::other(format!("path is not valid UTF-8: {}", path.display())))
}
