//! Provisions the migrated `SQLite` schema used by `SQLx` compile-time query
//! checks and, under `--features embedded-ui`, builds and embeds the dashboard
//! bundle into the daemon binary.

use std::{
    env,
    error::Error,
    fmt::Write as _,
    fs,
    io::{self},
    path::{Path, PathBuf},
};

#[cfg(feature = "embedded-ui")]
#[path = "build/dashboard.rs"]
mod dashboard;

use sqlx::{
    Connection,
    sqlite::{SqliteConnectOptions, SqliteConnection},
};

/// Always provisions the migration schema; embeds the dashboard only when the
/// `embedded-ui` feature is enabled.
fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .ok_or("Cargo did not provide CARGO_MANIFEST_DIR to the piqueld build script")?,
    );
    embed_migrations(&manifest_dir)?;

    #[cfg(feature = "embedded-ui")]
    dashboard::embed(&manifest_dir)?;
    Ok(())
}

/// Embeds the workspace migrations and builds the `SQLx` check database.
///
/// 1. Collect `migrations/*.sql` in name order and require contiguous
///    numeric prefixes starting at 1 (`0001_init.sql`, `0002_…`).
/// 2. Write `OUT_DIR/migrations.rs`, an `include_str!` slice of the files.
/// 3. Recreate `OUT_DIR/sqlx-build.db`, apply every migration, and point
///    `DATABASE_URL` at it so `SQLx` macros check against the real schema.
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
