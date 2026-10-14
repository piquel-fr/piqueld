//! Private links between environments, resolved from their active targets.
use super::{Store, StoreError};
use piqueld_core::links::{Bindings, Link, LinkParty, LinkReport};
use piqueld_core::manifest::{ValidatedExport, ValidatedImport};
use piqueld_core::{
    ApplicationId, ApplicationName, EnvironmentId, ObservedApplication, ResolvedApplication,
};
use sqlx::SqliteConnection;

/// Decodes a JSON value read from a target, absent when the target has none.
fn decode<T: serde::de::DeserializeOwned + Default>(json: Option<String>) -> Result<T, StoreError> {
    json.map_or_else(
        || Ok(T::default()),
        |json| serde_json::from_str(&json).map_err(StoreError::corrupt),
    )
}

impl Store {
    /// Every environment and preview that has deployed and is not being
    /// deleted, with the links its active target declares, plus `including`
    /// whatever its state.
    async fn link_parties(&self, including: &EnvironmentId) -> Result<Vec<LinkParty>, StoreError> {
        let including = including.as_str();
        let rows = sqlx::query!(
            r#"SELECT e.id AS "id!",e.name AS "name!",e.kind AS "kind!",a.id AS "application_id!",a.name AS "application!",json_extract(e.resolved_json,'$.exports') AS "exports: String",json_extract(e.resolved_json,'$.imports') AS "imports: String",json_extract(e.resolved_json,'$.bindings') AS "bindings: String" FROM environments e JOIN applications a ON a.id=e.application_id WHERE (e.resolved_json IS NOT NULL AND e.delete_intent=0 AND a.delete_intent=0) OR e.id=?1"#,
            including
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?;
        rows.into_iter()
            .map(|row| {
                Ok(LinkParty {
                    environment: EnvironmentId::parse(row.id).map_err(StoreError::corrupt)?,
                    application_id: ApplicationId::parse(row.application_id)
                        .map_err(StoreError::corrupt)?,
                    application: ApplicationName::parse(row.application)
                        .map_err(StoreError::corrupt)?,
                    name: row.name,
                    preview: row.kind == "preview",
                    exports: decode(row.exports)?,
                    imports: decode(row.imports)?,
                    bindings: decode(row.bindings)?,
                })
            })
            .collect()
    }

    /// Every application, keyed by its current name.
    async fn applications_by_name_on(
        connection: &mut SqliteConnection,
    ) -> Result<Bindings, StoreError> {
        sqlx::query!(r#"SELECT id AS "id!",name AS "name!" FROM applications"#)
            .fetch_all(connection)
            .await
            .map_err(StoreError::database)?
            .into_iter()
            .map(|row| {
                Ok((
                    ApplicationName::parse(row.name).map_err(StoreError::corrupt)?,
                    ApplicationId::parse(row.id).map_err(StoreError::corrupt)?,
                ))
            })
            .collect()
    }

    /// Binds the applications `target`'s exports and imports name before it
    /// is saved, keeping those its environment's active target bound while
    /// they still exist; see [`piqueld_core::links::bind`].
    pub(super) async fn bind_links_on(
        connection: &mut SqliteConnection,
        target: &mut ResolvedApplication,
    ) -> Result<(), StoreError> {
        if target.exports.is_empty() && target.imports.is_empty() {
            target.bindings.clear();
            return Ok(());
        }
        let id = target.id.as_str();
        let previous = sqlx::query_scalar!(
            r#"SELECT json_extract(resolved_json,'$.bindings') AS "bindings: String" FROM environments WHERE id=?1"#,
            id
        )
        .fetch_optional(&mut *connection)
        .await
        .map_err(StoreError::database)?
        .flatten();
        target.bindings = piqueld_core::links::bind(
            &target.exports,
            &target.imports,
            &decode(previous)?,
            &Self::applications_by_name_on(connection).await?,
        );
        Ok(())
    }

    /// Link parties as [`Self::link_parties`] reads them, with the side of
    /// `environment` replaced by `exports` and `imports` as a deployment of
    /// them binds them: with `bindings`, or else as saving them would.
    async fn link_parties_with(
        &self,
        environment: &EnvironmentId,
        exports: &[ValidatedExport],
        imports: &[ValidatedImport],
        bindings: &Bindings,
    ) -> Result<Vec<LinkParty>, StoreError> {
        let mut parties = self.link_parties(environment).await?;
        if let Some(party) = parties
            .iter_mut()
            .find(|party| &party.environment == environment)
        {
            party.bindings = if bindings.is_empty() {
                let applications = Self::applications_by_name_on(
                    &mut *self.pool.acquire().await.map_err(StoreError::database)?,
                )
                .await?;
                piqueld_core::links::bind(exports, imports, &party.bindings, &applications)
            } else {
                bindings.clone()
            };
            party.exports = exports.to_vec();
            party.imports = imports.to_vec();
        }
        Ok(parties)
    }

    /// The links `target` takes part in: its imports and exports as it
    /// declares them, against every other environment's active target.
    async fn links_of(&self, target: &ResolvedApplication) -> Result<Vec<Link>, StoreError> {
        // Most targets declare no links, so they skip the read.
        if target.exports.is_empty() && target.imports.is_empty() {
            return Ok(Vec::new());
        }
        let parties = self
            .link_parties_with(
                &target.id,
                &target.exports,
                &target.imports,
                &target.bindings,
            )
            .await?;
        Ok(piqueld_core::links::links(&parties)
            .into_iter()
            .filter(|link| link.importer == target.id || link.exporter == target.id)
            .collect())
    }

    /// Projects the links `target` takes part in onto it; see
    /// [`ResolvedApplication::with_links`].
    /// # Errors
    /// Returns storage or decoding errors.
    pub async fn with_links(
        &self,
        target: ResolvedApplication,
        observed: &ObservedApplication,
    ) -> Result<ResolvedApplication, StoreError> {
        // Boxed: the read would otherwise grow every planning future.
        let links = Box::pin(self.links_of(&target)).await?;
        Ok(target.with_links(&links, observed))
    }

    /// Reports the links of `environment`: whether each of its imports
    /// connects, and who imports its services. They are those it deploys, or
    /// `own`, bound as deploying them would, when given.
    /// # Errors
    /// Returns storage or decoding errors.
    pub async fn link_report(
        &self,
        environment: &EnvironmentId,
        own: Option<(&[ValidatedExport], &[ValidatedImport])>,
    ) -> Result<LinkReport, StoreError> {
        let parties = match own {
            Some((exports, imports)) => {
                Box::pin(self.link_parties_with(environment, exports, imports, &Bindings::new()))
                    .await?
            }
            None => Box::pin(self.link_parties(environment)).await?,
        };
        Ok(piqueld_core::links::report(environment, &parties))
    }
}
