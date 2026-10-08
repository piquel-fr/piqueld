use super::*;

#[test]
fn hostnames_render_per_environment_before_deployment() {
    let template = template(&format!(
        "{VARIABLES}\n[[spec.routes]]\nhostname = \"${{{{ vars.domain }}}}\"\nservice = \"web\"\nport = 80\n[[spec.routes]]\nhostname = \"${{{{ deployment.id }}}}.piquel.fr\"\nservice = \"web\"\nport = 80\n"
    ));
    let hostnames = |environment: &str| {
        template
            .hostnames(&EnvironmentName::parse(environment).unwrap())
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
    };
    // Routes that only render for a deployment reserve nothing yet.
    assert_eq!(hostnames("staging"), ["staging.piquel.fr"]);
    assert_eq!(hostnames("production"), ["piquel.fr"]);
}

/// Saved configuration and replay fingerprints were stored as normalized
/// applications; literal templates must serialize identically.
#[test]
fn literal_templates_serialize_like_normalized_applications() {
    let normalized = parse_toml(include_str!(
        "../../../tests/fixtures/manifests/prebuilt.toml"
    ))
    .unwrap()
    .normalize(id());
    let template = ApplicationTemplate::from(&normalized);
    assert_eq!(
        serde_json::to_string(&template).unwrap(),
        normalized.canonical_json().unwrap()
    );
    assert_eq!(
        serde_json::from_str::<ApplicationTemplate>(&normalized.canonical_json().unwrap()).unwrap(),
        template
    );
}

#[test]
fn literal_rendering_conversion_hash_and_exports_match_the_domain_model() {
    for manifest in [base(), complete()] {
        let validated = manifest.clone().validate_template().unwrap();
        assert_eq!(validated.name().as_str(), "notes");
        assert_eq!(validated.spec(), &manifest.spec);
        let normalized = manifest.clone().validate().unwrap().normalize(id());
        assert_eq!(
            validated.clone().into_literal().unwrap().normalize(id()),
            normalized
        );
        let template = validated.normalize(id());
        assert_eq!(template.id(), &id());
        assert_eq!(template.metadata(), normalized.metadata());
        assert_eq!(template.spec(), &manifest.spec);
        assert_eq!(ApplicationTemplate::from(&normalized), template);
        assert_eq!(
            template.canonical_json().unwrap(),
            normalized.canonical_json().unwrap()
        );
        assert_eq!(template.spec_hash(), normalized.spec_hash());
        assert_eq!(template.render(&saved()).unwrap().application, normalized);
        assert_eq!(
            parse_toml(&template.export_toml().unwrap())
                .unwrap()
                .normalize(id()),
            normalized
        );
        assert_eq!(
            parse_template_json(&serde_json::to_string(&template.to_manifest()).unwrap())
                .unwrap()
                .normalize(id()),
            template
        );
    }
}

#[test]
fn normalization_orders_unordered_collections_but_preserves_process_order() {
    let mut manifest = complete();
    let mut third = manifest.spec.services[0].clone();
    third.name = "c-image".into();
    manifest.spec.services.push(third);
    manifest.spec.services[0].depends_on = vec!["c-image".into(), "b-build".into()];
    manifest.spec.volumes = vec![Volume { name: "z".into() }, Volume { name: "a".into() }];
    manifest.spec.services[0].mounts = vec![
        Mount {
            volume: "z".into(),
            target: "/z".into(),
            read_only: true,
        },
        Mount {
            volume: "a".into(),
            target: "/a".into(),
            read_only: true,
        },
    ];
    manifest.spec.services[0].secrets = vec![
        SecretMount {
            name: "z".into(),
            target: "/run/secrets/z".into(),
        },
        SecretMount {
            name: "a".into(),
            target: "/run/secrets/a".into(),
        },
    ];
    manifest.spec.secrets = serde_json::from_value(json!([
        {"name":"z", "generate":{"type":"random", "bytes":32, "encoding":"hex"}},
        {"name":"a", "generate":{"type":"random", "bytes":32, "encoding":"hex"}}
    ]))
    .unwrap();
    let first = normalize(manifest.clone());
    manifest.spec.services.reverse();
    manifest.spec.routes.reverse();
    manifest.spec.volumes.reverse();
    manifest.spec.secrets.reverse();
    for service in &mut manifest.spec.services {
        service.mounts.reverse();
        service.secrets.reverse();
        service.depends_on.reverse();
    }
    let second = normalize(manifest.clone());
    assert_eq!(first, second);
    assert_eq!(
        first.canonical_json().unwrap(),
        second.canonical_json().unwrap()
    );
    assert_eq!(first.spec_hash(), second.spec_hash());
    assert_eq!(
        first.spec_hash(),
        manifest.validate().unwrap().normalize(id()).spec_hash()
    );
    assert_eq!(
        first.spec().services[0].command,
        [Template::from("sh"), Template::from("-c")]
    );
    assert_eq!(first.spec().services[0].depends_on, ["b-build", "c-image"]);
    assert_eq!(normalize(first.to_manifest()), first);
}

