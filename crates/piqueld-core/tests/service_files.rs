//! Read-only service files: validation, verbatim contents, and repository
//! files captured with a deployment's manifest.

use piqueld_core::api::ManifestChange;
use piqueld_core::manifest::{
    ApplicationTemplate, FileCaptureError, FileSource, RenderContext, RenderTarget,
    RepositoryFiles, ServiceFile, ValidationErrors, parse_template_toml,
};
use piqueld_core::resource::{ResolutionSet, ResolvedApplication, ResolvedSource};
use piqueld_core::{
    ApplicationId, DockerConfigName, EnvironmentId, EnvironmentName, InstanceId,
    NormalizedApplication, Release, codes, compile_application,
};

/// A manifest of `notes`, with `spec` appended as written.
fn template(spec: &str) -> Result<ApplicationTemplate, ValidationErrors> {
    parse_template_toml(&format!(
        "api_version = \"piqueld.dev/v1alpha1\"\nkind = \"Application\"\n[metadata]\nname = \"notes\"\n{spec}"
    ))
    .map(|template| template.normalize(ApplicationId::parse("app-notes-01").unwrap()))
}

/// An image service named `name`, with `files` appended as written.
fn service(name: &str, files: &str) -> String {
    format!(
        "[[spec.services]]\nname = \"{name}\"\n[spec.services.source]\ntype = \"image\"\nimage = \"ghcr.io/example/notes:1.4.0\"\n{files}"
    )
}

const REPOSITORY: &str = "[spec.manifest]\npath = \"piqueld.toml\"\n[spec.manifest.repository]\nurl = \"https://example.com/notes.git\"\nbranch = \"main\"\n";

fn environment() -> EnvironmentId {
    EnvironmentId::parse("env-production-01").unwrap()
}

fn render(template: &ApplicationTemplate) -> NormalizedApplication {
    template
        .render(&RenderContext::deployment(
            RenderTarget::Environment(EnvironmentName::parse("production").unwrap()),
            "operation-1".into(),
        ))
        .unwrap()
        .application
}

/// Compiles `application`, whose services all run the same image.
fn compile(application: &NormalizedApplication) -> Result<ResolvedApplication, Vec<String>> {
    let source = ResolvedSource::parse_image(
        "ghcr.io/example/notes:1.4.0",
        format!("ghcr.io/example/notes@sha256:{}", "a".repeat(64)),
    )
    .unwrap();
    let resolutions = ResolutionSet {
        sources: application
            .spec()
            .services
            .iter()
            .map(|service| (service.name.clone(), source.clone()))
            .collect(),
        secret_names: std::collections::BTreeMap::default(),
    };
    compile_application(
        application,
        &environment(),
        InstanceId::parse("instance").unwrap(),
        &resolutions,
    )
    .map_err(|errors| errors.into_iter().map(|error| error.code).collect())
}

fn captured(content: &str) -> RepositoryFiles {
    [("otel.yaml".to_owned(), content.to_owned())]
        .into_iter()
        .collect()
}

#[test]
fn contents_are_mounted_verbatim_and_survive_storage() {
    let content = "${{ vars.missing }} ${HOME} {{ .Values }}";
    let template = template(&service(
        "web",
        &format!("[[spec.services.files]]\ntarget = \"/etc/app.conf\"\ncontent = '{content}'\n"),
    ))
    .unwrap();
    let application = render(&template);
    assert_eq!(
        application.spec().services[0].files[0].source,
        FileSource::Content(content.into())
    );
    let stored: NormalizedApplication =
        serde_json::from_str(&application.canonical_json().unwrap()).unwrap();
    assert_eq!(stored, application);
    let web = &compile(&application).unwrap().services[0];
    assert_eq!(web.files[0].content, content);
    assert_eq!(web.files[0].mode.bits(), 0o444);
}

