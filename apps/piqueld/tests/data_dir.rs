//! Privacy enforcement for the daemon's single data directory.

#[path = "support/fixture_dir.rs"]
mod fixture_dir;

use std::io::ErrorKind;
use std::path::Path;

async fn prepare_data_dir(path: &Path) -> std::io::Result<()> {
    piqueld::prepare_data_dir(fixture_dir::relative(path)).await
}

/// A temporary directory below the working directory, see
/// [`fixture_dir::relative`].
fn fixture() -> tempfile::TempDir {
    tempfile::tempdir_in(".").expect("temporary directory")
}

/// Asserts `path` is refused as a data directory with `kind`, and an error
/// message containing `message`, which may be empty.
async fn assert_refused(path: &Path, kind: ErrorKind, message: &str) {
    let error = prepare_data_dir(path)
        .await
        .expect_err("the data directory is refused");
    assert_eq!(error.kind(), kind);
    assert!(error.to_string().contains(message), "{error}");
}

#[tokio::test]
async fn missing_data_dir_is_created_private() {
    let directory = fixture();
    let data_dir = directory.path().join("var").join("lib").join("piqueld");

    prepare_data_dir(&data_dir)
        .await
        .expect("missing data directories are created");

    assert_eq!(fixture_dir::mode(&data_dir), 0o700);
}

#[tokio::test]
async fn nested_creation_grants_no_group_or_other_access_to_new_components() {
    let directory = fixture();
    let outer = directory.path().join("a");
    std::fs::create_dir(&outer).expect("outer component exists");
    fixture_dir::set_mode(&outer, 0o755);

    prepare_data_dir(&outer.join("b").join("c"))
        .await
        .expect("nested data directories are created under an existing parent");

    for component in [outer.join("b"), outer.join("b").join("c")] {
        assert_eq!(fixture_dir::mode(&component), 0o700);
    }
}

#[tokio::test]
async fn existing_private_data_dir_is_kept_unchanged() {
    let directory = fixture();
    let data_dir = directory.path().join("private");
    std::fs::create_dir(&data_dir).expect("data dir is created");
    fixture_dir::set_mode(&data_dir, 0o700);

    prepare_data_dir(&data_dir)
        .await
        .expect("private data directories are accepted");

    assert_eq!(fixture_dir::mode(&data_dir), 0o700);
}

#[tokio::test]
async fn permissive_data_dir_is_rejected_without_chmodding_it() {
    let directory = fixture();
    let data_dir = directory.path().join("shared");
    std::fs::create_dir(&data_dir).expect("data dir is created");
    fixture_dir::set_mode(&data_dir, 0o755);

    assert_refused(&data_dir, ErrorKind::InvalidInput, "must be private").await;
    assert_eq!(fixture_dir::mode(&data_dir), 0o755);
}

#[tokio::test]
async fn unsafe_writable_ancestor_is_rejected() {
    let directory = fixture();
    let shared = directory.path().join("shared");
    let data_dir = shared.join("piqueld");
    std::fs::create_dir_all(&data_dir).expect("data directory tree is created");
    fixture_dir::set_mode(&shared, 0o777);
    fixture_dir::set_mode(&data_dir, 0o700);

    assert_refused(&data_dir, ErrorKind::PermissionDenied, "ancestor").await;
}

#[tokio::test]
async fn symlinked_data_dir_is_rejected() {
    let directory = fixture();
    let target = directory.path().join("target");
    std::fs::create_dir(&target).expect("target directory is created");
    let link = directory.path().join("link");
    std::os::unix::fs::symlink(&target, &link).expect("data dir symlink is created");

    assert_refused(&link, ErrorKind::InvalidInput, "not a real directory").await;
}

#[tokio::test]
async fn symlinked_intermediate_component_is_rejected() {
    let directory = fixture();
    let real = directory.path().join("real");
    std::fs::create_dir_all(real.join("piqueld")).expect("real directory tree is created");
    let link = directory.path().join("link");
    std::os::unix::fs::symlink(&real, &link).expect("intermediate symlink is created");

    assert_refused(&link.join("piqueld"), ErrorKind::InvalidInput, "").await;
}

#[tokio::test]
async fn parent_component_paths_are_rejected() {
    let directory = fixture();
    let path = directory.path().join("state").join("..").join("piqueld");

    assert_refused(&path, ErrorKind::InvalidInput, "parent component").await;
}

#[tokio::test]
async fn non_directory_data_path_is_rejected() {
    let directory = fixture();
    let file = directory.path().join("file");
    std::fs::write(&file, b"not a directory").expect("file is written");

    assert_refused(&file, ErrorKind::InvalidInput, "").await;
}

#[tokio::test]
async fn paths_without_a_dedicated_directory_are_rejected() {
    for path in ["", ".", "./"] {
        let error = piqueld::prepare_data_dir(Path::new(path))
            .await
            .expect_err("path without a dedicated directory is rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }
}

#[tokio::test]
async fn root_directory_is_rejected() {
    assert_refused(Path::new("/"), ErrorKind::InvalidInput, "").await;
}
