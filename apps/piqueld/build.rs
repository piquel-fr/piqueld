//! Provisions the migrated `SQLite` schema used by `SQLx` compile-time query
//! checks and, under `--features embedded-ui`, embeds the Trunk dashboard
//! bundle into the daemon binary.

use std::{
    env,
    error::Error,
    fmt::Write as _,
    fs,
    io::{self},
    path::{Path, PathBuf},
    process::Command,
};

use sha2::{Digest, Sha256};
use sqlx::{
    Connection,
    sqlite::{SqliteConnectOptions, SqliteConnection},
};

fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .ok_or("Cargo did not provide CARGO_MANIFEST_DIR to the piqueld build script")?,
    );
    embed_migrations(&manifest_dir)?;

    if env::var_os("CARGO_FEATURE_EMBEDDED_UI").is_some() {
        embed_dashboard(&manifest_dir)?;
    }
    Ok(())
}

fn embed_migrations(manifest_dir: &Path) -> Result<(), Box<dyn Error>> {
    let migrations_dir = manifest_dir.join("../../migrations").canonicalize()?;
    println!("cargo:rerun-if-changed={}", migrations_dir.display());

    let mut migrations = fs::read_dir(&migrations_dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    migrations.retain(|path| path.extension().is_some_and(|extension| extension == "sql"));
    migrations.sort();

    let migrations = migrations
        .into_iter()
        .map(|path| {
            println!("cargo:rerun-if-changed={}", path.display());
            let sql = fs::read_to_string(&path)?;
            Ok((path, sql))
        })
        .collect::<Result<Vec<_>, std::io::Error>>()?;

    let mut expected_version = 0_u64;
    for (path, _) in &migrations {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                io::Error::other(format!(
                    "migration path is not valid UTF-8: {}",
                    path.display()
                ))
            })?;
        let version = file_name
            .split_once('_')
            .and_then(|(prefix, _)| prefix.parse::<u64>().ok())
            .ok_or_else(|| {
                io::Error::other(format!(
                    "migration file name must start with a numeric version prefix: {file_name}"
                ))
            })?;
        expected_version += 1;
        if version != expected_version {
            return Err(io::Error::other(format!(
                "migration numbers must be contiguous starting at 1: expected {expected_version}, found {version} in {file_name}"
            ))
            .into());
        }
    }

    let out_dir = PathBuf::from(
        env::var_os("OUT_DIR")
            .ok_or("Cargo did not provide OUT_DIR to the piqueld build script")?,
    );
    let mut embedded_migrations = String::from("&[\n");
    for (path, _) in &migrations {
        let path = path.to_str().ok_or_else(|| {
            io::Error::other(format!(
                "migration path is not valid UTF-8: {}",
                path.display()
            ))
        })?;
        writeln!(embedded_migrations, "    include_str!({path:?}),")?;
    }
    embedded_migrations.push_str("]\n");
    fs::write(out_dir.join("migrations.rs"), embedded_migrations)?;

    let database_path = out_dir.join("sqlx-build.db");
    match fs::remove_file(&database_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let options = SqliteConnectOptions::new()
            .filename(&database_path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut connection = SqliteConnection::connect_with(&options).await?;
        for (path, migration) in &migrations {
            sqlx::raw_sql(migration)
                .execute(&mut connection)
                .await
                .map_err(|error| {
                    io::Error::other(format!("failed to apply {}: {error}", path.display()))
                })?;
        }
        connection.close().await?;
        Ok::<(), Box<dyn Error>>(())
    })?;

    println!(
        "cargo:rustc-env=DATABASE_URL=sqlite://{}",
        database_path.display()
    );
    println!("cargo:rustc-env=SQLX_OFFLINE=false");
    Ok(())
}

