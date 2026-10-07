use super::*;

#[test]
fn rendering_selects_each_environments_values_and_never_reevaluates_text() {
    let template = template(VARIABLES);
    let exported = parse_template_toml(&template.export_toml().unwrap())
        .unwrap()
        .normalize(template.id().clone());
    assert_eq!(exported, template);
    let staging = render(&template, "staging").unwrap();
    let service = &staging.application.spec().services[0];
    assert_eq!(service.replicas, 1);
    assert_eq!(
        service.environment["GREETING"],
        "hello staging, keep ${HOME} and $USER, escape ${{ vars.tag }}"
    );
    assert_eq!(
        staging.values["vars.domain"],
        VariableValue::String("staging.piquel.fr".into())
    );
    assert_eq!(
        staging.values["deployment.id"],
        VariableValue::String("operation-1".into())
    );

    let production = render(&template, "production").unwrap();
    assert_eq!(production.application.spec().services[0].replicas, 3);
    assert_eq!(
        production.values["vars.domain"],
        VariableValue::String("piquel.fr".into())
    );

    // A value that looks like a reference is substituted as text, not evaluated.
    let quoted = template.clone();
    let mut manifest = quoted.to_manifest();
    manifest
        .spec
        .variables
        .insert("tag".into(), Variable::String("$${{ vars.domain }}".into()));
    manifest.spec.services[0]
        .environment
        .insert("TAG".into(), "${{ vars.tag }}".into());
    manifest.spec.services[0].source = Source::Image {
        image: "nginx:stable".into(),
    };
    let rendered = render(
        &manifest
            .validate_template()
            .unwrap()
            .normalize(template.id().clone()),
        "production",
    )
    .unwrap();
    assert_eq!(
        rendered.application.spec().services[0].environment["TAG"],
        "${{ vars.domain }}"
    );
}

#[test]
fn whole_references_keep_their_type_and_mixed_text_renders_to_a_string() {
    let typed = |replicas: &str, value: &str| {
        let spec = format!("[spec.variables]\nweb_replicas = {value}\ntag = \"stable\"\n");
        let text = manifest(&spec).replace("\"${{ vars.web_replicas }}\"", replicas);
        parse_template_toml(&text).unwrap().normalize(id())
    };
    let integer = typed("\"${{ vars.web_replicas }}\"", "2");
    assert_eq!(
        render(&integer, "production")
            .unwrap()
            .application
            .spec()
            .services[0]
            .replicas,
        2
    );
    // Strings are never parsed into numbers, even when they look like one.
    for (replicas, value) in [
        ("\"${{ vars.web_replicas }}\"", "\"2\""),
        ("\"${{ vars.web_replicas }}\"", "true"),
        ("\"1${{ vars.web_replicas }}\"", "2"),
    ] {
        let errors = render(&typed(replicas, value), "production").unwrap_err();
        assert_eq!(
            codes_and_paths(&errors),
            [(codes::VARIABLE_TYPE_INVALID, "spec.services[0].replicas")],
            "{replicas} with {value}"
        );
    }
    // A rendered value still passes the field's own validation.
    let errors = render(&typed("\"${{ vars.web_replicas }}\"", "0"), "production").unwrap_err();
    assert_eq!(
        codes_and_paths(&errors),
        [(codes::REPLICAS_OUT_OF_RANGE, "spec.services[0].replicas")]
    );
    // Mixed interpolation yields a string, which an enum field may accept.
    let order = manifest("[spec.variables]\nweb_replicas = 1\ntag = \"stable\"\nkind = \"stop\"\n")
        .replace(
            "[spec.services.source]",
            "[spec.services.rollout]\norder = \"${{ vars.kind }}-first\"\n[spec.services.source]",
        );
    let order = parse_template_toml(&order).unwrap().normalize(id());
    assert_eq!(
        render(&order, "production")
            .unwrap()
            .application
            .spec()
            .services[0]
            .rollout
            .order,
        Some(RolloutOrder::StopFirst)
    );
}

