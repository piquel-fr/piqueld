//! Git manifest validation and immutable resolution contracts.
use piqueld_core::manifest::{Build, Source};
use piqueld_core::resource::{ResolvedSource, Sha256Digest};
use piqueld_core::{ApplicationId, InstanceId, ResolutionSet, compile_application, parse_json};

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
    } = &parsed.spec.services[0].source
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
        ("branch", "main~1"),
        ("commit", "HEAD"),
        ("url", "--help"),
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
        "web".into(),
        ResolvedSource::Git {
            requested: app.spec.services[0].source.clone(),
            commit: "b".repeat(40),
            image_id: image_id.clone(),
        },
    );
    let instance = InstanceId::parse("test").unwrap();
    let resolved = compile_application(&app, instance.clone(), &resolutions).unwrap();
    assert_eq!(resolved.services[0].image, image_id.as_str());
    assert_eq!(resolved.reusable_resolutions(&app), resolutions);
    let mut changed = app.clone();
    let Source::Git { repository, .. } = &mut changed.spec.services[0].source else {
        unreachable!()
    };
    repository.commit = Some("c".repeat(40));
    assert!(compile_application(&changed, instance, &resolutions).is_err());
    assert!(resolved.reusable_resolutions(&changed).sources.is_empty());
}
