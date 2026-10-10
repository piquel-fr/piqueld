//! Immutable releases: what a tracking environment's successful preparation
//! deployed, reusable by other environments without rebuilding.

use crate::manifest::{
    ApplicationTemplate, GitRevision, RenderContext, RenderTarget, Rendering, SourceRepository,
    ValidatedSource, ValidationErrors,
};
use crate::resource::{ResolvedApplication, ResolvedSource, Sha256Digest};
use crate::{EnvironmentName, ServiceName};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use utoipa::ToSchema;

/// Envelope version of [`Release::content_hash`].
const CONTENT_HASH_VERSION: &str = "piqueld-release/v1";

/// One service's rendered build inputs, keyed by field path from the service:
/// every source field `ApplicationSpec::visit_values` lets reference variables
/// (`source.image` or `source.build.*`), plus a Git source's
/// `source.repository` and the `source.commit` it was built from.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct BuildInputs(BTreeMap<String, String>);

impl BuildInputs {
    /// The inputs of a rendered `source`, built at `commit` when it is a Git source.
    #[must_use]
    pub fn new(source: &ValidatedSource, commit: Option<&str>) -> Self {
        let mut inputs = source.rendered_inputs();
        if let ValidatedSource::Git { repository, .. } = source {
            let url = match repository {
                SourceRepository::Git(repository) => repository.url.clone(),
                SourceRepository::Manifest(_) => "self".into(),
            };
            inputs.insert("source.repository".into(), url);
            if let Some(commit) = commit {
                inputs.insert("source.commit".into(), commit.into());
            }
        }
        Self(inputs)
    }

    /// Each field's rendered value.
    #[must_use]
    pub const fn fields(&self) -> &BTreeMap<String, String> {
        &self.0
    }

    /// Fields whose values differ from `other`'s, including fields only one has.
    #[must_use]
    pub fn differences(&self, other: &Self) -> Vec<String> {
        self.0
            .keys()
            .chain(other.0.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|field| self.0.get(*field) != other.0.get(*field))
            .cloned()
            .collect()
    }
}

/// Each service's [`BuildInputs`]: what a release's images were built or
/// pulled from. An environment may run a release's images only where its own
/// rendering of the release's manifest has the same fingerprint.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct BuildFingerprint(BTreeMap<ServiceName, BuildInputs>);

impl BuildFingerprint {
    /// Each service's build inputs.
    #[must_use]
    pub const fn services(&self) -> &BTreeMap<ServiceName, BuildInputs> {
        &self.0
    }
}

/// The immutable output of one preparation in a tracking environment: its
/// manifest, and each service's source provenance and prepared image. It
/// belongs to the application, and preparations with the same
/// [`Self::content_hash`] share it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Release {
    /// The manifest as captured, with references unresolved.
    template: ApplicationTemplate,
    /// Commit the manifest was read from; absent for saved manifests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    /// Each service's provenance and image: a registry digest for images
    /// pulled by reference, the commit and local image ID for Git builds.
    sources: BTreeMap<ServiceName, ResolvedSource>,
}

impl Release {
    /// Records what `target`, prepared from `template` read at `commit` when
    /// repository-backed, runs.
    #[must_use]
    pub fn new(
        template: ApplicationTemplate,
        commit: Option<String>,
        target: &ResolvedApplication,
    ) -> Self {
        Self {
            template,
            commit,
            sources: target
                .services
                .iter()
                .map(|service| (service.logical_name.clone(), service.source.clone()))
                .collect(),
        }
    }

    /// The manifest as captured, with references unresolved.
    #[must_use]
    pub const fn template(&self) -> &ApplicationTemplate {
        &self.template
    }

    /// Commit the manifest was read from; absent for saved manifests.
    #[must_use]
    pub fn commit(&self) -> Option<&str> {
        self.commit.as_deref()
    }

    /// Each service's provenance and prepared image.
    #[must_use]
    pub const fn sources(&self) -> &BTreeMap<ServiceName, ResolvedSource> {
        &self.sources
    }

