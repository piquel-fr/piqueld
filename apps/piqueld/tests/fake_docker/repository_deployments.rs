use super::*;
use piqueld::api::{Mutation, MutationResponse, PreviewMutation};
use piqueld_core::manifest::{
    GitRepository, ManifestRevision, RepositoryManifest, SourceRepository,
};
use piqueld_core::{EnvironmentName, TrackedBranch};
use piqueld_store::StoreError;
use repository::Repository;

impl Repository {
    /// The fixture manifest, connected to `path` in this repository.
    fn manifest(&self, path: &str) -> ApplicationManifest {
        let mut application = manifest();
        application.spec.manifest = Some(RepositoryManifest {
            repository: GitRepository {
                url: self.path().display().to_string(),
                branch: "main".into(),
                commit: None,
            },
            path: path.into(),
            sync: piqueld_core::sync::RepositorySync::Off,
        });
        application
    }

    /// Writes `manifest` to `path`, without committing it.
    fn write(&self, path: &str, manifest: &ApplicationManifest) {
        std::fs::write(
            self.path().join(path),
            serde_json::to_vec(manifest).unwrap(),
        )
        .unwrap();
    }

    /// Commits `manifest` and a new `Dockerfile` on `feat/local` of a
    /// working copy, a branch this repository never has, and bundles it
    /// against `base`. Returns the snapshot commit and the bundle.
    fn snapshot(&self, manifest: &ApplicationManifest, base: &str) -> (String, Vec<u8>) {
        let work = self.working_copy();
        work.git(&["switch", "-qc", "feat/local"]);
        let manifest = serde_json::to_vec(manifest).unwrap();
        let commit = work.commit(&[
            ("app.json", &manifest),
            ("Dockerfile", b"FROM alpine:3.21\n"),
        ]);
        (commit, work.bundle("feat/local", Some(base)))
    }
}

/// Builds `Dockerfile` from the manifest's own repository and revision.
fn self_source() -> piqueld_core::Source {
    piqueld_core::Source::Git {
        repository: SourceRepository::Manifest(
            piqueld_core::manifest::ManifestRepository::Manifest,
        ),
        build: git_fixture::dockerfile_build(),
    }
}

impl ControllerHarness {
    /// Deploys `id`, fetching its tracked manifest, and runs the controller.
    async fn deploy(&self, id: &EnvironmentId) -> Operation {
        self.deploy_revision(id, None).await
    }

    /// Deploys `id` at `revision`, or what it tracks, and runs the controller.
    async fn deploy_revision(
        &self,
        id: &EnvironmentId,
        revision: Option<ManifestRevision>,
    ) -> Operation {
        let MutationResponse::Operation(accepted) = self
            .applications()
            .accept(
                Daemon,
                Mutation::Deploy {
                    id: id.clone(),
                    revision,
                    rebuild: false,
                },
                None,
                true,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("operation expected")
        };
        self.scan().await;
        self.operation(&accepted.operation_id).await
    }

    /// The warnings recorded on `operation`'s deployment.
    async fn warnings(&self, operation: &Operation) -> Vec<String> {
        let deployments = self
            .store
            .deployments(&operation.environment_id, None, 3)
            .await
            .unwrap();
        deployments
            .items
            .into_iter()
            .filter(|deployment| deployment.operation.id == operation.id)
            .flat_map(|deployment| deployment.warnings)
            .map(|warning| warning.code)
            .collect()
    }

    /// Creates `name`, an environment of `production`'s application following `branch`.
    async fn create_environment(
        &self,
        production: &EnvironmentId,
        name: &str,
        branch: &str,
    ) -> EnvironmentId {
        let application = self.store.get(production).await.unwrap();
        let MutationResponse::Environment(environment) = self
            .applications()
            .accept(
                Daemon,
                Mutation::CreateEnvironment {
                    application: application.environment.application_id,
                    name: EnvironmentName::parse(name).unwrap(),
                    source: Some(piqueld::api::SourceChoice::Branch(
                        TrackedBranch::new(branch.into(), None).unwrap(),
                    )),
                },
                None,
                true,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("environment expected")
        };
        environment.id
    }