/// Resolves, builds if needed, and embeds the dashboard bundle.
///
/// `PIQUELD_UI_DIST` supplies a prebuilt Trunk distribution and skips tool
/// invocation entirely; hermetic packagers such as Nix use that escape hatch.
fn embed_dashboard(manifest_dir: &Path) -> Result<(), Box<dyn Error>> {
    let ui_dir = manifest_dir
        .parent()
        .ok_or_else(|| io::Error::other("piqueld manifest has no parent directory"))?
        .join("piqueld-ui")
        .canonicalize()?;
    println!("cargo:rerun-if-env-changed=PIQUELD_UI_DIST");
    for file in walk_files(&ui_dir)? {
        // The Tailwind output below `generated/` is a build product written
        // by this script, not a source; tracking it would couple successive
        // builds to this script's own writes.
        if file.starts_with(ui_dir.join("generated")) {
            continue;
        }
        println!("cargo:rerun-if-changed={}", file.display());
    }
    let lockfile = ui_dir.join("../../Cargo.lock").canonicalize()?;
    println!("cargo:rerun-if-changed={}", lockfile.display());

    let out_dir = PathBuf::from(
        env::var_os("OUT_DIR")
            .ok_or("Cargo did not provide OUT_DIR to the piqueld build script")?,
    );
    let prebuilt = env::var_os("PIQUELD_UI_DIST");
    let dist = match prebuilt.as_ref() {
        Some(prebuilt) => PathBuf::from(prebuilt),
        None => build_bundle(&ui_dir, &out_dir)?,
    };
    let dist = dist.canonicalize().map_err(|error| {
        io::Error::other(format!(
            "dashboard distribution {} is unreadable: {error}",
            dist.display()
        ))
    })?;
    if !dist.join("index.html").is_file() {
        return Err(io::Error::other(format!(
            "dashboard distribution {} has no index.html",
            dist.display()
        ))
        .into());
    }
    // Only an externally supplied distribution is an input; the bundle built
    // into OUT_DIR is this script's own output.
    if prebuilt.is_some() {
        for file in walk_files(&dist)? {
            println!("cargo:rerun-if-changed={}", file.display());
        }
    }
    write_bundle_module(&dist, &out_dir)?;
    Ok(())
}

/// Runs Tailwind and Trunk to produce the release dashboard bundle.
fn build_bundle(ui_dir: &Path, out_dir: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let trunk_input = prepare_trunk_input(ui_dir, out_dir)?;
    let dist = out_dir.join("dashboard-dist");
    run_trunk(ui_dir, out_dir, &trunk_input, &dist)?;
    Ok(dist)
}

/// Regenerates the Tailwind stylesheet consumed by the dashboard shell.
///
/// The generated stylesheet and staged Trunk shell live entirely in `OUT_DIR`,
/// allowing release builds from read-only source trees.
fn prepare_trunk_input(ui_dir: &Path, out_dir: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let generated = out_dir.join("generated");
    fs::create_dir_all(&generated)?;
    let stylesheet = generated.join("style.css");
    let child = Command::new("tailwindcss")
        .arg("--input")
        .arg(ui_dir.join("tailwind.css"))
        .arg("--output")
        .arg("-")
        .arg("--minify")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| missing_tool("tailwindcss", &error))?;
    let output = child.wait_with_output()?;
    ensure_success("tailwindcss", output.status)?;
    fs::write(&stylesheet, output.stdout)?;
    let manifest = ui_dir.join("Cargo.toml").canonicalize()?;
    let manifest = relative_path(out_dir, &manifest)?;
    let shell = fs::read_to_string(ui_dir.join("index.html"))?;
    let shell = shell.replace(
        "<link data-trunk rel=\"rust\"",
        &format!(
            "<link data-trunk rel=\"rust\" href=\"{}\"",
            manifest.display()
        ),
    );
    let input = out_dir.join("dashboard-index.html");
    fs::write(&input, shell)?;
    Ok(input)
}

fn relative_path(from: &Path, to: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let from = from.canonicalize()?;
    let from = from.components().collect::<Vec<_>>();
    let to = to.components().collect::<Vec<_>>();
    let common = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = PathBuf::new();
    for _ in common..from.len() {
        relative.push("..");
    }
    for component in &to[common..] {
        relative.push(component.as_os_str());
    }
    if relative.as_os_str().is_empty() {
        return Err(io::Error::other("dashboard manifest cannot equal OUT_DIR").into());
    }
    Ok(relative)
}

/// Runs Trunk inside the UI crate directory.
///
/// Trunk shells out to Cargo for the wasm32 compilation. That nested Cargo
/// must never share this build's target directory: the outer Cargo holds its
/// build-directory lock until the build script returns, so sharing would
/// deadlock. The nested invocation therefore compiles into a sibling
/// directory, and Cargo's jobserver and flag plumbing are stripped because
/// they describe the host build rather than the wasm build.
fn run_trunk(
    ui_dir: &Path,
    out_dir: &Path,
    input: &Path,
    dist: &Path,
) -> Result<(), Box<dyn Error>> {
    let cargo_target_dir = isolated_ui_target(out_dir)?;
    println!(
        "cargo:warning=building embedded dashboard bundle (Trunk target dir: {})",
        cargo_target_dir.display()
    );

    let output = Command::new("trunk")
        .current_dir(ui_dir)
        .arg("build")
        .arg(input)
        .arg("--release")
        .arg("--locked")
        .arg("--public-url")
        .arg("/dashboard/")
        .arg("--dist")
        .arg(dist)
        .env_remove("NO_COLOR")
        .env_remove("CARGO_MAKEFLAGS")
        .env_remove("RUSTFLAGS")
        .env_remove("RUSTDOCFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env("CARGO_TARGET_DIR", &cargo_target_dir)
        .output()
        .map_err(|error| missing_tool("trunk", &error))?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "trunk failed to build the dashboard bundle:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ))
        .into());
    }
    Ok(())
}

