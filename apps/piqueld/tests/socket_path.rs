//! Privacy enforcement for the Unix API socket's parent directory.

use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn missing_socket_parent_is_created_private() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let parent = directory.path().join("run").join("piqueld");

    piqueld::prepare_socket_directory(&parent)
        .await
        .expect("missing socket parents are created");

    let mode = std::fs::symlink_metadata(&parent)
        .expect("created parent metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[tokio::test]
async fn existing_private_socket_parent_is_kept_unchanged() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let parent = directory.path().join("private");
    std::fs::create_dir(&parent).expect("parent directory is created");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))
        .expect("parent permissions are set");

    piqueld::prepare_socket_directory(&parent)
        .await
        .expect("private socket parents are accepted");

    let mode = std::fs::symlink_metadata(&parent)
        .expect("existing parent metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[tokio::test]
async fn symlinked_socket_parent_is_rejected() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let target = directory.path().join("target");
    std::fs::create_dir(&target).expect("target directory is created");
    let link = directory.path().join("link");
    std::os::unix::fs::symlink(&target, &link).expect("parent symlink is created");

    let Err(error) = piqueld::prepare_socket_directory(&link).await else {
        panic!("symlinked socket parent was accepted");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("symlink"));
}

#[tokio::test]
async fn permissive_socket_parent_is_rejected() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let parent = directory.path().join("shared");
    std::fs::create_dir(&parent).expect("parent directory is created");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755))
        .expect("parent permissions are set");

    let Err(error) = piqueld::prepare_socket_directory(&parent).await else {
        panic!("permissive socket parent was accepted");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("must be private"));
}

#[tokio::test]
async fn socket_directly_under_the_root_directory_is_rejected() {
    let Err(error) = piqueld::prepare_socket_directory(std::path::Path::new("/")).await else {
        panic!("root socket parent was accepted");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("private directory"));
}