    /// Runs `mutation` of a preview and the controller, and returns how its
    /// operation ended, unless a deletion removed it, with the preview.
    async fn run_preview(&self, mutation: Mutation) -> (Option<Operation>, Option<EnvironmentId>) {
        let (operation, preview) = match self
            .applications()
            .accept(Daemon, mutation, None, false, None)
            .await
            .unwrap()
        {
            MutationResponse::Preview(created) => (created.operation, Some(created.preview.id)),
            MutationResponse::Operation(operation) => (operation, None),
            _ => panic!("preview operation expected"),
        };
        self.scan().await;
        let operation = self.store.operation(&operation.operation_id).await.ok();
        (operation, preview)
    }
}

/// Repository files are read at the commit each deployment fetches, mount
/// their bytes verbatim, and a missing one fails the fetch naming it.
/// Saved manifests keep the path, never the bytes.
#[tokio::test]
async fn repository_files_are_captured_at_the_fetched_commit() {
    use piqueld_core::manifest::{FileMode, FileSource, ServiceFile};
    let repository = Repository::new();
    let harness = ControllerHarness::new().await;
    let mut main = repository.manifest("app.json");
    main.spec.services[0].files = vec![ServiceFile {
        target: "/etc/otel.yaml".into(),
        source: FileSource::Path("otel.yaml".into()),
        mode: FileMode::default(),
    }];
    repository.write("app.json", &main);
    let otel = repository.path().join("otel.yaml");
    std::fs::write(&otel, "home: ${HOME}\n").unwrap();
    repository.commit(&[]);
    let first = harness.apply(main.clone(), Some(0)).await;
    harness.finish(&first).await;
    let environment = first.environment_id;
    let deployed = async || {
        let current = harness.store.get(&environment).await.unwrap();
        assert!(current.application.application.files().is_empty());
        current.resolved.unwrap().services[0].files[0]
            .content
            .clone()
    };
    assert_eq!(deployed().await, "home: ${HOME}\n");

    std::fs::write(&otel, "home: /root\n").unwrap();
    repository.commit(&[]);
    let changed = harness.deploy(&environment).await;
    assert_eq!(changed.state, OperationState::Succeeded);
    assert_eq!(deployed().await, "home: /root\n");

    std::fs::remove_file(&otel).unwrap();
    repository.commit(&[]);
    let missing = harness.deploy(&environment).await;
    assert_eq!(
        (missing.state, missing.error_code.as_deref()),
        (OperationState::Failed, Some("manifest_invalid"))
    );
    assert_eq!(
        missing.error_message.as_deref(),
        Some("repository file otel.yaml does not exist at the fetched commit")
    );
    assert_eq!(deployed().await, "home: /root\n");
}

