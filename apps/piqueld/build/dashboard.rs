//! Builds the dashboard bundle and embeds it into the daemon (`embedded-ui`).

use std::{
    env,
    error::Error,
    fmt::Write as _,
    fs, io,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::nested::NestedCargo;

/// Builds the dashboard bundle and writes `OUT_DIR/ui_assets.rs`, which embeds
/// every bundle file into the daemon.
pub fn embed(manifest_dir: &Path) -> Result<(), Box<dyn Error>> {
    let dashboard = Dashboard::new(manifest_dir)?;
    dashboard.track_inputs()?;
    let dist = dashboard.build()?;
    dashboard.write_bundle_module(&dist)
}

/// The dashboard bundle: `piqueld-ui` compiled for wasm32 by a nested Cargo,
/// bound by wasm-bindgen in-process, and loaded by a generated shell. Cargo is
/// the only tool involved.
///
/// Every file except the shell carries a content hash, so the daemon can serve
/// it as immutable:
///
/// ```text
/// index.html                  shell, from apps/piqueld-ui/index.html
/// start-<hash>.js             loader module that initialises the bindings
/// piqueld-ui-<hash>.js        wasm-bindgen JavaScript bindings
/// piqueld-ui-<hash>_bg.wasm   the dashboard itself
/// style-<hash>.css            `STYLESHEETS`, concatenated in cascade order
/// snippets/...                `inline_js` modules emitted by wasm-bindgen
/// ```
///
/// The shell has no inline scripts, so the Content-Security-Policy is a
/// constant. Running Cargo from a build script is a stopgap until artifact
/// dependencies stabilise; see `docs/dev/architecture/adr/0002-cargo-built-dashboard.md`.
struct Dashboard {
    /// Canonical `apps/piqueld-ui` directory.
    ui_dir: PathBuf,
    /// This build script's `OUT_DIR`.
    out_dir: PathBuf,
}

impl Dashboard {
    /// Target triple of the nested build.
    const TARGET: &str = "wasm32-unknown-unknown";
    /// Cargo profile of the nested build, defined in the workspace manifest.
    const PROFILE: &str = "dashboard";
    /// Every stylesheet in `apps/piqueld-ui/styles/`, in cascade order.
    const STYLESHEETS: [&str; 4] = ["reset.css", "theme.css", "layout.css", "components.css"];

    fn new(manifest_dir: &Path) -> Result<Self, Box<dyn Error>> {
        let ui_dir = manifest_dir
            .parent()
            .ok_or_else(|| io::Error::other("piqueld manifest has no parent directory"))?
            .join("piqueld-ui")
            .canonicalize()?;
        Ok(Self {
            ui_dir,
            out_dir: crate::out_dir()?,
        })
    }

    /// Reruns this script when anything the nested build reads changes.
    ///
    /// Cargo cannot infer these inputs because a separate Cargo invocation
    /// compiles them. Directories are scanned recursively, so new files count.
    fn track_inputs(&self) -> Result<(), Box<dyn Error>> {
        for input in [
            "",
            "../../Cargo.toml",
            "../../Cargo.lock",
            "../../crates/piqueld-core",
            "../../crates/piqueld-client",
        ] {
            println!(
                "cargo:rerun-if-changed={}",
                self.ui_dir.join(input).canonicalize()?.display()
            );
        }
        Ok(())
    }

    /// Builds the bundle into `OUT_DIR/dashboard-dist`, replacing any earlier one.
    fn build(&self) -> Result<PathBuf, Box<dyn Error>> {
        let dist = self.out_dir.join("dashboard-dist");
        if dist.exists() {
            fs::remove_dir_all(&dist)?;
        }
        fs::create_dir_all(&dist)?;

        let wasm = self.compile()?;
        // The bindings are a pure function of the module and the pinned
        // wasm-bindgen version, so the module's digest names both.
        let bindings = format!("piqueld-ui-{}", digest(&fs::read(&wasm)?));
        wasm_bindgen_cli_support::Bindgen::new()
            .input_path(&wasm)
            .web(true)?
            .typescript(false)
            // Unlike the CLI, the library omits the bindings' default module
            // path unless asked; the loader relies on it to find the module.
            .omit_default_module_path(false)
            .out_name(&bindings)
            .generate(&dist)?;

        let stylesheet = write_hashed(&dist, "style", "css", self.stylesheet()?.as_bytes())?;
        let loader = format!("import init from \"./{bindings}.js\";\nawait init();\n");
        let loader = write_hashed(&dist, "start", "js", loader.as_bytes())?;
        let mut shell = fs::read_to_string(self.ui_dir.join("index.html"))?;
        for (placeholder, file) in [
            ("{{stylesheet}}", stylesheet),
            ("{{bindings}}", format!("{bindings}.js")),
            ("{{wasm}}", format!("{bindings}_bg.wasm")),
            ("{{loader}}", loader),
        ] {
            if !shell.contains(placeholder) {
                return Err(io::Error::other(format!(
                    "apps/piqueld-ui/index.html has no {placeholder} placeholder"
                ))
                .into());
            }
            shell = shell.replace(placeholder, &file);
        }
        fs::write(dist.join("index.html"), shell)?;
        Ok(dist)
    }

    /// Returns the dashboard module: `PIQUELD_DASHBOARD_WASM` when set, as the
    /// Nix packages do with their own cached build, or else `piqueld-ui`
    /// compiled for wasm32 with the `dashboard` profile.
    fn compile(&self) -> Result<PathBuf, Box<dyn Error>> {
        println!("cargo:rerun-if-env-changed=PIQUELD_DASHBOARD_WASM");
        if let Some(wasm) = env::var_os("PIQUELD_DASHBOARD_WASM") {
            let wasm = PathBuf::from(wasm);
            println!(
                "cargo:warning=embedding the prebuilt dashboard {} from PIQUELD_DASHBOARD_WASM; \
                 changes under apps/piqueld-ui are not included",
                wasm.display()
            );
            println!("cargo:rerun-if-changed={}", wasm.display());
            return Ok(wasm);
        }
        Ok(NestedCargo::new("ui")?
            .build(&self.ui_dir, "piqueld-ui", Self::PROFILE, Self::TARGET)?
            .join("piqueld-ui.wasm"))
    }

    /// Concatenates `STYLESHEETS` into one sheet, rejecting stylesheets that
    /// are missing from the list so none is silently left out.
    fn stylesheet(&self) -> Result<String, Box<dyn Error>> {
        let styles = self.ui_dir.join("styles");
        for file in walk_files(&styles)? {
            let name = file.file_name().and_then(|name| name.to_str());
            if !name.is_some_and(|name| Self::STYLESHEETS.contains(&name)) {
                return Err(io::Error::other(format!(
                    "{} is not listed in the build script's STYLESHEETS",
                    file.display()
                ))
                .into());
            }
        }
        let mut sheet = String::new();
        for name in Self::STYLESHEETS {
            sheet.push_str(&fs::read_to_string(styles.join(name))?);
            sheet.push('\n');
        }
        Ok(sheet)
    }

    /// Writes `OUT_DIR/ui_assets.rs`, embedding every bundle file by absolute path.
    fn write_bundle_module(&self, dist: &Path) -> Result<(), Box<dyn Error>> {
        let mut module = String::from(
            "// Generated by the piqueld build script; do not edit.\n\
             /// Dashboard bundle compiled by the piqueld build script.\n\
             pub static BUNDLE: &[(&str, &[u8])] = &[\n",
        );
        for file in walk_files(dist)? {
            let relative = file.strip_prefix(dist)?;
            let name = relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let path = crate::utf8(&file)?;
            writeln!(module, "    ({name:?}, include_bytes!({path:?})),")?;
        }
        module.push_str("];\n");
        fs::write(self.out_dir.join("ui_assets.rs"), module)?;
        Ok(())
    }
}

/// Writes `contents` to `<dir>/<stem>-<digest>.<extension>` and returns the
/// file name.
fn write_hashed(
    dir: &Path,
    stem: &str,
    extension: &str,
    contents: &[u8],
) -> Result<String, Box<dyn Error>> {
    let name = format!("{stem}-{}.{extension}", digest(contents));
    fs::write(dir.join(&name), contents)?;
    Ok(name)
}

/// Short content digest for asset names: the first 8 bytes of SHA-256 in hex.
fn digest(contents: &[u8]) -> String {
    Sha256::digest(contents)[..8]
        .iter()
        .fold(String::with_capacity(16), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Collects every regular file below `directory` in deterministic order.
fn walk_files(directory: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut files = Vec::new();
    let mut pending = vec![directory.to_owned()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let path = entry.path();
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}