#[test]
fn into_literal_reports_each_unresolved_slot_in_path_order() {
    let wire = serde_json::to_value(complete()).unwrap();
    let mut input = wire.clone();
    input["spec"]["variables"] = json!({"value":"unused"});
    for &(pointer, _) in FIELDS {
        *input.pointer_mut(pointer).unwrap() = json!("${{vars.value}}");
    }
    let validated = parse_template_json(&input.to_string()).unwrap();
    let errors = validated.into_literal().unwrap_err();
    let mut expected: Vec<_> = FIELDS
        .iter()
        .map(|&(_, path)| (codes::VARIABLE_UNRESOLVED, path))
        .collect();
    expected.sort_by_key(|&(_, path)| path);
    assert_eq!(codes_and_paths(&errors), expected);
    assert!(
        errors
            .0
            .iter()
            .all(|error| error.message == "references must be rendered for an environment first")
    );
}

#[test]
fn template_export_round_trips_overrides_escapes_and_identity() {
    let original = template(VARIABLES);
    let id = ApplicationId::parse("app-renamed-02").unwrap();
    let name = crate::ApplicationName::parse("renamed").unwrap();
    let changed = original.clone().with_id(id.clone()).with_name(name.clone());
    assert_eq!(changed.id(), &id);
    assert_eq!(&changed.metadata().name, &name);
    assert_eq!(changed.spec(), original.spec());
    assert_eq!(changed.spec_hash(), original.spec_hash());
    assert_ne!(
        changed.canonical_json().unwrap(),
        original.canonical_json().unwrap()
    );
    assert_eq!(changed.to_manifest().metadata.name, "renamed");
    assert!(changed.configures(&environment("production")));
    assert!(changed.configures(&environment("staging")));
    assert!(!changed.configures(&environment("other")));
    let exported = changed.export_toml().unwrap();
    assert!(exported.contains("$${{ vars.tag }}"));
    assert_eq!(
        parse_template_toml(&exported).unwrap().normalize(id),
        changed
    );
    assert_eq!(
        serde_json::from_str::<ApplicationTemplate>(&changed.canonical_json().unwrap()).unwrap(),
        changed
    );
    let mut edited = changed.to_manifest();
    edited
        .spec
        .variables
        .insert("tag".into(), Variable::String("next".into()));
    assert_ne!(normalize(edited).spec_hash(), changed.spec_hash());
    assert_eq!(
        render(&changed, "production").unwrap().values["app.name"],
        VariableValue::String("renamed".into())
    );
}

#[test]
fn revision_overrides_validate_and_do_not_change_the_spec_hash() {
    let mut manifest = base();
    manifest.spec.manifest = Some(repository());
    let template = normalize(manifest);
    let sha = "a".repeat(40);
    let pinned = template
        .clone()
        .with_manifest_revision(&ManifestRevision::Commit(sha.clone()))
        .unwrap();
    assert_eq!(
        pinned.spec().manifest.as_ref().unwrap().repository.commit,
        Some(sha)
    );
    let branched = pinned
        .clone()
        .with_manifest_revision(&ManifestRevision::Branch("release".into()))
        .unwrap();
    let repository = &branched.spec().manifest.as_ref().unwrap().repository;
    assert_eq!(repository.branch, "release");
    assert_eq!(repository.commit, None);
    assert_eq!(pinned.spec_hash(), template.spec_hash());
    assert_eq!(branched.spec_hash(), template.spec_hash());
    assert_eq!(normalize(base()).spec_hash(), template.spec_hash());
    for revision in [
        ManifestRevision::Branch("-bad".into()),
        ManifestRevision::Commit("short".into()),
    ] {
        let errors = template
            .clone()
            .with_manifest_revision(&revision)
            .unwrap_err();
        assert!(
            errors
                .0
                .iter()
                .all(|error| error.path == "spec.manifest.repository")
        );
        assert_eq!(errors.0.len(), 1);
    }
    let errors = normalize(base())
        .with_manifest_revision(&ManifestRevision::Branch("main".into()))
        .unwrap_err();
    assert_eq!(
        codes_and_paths(&errors),
        [("manifest_repository_required", "spec.manifest.repository")]
    );
}