/// A fetched file's `spec.manifest` never redirects later fetches: it is
/// ignored, with a warning when it names another repository or path, and
/// may be omitted.
#[tokio::test]
async fn fetched_connections_are_ignored_with_a_warning() {
    let repository = Repository::new();
    let harness = ControllerHarness::new().await;
    let initial = repository.manifest("app.json");
    let mut redirected = initial.clone();
    let declared = redirected.spec.manifest.as_mut().unwrap();
    declared.path = "next.json".into();
    declared.repository.url = "/elsewhere".into();
    declared.repository.branch = "release".into();
    redirected.spec.services[0].replicas = 2.into();
    repository.write("app.json", &redirected);
    repository.commit(&[]);
    let first = harness.apply(initial.clone(), Some(0)).await;
    harness.finish(&first).await;
    assert_eq!(
        harness.warnings(&first).await,
        ["manifest_connection_ignored"]
    );
    let current = harness.store.get(&first.environment_id).await.unwrap();
    assert_eq!(current.resolved.as_ref().unwrap().services[0].replicas, 2);
    assert_eq!(
        current.tracking().unwrap().repository(),
        initial.spec.manifest
    );
    assert_eq!(
        current.application.application.spec().manifest,
        initial.spec.manifest
    );
    // Only the branch differs, as it does between environments: no warning.
    let mut branched = initial.clone();
    branched.spec.manifest.as_mut().unwrap().repository.branch = "release".into();
    // Without `spec.manifest`, the application stays connected, and
    // references that need it still validate.
    let mut bare = initial.clone();
    bare.spec.manifest = None;
    bare.spec.services[0]
        .environment
        .insert("REVISION".into(), "${{ git.sha }}".into());
    for file in [branched, bare] {
        repository.write("app.json", &file);
        repository.commit(&[]);
        let deployed = harness.deploy(&first.environment_id).await;
        assert_eq!(deployed.state, OperationState::Succeeded);
        assert_eq!(harness.warnings(&deployed).await, Vec::<String>::new());
        let current = harness.store.get(&first.environment_id).await.unwrap();
        assert_eq!(
            current.tracking().unwrap().repository(),
            initial.spec.manifest
        );
    }
}

/// Every deployment of a branch records its head for sync to follow from.
#[tokio::test]
async fn deployments_record_the_branch_head_sync_follows_from() {
    let repository = Repository::new();
    let harness = ControllerHarness::new().await;
    let main = repository.manifest("app.json");
    repository.write("app.json", &main);
    let first = repository.commit(&[]);
    let production = harness.apply(main.clone(), Some(0)).await;
    harness.finish(&production).await;
    let production = production.environment_id;
    let synced = async || {
        harness
            .store
            .get(&production)
            .await
            .unwrap()
            .environment
            .synced
            .and_then(|head| head.commit)
    };
    // A deployment of its branch records the head it fetched.
    assert_eq!(synced().await.as_deref(), Some(first.as_str()));
    // A one-off deployment of an older commit records the branch's head
    // then, so it stays until the branch moves.
    let second = repository.commit(&[]);
    let deployed = harness
        .deploy_revision(&production, Some(ManifestRevision::Commit(first.clone())))
        .await;
    assert_eq!(deployed.state, OperationState::Succeeded);
    assert_eq!(synced().await.as_deref(), Some(second.as_str()));
}

