//! Manifest variables: reference syntax, declarations, and rendering per environment.
use piqueld_core::manifest::{
    ApplicationTemplate, RenderContext, Template, VariableValue, parse_template_toml,
    variables::{Reference, Segment, SystemVariable},
};
use piqueld_core::{ApplicationId, EnvironmentName, codes};

/// A manifest with one web service, `spec` appended as written.
fn manifest(spec: &str) -> String {
    format!(
        r#"api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
{spec}
[[spec.services]]
name = "web"
replicas = "${{{{ vars.web_replicas }}}}"
[spec.services.source]
type = "image"
image = "nginx:${{{{ vars.tag }}}}"
[spec.services.environment]
GREETING = "hello ${{{{ env.name }}}}, keep ${{HOME}} and $USER, escape $${{{{ vars.tag }}}}"
"#
    )
}

const VARIABLES: &str = r#"
[spec.variables]
web_replicas = 1
tag = "stable"
domain = "piquel.fr"

[spec.environments.staging.variables]
domain = "staging.piquel.fr"

[spec.environments.production.variables]
web_replicas = 3
"#;

fn template(spec: &str) -> ApplicationTemplate {
    parse_template_toml(&manifest(spec))
        .unwrap()
        .normalize(ApplicationId::parse("app-notes-01").unwrap())
}

fn render(
    template: &ApplicationTemplate,
    environment: &str,
) -> Result<piqueld_core::manifest::Rendering, piqueld_core::ValidationErrors> {
    template.render(&RenderContext::deployment(
        EnvironmentName::parse(environment).unwrap(),
        "operation-1".into(),
    ))
}

fn codes_and_paths(errors: &piqueld_core::ValidationErrors) -> Vec<(&str, &str)> {
    errors
        .0
        .iter()
        .map(|error| (error.code.as_str(), error.path.as_str()))
        .collect()
}

#[test]
fn references_allow_whitespace_and_escapes_while_shell_syntax_stays_text() {
    let segments = |text: &str| Template::from(text).segments().unwrap();
    let domain = Segment::Reference(Reference::Variable("domain".into()));
    assert_eq!(segments("${{vars.domain}}"), std::slice::from_ref(&domain));
    assert_eq!(
        segments("${{  vars.domain \t}}"),
        std::slice::from_ref(&domain)
    );
    assert_eq!(
        segments("api.${{ vars.domain }}!"),
        [
            Segment::Text("api.".into()),
            domain,
            Segment::Text("!".into())
        ]
    );
    assert_eq!(
        segments("${HOME} $USER $${{ vars.domain }}"),
        [Segment::Text("${HOME} $USER ${{ vars.domain }}".into())]
    );
    assert_eq!(
        segments("${{ env.slug }}"),
        [Segment::Reference(Reference::System(
            SystemVariable::EnvSlug
        ))]
    );
    for (text, code) in [
        ("${{ vars.domain", codes::TEMPLATE_INVALID),
        ("${{ domain }}", codes::TEMPLATE_INVALID),
        ("${{ vars. }}", codes::TEMPLATE_INVALID),
        ("${{ secrets.token }}", codes::VARIABLE_RESERVED),
        ("${{ var.domain }}", codes::VARIABLE_NAMESPACE_UNKNOWN),
        ("${{ env.region }}", codes::VARIABLE_UNKNOWN),
    ] {
        assert_eq!(
            Template::from(text).segments().unwrap_err().code(),
            code,
            "{text}"
        );
    }
    let literal = Template::literal("${{ vars.domain }}");
    assert_eq!(literal.as_str(), "$${{ vars.domain }}");
    assert_eq!(literal.as_literal().as_deref(), Some("${{ vars.domain }}"));
}

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
    manifest.spec.variables.insert(
        "tag".into(),
        piqueld_core::manifest::Variable::String("$${{ vars.domain }}".into()),
    );
    manifest.spec.services[0]
        .environment
        .insert("TAG".into(), "${{ vars.tag }}".into());
    manifest.spec.services[0].source = piqueld_core::manifest::Source::Image {
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
        parse_template_toml(&text)
            .unwrap()
            .normalize(ApplicationId::parse("app-notes-01").unwrap())
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
    let order = parse_template_toml(&order)
        .unwrap()
        .normalize(ApplicationId::parse("app-notes-01").unwrap());
    assert_eq!(
        render(&order, "production")
            .unwrap()
            .application
            .spec()
            .services[0]
            .rollout
            .order,
        Some(piqueld_core::manifest::RolloutOrder::StopFirst)
    );
}

