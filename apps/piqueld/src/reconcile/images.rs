//! Retention of the images this installation builds: cleanup, and making
//! sure a retained image is present before a deployment uses it again.
use super::{Controller, DockerApi, OperationError};
use crate::docker::LocalImage;
use piqueld_core::{
    InstanceId, ServiceImage, ServiceName, api::ImageStatus, resource::ResolvedSource,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::{Notify, OwnedRwLockReadGuard, RwLock, watch};
use tokio_util::sync::CancellationToken;

/// Cleanup's schedule and state, and the lock that keeps it from removing an
/// image a preparation is about to record.
pub(super) struct ImageRetention {
    /// Held shared by each preparation and promotion, from before it
    /// resolves, builds, or checks images until its target is saved, and
    /// exclusively by cleanup while it reads the retention roots and removes
    /// images. Cleanup only tries to take it, so it never delays a
    /// deployment for long. Shared with the runtime boundary, which accepts
    /// promotions.
    in_use: Arc<RwLock<()>>,
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
            in_use: Arc::default(),
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
pub struct ImagesInUse {
    _guard: OwnedRwLockReadGuard<()>,
}

impl ImagesInUse {
    /// Keeps cleanup, which takes `lock` exclusively, from removing images
    /// until the returned guard drops.
    pub async fn hold(lock: Arc<RwLock<()>>) -> Self {
        Self {
            _guard: lock.read_owned().await,
        }
    }

    /// Makes sure every image of `sources` is present in `docker` before a
    /// deployment uses it again, pulling a missing registry image again by
    /// its digest. Promoting a release calls it through the runtime boundary
    /// (see `RuntimeBoundary::reuse_images`), and restoring a deployment
    /// (#172) will too, keeping this guard until they saved their target.
    ///
    /// # Errors
    /// `ImageUnavailable`, naming the service and image, for the first
    /// missing image that is a build or that can't be pulled; Docker errors
    /// when images can't be listed.
    pub async fn ensure<'a>(
        &self,
        docker: &impl DockerApi,
        sources: impl IntoIterator<Item = (&'a ServiceName, &'a ResolvedSource)>,
    ) -> Result<(), OperationError> {
        let present = LocalImage::present(&docker.images().await?);
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
            if let Err(error) = docker.resolve_image(digest.as_str()).await {
                return Err(OperationError::ImageUnavailable {
                    image,
                    source: Some(error),
                });
            }
        }
        Ok(())
    }
}

impl<D: DockerApi> Controller<D> {
    /// Keeps cleanup from removing images until the returned guard drops.
    pub async fn images_in_use(&self) -> ImagesInUse {
        ImagesInUse::hold(Arc::clone(&self.images.in_use)).await
    }

    /// The lock preparations and promotions hold against cleanup, for the
    /// runtime boundary.
    pub(super) fn images_lock(&self) -> Arc<RwLock<()>> {
        Arc::clone(&self.images.in_use)
    }

    /// The images this installation built, as of the last cleanup.
    #[must_use]
    pub fn image_status(&self) -> watch::Receiver<ImageStatus> {
        self.images.status.subscribe()
    }

    /// Removes each image this installation built that no retention root
    /// keeps, by ID or repository digest, and no container uses, one by one,
    /// by ID. Returns `false`
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
                || image.retained_by(&retained)
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