/// Environments following different branches each deploy their own
/// branch, and a fetch for one never changes what the other deploys.
#[tokio::test]
async fn environments_on_different_branches_deploy_independently() {
    let repository = Repository::new();
    let harness = ControllerHarness::new().await;
    let mut main = repository.manifest("app.json");
    repository.write("app.json", &main);
    repository.commit(&[]);
    repository.git(&["checkout", "-b", "release"]);
    let mut release = main.clone();
    release.spec.services[0].replicas = 3.into();
    repository.write("app.json", &release);
    repository.commit(&[]);
    repository.git(&["checkout", "main"]);
    let production = harness.apply(main.clone(), Some(0)).await;
    harness.finish(&production).await;
    let production = production.environment_id;
    let staging = harness
        .create_environment(&production, "staging", "release")
        .await;
    let replicas = async |environment: &EnvironmentId| {
        let current = harness.store.get(environment).await.unwrap();
        (
            current
                .resolved
                .map(|resolved| resolved.services[0].replicas),
            current.own,
        )
    };
    let deployed = harness.deploy(&staging).await;
    assert_eq!(deployed.state, OperationState::Succeeded);
    let (staged, fetched) = replicas(&staging).await;
    assert_eq!(staged, Some(3));
    let (before, production_manifest) = replicas(&production).await;
    assert_eq!(before, Some(1));
    // Only a manifest fetched from the branch `spec.manifest` names
    // becomes the application's saved one.
    let saved_replicas = async || {
        let current = harness.store.get(&production).await.unwrap();
        current.application.application.spec().services[0]
            .replicas
            .clone()
    };
    assert_eq!(saved_replicas().await, main.spec.services[0].replicas);
    let deployed = harness.deploy(&production).await;
    assert_eq!(deployed.state, OperationState::Succeeded);
    assert_eq!(saved_replicas().await, main.spec.services[0].replicas);
    // A new main commit reaches production only, when it deploys.
    main.spec.services[0].replicas = 2.into();
    repository.write("app.json", &main);
    repository.commit(&[]);
    let deployed = harness.deploy(&production).await;
    assert_eq!(deployed.state, OperationState::Succeeded);
    assert_eq!(replicas(&production).await.0, Some(2));
    assert_ne!(replicas(&production).await.1, production_manifest);
    assert_eq!(replicas(&staging).await, (Some(3), fetched));
    // Pointing staging at main fetches main from its next deployment.
    harness
        .applications()
        .accept(
            Daemon,
            Mutation::SetBranch {
                id: staging.clone(),
                branch: TrackedBranch::new("main".into(), None).unwrap(),
            },
            None,
            true,
            None,
        )
        .await
        .unwrap();
    assert_eq!(replicas(&staging).await.0, Some(3));
    let deployed = harness.deploy(&staging).await;
    assert_eq!(deployed.state, OperationState::Succeeded);
    assert_eq!(replicas(&staging).await.0, Some(2));
    // One-off revisions deploy another branch without changing staging's.
    let deployed = harness
        .deploy_revision(&staging, Some(ManifestRevision::Branch("release".into())))
        .await;
    assert_eq!(deployed.state, OperationState::Succeeded);
    assert_eq!(replicas(&staging).await.0, Some(3));
    let current = harness.store.get(&staging).await.unwrap();
    assert_eq!(
        current.environment.source,
        piqueld_core::EnvironmentSource::Branch(TrackedBranch::new("main".into(), None).unwrap())
    );
}

/// Deleting a preview removes every volume it ever created, including one
/// its branch's manifest later dropped, while environments keep theirs.
/// Unlike environments, it records no release.
#[tokio::test]
async fn deleting_a_preview_removes_every_volume_it_ever_created() {
    let repository = Repository::new();
    let harness = ControllerHarness::new().await;
    let main = repository.manifest("app.json");
    repository.write("app.json", &main);
    repository.commit(&[]);
    let production = harness.apply(main.clone(), Some(0)).await;
    harness.finish(&production).await;
    repository.git(&["checkout", "-b", "feat/cache"]);
    let mut cached = main.clone();
    cached
        .spec
        .volumes
        .push(serde_json::from_value(serde_json::json!({"name": "cache"})).unwrap());
    cached.spec.services[0].mounts.push(
        serde_json::from_value(
            serde_json::json!({"volume": "cache", "target": "/var/cache/notes"}),
        )
        .unwrap(),
    );
    repository.write("app.json", &cached);
    repository.commit(&[]);
    let volumes = async || {
        let observed = harness.docker.observed.lock().await;
        observed
            .volumes
            .iter()
            .map(|volume| volume.name.clone())
            .collect::<Vec<_>>()
    };
    let before = volumes().await;

    let (created, preview) = harness
        .run_preview(Mutation::Preview(PreviewMutation::Create {
            application: harness
                .store
                .get(&production.environment_id)
                .await
                .unwrap()
                .environment
                .application_id,
            branch: "feat/cache".parse().unwrap(),
            slot: None,
        }))
        .await;
    assert_eq!(created.unwrap().state, OperationState::Succeeded);
    let preview = preview.unwrap();
    // Previews never record releases; the environment that deployed did.
    assert_eq!(harness.store.current_release(&preview).await.unwrap(), None);
    assert!(
        harness
            .store
            .current_release(&production.environment_id)
            .await
            .unwrap()
            .is_some()
    );
    // The branch drops the cache volume; the preview keeps it until deleted.
    repository.write("app.json", &main);
    repository.commit(&[]);
    let (deployed, _) = harness
        .run_preview(Mutation::Preview(PreviewMutation::Deploy {
            id: preview.clone(),
        }))
        .await;
    assert_eq!(deployed.unwrap().state, OperationState::Succeeded);
    let inventory = harness.store.preview_volumes(&preview).await.unwrap();
    assert_eq!(inventory.len(), 2);
    assert!(inventory.iter().all(|name| !before.contains(name)));
    let current = volumes().await;
    assert!(inventory.iter().all(|name| current.contains(name)));

    let (deleted, _) = harness
        .run_preview(Mutation::Preview(PreviewMutation::Delete {
            id: preview.clone(),
        }))
        .await;
    assert!(deleted.is_none(), "{deleted:?}");
    assert!(matches!(
        harness.store.get(&preview).await,
        Err(StoreError::NotFound)
    ));
    let remaining = volumes().await;
    assert!(inventory.iter().all(|name| !remaining.contains(name)));
    assert_eq!(remaining, before, "production keeps its volume");
}