#[test]
fn deserialization_revalidates_and_normalizes_saved_templates() {
    let template = normalize(complete());
    let mut wire = serde_json::to_value(&template).unwrap();
    wire["spec"]["services"].as_array_mut().unwrap().reverse();
    wire["spec"]["routes"].as_array_mut().unwrap().reverse();
    assert_eq!(
        serde_json::from_value::<ApplicationTemplate>(wire.clone()).unwrap(),
        template
    );
    for (pointer, value, diagnostic) in [
        ("/id", json!(""), "application"),
        ("/api_version", json!("bad"), codes::API_VERSION_UNSUPPORTED),
        ("/kind", json!("bad"), codes::KIND_UNSUPPORTED),
        ("/metadata/name", json!("BAD"), codes::NAME_INVALID),
        (
            "/spec/services/0/replicas",
            json!(0),
            codes::REPLICAS_OUT_OF_RANGE,
        ),
        (
            "/spec/services/0/replicas",
            json!("${{vars.missing}}"),
            codes::VARIABLE_UNDECLARED,
        ),
    ] {
        let mut invalid = wire.clone();
        *invalid.pointer_mut(pointer).unwrap() = value;
        let error = serde_json::from_value::<ApplicationTemplate>(invalid)
            .unwrap_err()
            .to_string();
        assert!(error.contains(diagnostic), "{pointer}: {error}");
    }
    wire["unexpected"] = json!(true);
    assert!(
        serde_json::from_value::<ApplicationTemplate>(wire)
            .unwrap_err()
            .to_string()
            .contains("unknown field")
    );
}

#[test]
fn saved_hostnames_skip_missing_invalid_and_deployment_only_values() {
    let mut manifest = base();
    manifest.spec.manifest = Some(repository());
    manifest.spec.variables.insert(
        "domain".into(),
        Variable::String("UPPER.Example.COM.".into()),
    );
    manifest
        .spec
        .variables
        .insert("invalid".into(), Variable::String("not a hostname".into()));
    manifest.spec.environments.insert(
        "staging".into(),
        EnvironmentConfig {
            visibility: Visibility::Public,
            variables: std::collections::BTreeMap::from([(
                "missing".into(),
                Variable::String("stage.example.com".into()),
            )]),
        },
    );
    manifest.spec.routes = [
        "${{vars.domain}}",
        "${{vars.invalid}}",
        "${{vars.missing}}",
        "${{deployment.id}}.example.com",
        "${{git.sha}}.example.com",
        "literal.example.com",
    ]
    .map(|hostname| Route {
        hostname: hostname.into(),
        visibility: Visibility::Private,
        service: Some("web".into()),
        port: Some(80),
        redirect: None,
    })
    .into();
    let template = normalize(manifest);
    let names: Vec<_> = template
        .hostnames(&environment("production"))
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(names, ["upper.example.com", "literal.example.com"]);
}

#[test]
fn validation_errors_sort_by_path_then_code_stably() {
    let error = |path: &str, code: &str, message: &str| ValidationError {
        path: path.into(),
        code: code.into(),
        message: message.into(),
    };
    let errors = ValidationErrors::sorted(vec![
        error("z", "a", "last"),
        error("a", "z", "third"),
        error("a", "a", "first"),
        error("a", "a", "second"),
    ]);
    assert_eq!(
        codes_and_paths(&errors),
        [("a", "a"), ("a", "a"), ("z", "a"), ("a", "z")]
    );
    assert_eq!(errors.0[0].message, "first");
    assert_eq!(errors.0[1].message, "second");
    assert_eq!(ValidationErrors::sorted(Vec::new()).0, Vec::new());
}

#[test]
fn into_literal_preserves_text_escapes_and_rejects_unresolved_typed_escapes() {
    let mut manifest = base();
    manifest.spec.services[0]
        .environment
        .insert("VALUE".into(), "$${{vars.literal}}".into());
    let literal = manifest
        .clone()
        .validate_template()
        .unwrap()
        .into_literal()
        .unwrap();
    assert_eq!(
        literal.spec().services[0].environment["VALUE"],
        "${{vars.literal}}"
    );
    let normalized = literal.normalize(id());
    let converted = ApplicationTemplate::from(&normalized);
    assert_eq!(
        converted.spec().services[0].environment["VALUE"],
        "$${{vars.literal}}"
    );
    assert_eq!(converted.render(&saved()).unwrap().application, normalized);
    manifest.spec.services[0].replicas = Typed::Template("$${{vars.literal}}".into());
    let errors = manifest
        .validate_template()
        .unwrap()
        .into_literal()
        .unwrap_err();
    assert_eq!(
        codes_and_paths(&errors),
        [(codes::VARIABLE_UNRESOLVED, "spec.services[0].replicas")]
    );
}
