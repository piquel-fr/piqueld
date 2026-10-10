//! Deployments whose images cleanup keeps, by retention root.
use super::{Store, StoreError};
use piqueld_core::{ImmutableImage, RetentionRoot, resource::ResolvedApplication};
use std::collections::BTreeSet;

impl Store {
    /// Every image a retention root keeps, given how many successful
    /// deployments each environment keeps. Fails rather than skipping a
    /// target that doesn't decode, so cleanup never runs on a partial set.
    /// # Errors
    /// Returns storage or decoding errors.
    pub async fn retained_images(&self, keep: u32) -> Result<BTreeSet<ImmutableImage>, StoreError> {
        let mut images = BTreeSet::new();
        for root in RetentionRoot::ALL {
            for target in self.retained(root, keep).await? {
                images.extend(target.images());
            }
        }
        Ok(images)
    }

    /// The prepared targets `root` keeps, across every environment and
    /// preview.
    /// # Errors
    /// Returns storage or decoding errors.
    pub async fn retained(
        &self,
        root: RetentionRoot,
        keep: u32,
    ) -> Result<Vec<ResolvedApplication>, StoreError> {
        let targets = match root {
            RetentionRoot::Current => sqlx::query_scalar!(
                r#"SELECT resolved_json AS "json!" FROM environments WHERE resolved_json IS NOT NULL"#
            )
            .fetch_all(&self.pool)
            .await,
            RetentionRoot::Latest => sqlx::query_scalar!(
                r#"SELECT o.target_json AS "json!" FROM operations o WHERE o.target_json IS NOT NULL AND o.id=(SELECT latest.id FROM operations latest WHERE latest.environment_id=o.environment_id ORDER BY latest.created_at_ms DESC,latest.id DESC LIMIT 1)"#
            )
            .fetch_all(&self.pool)
            .await,
            // Only environments: previews are never restored.
            RetentionRoot::Recent => sqlx::query_scalar!(
                r#"SELECT json AS "json!" FROM (SELECT o.target_json AS json,ROW_NUMBER() OVER (PARTITION BY d.environment_id ORDER BY d.created_at_ms DESC,d.id DESC) AS rank FROM deployments d JOIN operations o ON o.id=d.id JOIN environments e ON e.id=d.environment_id WHERE e.kind='environment' AND d.succeeded_at_ms IS NOT NULL AND o.target_json IS NOT NULL) WHERE rank<=?1"#,
                keep
            )
            .fetch_all(&self.pool)
            .await,
            // #132 selects the current target of every environment a promoted
            // environment promotes from.
            RetentionRoot::PromotionSource => Ok(Vec::new()),
        }
        .map_err(StoreError::database)?;
        targets
            .iter()
            .map(|json| serde_json::from_str(json).map_err(StoreError::corrupt))
            .collect()
    }
}

#[cfg(test)]
mod tests;
