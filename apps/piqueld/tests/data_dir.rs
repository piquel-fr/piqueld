//! Privacy enforcement for the daemon's single data directory.

use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn missing_data_dir_is_created_private() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let data_dir = directory.path().join("var").join("lib").join("piqueld");

    piqueld::prepare_data_dir(&data_dir)
        .await
        .expect("missing data directories are created");

    let mode = std::fs::symlink_metadata(&data_dir)
        .expect("created data dir metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[tokio::test]
async fn nested_creation_grants_no_group_or_other_access_to_new_components() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let inner = directory.path().join("a").join("b").join("c");
    std::fs::create_dir(directory.path().join("a")).expect("outer component exists");
    std::fs::set_permissions(
        directory.path().join("a"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("outer component is permissive");

    piqueld::prepare_data_dir(&inner)
        .await
        .expect("nested data directories are created under an existing parent");

    for component in [
        directory.path().join("a").join("b"),
        directory.path().join("a").join("b").join("c"),
    ] {
        let mode = std::fs::symlink_metadata(&component)
            .expect("created component metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}

#[tokio::test]
async fn existing_private_data_dir_is_kept_unchanged() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let data_dir = directory.path().join("private");
    std::fs::create_dir(&data_dir).expect("data dir is created");
    std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700))
        .expect("data dir permissions are set");

    piqueld::prepare_data_dir(&data_dir)
        .await
        .expect("private data directories are accepted");

    let mode = std::fs::symlink_metadata(&data_dir)
        .expect("existing data dir metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[tokio::test]
async fn permissive_data_dir_is_rejected_without_chmodding_it() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let data_dir = directory.path().join("shared");
    std::fs::create_dir(&data_dir).expect("data dir is created");
    std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o755))
        .expect("data dir permissions are set");

    let Err(error) = piqueld::prepare_data_dir(&data_dir).await else {
        panic!("permissive data dir was accepted");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("must be private"));
    let mode = std::fs::symlink_metadata(&data_dir)
        .expect("unchanged data dir metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
}

#[tokio::test]
async fn unsafe_writable_ancestor_is_rejected() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let shared = directory.path().join("shared");
    let data_dir = shared.join("piqueld");
    std::fs::create_dir_all(&data_dir).expect("data directory tree is created");
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777))
        .expect("ancestor permissions are set");
    std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700))
        .expect("data directory permissions are set");

    let error = piqueld::prepare_data_dir(&data_dir)
        .await
        .expect_err("unsafe writable ancestor is rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(error.to_string().contains("ancestor"));
}

#[tokio::test]
async fn symlinked_data_dir_is_rejected() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let target = directory.path().join("target");
    std::fs::create_dir(&target).expect("target directory is created");
    let link = directory.path().join("link");
    std::os::unix::fs::symlink(&target, &link).expect("data dir symlink is created");

    let Err(error) = piqueld::prepare_data_dir(&link).await else {
        panic!("symlinked data dir was accepted");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("not a real directory"));
}

#[tokio::test]
async fn symlinked_intermediate_component_is_rejected() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let real = directory.path().join("real").join("piqueld");
    std::fs::create_dir_all(&real).expect("real directory tree is created");
    let link = directory.path().join("link");
    std::os::unix::fs::symlink(directory.path().join("real"), &link)
        .expect("intermediate symlink is created");

    let Err(error) = piqueld::prepare_data_dir(&link.join("piqueld")).await else {
        panic!("symlinked intermediate component was accepted");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[tokio::test]
async fn parent_component_paths_are_rejected() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("state").join("..").join("piqueld");
    let Err(error) = piqueld::prepare_data_dir(&path).await else {
        panic!("parent component was accepted");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("parent component"));
}

#[tokio::test]
async fn non_directory_data_path_is_rejected() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let file = directory.path().join("file");
    std::fs::write(&file, b"not a directory").expect("file is written");

    let Err(error) = piqueld::prepare_data_dir(&file).await else {
        panic!("non-directory data path was accepted");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[tokio::test]
async fn root_directory_is_rejected() {
    let Err(error) = piqueld::prepare_data_dir(std::path::Path::new("/")).await else {
        panic!("root data directory was accepted");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}
