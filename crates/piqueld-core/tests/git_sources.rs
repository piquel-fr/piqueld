//! Git manifest validation and immutable resolution contracts.
use piqueld_core::edit::ApplicationEdit;
use piqueld_core::manifest::{Build, GitRepository, ManifestRevision, Source, SourceRepository};
use piqueld_core::resource::{ResolvedSource, Sha256Digest};
use piqueld_core::{
    ApplicationId, EnvironmentId, InstanceId, ResolutionSet, compile_application, parse_json,
};

fn manifest() -> serde_json::Value {
    serde_json::json!({
        "api_version": "piqueld.dev/v1alpha1", "kind": "Application", "metadata": {"name": "example"},
        "spec": {"services": [{"name": "web", "source": {"type": "git", "repository": {"url": "https://example.com/app.git", "branch": "main"}, "build": {"type": "docker", "dockerfile": "./Dockerfile"}}}]}
    })
}

#[test]
fn git_sources_validate_paths_and_explicit_build_backend() {
    let valid = manifest();
    let parsed = parse_json(&valid.to_string())
        .unwrap()
        .normalize(ApplicationId::parse("example-id").unwrap());
    let Source::Git {
        build: Build::Docker { context, .. },
        ..
    } = &parsed.spec().services[0].source
    else {
        panic!("Git source expected")
    };
    assert_eq!(context, ".");
    for (field, value) in [
        ("dockerfile", "../Dockerfile"),
        ("context", "/etc"),
        ("dockerfile", ".git/config"),
        ("type", "auto"),
    ] {
        let mut invalid = valid.clone();
        invalid["spec"]["services"][0]["source"]["build"][field] = value.into();
        assert!(
            parse_json(&invalid.to_string()).is_err(),
            "{field}: {value}"
        );
    }
    for (field, value) in [
        ("branch", "--upload-pack=bad"),
        ("branch", "@"),
        ("branch", "main~1"),
        ("commit", "HEAD"),
        ("url", "--help"),
        ("url", "https://user:token@example.com/repo.git"),
        ("url", "HTTPS://token@example.com/repo.git"),
        ("url", "ssh://git:password@example.com/repo.git"),
    ] {
        let mut invalid = valid.clone();
        invalid["spec"]["services"][0]["source"]["repository"][field] = value.into();
        assert!(
            parse_json(&invalid.to_string()).is_err(),
            "{field}: {value}"
        );
    }
}

#[test]
fn git_resolution_retains_commit_and_local_image_and_rejects_mismatched_inputs() {
    let app = parse_json(&manifest().to_string())
        .unwrap()
        .normalize(ApplicationId::parse("example-id").unwrap());
    let mut resolutions = ResolutionSet::default();
    let image_id = Sha256Digest::parse(format!("sha256:{}", "a".repeat(64))).unwrap();
    resolutions.sources.insert(
        piqueld_core::ServiceName::parse("web").unwrap(),
        ResolvedSource::Git {
            requested: app.spec().services[0].source.clone(),
            commit: "b".repeat(40),
            image_id: image_id.clone(),
        },
    );
    let instance = InstanceId::parse("test").unwrap();
    let environment = EnvironmentId::parse("example-id").unwrap();
    let resolved = compile_application(&app, &environment, instance.clone(), &resolutions).unwrap();
    assert_eq!(resolved.services[0].image.as_str(), image_id.as_str());
    assert_eq!(resolved.reusable_resolutions(&app), resolutions);
    let mut changed = app.to_manifest();
    let Source::Git {
        repository: SourceRepository::Git(repository),
        ..
    } = &mut changed.spec.services[0].source
    else {
        unreachable!()
    };
    repository.commit = Some("c".repeat(40));
    let changed = changed.validate().unwrap().normalize(app.id().clone());
    assert!(compile_application(&changed, &environment, instance, &resolutions).is_err());
    assert!(resolved.reusable_resolutions(&changed).sources.is_empty());
}

#[test]
fn self_sources_build_from_the_manifest_revision() {
    let mut valid = manifest();
    valid["spec"]["services"][0]["source"]["repository"] = "self".into();
    let error = parse_json(&valid.to_string()).unwrap_err();
    assert_eq!(error.0[0].code, "manifest_repository_required");
    valid["spec"]["manifest"] = serde_json::json!({
        "path": "app.json",
        "repository": {"url": "https://example.com/app.git", "branch": "main"},
    });
    let app = parse_json(&valid.to_string())
        .unwrap()
        .normalize(ApplicationId::parse("example-id").unwrap());
    let manifest_repository = |app: &piqueld_core::NormalizedApplication| {
        app.spec().manifest.as_ref().unwrap().repository.clone()
    };
    let service_repository = |app: &piqueld_core::NormalizedApplication| {
        let Source::Git { repository, .. } = &app.spec().services[0].source else {
            unreachable!()
        };
        repository.clone()
    };

    let commit = "b".repeat(40);
    let pinned = app.clone().pin_manifest_sources(&commit);
    assert_eq!(
        service_repository(&pinned),
        SourceRepository::Git(GitRepository {
            commit: Some(commit.clone()),
            ..manifest_repository(&app)
        })
    );
    assert_eq!(pinned.spec().manifest, app.spec().manifest);

    let feature = app
        .clone()
        .with_manifest_revision(&ManifestRevision::Branch("feature".into()))
        .unwrap();
    assert_eq!(manifest_repository(&feature).branch, "feature");
    let at_commit = feature
        .with_manifest_revision(&ManifestRevision::Commit(commit.clone()))
        .unwrap();
    assert_eq!(manifest_repository(&at_commit).commit, Some(commit));
    assert!(
        app.clone()
            .with_manifest_revision(&ManifestRevision::Commit("HEAD".into()))
            .is_err()
    );

    // Disconnecting keeps building from the former manifest repository.
    let mut disconnected = app.to_manifest();
    ApplicationEdit::Repository(None)
        .apply(&mut disconnected)
        .unwrap();
    let disconnected = disconnected.validate().unwrap().normalize(app.id().clone());
    assert_eq!(
        service_repository(&disconnected),
        SourceRepository::Git(manifest_repository(&app))
    );
    assert!(
        disconnected
            .with_manifest_revision(&ManifestRevision::Branch("main".into()))
            .is_err()
    );
}