#[test]
fn variables_without_a_value_fail_only_the_environment_that_needs_them() {
    // `tag` has a value only in staging; `domain` only in production.
    let template = template(
        "[spec.variables]\nweb_replicas = 1\n[spec.environments.staging.variables]\ntag = \"stable\"\n[spec.environments.production.variables]\ndomain = \"piquel.fr\"\n",
    );
    assert!(render(&template, "staging").is_ok());
    let errors = render(&template, "production").unwrap_err();
    assert_eq!(
        codes_and_paths(&errors),
        [(
            codes::VARIABLE_VALUE_MISSING,
            "spec.services[0].source.image"
        )]
    );
    assert_eq!(
        errors.0[0].message,
        "vars.tag has no value for environment production"
    );
    let production = EnvironmentName::parse("production").unwrap();
    let values = template.values(&production);
    assert_eq!(values["tag"], None);
    assert_eq!(
        values["domain"],
        Some(VariableValue::String("piquel.fr".into()))
    );
}

/// Repeating a large variable cannot expand a small manifest without bound.
#[test]
fn rendering_stops_at_its_size_budget() {
    let large = "x".repeat(60_000);
    let references = "${{ vars.large }}".repeat(100);
    let template = template(&format!(
        "[spec.variables]\nweb_replicas = 1\ntag = \"stable\"\nlarge = \"{large}\"\n"
    ))
    .to_manifest();
    let mut manifest = template;
    manifest.spec.services[0]
        .environment
        .insert("LARGE".into(), references.as_str().into());
    let template = manifest.validate_template().unwrap().normalize(id());
    let errors = render(&template, "production").unwrap_err();
    assert_eq!(
        codes_and_paths(&errors),
        [(
            codes::VARIABLE_VALUE_EXCESSIVE,
            "spec.services[0].environment.LARGE"
        )]
    );
}

#[test]
fn every_templated_field_renders_from_variables() {
    let literal = complete();
    let expected = literal.clone().validate().unwrap().normalize(id());
    let wire = serde_json::to_value(&literal).unwrap();
    for &(pointer, path) in FIELDS {
        let mut input = wire.clone();
        let value = input.pointer(pointer).unwrap().clone();
        *input.pointer_mut(pointer).unwrap() = json!("${{ vars.value }}");
        input["spec"]["variables"] = json!({"value": value});
        let template = normalize(serde_json::from_value(input).unwrap());
        let before = template.clone();
        let rendering =
            render(&template, "production").unwrap_or_else(|errors| panic!("{path}: {errors}"));
        assert_eq!(rendering.application, expected, "{path}");
        assert_eq!(
            render(&template, "production").unwrap(),
            rendering,
            "{path}"
        );
        assert_eq!(template, before);
        assert!(
            rendering
                .application
                .to_manifest()
                .spec
                .variables
                .is_empty()
        );
        assert!(
            rendering
                .application
                .to_manifest()
                .spec
                .environments
                .is_empty()
        );
        assert_eq!(
            serde_json::to_value(&rendering.values["vars.value"]).unwrap(),
            value
        );
    }
}

#[test]
fn every_templated_field_reports_its_missing_value_path() {
    let wire = serde_json::to_value(complete()).unwrap();
    for &(pointer, path) in FIELDS {
        let mut input = wire.clone();
        let value = input.pointer(pointer).unwrap().clone();
        *input.pointer_mut(pointer).unwrap() = json!("${{ vars.value }}");
        input["spec"]["environments"] = json!({"staging": {"variables": {"value": value}}});
        let template = normalize(serde_json::from_value(input).unwrap());
        let errors = render(&template, "production").unwrap_err();
        // Normalization sorts the templated hostname before literal hostnames.
        let path = if pointer == "/spec/routes/1/hostname" {
            "spec.routes[0].hostname"
        } else {
            path
        };
        assert_eq!(
            codes_and_paths(&errors),
            [(codes::VARIABLE_VALUE_MISSING, path)]
        );
        assert_eq!(
            errors.0[0].message,
            "vars.value has no value for environment production"
        );
        assert!(render(&template, "staging").is_ok(), "{path}");
    }
}

