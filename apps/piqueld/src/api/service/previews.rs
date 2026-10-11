//! Preview queries with their branch state, pruning previews whose branch is
//! gone, and deploying snapshots of local worktrees. Creating, deploying, and
//! deleting one are mutations.
use super::views::status_view;
use super::{
    Actor, ApplicationError, ApplicationService, Mutation, MutationResponse, PreviewMutation,
};
use crate::git::Heads;
use crate::store::{PreviewOrigins, StoreError};
use axum::body::Bytes;
use futures_util::Stream;
use piqueld_core::{
    ApplicationId, EnvironmentId, GitBranch, PreviewSlot,
    api::{BranchState, DeletedPreview, EnvironmentView, LocalSnapshot, PreviewView},
    manifest::RepositoryManifest,
};
use std::sync::Arc;

impl ApplicationService {
    /// Lists an application's previews, reading every branch state with one
    /// `git ls-remote` of its manifest repository, shared with concurrent
    /// reads (see `Listings`).
    ///
    /// # Errors
    /// Returns absence or storage errors. Repository failures are reported as
    /// `unknown` branch states instead.
    pub async fn previews(
        &self,
        application: &ApplicationId,
    ) -> Result<Vec<PreviewView>, ApplicationError> {
        let (connection, heads) = self.heads(application).await?;
        let mut views = Vec::new();
        for preview in self.store.previews(application).await? {
            views.push(
                self.preview_view(preview, connection.as_ref(), &heads)
                    .await?,
            );
        }
        Ok(views)
    }

    /// Reads one preview and its branch state. Environments are `NotFound`.
    ///
    /// # Errors
    /// Returns absence or storage errors.
    pub async fn preview(&self, id: &EnvironmentId) -> Result<PreviewView, ApplicationError> {
        let preview = self.store.get(id).await?.environment;
        if preview.preview().is_none() {
            return Err(StoreError::NotFound.into());
        }
        let (connection, heads) = self.heads(&preview.application_id).await?;
        self.preview_view(preview, connection.as_ref(), &heads)
            .await
    }

