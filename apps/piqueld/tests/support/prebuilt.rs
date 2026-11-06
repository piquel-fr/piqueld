//! The prebuilt `notes` fixture: a `web` service running a public image,
//! with a `data` volume, resolved to one pinned digest.

use piqueld_core::manifest::ApplicationManifest;
use piqueld_core::resource::{
    ResolutionSet, ResolvedApplication, ResolvedSource, compile_application,
};
use piqueld_core::{ApplicationId, EnvironmentId, InstanceId, NormalizedApplication};

/// The image every fixture service runs.
pub const IMAGE: &str = "ghcr.io/example/notes:1.4.0";

/// The fixture manifest, as written.
pub fn manifest() -> ApplicationManifest {
    toml::from_str(include_str!(
        "../../../../crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml"
    ))
    .expect("fixture is valid")
}

/// The fixture saved as application `id`.
pub fn application(id: &str) -> NormalizedApplication {
    manifest()
        .validate()
        .expect("fixture is valid")
        .normalize(ApplicationId::parse(id).expect("fixture ID is valid"))
}

/// Resolves every service of `application` to [`IMAGE`], pinned to `a`s.
pub fn resolutions(application: &NormalizedApplication) -> ResolutionSet {
    let pinned = format!("ghcr.io/example/notes@sha256:{}", "a".repeat(64));
    let source = ResolvedSource::parse_image(IMAGE, pinned).expect("fixture digest is valid");
    ResolutionSet {
        secret_names: std::collections::BTreeMap::default(),
        sources: application
            .spec()
            .services
            .iter()
            .map(|service| (service.name.clone(), source.clone()))
            .collect(),
    }
}

/// The runtime target of `application`'s environment `environment`, owned by
/// installation `instance`, with [`resolutions`].
pub fn resolve(
    application: &NormalizedApplication,
    environment: &EnvironmentId,
    instance: &InstanceId,
) -> ResolvedApplication {
    compile_application(
        application,
        environment,
        instance.clone(),
        &resolutions(application),
    )
    .expect("fixture resolves")
}

/// [`resolve`] for whichever environment of `application` it is given, as
/// `Store::save_application` imports a prepared target.
pub fn resolver<'a>(
    application: &'a NormalizedApplication,
    instance: &'a InstanceId,
) -> impl Fn(&EnvironmentId) -> ResolvedApplication + Sync + 'a {
    move |environment| resolve(application, environment, instance)
}
