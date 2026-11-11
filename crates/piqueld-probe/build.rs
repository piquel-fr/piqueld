//! Exports this crate's directory to the build scripts of the crates that
//! depend on it, as `DEP_PIQUELD_PROBE_DIR`: `piqueld-docker`'s builds the
//! probe binary from it.

use std::env::{self, VarError};

fn main() -> Result<(), VarError> {
    println!("cargo::metadata=dir={}", env::var("CARGO_MANIFEST_DIR")?);
    Ok(())
}