#[test]
fn visit_values_covers_all_present_slots_and_skips_absent_options() {
    let mut spec = complete().spec;
    let mut paths = Vec::new();
    spec.visit_values(&mut |path, _| paths.push(path.to_owned()));
    paths.sort();
    let mut expected: Vec<_> = FIELDS.iter().map(|(_, path)| (*path).to_owned()).collect();
    expected.sort();
    assert_eq!(paths, expected);
    for service in &mut spec.services {
        service.command.clear();
        service.arguments.clear();
        service.environment.clear();
        service.healthcheck = None;
        service.resources = Some(ResourceLimits {
            cpu_millis: None,
            memory_bytes: None,
        });
        service.rollout = Rollout::default();
        if let Source::Git {
            build: Build::Docker { args, target, .. },
            ..
        } = &mut service.source
        {
            args.clear();
            *target = None;
        }
    }
    spec.jobs.clear();
    spec.routes.clear();
    paths.clear();
    spec.visit_values(&mut |path, _| paths.push(path.to_owned()));
    assert_eq!(
        paths,
        [
            "spec.services[0].source.image",
            "spec.services[0].replicas",
            "spec.services[1].source.build.dockerfile",
            "spec.services[1].source.build.context",
            "spec.services[1].replicas"
        ]
    );
}

#[test]
fn contexts_supply_exact_system_values_and_explain_unavailable_ones() {
    let mut manifest = base();
    manifest.spec.manifest = Some(repository());
    let spec = &manifest.spec;
    let pinned_sha = "a".repeat(40);
    let deployment = RenderContext::deployment(environment("production"), "operation-1".into());
    let mut with_git = deployment.clone();
    with_git.git = Some(GitRevision {
        branch: "release".into(),
        sha: pinned_sha.clone(),
    });
    let preview = RenderContext::preview(environment("production"), spec.manifest.as_ref());
    let mut pinned = spec.clone();
    pinned.manifest.as_mut().unwrap().repository.commit = Some(pinned_sha.clone());
    let pinned_preview =
        RenderContext::preview(environment("production"), pinned.manifest.as_ref());
    assert_eq!(
        RenderContext::preview(environment("production"), None).git,
        None
    );
    for (context, git, deployment_id) in [
        (deployment, None, Some("operation-1")),
        (
            with_git,
            Some(("release", pinned_sha.as_str())),
            Some("operation-1"),
        ),
        (
            preview,
            Some(("main", "0000000000000000000000000000000000000000")),
            Some(PREVIEW_DEPLOYMENT_ID),
        ),
        (
            pinned_preview,
            Some(("main", pinned_sha.as_str())),
            Some(PREVIEW_DEPLOYMENT_ID),
        ),
        (saved(), None, None),
    ] {
        let mut expected = std::collections::BTreeMap::from([
            ("app.name".to_owned(), VariableValue::String("notes".into())),
            (
                "env.name".to_owned(),
                VariableValue::String("production".into()),
            ),
            (
                "env.slug".to_owned(),
                VariableValue::String("production".into()),
            ),
        ]);
        if let Some((branch, sha)) = git {
            assert_eq!(
                context.git.as_ref().unwrap(),
                &GitRevision {
                    branch: branch.into(),
                    sha: sha.into()
                }
            );
            expected.insert("git.branch".into(), VariableValue::String(branch.into()));
            expected.insert("git.sha".into(), VariableValue::String(sha.into()));
        }
        if let Some(value) = deployment_id {
            expected.insert("deployment.id".into(), VariableValue::String(value.into()));
        }
        assert_eq!(
            normalize(manifest.clone()).render(&context).unwrap().values,
            expected
        );
        for system in SystemVariable::ALL {
            let mut input = manifest.clone();
            input.spec.services[0].environment.insert(
                "VALUE".into(),
                format!("${{{{ {} }}}}", system.as_str()).into(),
            );
            let result = normalize(input).render(&context);
            if let Some(value) = expected.get(system.as_str()) {
                assert_eq!(
                    result.unwrap().application.spec().services[0].environment["VALUE"],
                    value.to_string()
                );
            } else {
                let errors = result.unwrap_err();
                assert_eq!(
                    codes_and_paths(&errors),
                    [(
                        codes::VARIABLE_UNAVAILABLE,
                        "spec.services[0].environment.VALUE"
                    )]
                );
                let reason = if system == SystemVariable::DeploymentId {
                    "when deploying production"
                } else {
                    "for repository-backed deployments of production"
                };
                assert_eq!(
                    errors.0[0].message,
                    format!("{} is only set {reason}", system.as_str())
                );
            }
        }
    }
}