#[tokio::test]
async fn missing_invalid_and_failed_build_manifests_preserve_accepted_configuration() {
    let repository = Repository::new();
    repository.commit(&[]);
    let harness = ControllerHarness::new().await;
    let initial = repository.manifest("app.json");
    repository.write("app.json", &initial);
    repository.commit(&[]);
    let first = harness.apply(initial.clone(), Some(0)).await;
    harness.finish(&first).await;
    let before = harness.store.get(&first.environment_id).await.unwrap();
    std::fs::remove_file(repository.path().join("app.json")).unwrap();
    repository.commit(&[]);
    let missing = harness.deploy(&first.environment_id).await;
    assert_eq!(missing.error_code.as_deref(), Some("manifest_not_found"));
    let mut invalid = initial.clone();
    invalid.metadata.name = "different-application".into();
    repository.write("app.json", &invalid);
    repository.commit(&[]);
    let invalid = harness.deploy(&first.environment_id).await;
    assert_eq!(invalid.error_code.as_deref(), Some("manifest_invalid"));
    let build = git_fixture::GitBuildFixture::new();
    let mut failing = initial.clone();
    failing.spec.manifest = None;
    failing.spec.services[0].source = build.failing_source();
    repository.write("app.json", &failing);
    repository.commit(&[]);
    let failed = harness.deploy(&first.environment_id).await;
    assert_eq!(failed.error_code.as_deref(), Some("git_build_failed"));
    let after = harness.store.get(&first.environment_id).await.unwrap();
    assert_eq!(before.application, after.application);
    assert_eq!(before.application.generation, after.application.generation);
    assert_eq!(before.resolved, after.resolved);
    assert_eq!(harness.store.list(None, 50).await.unwrap().items.len(), 1);
}

