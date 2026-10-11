//! Embeds the static `piqueld-probe` that HTTP health checks run in services.

use std::{env, error::Error, fs, path::Path, path::PathBuf};

use crate::nested::NestedCargo;

/// Points `PIQUELD_PROBE_BINARY` at a static probe for the daemon's
/// architecture: `PIQUELD_PROBE` when set, as the Nix packages do with their
/// own static build, or else a copy in `OUT_DIR` of `piqueld-probe` built for
/// the matching musl target with the size-optimised `probe` profile.
pub fn embed(manifest_dir: &Path) -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-env-changed=PIQUELD_PROBE");
    let binary = if let Some(binary) = env::var_os("PIQUELD_PROBE") {
        let binary = PathBuf::from(binary);
        println!(
            "cargo:warning=embedding the prebuilt health probe {} from PIQUELD_PROBE; \
             changes under crates/piqueld-probe are not included",
            binary.display()
        );
        println!("cargo:rerun-if-changed={}", binary.display());
        binary
    } else {
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
        // writes `built` while this script runs, so tracking it would always
        // look newer than the last run and rebuild the daemon every time.
        let out_dir = PathBuf::from(
            env::var_os("OUT_DIR")
                .ok_or("Cargo did not provide OUT_DIR to the piqueld build script")?,
        );
        let copy = out_dir.join("piqueld-probe");
        fs::copy(&built, &copy)?;
        copy
    };
    println!("cargo:rustc-env=PIQUELD_PROBE_BINARY={}", binary.display());
    Ok(())
}
