//! Local image inventory and removal of this installation's builds.
use super::{BTreeMap, BollardDocker, BollardResult, DockerError, InstanceId, LocalImage};
use bollard::models::ImageSummary;
use bollard::query_parameters::{
    ListContainersOptionsBuilder, ListImagesOptionsBuilder, RemoveImageOptionsBuilder,
};
use std::collections::BTreeSet;

impl BollardDocker {
    /// Lists every image with its repository digests, including untagged
    /// parents the classic image store otherwise hides, and marks those a
    /// container uses.
    pub(super) async fn list_local_images(&self) -> Result<Vec<LocalImage>, DockerError> {
        let used = self
            .client()?
            .list_containers(Some(
                ListContainersOptionsBuilder::default().all(true).build(),
            ))
            .await
            .or_request("list containers")?
            .into_iter()
            .filter_map(|container| container.image_id)
            .collect::<BTreeSet<_>>();
        let images = self
            .client()?
            .list_images(Some(
                ListImagesOptionsBuilder::default()
                    .all(true)
                    .digests(true)
                    .build(),
            ))
            .await
            .or_request("list images")?;
        Ok(images
            .into_iter()
            .filter_map(|image| {
                let used = used.contains(&image.id);
                LocalImage::listed(image, used)
            })
            .collect())
    }

    /// Rechecks that `instance` built image `id`, then removes it by ID. A
    /// missing image, before or during removal, counts as removed.
    pub(super) async fn remove_built_image(
        &self,
        instance: &InstanceId,
        id: &piqueld_core::Sha256Digest,
    ) -> Result<(), DockerError> {
        let Some(image) = self
            .client()?
            .inspect_image(id.as_str())
            .await
            .found()
            .or_request("inspect image")?
        else {
            return Ok(());
        };
        let labels = image
            .config
            .and_then(|config| config.labels)
            .unwrap_or_default()
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        if !LocalImage::labelled_for(&labels, instance) {
            return Err(DockerError::OwnershipConflict);
        }
        self.client()?
            .remove_image(
                id.as_str(),
                Some(
                    RemoveImageOptionsBuilder::default()
                        .force(false)
                        .noprune(true)
                        .build(),
                ),
                None,
            )
            .await
            .found()
            .or_request("remove image")?;
        Ok(())
    }
}

impl LocalImage {
    /// The image Docker listed as `image`, or `None` when Docker reports its
    /// ID in another form (a non-SHA-256 store), so it is never removed.
    /// Repository digests in another form are skipped.
    ///
    /// The containerd image store lists an image once per manifest, with
    /// that manifest's digest as its ID and descriptor, every name pointing
    /// at it in `RepoTags`, but only one name's digest in `RepoDigests`.
    /// Each name's repository at the ID is a digest of the image, as image
    /// inspection reports, so they are added. The classic store lists every
    /// repository digest, and its IDs are config digests no name points at.
    fn listed(image: ImageSummary, used: bool) -> Option<Self> {
        let id = piqueld_core::Sha256Digest::parse(image.id).ok()?;
        let manifest = image
            .descriptor
            .and_then(|descriptor| descriptor.digest)
            .is_some_and(|digest| digest == id.as_str());
        let mut repo_digests = image
            .repo_digests
            .into_iter()
            .filter_map(|digest| piqueld_core::RepositoryDigest::parse(digest).ok())
            .collect::<BTreeSet<_>>();
        if manifest {
            repo_digests.extend(
                image
                    .repo_tags
                    .into_iter()
                    .filter_map(|name| piqueld_core::ImageReference::parse(name).ok())
                    .map(|name| name.pinned(&id)),
            );
        }
        Some(Self {
            used,
            id,
            repo_digests: repo_digests.into_iter().collect(),
            labels: image.labels.into_iter().collect(),
            size: u64::try_from(image.size).unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::OciDescriptor;

    const DIGEST: &str = "sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc";
    const CONFIG: &str = "sha256:bf8527eb54c3680e728d5b4b383a8ba730d72dae7236fbc8dff97ed6b224a731";

    fn digests(image: ImageSummary) -> Vec<String> {
        LocalImage::listed(image, false)
            .unwrap()
            .repo_digests
            .iter()
            .map(|digest| digest.as_str().to_owned())
            .collect()
    }

    /// `alpine:3.20`, also pulled by digest from Docker Hub's registry
    /// endpoint, as each store lists it.
    #[test]
    fn listed_images_carry_every_repository_digest_in_either_store() {
        let both = vec![
            format!("alpine@{DIGEST}"),
            format!("registry-1.docker.io/library/alpine@{DIGEST}"),
        ];
        let classic = ImageSummary {
            id: CONFIG.to_owned(),
            repo_tags: vec!["alpine:3.20".to_owned()],
            repo_digests: both.clone(),
            ..ImageSummary::default()
        };
        assert_eq!(digests(classic), both);
        let containerd = ImageSummary {
            id: DIGEST.to_owned(),
            repo_tags: vec![
                "alpine:3.20".to_owned(),
                format!("registry-1.docker.io/library/alpine@{DIGEST}"),
            ],
            repo_digests: vec![format!("registry-1.docker.io/library/alpine@{DIGEST}")],
            descriptor: Some(OciDescriptor {
                digest: Some(DIGEST.to_owned()),
                ..OciDescriptor::default()
            }),
            ..ImageSummary::default()
        };
        assert_eq!(digests(containerd), both);
    }
}