/// The connection's branch deploys the saved manifest, edits included,
/// until a push changes the manifest there, which replaces it.
#[tokio::test]
async fn edits_deploy_until_a_push_changes_the_manifest() {
    let repository = Repository::new();
    let harness = ControllerHarness::new().await;
    let initial = repository.manifest("app.json");
    repository.write("app.json", &initial);
    repository.commit(&[]);
    let first = harness.apply(initial.clone(), Some(0)).await;
    harness.finish(&first).await;
    let environment = first.environment_id.clone();
    let saved = || async { harness.store.get(&environment).await.unwrap() };
    let id = saved().await.application.application.id().clone();
    let generation = saved().await.application.generation;
    let mut edited = initial.clone();
    edited.spec.services[0].replicas = 4.into();
    harness.save(edited, Some(generation)).await;
    // A push that leaves the manifest alone deploys the edit.
    repository.commit(&[("README.md", b"notes")]);
    harness.finish(&harness.deploy(&environment).await).await;
    let current = saved().await;
    assert_eq!(current.own.unwrap().spec().services[0].replicas, 4.into());
    let view = harness.applications().application(&id).await.unwrap();
    assert!(view.edited());
    assert_eq!(
        view.edits()
            .into_iter()
            .map(|edit| edit.field)
            .collect::<Vec<_>>(),
        ["services.web.replicas"]
    );
    // One that changes it replaces the saved manifest.
    let mut pushed = initial;
    pushed.spec.services[0]
        .environment
        .insert("MODE".into(), "pushed".into());
    repository.write("app.json", &pushed);
    repository.commit(&[]);
    harness.finish(&harness.deploy(&environment).await).await;
    let current = saved().await;
    for deployed in [
        current.own.as_ref().unwrap(),
        &current.application.application,
    ] {
        let web = &deployed.spec().services[0];
        assert_eq!(web.replicas, 1.into());
        assert_eq!(web.environment["MODE"], "pushed");
    }
    assert_eq!(
        current.environment.resolved_generation,
        Some(current.application.generation)
    );
    let view = harness.applications().application(&id).await.unwrap();
    assert!(!view.edited());
}

#[tokio::test]
async fn retry_after_reopening_store_uses_fetched_manifest_and_new_deploy_fetches_again() {
    let repository = Repository::new();
    let mut harness = ControllerHarness::new().await;
    let initial = repository.manifest("app.json");
    repository.write("app.json", &initial);
    repository.commit(&[]);
    let first = harness.apply(initial.clone(), Some(0)).await;
    harness.finish(&first).await;
    let build = git_fixture::GitBuildFixture::new();
    let mut candidate = initial.clone();
    candidate.spec.services[0].source = build.failing_source();
    repository.write("app.json", &candidate);
    repository.commit(&[]);
    let failed = harness.deploy(&first.environment_id).await;
    assert_eq!(failed.error_code.as_deref(), Some("git_build_failed"));
    repository.write("app.json", &initial);
    repository.commit(&[]);
    harness.reopen().await;
    let retry = harness
        .applications()
        .reconcile(&first.environment_id, None)
        .await
        .unwrap();
    assert_eq!(retry.id, failed.id);
    harness.scan().await;
    assert_eq!(
        harness.operation(&retry.id).await.state,
        OperationState::Failed
    );
    assert_eq!(
        harness.deploy(&first.environment_id).await.state,
        OperationState::Succeeded
    );
}

#[tokio::test]
async fn self_sources_build_the_manifest_commit_including_one_time_revisions() {
    let repository = Repository::new();
    let harness = ControllerHarness::new().await;
    let mut fetched = repository.manifest("app.json");
    fetched.spec.services[0].source = self_source();
    repository.write("app.json", &fetched);
    std::fs::write(repository.path().join("Dockerfile"), "FROM alpine:3.20\n").unwrap();
    let main = repository.commit(&[]);
    repository.git(&["checkout", "-b", "feature"]);
    std::fs::write(repository.path().join("Dockerfile"), "FROM alpine:3.21\n").unwrap();
    let feature = repository.commit(&[]);
    repository.git(&["checkout", "main"]);
    let mut bootstrap = repository.manifest("app.json");
    bootstrap.spec.services.clear();
    let saved = harness.save(bootstrap, Some(0)).await;
    let environment = harness.production(&saved).await;
    // The override is not saved, and saved configuration keeps "self":
    // only the fetch of `main` itself becomes the saved manifest.
    let expected_spec = fetched
        .clone()
        .validate()
        .unwrap()
        .normalize(ApplicationId::parse(&saved.application_id).unwrap())
        .to_manifest()
        .spec;
    let mut saved_main = false;
    for (revision, expected) in [
        (
            Some(ManifestRevision::Commit(feature.clone())),
            feature.clone(),
        ),
        (None, main),
        (Some(ManifestRevision::Branch("feature".into())), feature),
    ] {
        let one_off = revision.clone();
        let deployment = harness.deploy_revision(&environment, revision).await;
        assert_eq!(deployment.state, OperationState::Succeeded);
        let stored = harness.store.get(&environment).await.unwrap();
        assert!(
            matches!(&stored.resolved.as_ref().unwrap().services[0].source, ResolvedSource::Git { commit, .. } if commit == &expected)
        );
        saved_main |= one_off.is_none();
        assert_eq!(
            stored.application.application.to_manifest().spec == expected_spec,
            saved_main
        );
    }
    // "self" builds the repository the manifest was fetched from, whatever
    // the file's own `spec.manifest` says.
    let mut moved = fetched;
    moved.spec.manifest.as_mut().unwrap().repository.url = "/elsewhere".into();
    repository.write("app.json", &moved);
    let head = repository.commit(&[]);
    let deployment = harness.deploy(&environment).await;
    assert_eq!(deployment.state, OperationState::Succeeded);
    let stored = harness.store.get(&environment).await.unwrap();
    assert!(
        matches!(&stored.resolved.as_ref().unwrap().services[0].source, ResolvedSource::Git { commit, .. } if commit == &head)
    );
}

