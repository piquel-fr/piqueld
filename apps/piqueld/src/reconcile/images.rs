//! Retention of the images this installation builds: cleanup, and making
//! sure a retained image is present before a deployment uses it again.
use super::{Controller, DockerApi, OperationError};
use crate::docker::LocalImage;
use piqueld_core::{
    InstanceId, ServiceImage, ServiceName, api::ImageStatus, resource::ResolvedSource,
};
use std::time::Duration;
use tokio::sync::{Notify, RwLock, RwLockReadGuard, watch};
use tokio_util::sync::CancellationToken;

/// Cleanup's schedule and state, and the lock that keeps it from removing an
/// image a preparation is about to record.
pub(super) struct ImageRetention {
    /// Held shared by each preparation, from before it resolves or builds
    /// images until its target is saved, and exclusively by cleanup while it
    /// reads the retention roots and removes images. Cleanup only tries to
    /// take it, so it never delays a deployment for long.
    in_use: RwLock<()>,
    /// Successful deployments per environment whose images are kept.
    keep_deployments: u32,
    /// Time between periodic cleanups.
    interval: Duration,
    /// Wakes cleanup when an operation finishes.
    requested: Notify,
    status: watch::Sender<ImageStatus>,
}

impl Default for ImageRetention {
    fn default() -> Self {
        Self::new(&crate::config::ImagesConfig::default())
    }
}

impl ImageRetention {
    pub(super) fn new(config: &crate::config::ImagesConfig) -> Self {
        Self {
            in_use: RwLock::new(()),
            keep_deployments: config.keep_deployments,
            interval: Duration::from_secs(config.cleanup_interval_seconds),
            requested: Notify::new(),
            status: watch::Sender::default(),
        }
    }

    /// Asks for a cleanup soon, e.g. once a deployment finished.
    pub(super) fn request_cleanup(&self) {
        self.requested.notify_one();
    }
}

/// Proof that cleanup isn't running and won't start: hold it from before
/// choosing the images a deployment uses until its target is saved, after
/// which its images are retention roots.
pub struct ImagesInUse<'a> {
    _guard: RwLockReadGuard<'a, ()>,
}

impl<D: DockerApi> Controller<D> {
    /// Keeps cleanup from removing images until the returned guard drops.
    pub async fn images_in_use(&self) -> ImagesInUse<'_> {
        ImagesInUse {
            _guard: self.images.in_use.read().await,
        }
    }

    /// The images this installation built, as of the last cleanup.
    #[must_use]
    pub fn image_status(&self) -> watch::Receiver<ImageStatus> {
        self.images.status.subscribe()
    }

    /// Makes sure every image of `sources` is present before a deployment
    /// uses it again, pulling a missing registry image again by its digest.
    /// Restoring a deployment (#172) and promoting a release (#132) call it,
    /// holding `in_use` until they saved their target.
    ///
    /// # Errors
    /// `ImageUnavailable`, naming the service and image, for the first
    /// missing image that is a build or that can't be pulled; Docker errors
    /// when images can't be listed.
    pub async fn ensure_images<'a>(
        &self,
        _in_use: &ImagesInUse<'_>,
        sources: impl IntoIterator<Item = (&'a ServiceName, &'a ResolvedSource)>,
    ) -> Result<(), OperationError> {
        let present = LocalImage::present(&self.docker.images().await?);
        for (service, source) in present.missing(sources) {
            let image = ServiceImage {
                service: service.clone(),
                image: source.image(),
            };
            let Some(digest) = source.repository_digest() else {
                return Err(OperationError::ImageUnavailable {
                    image,
                    source: None,
                });
            };
            if let Err(error) = self.docker.resolve_image(digest.as_str()).await {
                return Err(OperationError::ImageUnavailable {
                    image,
                    source: Some(error),
                });
            }
        }
        Ok(())
    }

    /// Removes each image this installation built that no retention root
    /// keeps and no container uses, one by one, by ID. Returns `false`
    /// without doing anything while a preparation may be recording images;
    /// the next request or period cleans up instead.
    ///
    /// # Errors
    /// Returns storage errors, or Docker errors listing images. A failed
    /// removal is journaled and the image kept.
    /// # Panics
    /// Panics if the store violates its validated instance identity invariant.
    pub async fn clean_images(&self) -> Result<bool, OperationError> {
        let Ok(_exclusive) = self.images.in_use.try_write() else {
            return Ok(false);
        };
        let instance =
            InstanceId::parse(self.store.instance_id()).expect("store instance identity is valid");
        let retained = self
            .store
            .retained_images(self.images.keep_deployments)
            .await?;
        let (mut kept, mut reclaimed) = (0_u32, 0_u64);
        for image in self.docker.images().await? {
            if !image.built_by(&instance) {
                continue;
            }
            if image.used
                || retained.contains(image.id.as_str())
                || !self.remove_image(&instance, &image).await?
            {
                kept += 1;
            } else {
                reclaimed += image.size;
            }
        }
        let now = crate::store::now_ms();
        self.images.status.send_modify(|status| {
            status.images = kept;
            status.reclaimed_bytes += reclaimed;
            status.cleaned_at_ms = Some(now);
        });
        Ok(true)
    }

    /// Cleans up every interval and after operations finish, until
    /// cancelled. Failures are reported as daemon diagnostics.
    pub async fn run_image_cleanup(&self, cancellation: CancellationToken) {
        let mut interval = tokio::time::interval(self.images.interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return,
                _ = interval.tick() => {}
                () = self.images.requested.notified() => {}
            }
            if let Err(error) = self.clean_images().await {
                tracing::warn!(%error, "image cleanup failed");
                self.store
                    .report_diagnostic(&error.diagnostic(), None)
                    .await;
            }
        }
    }

    /// Journals removing one image as a daemon action, and returns whether it
    /// was removed. A failed removal is journaled, and the image kept.
    async fn remove_image(
        &self,
        instance: &InstanceId,
        image: &LocalImage,
    ) -> Result<bool, crate::store::StoreError> {
        let journal = self
            .store
            .begin_daemon_action(None, "remove_image", Some(image.id.as_str()))
            .await?;
        let removed = match self.store.action_request(&journal, 1).await {
            Ok(()) => self
                .docker
                .remove_image(instance, &image.id)
                .await
                .map_err(OperationError::from),
            Err(error) => Err(error.into()),
        };
        self.store
            .finish_action(
                &journal,
                removed.as_ref().err().map(OperationError::diagnostic),
            )
            .await?;
        Ok(removed.is_ok())
    }
}
