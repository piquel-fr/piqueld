//! Nested Cargo builds of artifacts the daemon embeds but compiles for another
//! target: the dashboard (wasm32) and the health probe (musl).

use std::{
    env,
    error::Error,
    io,
    path::{Path, PathBuf},
    process::Command,
};

/// A nested build into its own target directory beside the outer one.
///
/// The nested Cargo must never share this build's target directory: the outer
/// Cargo holds its build-directory lock until the build script returns, so
/// sharing would deadlock. The nested Cargo joins the outer build's jobserver
/// (`CARGO_MAKEFLAGS`), so both share one limit on parallel jobs.
///
/// The outer rustflags, rustc wrappers, and profile overrides are stripped
/// because they describe the host build: host rustflags such as
/// `-C target-cpu=native` or linker arguments break a wasm32 build, and would
/// also override the nested target's own rustflags
/// (`CARGO_TARGET_<TRIPLE>_RUSTFLAGS`, `target.<triple>.rustflags`), which
/// still apply; `cargo clippy` sets `RUSTC_WORKSPACE_WRAPPER`. The nested
/// profile alone defines the rest.
pub struct NestedCargo {
    /// `<outer target dir>-<suffix>`, e.g. `target-ui`.
    target_dir: PathBuf,
}

impl NestedCargo {
    /// Prepares a nested build into `<outer target dir>-<suffix>`.
    pub fn new(suffix: &str) -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            target_dir: Self::sibling_target_dir(&crate::out_dir()?, suffix)?,
        })
    }

    /// Builds `package` with `profile` for `target` from `dir`, and returns the
    /// directory holding its artifacts.
    pub fn build(
        &self,
        dir: &Path,
        package: &str,
        profile: &str,
        target: &str,
    ) -> Result<PathBuf, Box<dyn Error>> {
        const HOST_SETTINGS: [&str; 6] = [
            "RUSTFLAGS",
            "RUSTDOCFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_BUILD_RUSTFLAGS",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
        ];
        let cargo = env::var_os("CARGO")
            .ok_or("Cargo did not provide CARGO to the piqueld build script")?;
        println!(
            "cargo:warning=building embedded {package} (nested target dir: {})",
            self.target_dir.display()
        );
        let mut command = Command::new(cargo);
        command
            .current_dir(dir)
            .args(["build", "--locked", "--package", package])
            .args(["--profile", profile, "--target", target])
            .env("CARGO_TARGET_DIR", &self.target_dir);
        for (key, _) in env::vars_os() {
            if key.to_str().is_some_and(|key| {
                HOST_SETTINGS.contains(&key) || key.starts_with("CARGO_PROFILE_")
            }) {
                command.env_remove(key);
            }
        }
        let output = command.output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "cargo failed to build {package}; is the {target} target installed \
                 (`rustup target add {target}`)?\n{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            ))
            .into());
        }
        Ok(self.target_dir.join(target).join(profile))
    }

    /// Derives `<target dir>-<suffix>` from the canonical `OUT_DIR` layout
    /// `<target>/<profile>/build/<pkg>-<hash>/out`.
    fn sibling_target_dir(out_dir: &Path, suffix: &str) -> Result<PathBuf, Box<dyn Error>> {
        const OUT_TO_TARGET_DEPTH: usize = 4;
        let target_dir = out_dir
            .canonicalize()?
            .ancestors()
            .nth(OUT_TO_TARGET_DEPTH)
            .ok_or_else(|| io::Error::other("unexpected OUT_DIR depth for target-dir derivation"))?
            .to_owned();
        if !target_dir.is_dir() {
            return Err(io::Error::other(format!(
                "derived Cargo target directory {} does not exist",
                target_dir.display()
            ))
            .into());
        }
        let name = target_dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| io::Error::other("Cargo target directory name is not valid UTF-8"))?
            .to_owned();
        let parent = target_dir
            .parent()
            .ok_or_else(|| io::Error::other("Cargo target directory has no parent"))?
            .to_owned();
        Ok(parent.join(format!("{name}-{suffix}")))
    }
}