#[test]
fn validation_keeps_files_apart_from_mounts_secrets_and_piqueld() {
    let files = [
        ("/etc/a", "path = \"a.yaml\""),
        ("/run/secrets/token", "content = \"x\""),
        ("/run/piqueld/probe", "content = \"x\""),
        ("/data", "content = \"x\""),
        ("/etc/b", "content = \"x\""),
        ("/etc/b", "content = \"x\""),
        ("etc/c", "content = \"x\""),
    ]
    .map(|(target, source)| format!("[[spec.services.files]]\ntarget = \"{target}\"\n{source}\n"))
    .concat();
    let oversized = format!(
        "[[spec.services.files]]\ntarget = \"/etc/large\"\ncontent = \"{}\"\n",
        "x".repeat(ServiceFile::MAX_BYTES + 1)
    );
    let errors = template(&format!(
        "[[spec.volumes]]\nname = \"data\"\n{}[[spec.services.mounts]]\nvolume = \"data\"\ntarget = \"/data\"\n{files}{oversized}",
        service("web", "")
    ))
    .unwrap_err();
    assert_eq!(
        errors
            .0
            .iter()
            .map(|error| (error.code.as_str(), error.path.as_str()))
            .collect::<Vec<_>>(),
        [
            (
                codes::FILE_REQUIRES_REPOSITORY,
                "spec.services[0].files[0].path"
            ),
            (
                codes::FILE_TARGET_INVALID,
                "spec.services[0].files[1].target"
            ),
            (
                codes::FILE_TARGET_INVALID,
                "spec.services[0].files[2].target"
            ),
            (
                codes::FILE_TARGET_INVALID,
                "spec.services[0].files[3].target"
            ),
            (
                codes::FILE_TARGET_INVALID,
                "spec.services[0].files[5].target"
            ),
            (
                codes::MOUNT_TARGET_UNSAFE,
                "spec.services[0].files[6].target"
            ),
            (codes::FILE_TOO_LARGE, "spec.services[0].files[7].content"),
        ]
    );
}

#[test]
fn repository_files_are_captured_hashed_and_mounted_by_content() {
    let file = "[[spec.services.files]]\ntarget = \"/etc/otel.yaml\"\npath = \"otel.yaml\"\nmode = \"0440\"\n";
    let template = template(&format!(
        "{REPOSITORY}{}{}",
        service("api", ""),
        service("web", file)
    ))
    .unwrap();
    assert_eq!(
        compile(&render(&template)).unwrap_err(),
        [codes::FILE_UNRESOLVED]
    );

    let deployed = |content: &str| {
        let template = template.clone().with_files(captured(content)).unwrap();
        let application = render(&template);
        (template, compile(&application).unwrap(), application)
    };
    let (first, target, application) = deployed("a: 1");
    let (_, changed, changed_application) = deployed("a: 2");
    let [api, web] = &target.services[..] else {
        panic!("two services")
    };
    assert_eq!(api.files.len(), 0);
    assert_eq!(
        (web.files[0].content.as_str(), web.files[0].mode.bits()),
        ("a: 1", 0o440)
    );
    assert_eq!(
        web.files[0].config_name,
        DockerConfigName::for_file(&environment(), "a: 1")
    );
    // New content is a new config for the service mounting it only, and a
    // new specification.
    assert_ne!(
        web.files[0].config_name,
        changed.services[1].files[0].config_name
    );
    assert_eq!(api.files, changed.services[0].files);
    assert_ne!(application.spec_hash(), changed_application.spec_hash());

    // A release keeps the bytes it captured, so promotions mount them.
    let release = Release::new(first.clone(), Some("0".repeat(40)), &target);
    let promoted = release
        .render(EnvironmentName::parse("staging").unwrap(), None)
        .unwrap()
        .application;
    assert_eq!(promoted.files().get("otel.yaml"), Some("a: 1"));
    let stored: Release = serde_json::from_str(&serde_json::to_string(&release).unwrap()).unwrap();
    assert_eq!(stored.content_hash(), release.content_hash());
    let (second, changed_target, _) = deployed("a: 2");
    assert_ne!(
        Release::new(second, Some("0".repeat(40)), &changed_target).content_hash(),
        release.content_hash()
    );

    // Saved manifests leave the captured bytes to deployments.
    assert!(first.clone().without_files().files().is_empty());
    assert_eq!(
        template
            .clone()
            .with_files(captured(&"x".repeat(ServiceFile::MAX_BYTES + 1))),
        Err(FileCaptureError::TooLarge("otel.yaml".into()))
    );

    let plan = ManifestChange::between(None, template.spec());
    let change = plan
        .iter()
        .find(|change| change.field == "services.web.files./etc/otel.yaml")
        .unwrap();
    assert_eq!(change.after.as_deref(), Some("path: otel.yaml, mode: 0440"));
    assert!(
        plan.iter()
            .all(|change| change.field != "services.web.files")
    );
}
