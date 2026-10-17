//! Starting the real daemon binary.

/// Starts `piqueld` with the configuration at `config`, expects it to exit
/// unsuccessfully, and returns what it wrote to standard error.
pub fn refused_start(config: &std::path::Path) -> String {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_piqueld"))
        .arg("--config")
        .arg(config)
        .output()
        .expect("piqueld runs");
    assert!(!output.status.success(), "piqueld started");
    String::from_utf8(output.stderr).expect("piqueld output is UTF-8")
}