#[test]
fn variable_values_render_system_references_overrides_and_native_interpolation() {
    let mut manifest = base();
    manifest.spec.variables = std::collections::BTreeMap::from([
        (
            "text".into(),
            Variable::String("${{app.name}}/${{env.name}}/${{env.slug}}/${{deployment.id}}".into()),
        ),
        ("flag".into(), Variable::Boolean(true)),
        ("number".into(), Variable::Integer(-12)),
        ("changed".into(), Variable::Integer(1)),
    ]);
    manifest.spec.environments.insert(
        "production".into(),
        EnvironmentConfig {
            variables: std::collections::BTreeMap::from([
                ("changed".into(), Variable::Boolean(false)),
                ("only".into(), Variable::String("${{env.name}}".into())),
            ]),
        },
    );
    manifest.spec.services[0].environment.insert(
        "VALUES".into(),
        "${{vars.text}}: ${{vars.flag}}/${{vars.number}}/${{vars.changed}}".into(),
    );
    let template = normalize(manifest);
    let result = render(&template, "production").unwrap();
    assert_eq!(
        result.application.spec().services[0].environment["VALUES"],
        "notes/production/production/operation-1: true/-12/false"
    );
    assert_eq!(result.values["vars.changed"], VariableValue::Boolean(false));
    assert_eq!(
        result.values["vars.only"],
        VariableValue::String("production".into())
    );
    assert_eq!(template.values(&environment("staging"))["only"], None);
    assert_eq!(template.values(&environment("production"))["text"], None);
    assert_eq!(
        render(&template, "staging").unwrap().values["vars.changed"],
        VariableValue::Integer(1)
    );
}

#[test]
fn typed_fields_reject_wrong_types_ranges_and_mixed_text() {
    let wire = serde_json::to_value(complete()).unwrap();
    for (pointer, path, expression, value) in [
        (
            "/spec/services/0/replicas",
            "spec.services[0].replicas",
            "${{vars.n}}",
            json!(70000),
        ),
        (
            "/spec/services/0/replicas",
            "spec.services[0].replicas",
            "${{vars.n}}",
            json!(true),
        ),
        (
            "/spec/services/0/resources/cpu_millis",
            "spec.services[0].resources.cpu_millis",
            "${{vars.n}}",
            json!(-1),
        ),
        (
            "/spec/services/0/replicas",
            "spec.services[0].replicas",
            "${{vars.n}}${{vars.n}}",
            json!(2),
        ),
        (
            "/spec/services/0/replicas",
            "spec.services[0].replicas",
            " ${{vars.n}}",
            json!(2),
        ),
        (
            "/spec/services/0/replicas",
            "spec.services[0].replicas",
            "$${{vars.n}}",
            json!(2),
        ),
        (
            "/spec/services/0/rollout/order",
            "spec.services[0].rollout.order",
            "${{vars.n}}",
            json!("sideways"),
        ),
    ] {
        let mut input = wire.clone();
        *input.pointer_mut(pointer).unwrap() = json!(expression);
        input["spec"]["variables"] = json!({"n": value});
        let errors = render(
            &normalize(serde_json::from_value(input).unwrap()),
            "production",
        )
        .unwrap_err();
        assert_eq!(
            codes_and_paths(&errors),
            [(codes::VARIABLE_TYPE_INVALID, path)],
            "{expression} with {value}"
        );
        assert!(errors.0[0].message.starts_with("renders to "));
        assert!(errors.0[0].message.contains(", expected "));
    }
    for order in ["start-first", "stop-first"] {
        let mut input = wire.clone();
        input["spec"]["services"][0]["rollout"]["order"] = json!("${{ \t vars.n \n }}");
        input["spec"]["variables"] = json!({"n": order});
        let result = render(
            &normalize(serde_json::from_value(input).unwrap()),
            "production",
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(result.application.spec().services[0].rollout.order).unwrap(),
            json!(order)
        );
    }
}