/// A local snapshot of a never-pushed branch, uploaded thin, deploys its
/// own manifest and builds `"self"` sources from the snapshot, through
/// the preview's normal deployment path. Its history records where it
/// came from, and sync records the branch as absent.
#[tokio::test]
async fn local_snapshots_deploy_their_manifest_and_build_self_sources_from_the_snapshot() {
    let repository = Repository::new();
    let harness = ControllerHarness::new().await;
    let mut fetched = repository.manifest("app.json");
    fetched.spec.services[0].source = self_source();
    repository.write("app.json", &fetched);
    std::fs::write(repository.path().join("Dockerfile"), "FROM alpine:3.20\n").unwrap();
    let base = repository.commit(&[]);
    let mut bootstrap = repository.manifest("app.json");
    bootstrap.spec.services.clear();
    let saved = harness.save(bootstrap, Some(0)).await;
    let application = ApplicationId::parse(saved.application_id).unwrap();

    // The snapshot changes the manifest and the Dockerfile on a branch
    // the repository never had.
    let mut local = fetched.clone();
    local.spec.services[0]
        .environment
        .insert("SNAPSHOT".into(), "yes".into());
    let (snapshot, bundle) = repository.snapshot(&local, &base);
    let created = harness
        .applications()
        .deploy_snapshot(
            Daemon,
            (
                application,
                piqueld_core::GitBranch::parse("feat/local").unwrap(),
                None,
            ),
            (
                futures_util::stream::once(async {
                    Ok::<_, std::io::Error>(axum::body::Bytes::from(bundle))
                }),
                None,
            ),
            None,
        )
        .await
        .unwrap();
    let MutationResponse::Preview(created) = created else {
        panic!("preview expected")
    };
    harness.scan().await;
    let operation = harness.operation(&created.operation.operation_id).await;
    assert_eq!(operation.state, OperationState::Succeeded);
    let stored = harness.store.get(&created.preview.id).await.unwrap();
    let service = &stored.resolved.as_ref().unwrap().services[0];
    assert!(matches!(&service.source, ResolvedSource::Git { commit, .. } if commit == &snapshot));
    assert_eq!(
        service.environment.get("SNAPSHOT").map(String::as_str),
        Some("yes")
    );
    let history = harness
        .store
        .deployments(&created.preview.id, None, 3)
        .await
        .unwrap();
    assert_eq!(
        history.items[0].origin,
        piqueld_core::api::DeploymentOrigin::Local(piqueld_core::api::LocalSnapshot {
            commit: snapshot.clone(),
            base: Some(base),
        })
    );
    assert_eq!(
        history.items[0].system_value(piqueld_core::manifest::SystemVariable::GitSha),
        Some(snapshot.as_str())
    );
    let synced = stored.environment.synced.unwrap();
    assert_eq!(synced.commit, None);
}