#[test]
fn invalid_references_fail_validation_with_their_field_paths() {
    let text = manifest(
        "[spec.variables]\nweb_replicas = 1\n[[spec.routes]]\nhostname = \"${{ app.domain }}\"\nservice = \"web\"\nport = 80\n",
    )
    .replace("${{ env.name }}", "${{ secrets.token }} ${{ git.sha }}")
    .replace("name = \"web\"", "name = \"${{ vars.tag }}\"");
    let errors = parse_template_toml(&text).unwrap_err();
    assert_eq!(
        codes_and_paths(&errors),
        [
            (codes::VARIABLE_UNKNOWN, "spec.routes[0].hostname"),
            ("route_service_missing", "spec.routes[0].service"),
            (
                codes::VARIABLE_RESERVED,
                "spec.services[0].environment.GREETING"
            ),
            (codes::VARIABLE_NOT_ALLOWED, "spec.services[0].name"),
            (codes::VARIABLE_UNDECLARED, "spec.services[0].source.image"),
        ]
    );
    // `git.*` needs a repository manifest, and variables reference only system variables.
    let text = manifest("[spec.variables]\nweb_replicas = 1\ntag = \"${{ vars.web_replicas }}\"\n")
        .replace("${{ env.name }}", "${{ git.sha }}");
    assert_eq!(
        codes_and_paths(&parse_template_toml(&text).unwrap_err()),
        [
            (
                codes::VARIABLE_UNAVAILABLE,
                "spec.services[0].environment.GREETING"
            ),
            (codes::VARIABLE_NOT_ALLOWED, "spec.variables.tag"),
        ]
    );
    // Literal parsing rejects any remaining reference.
    let errors = piqueld_core::parse_toml(&manifest(VARIABLES)).unwrap_err();
    assert!(
        errors
            .0
            .iter()
            .all(|error| error.code == codes::VARIABLE_UNRESOLVED),
        "{errors:?}"
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
    let normalized = piqueld_core::parse_toml(include_str!("fixtures/manifests/prebuilt.toml"))
        .unwrap()
        .normalize(ApplicationId::parse("app-notes-01").unwrap());
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
    let template = manifest
        .validate_template()
        .unwrap()
        .normalize(ApplicationId::parse("app-notes-01").unwrap());
    let errors = render(&template, "production").unwrap_err();
    assert_eq!(
        codes_and_paths(&errors),
        [(
            codes::VARIABLE_VALUE_EXCESSIVE,
            "spec.services[0].environment.LARGE"
        )]
    );
}

/// Oversized keys and repeated bad references cannot multiply diagnostics.
#[test]
fn diagnostics_stay_bounded_for_oversized_names_and_repeated_references() {
    let name = "n".repeat(10_000);
    let references = "${{ vars.missing }}".repeat(1_000);
    let text = manifest(&format!(
        "[spec.variables]\nweb_replicas = 1\ntag = \"stable\"\n{name} = \"{references}\"\n"
    ))
    .replace("hello ${{ env.name }}", &references);
    let errors = parse_template_toml(&text).unwrap_err();
    assert_eq!(errors.0.len(), 3, "{:?}", codes_and_paths(&errors));
    assert!(errors.0.iter().all(|error| error.path.len() < 300));
}

#[test]
fn variable_text_round_trips_blank_and_numeric_looking_strings() {
    use piqueld_core::manifest::Variable;
    for value in [
        Variable::String("".into()),
        Variable::String("  ".into()),
        Variable::String("3".into()),
        Variable::String("true".into()),
        Variable::String("\"quoted\"".into()),
        Variable::String("piquel.fr".into()),
        Variable::Integer(3),
        Variable::Boolean(false),
    ] {
        let text = value.to_text();
        assert!(!text.trim().is_empty(), "{value:?} would read as no value");
        assert_eq!(Variable::from_text(&text), value, "{text}");
    }
}