/// Derives a dedicated Cargo target directory for the nested wasm build from
/// the canonical `OUT_DIR` layout `<target>/<profile>/build/<pkg>-<hash>/out`.
fn isolated_ui_target(out_dir: &Path) -> Result<PathBuf, Box<dyn Error>> {
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
    Ok(parent.join(format!("{name}-ui")))
}

/// Writes the generated module embedding every bundle file by absolute path.
fn write_bundle_module(dist: &Path, out_dir: &Path) -> Result<(), Box<dyn Error>> {
    let mut files = walk_files(dist)?;
    files.sort();
    let content_security_policy = content_security_policy(&dist.join("index.html"))?;
    let mut module = String::from(
        "// Generated by the piqueld build script; do not edit.\n\
         /// Dashboard bundle compiled from the Trunk distribution.\n\
         pub static BUNDLE: &[(&str, &[u8])] = &[\n",
    );
    for file in files {
        let relative = file.strip_prefix(dist)?;
        let name = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let path = file.to_str().ok_or_else(|| {
            io::Error::other(format!(
                "dashboard asset path is not valid UTF-8: {}",
                file.display()
            ))
        })?;
        writeln!(module, "    ({name:?}, include_bytes!({path:?})),")?;
    }
    module.push_str("];\n");
    writeln!(
        module,
        "\
         /// Content-Security-Policy matching the inline scripts in the bundle shell.\n\
         pub const DASHBOARD_CONTENT_SECURITY_POLICY: &str = {content_security_policy:?};"
    )?;
    fs::write(out_dir.join("ui_assets.rs"), module)?;
    Ok(())
}

/// Builds the dashboard CSP from the final Trunk shell, authorizing only its
/// exact inline scripts.
fn content_security_policy(index_path: &Path) -> Result<String, Box<dyn Error>> {
    let html = fs::read(index_path)?;
    let mut policy = String::from("default-src 'self'; script-src 'self'");
    for hash in inline_script_hashes(&html)? {
        write!(policy, " 'sha256-{hash}'")?;
    }
    policy.push_str(
        " 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data:; \
         font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; \
         form-action 'none'; frame-ancestors 'none'",
    );
    Ok(policy)
}

fn inline_script_hashes(html: &[u8]) -> Result<Vec<String>, Box<dyn Error>> {
    let html = std::str::from_utf8(html)?;
    let mut hashes = Vec::new();
    let mut offset = 0;
    while let Some(relative_start) = html[offset..].find("<script") {
        let start = offset + relative_start;
        let opening_end = start
            + html[start..].find('>').ok_or_else(|| {
                io::Error::other("dashboard shell has an unterminated script tag")
            })?;
        let content_start = opening_end + 1;
        let close_start = content_start
            + html[content_start..].find("</script>").ok_or_else(|| {
                io::Error::other("dashboard shell has an unterminated inline script")
            })?;
        let opening_tag = &html[start..=opening_end];
        if !opening_tag
            .split(|character: char| character.is_ascii_whitespace() || "<>=".contains(character))
            .any(|attribute| attribute.eq_ignore_ascii_case("src"))
        {
            let digest = Sha256::digest(&html.as_bytes()[content_start..close_start]);
            hashes.push(base64_encode(&digest));
        }
        offset = close_start + "</script>".len();
    }
    Ok(hashes)
}

/// Encodes a SHA-256 digest as the standard base64 form required by CSP.
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or_default();
        let third = chunk.get(2).copied().unwrap_or_default();
        encoded.push(TABLE[usize::from(first >> 2)] as char);
        encoded.push(TABLE[usize::from((first & 0b11) << 4 | second >> 4)] as char);
        if chunk.len() > 1 {
            encoded.push(TABLE[usize::from((second & 0b1111) << 2 | third >> 6)] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(TABLE[usize::from(third & 0b0011_1111)] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
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

fn missing_tool(tool: &str, error: &io::Error) -> io::Error {
    io::Error::other(format!(
        "{tool} is required to embed the dashboard bundle (--features \
         embedded-ui); install it together with the wasm32-unknown-unknown \
         Rust target: {error}"
    ))
}

fn ensure_success(tool: &str, status: std::process::ExitStatus) -> Result<(), Box<dyn Error>> {
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{tool} exited with {status}")).into())
    }
}
