use super::{archive::Entries, *};
use crate::store::Store;

fn archive_with(manifest: &BackupManifest, extra: &[(&str, &[u8])]) -> NamedTempFile {
    let file = NamedTempFile::new().unwrap();
    let mut builder = tar::Builder::new(file.as_file());
    let mut entries = Entries {
        builder: &mut builder,
        mtime: 0,
        output: file.path(),
    };
    entries
        .file(Path::new(MANIFEST), &serde_json::to_vec(manifest).unwrap())
        .unwrap();
    for (name, data) in extra {
        entries.file(Path::new(name), data).unwrap();
    }
    builder.into_inner().unwrap();
    file
}

fn manifest(schema_version: u64) -> BackupManifest {
    BackupManifest {
        format: ARCHIVE_FORMAT,
        schema_version,
        daemon_version: "0.0.0".into(),
        instance_id: "instance-test".into(),
        created_at_ms: 1,
    }
}

#[tokio::test]
async fn backup_restores_database_key_ingress_and_tailnet_state() {
    let source = tempfile::tempdir().unwrap();
    let store = Store::open(source.path().join(DATABASE)).await.unwrap();
    fs::write(source.path().join(SECRET_KEY), [7; 32]).unwrap();
    fs::create_dir_all(source.path().join("ingress/data/caddy")).unwrap();
    fs::write(source.path().join("ingress/data/caddy/cert.pem"), b"cert").unwrap();
    fs::create_dir_all(source.path().join("ingress/control")).unwrap();
    fs::write(source.path().join("ingress/control/runtime"), b"skip").unwrap();
    fs::create_dir_all(source.path().join(TAILNET)).unwrap();
    fs::write(source.path().join("tailscale/tailscaled.state"), b"node").unwrap();
    let _socket =
        std::os::unix::net::UnixListener::bind(source.path().join("tailscale/tailscaled.sock"))
            .unwrap();

    let output = source.path().join("backup.tar");
    let written = Backups::new(source.path()).create(&output).await.unwrap();
    assert_eq!(written.schema_version, SCHEMA_VERSION);
    assert_eq!(
        store.last_backup_at_ms().await.unwrap(),
        Some(written.created_at_ms)
    );
    assert!(matches!(
        Backups::new(source.path()).create(&output).await,
        Err(BackupError::Io { .. })
    ));

    let target = tempfile::tempdir().unwrap();
    let data_dir = target.path().join("restored");
    let restored = Backups::new(&data_dir).restore(&output).await.unwrap();
    assert_eq!(restored, written);
    assert_eq!(fs::read(data_dir.join(SECRET_KEY)).unwrap(), [7; 32]);
    assert_eq!(
        fs::read(data_dir.join("ingress/data/caddy/cert.pem")).unwrap(),
        b"cert"
    );
    assert!(!data_dir.join("ingress/control").exists());
    assert_eq!(
        fs::read(data_dir.join("tailscale/tailscaled.state")).unwrap(),
        b"node"
    );
    assert!(!data_dir.join("tailscale/tailscaled.sock").exists());
    let reopened = Store::open(data_dir.join(DATABASE)).await.unwrap();
    assert_eq!(reopened.instance_id(), store.instance_id());

    assert!(matches!(
        Backups::new(&data_dir).restore(&output).await,
        Err(BackupError::NotEmpty(_))
    ));
}

#[tokio::test]
async fn migration_writes_a_restorable_backup_of_the_previous_schema() {
    let directory = tempfile::tempdir().unwrap();
    let previous = SCHEMA_VERSION - 1;
    DatabaseFile::create_at_version(
        &directory.path().join(DATABASE),
        usize::try_from(previous).unwrap(),
    )
    .await;

    let backups = Backups::new(directory.path());
    let (archive, written) = backups.before_migration().await.unwrap().unwrap();
    assert_eq!(written.schema_version, previous);
    Store::open(directory.path().join(DATABASE)).await.unwrap();
    assert!(backups.before_migration().await.unwrap().is_none());
    let restored = Backups::new(&directory.path().join("rollback"))
        .restore(&archive)
        .await
        .unwrap();
    assert_eq!(restored.schema_version, previous);
}

#[tokio::test]
async fn restore_rejects_newer_schemas_and_unexpected_entries() {
    let newer = archive_with(&manifest(SCHEMA_VERSION + 1), &[(DATABASE, b"")]);
    let unexpected = archive_with(&manifest(1), &[("authorized_keys", b"")]);
    for (archive, expected) in [(newer, "newer"), (unexpected, "unexpected")] {
        let target = tempfile::tempdir().unwrap();
        let data_dir = target.path().join("data");
        let error = Backups::new(&data_dir)
            .restore(archive.path())
            .await
            .unwrap_err();
        match (expected, error) {
            ("newer", BackupError::NewerSchema { .. })
            | ("unexpected", BackupError::UnexpectedEntry(_)) => {}
            (_, error) => panic!("unexpected {expected} error: {error}"),
        }
        assert_eq!(fs::read_dir(&data_dir).unwrap().count(), 0);
    }
}

#[test]
fn unfinished_restores_are_refused_even_with_a_database() {
    let data_dir = tempfile::tempdir().unwrap();
    let backups = Backups::new(data_dir.path());
    fs::write(data_dir.path().join(DATABASE), b"").unwrap();
    backups.ensure_restore_complete().unwrap();
    fs::create_dir(data_dir.path().join(".restore-abc")).unwrap();
    assert!(matches!(
        backups.ensure_restore_complete(),
        Err(BackupError::InterruptedRestore(_))
    ));
}