#[tokio::test]
async fn retry_after_reopening_store_builds_self_sources_at_the_fetched_commit() {
    let repository = Repository::new();
    let mut harness = ControllerHarness::new().await;
    repository.commit(&[]);
    repository.git(&["checkout", "-b", "feature"]);
    let mut fetched = repository.manifest("app.json");
    let mut built = fetched.spec.services[0].clone();
    built.name = "built".into();
    built.source = self_source();
    fetched.spec.services.push(built);
    repository.write("app.json", &fetched);
    let dockerfile = repository.path().join("Dockerfile");
    std::fs::write(&dockerfile, "FROM alpine:3.20\n").unwrap();
    let pinned = repository.commit(&[]);
    let mut bootstrap = repository.manifest("app.json");
    bootstrap.spec.services.clear();
    let saved = harness.save(bootstrap, Some(0)).await;
    let environment = harness.production(&saved).await;
    // The image service fails preparation after the manifest is fetched.
    harness.docker.arm_tag_flips(100).await;
    let failed = harness
        .deploy_revision(
            &environment,
            Some(ManifestRevision::Branch("feature".into())),
        )
        .await;
    assert_eq!(
        failed.error_code.as_deref(),
        Some("image_resolution_failed")
    );
    std::fs::write(&dockerfile, "FROM alpine:3.21\n").unwrap();
    repository.commit(&[]);
    harness.reopen().await;
    harness.docker.arm_tag_flips(0).await;
    let retry = harness
        .applications()
        .reconcile(&environment, None)
        .await
        .unwrap();
    assert_eq!(retry.id, failed.id);
    harness.finish(&retry).await;
    let resolved = harness.target(&environment).await;
    let commit = resolved
        .services
        .iter()
        .find_map(|service| match &service.source {
            ResolvedSource::Git { commit, .. } => Some(commit),
            ResolvedSource::Image { .. } => None,
        });
    assert_eq!(commit, Some(&pinned));
}

#[tokio::test]
async fn bootstrap_can_pin_manifest_revision_independently_from_git_build_revision() {
    let repository = Repository::new();
    let harness = ControllerHarness::new().await;
    let build_repository = Repository::new();
    std::fs::write(
        build_repository.path().join("Dockerfile"),
        "FROM alpine:3.20\n",
    )
    .unwrap();
    let build_commit = build_repository.commit(&[]);
    let mut fetched = repository.manifest("app.json");
    fetched.spec.services[0].source = piqueld_core::Source::Git {
        repository: piqueld_core::manifest::SourceRepository::Git(GitRepository {
            url: build_repository.path().display().to_string(),
            branch: "main".into(),
            commit: Some(build_commit.clone()),
        }),
        build: git_fixture::dockerfile_build(),
    };
    repository.write("app.json", &fetched);
    let pinned = repository.commit(&[]);
    let mut bootstrap = repository.manifest("app.json");
    bootstrap.spec.services.clear();
    bootstrap.spec.manifest.as_mut().unwrap().repository.commit = Some(pinned);
    fetched.metadata.name = "wrong-later-name".into();
    repository.write("app.json", &fetched);
    repository.commit(&[]);
    let saved = harness.save(bootstrap, Some(0)).await;
    let environment = harness.production(&saved).await;
    let deployment = harness.deploy(&environment).await;
    assert_eq!(deployment.state, OperationState::Succeeded);
    let resolved = harness
        .store
        .get(&environment)
        .await
        .unwrap()
        .resolved
        .unwrap();
    assert!(
        matches!(&resolved.services[0].source, ResolvedSource::Git { commit, .. } if commit == &build_commit)
    );
}
