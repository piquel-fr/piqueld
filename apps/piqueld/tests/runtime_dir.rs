//! Runtime directory validation, socket recovery, and independent-daemon exclusion.

#[path = "support/daemon.rs"]
mod daemon;
#[path = "support/fixture_dir.rs"]
mod fixture_dir;

use piqueld::RuntimeDir;
use std::{
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

struct Fixture {
    _directory: tempfile::TempDir,
    runtime: PathBuf,
}

impl Fixture {
    /// A runtime directory with permissions `mode`, below the working
    /// directory, see [`fixture_dir::relative`].
    fn new(mode: u32) -> Self {
        let directory = tempfile::tempdir_in(".").unwrap();
        let runtime = directory.path().join("run");
        std::fs::create_dir(&runtime).unwrap();
        fixture_dir::set_mode(&runtime, mode);
        Self {
            _directory: directory,
            runtime,
        }
    }

    /// The API socket, relative to the working directory so its path fits a
    /// socket address even in deeply nested checkouts.
    fn socket(&self) -> PathBuf {
        fixture_dir::relative(&self.runtime).join("piqueld.sock")
    }

    fn socket_inode(&self) -> u64 {
        std::fs::metadata(self.socket()).unwrap().ino()
    }

    async fn acquire(&self) -> anyhow::Result<RuntimeDir> {
        Self::acquire_at(&self.runtime).await
    }

    async fn acquire_at(path: &Path) -> anyhow::Result<RuntimeDir> {
        RuntimeDir::acquire(fixture_dir::relative(path)).await
    }

    /// Asserts acquiring `path` fails with an error mentioning `message`.
    async fn assert_refused(path: &Path, message: &str) {
        let error = Self::acquire_at(path).await.unwrap_err();
        assert!(format!("{error:#}").contains(message), "{error:#}");
    }
}

#[tokio::test]
async fn socket_uses_effective_group_and_0660_in_shared_and_private_directories() {
    for mode in [0o750, 0o700] {
        let fixture = Fixture::new(mode);
        let runtime = fixture.acquire().await.unwrap();
        let _listener = runtime.bind_api().await.unwrap();
        let socket = std::fs::metadata(fixture.socket()).unwrap();
        assert_eq!(fixture_dir::mode(&fixture.socket()), 0o660);
        assert_eq!(socket.uid(), rustix::process::geteuid().as_raw());
        assert_eq!(socket.gid(), rustix::process::getegid().as_raw());
        assert_eq!(fixture_dir::mode(&fixture.runtime), mode);
        tokio::net::UnixStream::connect(fixture.socket())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn missing_runtime_directory_is_not_created() {
    let fixture = Fixture::new(0o700);
    let missing = fixture.runtime.join("missing");
    Fixture::assert_refused(&missing, "prepare it before starting").await;
    assert!(!missing.exists());
}

#[tokio::test]
async fn unsafe_permissions_are_rejected_without_modification() {
    for mode in [0o770, 0o755, 0o707, 0o777] {
        let fixture = Fixture::new(mode);
        Fixture::assert_refused(
            &fixture.runtime,
            "must grant no group write or other access",
        )
        .await;
        assert_eq!(fixture_dir::mode(&fixture.runtime), mode);
    }
}

#[tokio::test]
async fn symlinks_and_unsafe_ancestors_are_rejected() {
    let fixture = Fixture::new(0o700);
    let link = fixture.runtime.with_file_name("link");
    std::os::unix::fs::symlink(&fixture.runtime, &link).unwrap();
    Fixture::assert_refused(&link, "not a real directory").await;
    let child = fixture.runtime.join("child");
    std::fs::create_dir(&child).unwrap();
    fixture_dir::set_mode(&child, 0o700);
    Fixture::assert_refused(&link.join("child"), "not a real directory").await;
    fixture_dir::set_mode(&fixture.runtime, 0o777);
    Fixture::assert_refused(&child, "not protected from replacement").await;
}

#[tokio::test]
async fn active_listener_without_a_directory_lock_is_preserved() {
    let fixture = Fixture::new(0o700);
    let listener = tokio::net::UnixListener::bind(fixture.socket()).unwrap();
    let inode = fixture.socket_inode();
    let runtime = fixture.acquire().await.unwrap();
    let error = runtime.bind_api().await.unwrap_err();
    assert!(format!("{error:#}").contains("active listener"));
    assert_eq!(fixture.socket_inode(), inode);
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
    let target = fixture.socket().with_file_name("target.sock");
    let _listener = tokio::net::UnixListener::bind(&target).unwrap();
    // Relative to the link's own directory.
    std::os::unix::fs::symlink("target.sock", fixture.socket()).unwrap();
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
    let inode = fixture.socket_inode();
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
    let error = daemon::refused_start(&config);
    assert!(
        error.contains("failed to lock runtime directory"),
        "{error}"
    );
    assert!(error.contains("directory already in use"), "{error}");
    assert!(!state.join("piqueld.db").exists());
    assert_eq!(fixture.socket_inode(), inode);
}