    /// Receives `bundle`, a snapshot of a local worktree of `branch` that
    /// announced `length` bytes (see `Snapshots`), and deploys it to the
    /// preview of `branch` and `slot`, creating that preview first if needed,
    /// as creating one does: idempotently, within the `[previews]` limits.
    ///
    /// # Errors
    /// Returns refused uploads, `preview_requires_repository`, and the
    /// errors of creating or deploying a preview.
    pub async fn deploy_snapshot<E: std::error::Error + Send + Sync + 'static>(
        &self,
        actor: Actor<'_>,
        (application, branch, slot): (ApplicationId, GitBranch, Option<PreviewSlot>),
        (bundle, length): (impl Stream<Item = Result<Bytes, E>>, Option<u64>),
        request_id: Option<&str>,
    ) -> Result<MutationResponse, ApplicationError> {
        let repository = self
            .store
            .application(&application)
            .await?
            .application
            .spec()
            .manifest
            .as_ref()
            .ok_or(StoreError::PreviewRequiresRepository)?
            .repository
            .url
            .clone();
        let snapshots = self.store.snapshots();
        let limit = self.store.max_upload_bytes();
        let received = snapshots
            .receive(bundle, (length, limit), &repository, self.store.checkouts())
            .await?;
        let mutation = Mutation::Preview(PreviewMutation::Local {
            application,
            branch,
            slot,
            repository,
            snapshot: LocalSnapshot {
                commit: received.commit.clone(),
                base: received.base.clone(),
            },
        });
        snapshots
            .keep(
                received,
                Box::pin(self.accept(actor, mutation, None, false, request_id)),
            )
            .await
    }

    /// Deletes each of `confirmed` that is a preview of `application` whose
    /// branch the repository confirms is gone, and keeps the others,
    /// including all of them when the application's repository changes
    /// meanwhile, and those running or deploying a local snapshot, whose
    /// branch may never have been pushed. Fails with
    /// `repository_unavailable`, deleting nothing, when the repository
    /// cannot be read: an unreadable repository never makes a branch gone.
    ///
    /// # Errors
    /// Returns absence, repository, authorization, or storage errors.
    pub async fn prune_previews(
        &self,
        actor: Actor<'_>,
        application: &ApplicationId,
        confirmed: &[EnvironmentId],
    ) -> Result<Vec<DeletedPreview>, ApplicationError> {
        let listing = self.heads(application).await?.1;
        let heads = listing.as_ref().as_ref().map_err(|error| {
            ApplicationError::RepositoryUnavailable(anyhow::anyhow!("{error:#}"))
        })?;
        let mut deleted = Vec::new();
        for preview in self.store.previews(application).await? {
            let origins = self.store.preview_origins(&preview.id).await?;
            if !confirmed.contains(&preview.id)
                || Self::branch_state(&preview, &origins, Ok(heads), None)? != BranchState::Gone
            {
                continue;
            }
            let mutation = Mutation::Preview(PreviewMutation::Prune {
                id: preview.id.clone(),
                repository: heads.url().to_owned(),
            });
            let operation = match self.accept(actor, mutation, None, false, None).await {
                Ok(MutationResponse::Operation(operation)) => operation,
                Err(ApplicationError::Store(StoreError::IdentityConflict)) => continue,
                Ok(_) => return Err(StoreError::Corrupt.into()),
                Err(error) => return Err(error),
            };
            deleted.push(DeletedPreview { preview, operation });
        }
        Ok(deleted)
    }

    /// The connection of `application`'s manifest repository, with its
    /// branches or why they could not be listed.
    async fn heads(
        &self,
        application: &ApplicationId,
    ) -> Result<(Option<RepositoryManifest>, Arc<anyhow::Result<Heads>>), ApplicationError> {
        let application = self.store.application(application).await?;
        let connection = application.application.spec().manifest.clone();
        let heads = match &connection {
            Some(connection) => self.listings.list(&connection.repository.url).await,
            None => Arc::new(Err(anyhow::anyhow!(
                "the application has no manifest repository"
            ))),
        };
        Ok((connection, heads))
    }

    /// Projects a preview with its status, latest operation, the hostnames
    /// its deployed target routes, its branch state, how `[previews]`
    /// bounded its deployed target, and whether it syncs with `connection`.
    async fn preview_view(
        &self,
        preview: EnvironmentView,
        connection: Option<&RepositoryManifest>,
        heads: &Result<Heads, anyhow::Error>,
    ) -> Result<PreviewView, ApplicationError> {
        // Deployed routes include hostnames that only render when deploying,
        // such as `${{ git.sha }}`.
        let hostnames = self
            .store
            .get(&preview.id)
            .await?
            .resolved
            .iter()
            .flat_map(|target| &target.routes)
            .map(|route| route.hostname.to_string())
            .collect();
        let origins = self.store.preview_origins(&preview.id).await?;
        let fetched = self.store.fetched_commit(&preview.id).await?;
        let branch = Self::branch_state(&preview, &origins, heads.as_ref(), fetched.as_deref())?;
        Ok(PreviewView {
            origin: origins.running(),
            status: status_view(self.store.status(&preview.id).await?),
            latest_operation: self
                .store
                .latest_operation_for_environment(&preview.id)
                .await?,
            hostnames,
            branch,
            bounds: self.store.preview_bounds(&preview.id).await?,
            sync: preview.sync_state(connection),
            preview,
        })
    }

    /// Where `preview`'s branch is in `heads`: `local` while it runs or
    /// deploys a local snapshot (see [`PreviewOrigins::local`]), otherwise
    /// compared with the commit it last `fetched`.
    fn branch_state(
        preview: &EnvironmentView,
        origins: &PreviewOrigins,
        heads: Result<&Heads, &anyhow::Error>,
        fetched: Option<&str>,
    ) -> Result<BranchState, ApplicationError> {
        let identity = preview.preview().ok_or(StoreError::NotFound)?;
        let branch = identity.branch.as_str();
        Ok(match heads {
            Err(error) => BranchState::Unknown {
                message: format!("{error:#}"),
            },
            Ok(heads) if origins.local() => BranchState::Local {
                head: heads.head(branch).map(str::to_owned),
            },
            Ok(heads) => heads.state(branch, fetched),
        })
    }
}
