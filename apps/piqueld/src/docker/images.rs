//! Local image inventory and removal of this installation's builds.
use super::{BTreeMap, BollardDocker, BollardResult, DockerError, InstanceId, LocalImage};
use bollard::query_parameters::{
    ListContainersOptionsBuilder, ListImagesOptionsBuilder, RemoveImageOptionsBuilder,
};
use std::collections::BTreeSet;

impl BollardDocker {
    /// Lists every image with its repository digests, including untagged
    /// parents the classic image store otherwise hides, and marks those a
    /// container uses. IDs and digests Docker reports in another form (a
    /// non-SHA-256 store) are skipped, so such images are never removed.
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
                Some(LocalImage {
                    used: used.contains(&image.id),
                    id: piqueld_core::Sha256Digest::parse(image.id).ok()?,
                    repo_digests: image
                        .repo_digests
                        .into_iter()
                        .filter_map(|digest| piqueld_core::RepositoryDigest::parse(digest).ok())
                        .collect(),
                    labels: image.labels.into_iter().collect(),
                    size: u64::try_from(image.size).unwrap_or_default(),
                })
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
