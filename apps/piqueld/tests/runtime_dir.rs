//! Runtime directory validation, socket recovery, and independent-daemon exclusion.

use piqueld::RuntimeDir;
use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

struct Fixture {
    _directory: tempfile::TempDir,
    runtime: PathBuf,
}

impl Fixture {
    fn new(mode: u32) -> Self {
        let directory = tempfile::tempdir_in(".").unwrap();
        let runtime = directory.path().join("run");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(mode)).unwrap();
        Self {
            _directory: directory,
            runtime,
        }
    }

    fn socket(&self) -> PathBuf {
        self.runtime.join("piqueld.sock")
    }

    async fn acquire(&self) -> anyhow::Result<RuntimeDir> {
        Self::acquire_at(&self.runtime).await
    }

    async fn acquire_at(path: &Path) -> anyhow::Result<RuntimeDir> {
        // Only inspect the owned fixture tree in Nix's user namespace.
        let cwd = std::env::current_dir().unwrap();
        RuntimeDir::acquire(path.strip_prefix(cwd).unwrap_or(path)).await
    }
}

#[tokio::test]
async fn socket_uses_effective_group_and_0660_in_shared_and_private_directories() {
    for mode in [0o750, 0o700] {
        let fixture = Fixture::new(mode);
        let runtime = fixture.acquire().await.unwrap();
        let _listener = runtime.bind_api().await.unwrap();
        let socket = std::fs::metadata(fixture.socket()).unwrap();
        assert_eq!(socket.permissions().mode() & 0o777, 0o660);
        assert_eq!(socket.uid(), rustix::process::geteuid().as_raw());
        assert_eq!(socket.gid(), rustix::process::getegid().as_raw());
        assert_eq!(
            std::fs::metadata(&fixture.runtime)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            mode
        );
        tokio::net::UnixStream::connect(fixture.socket())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn missing_runtime_directory_is_not_created() {
    let fixture = Fixture::new(0o700);
    let missing = fixture.runtime.join("missing");
    let error = Fixture::acquire_at(&missing).await.unwrap_err();
    assert!(format!("{error:#}").contains("prepare it before starting"));
    assert!(!missing.exists());
}

#[tokio::test]
async fn unsafe_permissions_are_rejected_without_modification() {
    for mode in [0o770, 0o755, 0o707, 0o777] {
        let fixture = Fixture::new(mode);
        let error = fixture.acquire().await.unwrap_err();
        assert!(format!("{error:#}").contains("must grant no group write or other access"));
        assert_eq!(
            std::fs::metadata(&fixture.runtime)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            mode
        );
    }
}

#[tokio::test]
async fn symlinks_and_unsafe_ancestors_are_rejected() {
    let fixture = Fixture::new(0o700);
    let link = fixture.runtime.with_file_name("link");
    std::os::unix::fs::symlink(&fixture.runtime, &link).unwrap();
    let error = Fixture::acquire_at(&link).await.unwrap_err();
    assert!(format!("{error:#}").contains("not a real directory"));
    let child = fixture.runtime.join("child");
    std::fs::create_dir(&child).unwrap();
    std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700)).unwrap();
    let error = Fixture::acquire_at(&link.join("child")).await.unwrap_err();
    assert!(format!("{error:#}").contains("not a real directory"));
    std::fs::set_permissions(&fixture.runtime, std::fs::Permissions::from_mode(0o777)).unwrap();
    let error = Fixture::acquire_at(&child).await.unwrap_err();
    assert!(format!("{error:#}").contains("not protected from replacement"));
}

#[tokio::test]
async fn active_listener_without_a_directory_lock_is_preserved() {
    let fixture = Fixture::new(0o700);
    let listener = tokio::net::UnixListener::bind(fixture.socket()).unwrap();
    let inode = std::fs::metadata(fixture.socket()).unwrap().ino();
    let runtime = fixture.acquire().await.unwrap();
    let error = runtime.bind_api().await.unwrap_err();
    assert!(format!("{error:#}").contains("active listener"));
    assert_eq!(std::fs::metadata(fixture.socket()).unwrap().ino(), inode);
    let _probe = listener.accept().await.unwrap();
}

#[tokio::test]
async fn stale_socket_is_recovered_after_owner_exits() {
    let fixture = Fixture::new(0o700);
    let runtime = fixture.acquire().await.unwrap();
    let listener = runtime.bind_api().await.unwrap();
    drop(listener);
    drop(runtime);
    let runtime = fixture.acquire().await.unwrap();
    let _listener = runtime.bind_api().await.unwrap();
    tokio::net::UnixStream::connect(fixture.socket())
        .await
        .unwrap();
}

#[tokio::test]
async fn regular_files_and_symlinks_at_socket_path_are_preserved() {
    let fixture = Fixture::new(0o700);
    let runtime = fixture.acquire().await.unwrap();
    std::fs::write(fixture.socket(), b"keep this").unwrap();
    let error = runtime.bind_api().await.unwrap_err();
    assert!(format!("{error:#}").contains("refusing to replace non-socket"));
    assert_eq!(std::fs::read(fixture.socket()).unwrap(), b"keep this");
    std::fs::remove_file(fixture.socket()).unwrap();
    let target = fixture.runtime.join("target.sock");
    let _listener = tokio::net::UnixListener::bind(&target).unwrap();
    std::os::unix::fs::symlink(&target, fixture.socket()).unwrap();
    assert!(runtime.bind_api().await.is_err());
    assert!(
        std::fs::symlink_metadata(fixture.socket())
            .unwrap()
            .is_symlink()
    );
}

#[tokio::test]
async fn competing_daemon_with_different_state_cannot_replace_socket_or_open_database() {
    let fixture = Fixture::new(0o700);
    let runtime = fixture.acquire().await.unwrap();
    let _listener = runtime.bind_api().await.unwrap();
    let inode = std::fs::metadata(fixture.socket()).unwrap().ino();
    let state = fixture.runtime.with_file_name("different-state");
    let config = fixture.runtime.with_file_name("config.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\ndata_dir={state:?}\nruntime_dir={:?}\n",
            fixture.runtime
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_piqueld"))
        .arg("--config")
        .arg(config)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(
        error.contains("failed to lock runtime directory"),
        "{error}"
    );
    assert!(error.contains("directory already in use"), "{error}");
    assert!(!state.join("piqueld.db").exists());
    assert_eq!(std::fs::metadata(fixture.socket()).unwrap().ino(), inode);
}