    /// The build inputs each service's image was prepared from.
    #[must_use]
    pub fn fingerprint(&self) -> BuildFingerprint {
        BuildFingerprint(
            self.sources
                .iter()
                .map(|(service, source)| (service.clone(), source.build_inputs()))
                .collect(),
        )
    }

    /// Versioned SHA-256 over the whole manifest, its commit, and every
    /// service's source and image. The manifest's name and location are
    /// included, since `${{ app.name }}` and `${{ git.branch }}` render them,
    /// so only preparations that render alike share a release.
    ///
    /// # Panics
    ///
    /// Panics only if the release cannot be serialized, a bug in its types.
    #[must_use]
    pub fn content_hash(&self) -> Sha256Digest {
        #[derive(Serialize)]
        struct Content<'a> {
            version: &'static str,
            template: &'a ApplicationTemplate,
            commit: Option<&'a str>,
            sources: &'a BTreeMap<ServiceName, ResolvedSource>,
        }
        let bytes = serde_json::to_vec(&Content {
            version: CONTENT_HASH_VERSION,
            template: &self.template,
            commit: self.commit(),
            sources: &self.sources,
        })
        .expect("releases serialize infallibly");
        Sha256Digest::parse(format!("sha256:{:x}", Sha256::digest(bytes)))
            .expect("a SHA-256 digest is 64 lowercase hexadecimal digits")
    }

    /// Renders the release's own manifest for `environment`, with that
    /// environment's variables, at the release's commit, and pins `"self"`
    /// sources to it. Nothing is rebuilt: compile the result with
    /// [`crate::compile_release`], which requires every build input to
    /// render as it did for the release.
    ///
    /// # Errors
    ///
    /// Returns the rendering's errors, e.g. a variable without a value there.
    pub fn render(
        &self,
        environment: EnvironmentName,
        deployment: Option<String>,
    ) -> Result<Rendering, ValidationErrors> {
        let git = self
            .template
            .spec()
            .manifest
            .as_ref()
            .zip(self.commit.as_ref())
            .map(|(manifest, commit)| GitRevision {
                branch: manifest.repository.branch.clone(),
                sha: commit.clone(),
            });
        // Releases are instantiated for environments, never for previews.
        let mut rendering = self.template.render(&RenderContext {
            target: RenderTarget::Environment(environment),
            git,
            deployment,
        })?;
        if let Some(commit) = &self.commit {
            rendering.application = rendering.application.pin_manifest_sources(commit);
        }
        Ok(rendering)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ApplicationId, EnvironmentId, InstanceId, ResolutionSet, codes, compile_application,
        compile_release, manifest::parse_template_toml,
    };

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A repository-backed shop whose `web` service builds `self` with `arg`
    /// as its `VITE_ORIGIN` build argument and runs with per-environment
    /// replicas and origin.
    fn template(arg: &str) -> ApplicationTemplate {
        parse_template_toml(&format!(
            r#"api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "shop"
[spec.manifest]
path = "piqueld.toml"
[spec.manifest.repository]
url = "https://example.com/shop.git"
branch = "main"
[spec.variables]
replicas = 1
[spec.environments.staging.variables]
domain = "staging.example.com"
[spec.environments.production.variables]
domain = "example.com"
replicas = 3
[[spec.services]]
name = "web"
replicas = "${{{{ vars.replicas }}}}"
[spec.services.environment]
ORIGIN = "https://${{{{ vars.domain }}}}"
[spec.services.source]
type = "git"
repository = "self"
[spec.services.source.build]
type = "docker"
dockerfile = "Dockerfile"
[spec.services.source.build.args]
VITE_ORIGIN = "{arg}"
REVISION = "${{{{ git.sha }}}}"
"#
        ))
        .unwrap()
        .normalize(ApplicationId::parse("app-shop-0001").unwrap())
    }

    fn environment(name: &str) -> EnvironmentId {
        EnvironmentId::parse(format!("env-{name}-0001")).unwrap()
    }

    fn instance() -> InstanceId {
        InstanceId::parse("instance").unwrap()
    }

    /// The release staging records after building `web` from `template`.
    fn staging_release(template: ApplicationTemplate) -> Release {
        let unprepared = Release {
            template,
            commit: Some(COMMIT.into()),
            sources: BTreeMap::new(),
        };
        let rendering = unprepared
            .render(EnvironmentName::parse("staging").unwrap(), None)
            .unwrap();
        let web = &rendering.application.spec().services[0];
        let resolutions = ResolutionSet {
            sources: BTreeMap::from([(
                web.name.clone(),
                ResolvedSource::Git {
                    requested: web.source.clone(),
                    commit: COMMIT.into(),
                    image_id: Sha256Digest::parse(format!("sha256:{}", "a".repeat(64))).unwrap(),
                },
            )]),
            secret_names: BTreeMap::new(),
        };
        let target = compile_application(
            &rendering.application,
            &environment("staging"),
            instance(),
            &resolutions,
        )
        .unwrap();
        Release::new(unprepared.template, unprepared.commit, &target)
    }

    #[test]
    fn records_provenance_images_and_fingerprint_and_hashes_content() {
        let release = staging_release(template("https://example.com"));
        let web = ServiceName::parse("web").unwrap();
        let ResolvedSource::Git {
            commit, image_id, ..
        } = &release.sources()[&web]
        else {
            panic!("Git build")
        };
        assert_eq!(
            (commit.as_str(), image_id.as_str()),
            (COMMIT, &*format!("sha256:{}", "a".repeat(64)))
        );
        assert_eq!(
            *release.fingerprint().services()[&web].fields(),
            BTreeMap::from(
                [
                    ("source.build.args.REVISION", COMMIT),
                    ("source.build.args.VITE_ORIGIN", "https://example.com"),
                    ("source.build.context", "."),
                    ("source.build.dockerfile", "Dockerfile"),
                    ("source.commit", COMMIT),
                    ("source.repository", "https://example.com/shop.git"),
                ]
                .map(|(field, value)| (field.to_owned(), value.to_owned()))
            )
        );
        // The name renders through `${{ app.name }}`, so it is content, like
        // a build argument.
        let renamed = Release {
            template: release
                .template
                .clone()
                .with_name(crate::ApplicationName::parse("store").unwrap()),
            ..release.clone()
        };
        assert_ne!(renamed.content_hash(), release.content_hash());
        // So is the branch the manifest was read from, `${{ git.branch }}`.
        let mut manifest = release.template.spec().manifest.clone().unwrap();
        manifest.repository.branch = "release".into();
        let branched = Release {
            template: release.template.clone().with_manifest(Some(manifest)),
            ..release.clone()
        };
        assert_ne!(branched.content_hash(), release.content_hash());
        assert_ne!(
            staging_release(template("https://shop.example.com")).content_hash(),
            release.content_hash()
        );
    }

    #[test]
    fn instantiates_for_an_environment_whose_variables_change_only_runtime_fields() {
        let release = staging_release(template("https://example.com"));
        let rendering = release
            .render(
                EnvironmentName::parse("production").unwrap(),
                Some("deployment".into()),
            )
            .unwrap();
        let target = compile_release(
            &rendering.application,
            &environment("production"),
            instance(),
            &release,
            BTreeMap::new(),
        )
        .unwrap();
        let web = &target.services[0];
        assert_eq!(web.replicas, 3);
        assert_eq!(web.environment["ORIGIN"], "https://example.com");
        assert_eq!(
            web.source,
            release.sources().values().next().unwrap().clone()
        );
    }

    #[test]
    fn refuses_an_environment_that_renders_a_build_argument_differently() {
        let release = staging_release(template("https://${{ vars.domain }}"));
        let rendering = release
            .render(EnvironmentName::parse("production").unwrap(), None)
            .unwrap();
        let errors = compile_release(
            &rendering.application,
            &environment("production"),
            instance(),
            &release,
            BTreeMap::new(),
        )
        .unwrap_err();
        assert_eq!(
            errors
                .iter()
                .map(|error| (error.code.as_str(), error.resource.as_str()))
                .collect::<Vec<_>>(),
            [(
                codes::RELEASE_INCOMPATIBLE,
                "web.source.build.args.VITE_ORIGIN"
            )]
        );
    }
}