#[test]
fn rendering_collects_sorted_errors_and_only_the_first_missing_value_per_field() {
    let mut manifest = base();
    manifest.spec.environments.insert(
        "staging".into(),
        EnvironmentConfig {
            variables: std::collections::BTreeMap::from([
                ("a".into(), Variable::Integer(1)),
                ("b".into(), Variable::Integer(2)),
            ]),
        },
    );
    manifest.spec.services[0].command = vec!["${{vars.b}}${{vars.a}}".into()];
    manifest.spec.services[0].replicas = Typed::Template("${{vars.a}}".into());
    manifest.spec.services[0]
        .environment
        .insert("VALUE".into(), "${{vars.a}}${{vars.b}}".into());
    let errors = render(&normalize(manifest), "production").unwrap_err();
    assert_eq!(
        codes_and_paths(&errors),
        [
            (codes::VARIABLE_VALUE_MISSING, "spec.services[0].command[0]"),
            (
                codes::VARIABLE_VALUE_MISSING,
                "spec.services[0].environment.VALUE"
            ),
            (codes::VARIABLE_VALUE_MISSING, "spec.services[0].replicas"),
        ]
    );
    assert!(errors.0[0].message.starts_with("vars.b "));
    assert!(errors.0[1].message.starts_with("vars.a "));
}

/// Direct spec rendering also reports malformed input, before save-time validation.
#[test]
fn raw_rendering_reports_parse_errors_and_variable_declaration_paths() {
    for (overridden, path) in [
        (false, "spec.variables.bad"),
        (true, "spec.environments.production.variables.bad"),
    ] {
        let mut spec = base().spec;
        let variable = Variable::String("${{deployment.id}}".into());
        if overridden {
            spec.environments.insert(
                "production".into(),
                EnvironmentConfig {
                    variables: std::collections::BTreeMap::from([("bad".into(), variable)]),
                },
            );
        } else {
            spec.variables.insert("bad".into(), variable);
        }
        spec.services[0].replicas = Typed::Template("${{".into());
        spec.services[0].command = vec!["prefix ${{".into()];
        let mut errors = Vec::new();
        spec.render("notes", &saved(), &mut errors);
        let mut expected = vec![
            (codes::TEMPLATE_INVALID, "spec.services[0].command[0]"),
            (codes::TEMPLATE_INVALID, "spec.services[0].replicas"),
            (codes::VARIABLE_UNAVAILABLE, path),
        ];
        expected.sort_by_key(|&(code, path)| (path, code));
        assert_eq!(codes_and_paths(&ValidationErrors::sorted(errors)), expected);
        assert!(spec.variables.is_empty());
        assert!(spec.environments.is_empty());
    }
}

/// Use raw rendering to isolate the expansion budget from per-field size limits.
#[test]
fn text_budget_accepts_exact_limit_and_reports_only_first_overflow() {
    const LIMIT: usize = 4 * 1024 * 1024;
    for (bytes, expected_errors) in [(LIMIT, 0), (LIMIT + 1, 1)] {
        let mut spec = base().spec;
        // Image is visited first, followed by environment entries in key order.
        spec.services[0].source = Source::Image { image: "".into() };
        spec.services[0].environment = std::collections::BTreeMap::from([
            ("A".into(), "é".repeat(bytes / 2).into()),
            ("B".into(), "x".repeat(bytes % 2).into()),
        ]);
        let mut errors = Vec::new();
        spec.render("notes", &saved(), &mut errors);
        assert_eq!(errors.len(), expected_errors);
        if expected_errors != 0 {
            assert_eq!(
                codes_and_paths(&ValidationErrors(errors)),
                [(
                    codes::VARIABLE_VALUE_EXCESSIVE,
                    "spec.services[0].environment.B"
                )]
            );
        }
    }
    let mut spec = base().spec;
    spec.services[0].command = vec!["x".repeat(LIMIT + 1).into(), "later".into()];
    let mut errors = Vec::new();
    spec.render("notes", &saved(), &mut errors);
    assert_eq!(
        codes_and_paths(&ValidationErrors(errors)),
        [(
            codes::VARIABLE_VALUE_EXCESSIVE,
            "spec.services[0].command[0]"
        )]
    );
    assert_eq!(spec.services[0].command[1], "later");
}
