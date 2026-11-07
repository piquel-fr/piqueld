//! Privacy enforcement for the daemon's single data directory.

#[path = "support/fixture_dir.rs"]
mod fixture_dir;

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

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
    let Err(error) = prepare_data_dir(path).await else {
        panic!("{} is accepted as a data directory", path.display());
    };
    assert_eq!(error.kind(), kind, "{}: {error}", path.display());
    assert!(
        error.to_string().contains(message),
        "{}: {error}",
        path.display()
    );
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

/// Data directories that could be replaced, shared or are no directory of
/// their own are refused with their reason, if it is specific.
#[tokio::test]
async fn unsafe_data_dirs_are_rejected() {
    type Case = (fn(&Path) -> PathBuf, ErrorKind, &'static str);
    let cases: [Case; 9] = [
        (
            |directory| {
                let shared = directory.join("shared");
                std::fs::create_dir_all(shared.join("piqueld")).expect("tree is created");
                fixture_dir::set_mode(&shared, 0o777);
                fixture_dir::set_mode(&shared.join("piqueld"), 0o700);
                shared.join("piqueld")
            },
            ErrorKind::PermissionDenied,
            "ancestor",
        ),
        (
            |directory| {
                let target = directory.join("target");
                std::fs::create_dir(&target).expect("target directory is created");
                std::os::unix::fs::symlink(&target, directory.join("link")).expect("symlink");
                directory.join("link")
            },
            ErrorKind::InvalidInput,
            "not a real directory",
        ),
        (
            |directory| {
                let real = directory.join("real");
                std::fs::create_dir_all(real.join("piqueld")).expect("tree is created");
                std::os::unix::fs::symlink(&real, directory.join("link")).expect("symlink");
                directory.join("link").join("piqueld")
            },
            ErrorKind::InvalidInput,
            "",
        ),
        (
            |directory| directory.join("state").join("..").join("piqueld"),
            ErrorKind::InvalidInput,
            "parent component",
        ),
        (
            |directory| {
                std::fs::write(directory.join("file"), b"not a directory").expect("file");
                directory.join("file")
            },
            ErrorKind::InvalidInput,
            "",
        ),
        // Paths without a dedicated directory.
        (|_| PathBuf::from("/"), ErrorKind::InvalidInput, ""),
        (|_| PathBuf::new(), ErrorKind::InvalidInput, ""),
        (|_| PathBuf::from("."), ErrorKind::InvalidInput, ""),
        (|_| PathBuf::from("./"), ErrorKind::InvalidInput, ""),
    ];
    for (setup, kind, message) in cases {
        let directory = fixture();
        let path = setup(directory.path());
        assert_refused(&path, kind, message).await;
    }
}
