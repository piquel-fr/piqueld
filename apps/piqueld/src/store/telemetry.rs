//! Environments granted the daemon's telemetry, which their receiving
//! service gets from every application.
use super::{Store, StoreError, acceptance::Accepted};
use piqueld_core::api::DiagnosticView;
use piqueld_core::{EnvironmentId, PlanDiagnostic, ResolvedApplication, ServiceName};
use sqlx::{Sqlite, Transaction};

impl Store {
    /// Whether environment `id` is granted telemetry and not being deleted.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn telemetry_granted(&self, id: &EnvironmentId) -> Result<bool, StoreError> {
        let id = id.as_str();
        let granted = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "granted!: i64" FROM environments e JOIN applications a ON a.id=e.application_id WHERE e.id=?1 AND e.telemetry=1 AND e.delete_intent=0 AND a.delete_intent=0"#,
            id
        )
        .fetch_one(&self.pool)
        .await
        .map_err(StoreError::database)?;
        Ok(granted > 0)
    }

    /// Every environment granted telemetry, neither stopped nor being
    /// deleted, whose active target deploys a receiving service, with that
    /// service.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn telemetry_receivers(
        &self,
    ) -> Result<Vec<(EnvironmentId, ServiceName)>, StoreError> {
        self.receivers(None).await
    }

    /// Environment `id`'s receiving service, while it is granted telemetry,
    /// neither stopped nor being deleted, and its active target deploys one.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn telemetry_receiver(
        &self,
        id: &EnvironmentId,
    ) -> Result<Option<ServiceName>, StoreError> {
        Ok(self
            .receivers(Some(id))
            .await?
            .pop()
            .map(|(_, service)| service))
    }

    /// [`Self::telemetry_receivers`], of environment `id` only if set.
    async fn receivers(
        &self,
        id: Option<&EnvironmentId>,
    ) -> Result<Vec<(EnvironmentId, ServiceName)>, StoreError> {
        let id = id.map(EnvironmentId::as_str);
        sqlx::query!(
            r#"SELECT e.id AS "id!", json_extract(s.value,'$.logical_name') AS "service!: String" FROM environments e JOIN applications a ON a.id=e.application_id JOIN json_each(e.resolved_json,'$.services') s WHERE (?1 IS NULL OR e.id=?1) AND e.telemetry=1 AND e.stopped=0 AND e.delete_intent=0 AND a.delete_intent=0 AND e.resolved_json IS NOT NULL AND json_extract(s.value,'$.receives_telemetry')=1 ORDER BY e.id"#,
            id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?
        .into_iter()
        .map(|row| {
            Ok((
                EnvironmentId::parse(row.id).map_err(StoreError::corrupt)?,
                ServiceName::parse(row.service).map_err(StoreError::corrupt)?,
            ))
        })
        .collect()
    }

    /// Grants or revokes environment `id`'s telemetry and responds with it,
    /// recording `environment_telemetry_changed` when that changes it.
    /// Previews are never granted it: they are `NotFound`. Nothing it
    /// deploys changes, so the application revision stays.
    pub(super) async fn set_telemetry_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: EnvironmentId,
        granted: bool,
        now: i64,
    ) -> Result<Accepted, StoreError> {
        let environment = Self::checked_environment_on(tx, &id, None).await?;
        if environment.delete_intent() {
            return Err(StoreError::Busy);
        }
        if environment.environment.telemetry != granted {
            let (row, value) = (id.as_str(), i64::from(granted));
            sqlx::query!(
                "UPDATE environments SET telemetry=?1,updated_at_ms=?2 WHERE id=?3",
                value,
                now,
                row
            )
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
            let name = &environment.environment.name;
            let message = if granted {
                format!("environment {name} is granted every application's telemetry")
            } else {
                format!("environment {name} no longer receives telemetry")
            };
            Self::environment_event_on(tx, row, "environment_telemetry_changed", &message, now)
                .await?;
        }
        Self::environment_accepted(tx, id).await
    }

    /// Warns deployment `operation_id` with `telemetry_not_granted`, once,
    /// when `target` has a receiving service its environment isn't granted
    /// telemetry for.
    ///
    /// # Errors
    /// Returns a storage error.
    pub(crate) async fn warn_ungranted_receiver(
        &self,
        operation_id: &str,
        target: &ResolvedApplication,
    ) -> Result<(), StoreError> {
        let Some(receiver) = target.telemetry_receiver() else {
            return Ok(());
        };
        if self.telemetry_granted(&target.id).await? {
            return Ok(());
        }
        let warning = DiagnosticView::from(PlanDiagnostic::telemetry_not_granted(
            receiver.logical_name.as_str(),
        ));
        let json = serde_json::to_string(&warning).map_err(StoreError::corrupt)?;
        let code = warning.code.as_str();
        sqlx::query!(
            "UPDATE deployments SET warnings_json=json_insert(warnings_json,'$[#]',json(?1)) WHERE id=?2 AND NOT EXISTS (SELECT 1 FROM json_each(warnings_json) WHERE json_extract(value,'$.code')=?3)",
            json,
            operation_id,
            code
        )
        .execute(&self.pool)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }
}
