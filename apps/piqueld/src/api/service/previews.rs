//! Preview queries with their branch state, and pruning previews whose branch
//! is gone. Creating, deploying, and deleting one are mutations.
use super::views::status_view;
use super::{
    Actor, ApplicationError, ApplicationService, Mutation, MutationResponse, PreviewMutation,
};
use crate::git::Heads;
use crate::store::StoreError;
use piqueld_core::{
    ApplicationId, EnvironmentId,
    api::{BranchState, DeletedPreview, EnvironmentView, PreviewView},
};

impl ApplicationService {
    /// Lists an application's previews, reading every branch state with one
    /// `git ls-remote` of its manifest repository.
    ///
    /// # Errors
    /// Returns absence or storage errors. Repository failures are reported as
    /// `unknown` branch states instead.
    pub async fn previews(
        &self,
        application: &ApplicationId,
    ) -> Result<Vec<PreviewView>, ApplicationError> {
        let heads = self.heads(application).await?;
        let mut views = Vec::new();
        for preview in self.store.previews(application).await? {
            views.push(self.preview_view(preview, &heads).await?);
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
        let heads = self.heads(&preview.application_id).await?;
        self.preview_view(preview, &heads).await
    }

    /// Deletes each of `confirmed` that is a preview of `application` whose
    /// branch the repository confirms is gone, and keeps the others,
    /// including all of them when the application's repository changes
    /// meanwhile. Fails with `repository_unavailable`, deleting nothing, when
    /// the repository cannot be read: an unreadable repository never makes a
    /// branch gone.
    ///
    /// # Errors
    /// Returns absence, repository, authorization, or storage errors.
    pub async fn prune_previews(
        &self,
        actor: Actor<'_>,
        application: &ApplicationId,
        confirmed: &[EnvironmentId],
    ) -> Result<Vec<DeletedPreview>, ApplicationError> {
        let heads = self
            .heads(application)
            .await?
            .map_err(ApplicationError::RepositoryUnavailable)?;
        let mut deleted = Vec::new();
        for preview in self.store.previews(application).await? {
            let Some(identity) = preview.preview() else {
                continue;
            };
            if !confirmed.contains(&preview.id)
                || heads.state(identity.branch.as_str(), None) != BranchState::Gone
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

    /// Lists the branches of `application`'s manifest repository, or why
    /// they could not be listed.
    async fn heads(
        &self,
        application: &ApplicationId,
    ) -> Result<Result<Heads, anyhow::Error>, ApplicationError> {
        let application = self.store.application(application).await?;
        Ok(match &application.application.spec().manifest {
            Some(connection) => Heads::list(&connection.repository.url).await,
            None => Err(anyhow::anyhow!(
                "the application has no manifest repository"
            )),
        })
    }

    /// Projects a preview with its status, latest operation, the hostnames
    /// its deployed target routes, and its branch state.
    async fn preview_view(
        &self,
        preview: EnvironmentView,
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
        let branch = match (heads, preview.preview()) {
            (Ok(heads), Some(identity)) => heads.state(
                identity.branch.as_str(),
                self.store.fetched_commit(&preview.id).await?.as_deref(),
            ),
            (Err(error), _) => BranchState::Unknown {
                message: format!("{error:#}"),
            },
            (Ok(_), None) => return Err(StoreError::NotFound.into()),
        };
        Ok(PreviewView {
            status: status_view(self.store.status(&preview.id).await?),
            latest_operation: self
                .store
                .latest_operation_for_environment(&preview.id)
                .await?,
            hostnames,
            branch,
            preview,
        })
    }
}
