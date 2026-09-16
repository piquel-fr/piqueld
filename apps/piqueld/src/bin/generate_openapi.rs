//! Generates or checks the `OpenAPI` specification and its Rust client bindings.

use anyhow::{Context, Result, ensure};
use piqueld::api::openapi_document;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const GENERATOR_VERSION: &str = "7.20.0";
const GENERATOR_SHA256: &str = "871e0155287a87b579ff31096b2d45b1f95a115edfe631411ec6cff4848d0f03";

/// Runs both stages before updating either checked-in artifact.
struct Generator {
    root: PathBuf,
}

impl Generator {
    fn run(&self, check: bool) -> Result<()> {
        let value = openapi_document();
        let paths = value
            .get("paths")
            .and_then(serde_json::Value::as_object)
            .context("OpenAPI document has no paths object")?;
        for path in paths.keys() {
            ensure!(
                path.starts_with("/api/"),
                "OpenAPI path is outside the API namespace: {path}"
            );
        }
        let document = format!("{}\n", serde_json::to_string_pretty(&value)?);
        let temporary = tempfile::tempdir().context("create API generation directory")?;
        let input = temporary.path().join("openapi.json");
        fs::write(&input, &document).context("write fresh OpenAPI generator input")?;

        let jar = self.generator_jar()?;
        let output = temporary.path().join("output");
        self.command(
            Command::new("java")
                .arg("-jar")
                .arg(jar)
                .args(["generate", "-g", "rust", "-i"])
                .arg(&input)
                .arg("-c")
                .arg(self.root.join("tools/client-codegen/config.json"))
                .arg("-t")
                .arg(self.root.join("tools/client-codegen/templates"))
                .arg("-o")
                .arg(&output)
                .args(["--global-property", "apis,apiDocs=false,apiTests=false"]),
        )?;

        let api_directory = output.join("src/apis");
        let generated = api_directory.join("default_api.rs");
        let files = fs::read_dir(&api_directory)
            .context("read generated API modules")?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        // Tags can split operations into multiple modules. Never silently drop them.
        ensure!(
            files == [generated.clone()],
            "expected a single default API module; update generation for the new API groups"
        );
        self.command(
            Command::new("rustfmt")
                .args(["--edition", "2024", "--config-path"])
                .arg(&self.root)
                .arg(&generated),
        )?;
        let client = fs::read_to_string(&generated).context("read formatted client bindings")?;

        let artifacts = [
            ("docs/openapi-v1.json", document),
            ("crates/piqueld-client/src/generated.rs", client),
        ];
        let mut stale = Vec::new();
        for (relative, contents) in artifacts {
            let path = self.root.join(relative);
            if check {
                match fs::read_to_string(&path) {
                    Ok(existing) if existing == contents => println!("{relative} is up to date"),
                    Ok(_) => stale.push(relative),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        stale.push(relative);
                    }
                    Err(error) => {
                        return Err(error).with_context(|| format!("read {}", path.display()));
                    }
                }
            } else {
                fs::write(&path, contents).with_context(|| format!("write {}", path.display()))?;
                println!("wrote {relative}");
            }
        }
        ensure!(
            stale.is_empty(),
            "generated artifacts are out of date: {}; run just generate",
            stale.join(", ")
        );
        Ok(())
    }

    fn generator_jar(&self) -> Result<PathBuf> {
        let cache = self.root.join("target/client-codegen");
        let filename = format!("openapi-generator-cli-{GENERATOR_VERSION}.jar");
        let jar = cache.join(&filename);
        if jar.try_exists().context("check cached OpenAPI Generator")? {
            Self::verify_jar(&jar)?;
        } else {
            fs::create_dir_all(&cache).context("create OpenAPI Generator cache")?;
            let download =
                tempfile::NamedTempFile::new_in(&cache).context("create generator download")?;
            let url = format!(
                "https://repo.maven.apache.org/maven2/org/openapitools/openapi-generator-cli/{GENERATOR_VERSION}/{filename}"
            );
            self.command(
                Command::new("curl")
                    .args([
                        "--fail",
                        "--location",
                        "--silent",
                        "--show-error",
                        &url,
                        "--output",
                    ])
                    .arg(download.path()),
            )?;
            Self::verify_jar(download.path())?;
            download
                .persist(&jar)
                .context("cache verified OpenAPI Generator")?;
        }
        Ok(jar)
    }

    fn verify_jar(path: &Path) -> Result<()> {
        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        ensure!(
            format!("{:x}", Sha256::digest(bytes)) == GENERATOR_SHA256,
            "OpenAPI Generator checksum mismatch: {}",
            path.display()
        );
        Ok(())
    }

    fn command(&self, command: &mut Command) -> Result<()> {
        command.current_dir(&self.root);
        let output = command
            .output()
            .with_context(|| format!("run {command:?}"))?;
        ensure!(
            output.status.success(),
            "{command:?} failed ({}):\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }
}

fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    ensure!(
        args.is_empty() || args == ["--check"],
        "usage: generate_openapi [--check]"
    );
    Generator {
        root: Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .context("locate repository root")?,
    }
    .run(!args.is_empty())
}
