//! Provisions the migrated `SQLite` schema used by `SQLx` compile-time query checks.

use std::{env, error::Error, fmt::Write, fs, path::PathBuf};

use sqlx::{
    Connection,
    sqlite::{SqliteConnectOptions, SqliteConnection},
};

fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .ok_or("Cargo did not provide CARGO_MANIFEST_DIR to the piqueld build script")?,
    );
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
                std::io::Error::other(format!(
                    "migration path is not valid UTF-8: {}",
                    path.display()
                ))
            })?;
        let version = file_name
            .split_once('_')
            .and_then(|(prefix, _)| prefix.parse::<u64>().ok())
            .ok_or_else(|| {
                std::io::Error::other(format!(
                    "migration file name must start with a numeric version prefix: {file_name}"
                ))
            })?;
        expected_version += 1;
        if version != expected_version {
            return Err(std::io::Error::other(format!(
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
            std::io::Error::other(format!(
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
                    std::io::Error::other(format!("failed to apply {}: {error}", path.display()))
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
