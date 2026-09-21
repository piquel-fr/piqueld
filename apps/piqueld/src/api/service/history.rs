//! Durable history, manifest export, and runtime logs.

use super::{ApplicationError, ApplicationService};
use crate::store::StoreError;
use piqueld_core::{
    ApplicationId, Event, Operation,
    api::{ApplicationLogs, BuildLogPage, BuildRecord, DeploymentView, LogStream, Page},
};

/// Saved configuration rendered for download or export.
pub struct ManifestExport {
    /// Suggested filename, derived from the validated application name.
    pub filename: String,
    /// Saved manifest encoded as TOML.
    pub contents: String,
}

impl ApplicationService {
    /// Exports saved configuration without consulting Docker.
    /// # Errors
    /// Returns absence, storage, or TOML serialization errors.
    pub async fn manifest(&self, id: &ApplicationId) -> Result<ManifestExport, ApplicationError> {
        let application = self.store.get(id).await?.application;
        Ok(ManifestExport {
            filename: format!("{}.toml", application.metadata().name),
            contents: toml::to_string_pretty(&application.to_manifest())
                .map_err(ApplicationError::ManifestSerialization)?,
        })
    }

    /// Reads a durable operation and its progress.
    /// # Errors
    /// Returns absence or storage errors.
    pub async fn operation(&self, id: &str) -> Result<Operation, ApplicationError> {
        Ok(self.store.operation(id).await?)
    }

    /// Lists deployment snapshots, newest first, three per page.
    /// # Errors
    /// Returns pagination, absence, or storage errors.
    pub async fn deployments(
        &self,
        id: &ApplicationId,
        cursor: Option<&str>,
    ) -> Result<Page<DeploymentView>, ApplicationError> {
        Ok(self.store.deployments(id, cursor, 3).await?)
    }

    /// Lists retained attempts for a deployment owned by this application.
    /// # Errors
    /// Returns pagination, absence, ownership, or storage errors.
    pub async fn deployment_attempts(
        &self,
        id: &ApplicationId,
        deployment: &str,
        cursor: Option<&str>,
    ) -> Result<Page<Operation>, ApplicationError> {
        let operation = self.store.operation(deployment).await?;
        if operation.application_id != *id {
            return Err(StoreError::NotFound.into());
        }
        self.store.deployment_manifest(deployment).await?;
        Ok(self
            .store
            .deployment_attempts(deployment, cursor, 100)
            .await?)
    }

    /// Lists informational history, oldest first.
    /// # Errors
    /// Returns pagination or storage errors.
    pub async fn events(
        &self,
        id: Option<&ApplicationId>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<Event>, ApplicationError> {
        Ok(self.store.events(id, cursor, limit).await?)
    }

    /// Lists build history, newest first.
    /// # Errors
    /// Returns pagination or storage errors.
    pub async fn builds(
        &self,
        id: Option<&ApplicationId>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<BuildRecord>, ApplicationError> {
        Ok(self.store.builds(id, cursor, limit).await?)
    }

    /// Reads a bounded page of persisted build output.
    /// # Errors
    /// Returns invalid query, absence, or storage errors.
    pub async fn build_logs(
        &self,
        id: i64,
        before: Option<i64>,
        stream: Option<LogStream>,
    ) -> Result<BuildLogPage, ApplicationError> {
        Ok(self.store.build_logs(id, before, stream).await?)
    }

    /// Reads bounded runtime logs for an existing application.
    /// # Errors
    /// Returns invalid query, absence, storage, or runtime errors.
    pub async fn logs(
        &self,
        id: &ApplicationId,
        service: Option<&str>,
        tail: u16,
        since_seconds: u32,
        stream: Option<LogStream>,
    ) -> Result<ApplicationLogs, ApplicationError> {
        if !(1..=1000).contains(&tail)
            || !(1..=86400).contains(&since_seconds)
            || service.is_some_and(|name| name.is_empty() || name.len() > 63)
        {
            return Err(ApplicationError::InvalidLogQuery);
        }
        self.store.get(id).await?;
        Ok(self
            .runtime
            .logs(id, service, tail, since_seconds, stream)
            .await?)
    }
}
