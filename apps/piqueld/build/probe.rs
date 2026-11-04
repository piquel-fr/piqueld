//! Embeds the static `piqueld-probe` that HTTP health checks run in services.

use std::{env, error::Error, fs, path::Path};

use crate::nested::NestedCargo;

/// Builds `piqueld-probe` for the musl target matching the daemon's
/// architecture with the size-optimised `probe` profile of `build/probe.toml`,
/// and points `PIQUELD_PROBE_BINARY` at a copy of it in `OUT_DIR`.
///
/// The probe's sources come from its crate's build script, as
/// `DEP_PIQUELD_PROBE_DIR`, so this works from the published crates too.
/// docs.rs builds have neither the musl target nor the network, and embed an
/// empty file instead.
pub fn embed(manifest_dir: &Path) -> Result<(), Box<dyn Error>> {
    let copy = crate::out_dir()?.join("piqueld-probe");
    println!("cargo:rustc-env=PIQUELD_PROBE_BINARY={}", copy.display());
    if env::var_os("DOCS_RS").is_some() {
        fs::write(&copy, [])?;
        return Ok(());
    }
    let probe_dir = env::var_os("DEP_PIQUELD_PROBE_DIR")
        .ok_or("piqueld-probe's build script did not export DEP_PIQUELD_PROBE_DIR")?;
    let target = format!("{}-unknown-linux-musl", env::var("CARGO_CFG_TARGET_ARCH")?);
    let built = NestedCargo::new("probe")?
        .build(
            Path::new(&probe_dir),
            "piqueld-probe",
            "probe",
            &target,
            Some(&manifest_dir.join("build/probe.toml")),
        )?
        .join("piqueld-probe");
    // Embed a copy that lives with this script's output. The nested build
    // writes `built` while this script runs, so tracking it would always look
    // newer than the last run and rebuild the daemon every time.
    fs::copy(&built, &copy)?;
    Ok(())
}
