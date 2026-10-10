//! Image retention: which deployments keep their images when cleanup runs,
//! and whether a release's images are still present.

use crate::{
    ImmutableImage, RepositoryDigest, ServiceName, resource::ResolvedApplication,
    resource::ResolvedSource,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use utoipa::ToSchema;

/// Why cleanup keeps a deployment's images. Cleanup removes only images this
/// installation built that no root keeps.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RetentionRoot {
    /// What an environment or preview currently runs.
    Current,
    /// The latest deployment of an environment or preview: in progress, or
    /// finished and retried with its prepared target until a newer one
    /// replaces it.
    Latest,
    /// One of an environment's last `keep_deployments` successful
    /// deployments, which restoring a deployment (#172) deploys again.
    /// Previews keep none.
    Recent,
    /// The current release of an environment that a promoted environment
    /// promotes from (#132), which populates it. There are no promoted
    /// environments yet, so nothing has this root.
    PromotionSource,
}

impl RetentionRoot {
    /// Every root, in the order cleanup reads them.
    pub const ALL: [Self; 4] = [
        Self::Current,
        Self::Latest,
        Self::Recent,
        Self::PromotionSource,
    ];
}

/// A service and the image it runs.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
pub struct ServiceImage {
    /// Logical service name.
    pub service: ServiceName,
    /// Its immutable image.
    pub image: ImmutableImage,
}

/// Whether a release's images are still present, so it can be deployed again.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReleaseAvailability {
    /// Every image is present.
    Present,
    /// Only registry images are missing: deploying the release pulls them
    /// again by digest.
    Pullable {
        /// The missing images.
        missing: Vec<ServiceImage>,
    },
    /// Built images are missing, so the release can't be deployed again.
    Unavailable {
        /// The missing images, built or pulled.
        missing: Vec<ServiceImage>,
    },
}

/// The images present in the local Docker Engine, by local image ID and by
/// repository digest.
#[derive(Clone, Debug, Default)]
pub struct LocalImages(BTreeSet<ImmutableImage>);

impl LocalImages {
    /// Images present under each of these IDs and repository digests.
    pub fn new(images: impl IntoIterator<Item = ImmutableImage>) -> Self {
        Self(images.into_iter().collect())
    }

    /// Whether `image` is present.
    #[must_use]
    pub fn contains(&self, image: &ImmutableImage) -> bool {
        self.0.contains(image)
    }

    /// The services of `sources` whose images aren't present, in order.
    pub fn missing<'a>(
        &self,
        sources: impl IntoIterator<Item = (&'a ServiceName, &'a ResolvedSource)>,
    ) -> Vec<(&'a ServiceName, &'a ResolvedSource)> {
        sources
            .into_iter()
            .filter(|(_, source)| !self.contains(&source.image()))
            .collect()
    }

    /// Whether each image of `sources` is present, and if not, whether the
    /// missing ones can be pulled again.
    pub fn availability<'a>(
        &self,
        sources: impl IntoIterator<Item = (&'a ServiceName, &'a ResolvedSource)>,
    ) -> ReleaseAvailability {
        let missing = self.missing(sources);
        let pullable = missing
            .iter()
            .all(|(_, source)| source.repository_digest().is_some());
        let missing = missing
            .into_iter()
            .map(|(service, source)| ServiceImage {
                service: service.clone(),
                image: source.image(),
            })
            .collect::<Vec<_>>();
        if missing.is_empty() {
            ReleaseAvailability::Present
        } else if pullable {
            ReleaseAvailability::Pullable { missing }
        } else {
            ReleaseAvailability::Unavailable { missing }
        }
    }
}

impl ResolvedSource {
    /// The registry digest Docker can pull again if the image is missing.
    /// A built image can't be recovered: rebuilding may produce another one.
    #[must_use]
    pub const fn repository_digest(&self) -> Option<&RepositoryDigest> {
        match self {
            Self::Image {
                digest_reference, ..
            } => Some(digest_reference),
            Self::Git { .. } => None,
        }
    }
}

impl ResolvedApplication {
    /// Each service and its resolved source. Jobs run their service's image.
    pub fn sources(&self) -> impl Iterator<Item = (&ServiceName, &ResolvedSource)> {
        self.services
            .iter()
            .map(|service| (&service.logical_name, &service.source))
    }

    /// Every image its services and jobs run.
    pub fn images(&self) -> impl Iterator<Item = ImmutableImage> {
        self.services
            .iter()
            .chain(self.jobs.iter().map(|job| &job.container))
            .map(|service| service.image.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Sha256Digest, manifest::ValidatedSource};
    use std::collections::BTreeMap;

    fn pulled(digest: char) -> ResolvedSource {
        ResolvedSource::parse_image(
            "example.com/web:1",
            format!("example.com/web@sha256:{}", digest.to_string().repeat(64)),
        )
        .unwrap()
    }

    fn built(id: char) -> ResolvedSource {
        ResolvedSource::Git {
            // Availability only reads the variant and image.
            requested: ValidatedSource::Image {
                image: "unused".into(),
            },
            commit: "a".repeat(40),
            image_id: Sha256Digest::parse(format!("sha256:{}", id.to_string().repeat(64))).unwrap(),
        }
    }

    #[test]
    fn missing_registry_images_are_pullable_and_missing_builds_unavailable() {
        let (web, api) = (
            ServiceName::parse("web").unwrap(),
            ServiceName::parse("api").unwrap(),
        );
        let sources = BTreeMap::from([(web.clone(), pulled('a')), (api.clone(), built('b'))]);
        let all = LocalImages::new(sources.values().map(ResolvedSource::image));
        assert_eq!(all.availability(&sources), ReleaseAvailability::Present);

        let built_only = LocalImages::new([built('b').image()]);
        assert_eq!(
            built_only.availability(&sources),
            ReleaseAvailability::Pullable {
                missing: vec![ServiceImage {
                    service: web.clone(),
                    image: pulled('a').image()
                }]
            }
        );

        let ReleaseAvailability::Unavailable { missing } =
            LocalImages::default().availability(&sources)
        else {
            panic!("a missing build makes the release unavailable");
        };
        assert_eq!(
            missing.iter().map(|m| &m.service).collect::<Vec<_>>(),
            [&api, &web]
        );
    }
}
