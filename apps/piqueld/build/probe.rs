//! Embeds the static `piqueld-probe` that HTTP health checks run in services.

use std::{env, error::Error, fs, path::Path, path::PathBuf};

use crate::nested::NestedCargo;

/// Builds `piqueld-probe` for the musl target matching the daemon's
/// architecture with the size-optimised `probe` profile, and points
/// `PIQUELD_PROBE_BINARY` at a copy of it in `OUT_DIR`.
pub fn embed(manifest_dir: &Path) -> Result<(), Box<dyn Error>> {
    let probe_dir = manifest_dir
        .join("../../crates/piqueld-probe")
        .canonicalize()?;
    for input in [
        probe_dir.clone(),
        manifest_dir.join("../../Cargo.toml").canonicalize()?,
        manifest_dir.join("../../Cargo.lock").canonicalize()?,
    ] {
        println!("cargo:rerun-if-changed={}", input.display());
    }
    let target = format!("{}-unknown-linux-musl", env::var("CARGO_CFG_TARGET_ARCH")?);
    let built = NestedCargo::new("probe")?
        .build(&probe_dir, "piqueld-probe", "probe", &target)?
        .join("piqueld-probe");
    // Embed a copy that lives with this script's output. The nested build
    // writes `built` while this script runs, so tracking it would always look
    // newer than the last run and rebuild the daemon every time.
    let out_dir = PathBuf::from(
        env::var_os("OUT_DIR")
            .ok_or("Cargo did not provide OUT_DIR to the piqueld build script")?,
    );
    let copy = out_dir.join("piqueld-probe");
    fs::copy(&built, &copy)?;
    println!("cargo:rustc-env=PIQUELD_PROBE_BINARY={}", copy.display());
    Ok(())
}
