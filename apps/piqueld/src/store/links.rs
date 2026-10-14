//! Private links between environments, resolved from their active targets.
use super::{Store, StoreError};
use piqueld_core::links::{Link, LinkParty, LinkReport};
use piqueld_core::manifest::{ValidatedExport, ValidatedImport};
use piqueld_core::{ApplicationName, EnvironmentId, ObservedApplication, ResolvedApplication};

impl Store {
    /// Every environment and preview that has deployed and is not being
    /// deleted, with the links its active target declares, plus `including`
    /// whatever its state.
    async fn link_parties(&self, including: &EnvironmentId) -> Result<Vec<LinkParty>, StoreError> {
        let including = including.as_str();
        let rows = sqlx::query!(
            r#"SELECT e.id AS "id!",e.name AS "name!",e.kind AS "kind!",a.name AS "application!",json_extract(e.resolved_json,'$.exports') AS "exports: String",json_extract(e.resolved_json,'$.imports') AS "imports: String" FROM environments e JOIN applications a ON a.id=e.application_id WHERE (e.resolved_json IS NOT NULL AND e.delete_intent=0 AND a.delete_intent=0) OR e.id=?1"#,
            including
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?;
        rows.into_iter()
            .map(|row| {
                fn decode<T: serde::de::DeserializeOwned>(
                    json: Option<String>,
                ) -> Result<Vec<T>, StoreError> {
                    json.map_or_else(
                        || Ok(Vec::new()),
                        |json| serde_json::from_str(&json).map_err(StoreError::corrupt),
                    )
                }
                Ok(LinkParty {
                    environment: EnvironmentId::parse(row.id).map_err(StoreError::corrupt)?,
                    application: ApplicationName::parse(row.application)
                        .map_err(StoreError::corrupt)?,
                    name: row.name,
                    preview: row.kind == "preview",
                    exports: decode(row.exports)?,
                    imports: decode(row.imports)?,
                })
            })
            .collect()
    }

    /// Link parties as [`Self::link_parties`] reads them, with the links of
    /// `environment` replaced by `own`, when given, as a plan would deploy them.
    async fn link_parties_with(
        &self,
        environment: &EnvironmentId,
        own: Option<(&[ValidatedExport], &[ValidatedImport])>,
    ) -> Result<Vec<LinkParty>, StoreError> {
        let mut parties = self.link_parties(environment).await?;
        if let Some((exports, imports)) = own
            && let Some(party) = parties
                .iter_mut()
                .find(|party| &party.environment == environment)
        {
            party.exports = exports.to_vec();
            party.imports = imports.to_vec();
        }
        Ok(parties)
    }

    /// The links `target` takes part in: its imports and exports as it
    /// declares them, against every other environment's active target.
    async fn links_of(&self, target: &ResolvedApplication) -> Result<Vec<Link>, StoreError> {
        let parties = self
            .link_parties_with(&target.id, Some((&target.exports, &target.imports)))
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
        let links = self.links_of(&target).await?;
        Ok(target.with_links(&links, observed))
    }

    /// Reports the links of `environment`: whether each of its imports
    /// connects, and who imports its services. They are those it deploys, or
    /// `own` when given, as a plan would deploy them.
    /// # Errors
    /// Returns storage or decoding errors.
    pub async fn link_report(
        &self,
        environment: &EnvironmentId,
        own: Option<(&[ValidatedExport], &[ValidatedImport])>,
    ) -> Result<LinkReport, StoreError> {
        Ok(piqueld_core::links::report(
            environment,
            &self.link_parties_with(environment, own).await?,
        ))
    }
}
