//! Status repository implementation.

use super::{
    ApplicationId, ApplicationState, ApplicationStatus, SqliteStore, StoreError, generation_i64,
    now_ms, valid_bounded_text,
};

impl SqliteStore {
    /// Reads the persisted lifecycle status for a non-deleted application.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when the application has no status or is
    /// deleted. Returns an error when the database cannot be read, the stored
    /// state is malformed, or the observed generation cannot be converted to
    /// `u64`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(
    /// #     store: &SqliteStore,
    /// #     id: &ApplicationId,
    /// # ) -> Result<(), StoreError> {
    /// let status = store.status(id).await?;
    /// println!("{:?}", status.state);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn status(&self, id: &ApplicationId) -> Result<ApplicationStatus, StoreError> {
        let id_value = id.as_str();
        let row = sqlx::query!(
            r#"SELECT s.state AS "state!",s.observed_generation,s.message,s.updated_at_ms AS "updated_at_ms!" FROM application_status s JOIN applications a ON a.id=s.application_id WHERE s.application_id=?1 AND a.deleted_at_ms IS NULL"#,
            id_value
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        Ok(ApplicationStatus {
            application_id: id.clone(),
            state: ApplicationState::parse(&row.state)?,
            observed_generation: row
                .observed_generation
                .map(u64::try_from)
                .transpose()
                .map_err(StoreError::corrupt)?,
            message: row.message,
            updated_at_ms: row.updated_at_ms,
        })
    }

    /// Persists an application state transition when it satisfies the lifecycle and generation constraints.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::IllegalTransition` for an invalid state or generation transition,
    /// `StoreError::InvalidInput` for an overlong message, and other store errors when persistence
    /// fails or the transition cannot be applied.
    ///
    /// # Examples
    ///
    /// ```
    /// # async fn example(
    /// #     store: &SqliteStore,
    /// #     id: &ApplicationId,
    /// # ) -> Result<(), StoreError> {
    /// store
    ///     .set_status(
    ///         id,
    ///         ApplicationState::Pending,
    ///         ApplicationState::Ready,
    ///         Some(1),
    ///         None,
    ///     )
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub(crate) async fn set_status(
        &self,
        id: &ApplicationId,
        from: ApplicationState,
        to: ApplicationState,
        observed_generation: Option<u64>,
        message: Option<&str>,
    ) -> Result<(), StoreError> {
        if !from.can_transition_to(to) {
            return Err(StoreError::IllegalTransition);
        }
        if observed_generation.is_some()
            && !matches!(
                to,
                ApplicationState::Ready | ApplicationState::Degraded | ApplicationState::Failed
            )
        {
            return Err(StoreError::IllegalTransition);
        }
        if to == ApplicationState::Ready && observed_generation.is_none() {
            return Err(StoreError::IllegalTransition);
        }
        if message.is_some_and(|value| !valid_bounded_text(value, 2048)) {
            return Err(StoreError::InvalidInput);
        }
        let mut connection = self.connection().await?;
        let observed_generation = observed_generation.map(generation_i64).transpose()?;
        let to_state = to.as_str();
        let from_state = from.as_str();
        let now = now_ms();
        let id_value = id.as_str();
        let changed = sqlx::query!(
            "UPDATE application_status SET state=?1,observed_generation=?2,message=?3,updated_at_ms=?4 WHERE application_id=?5 AND state=?6 AND (?2 IS NULL OR (observed_generation IS NULL OR ?2 >= observed_generation)) AND (?2 IS NULL OR ?2 <= (SELECT generation FROM applications WHERE id=?5)) AND (?1 != 'ready' OR ?2 = (SELECT generation FROM applications WHERE id=?5)) AND (?1 != 'deleting' OR (SELECT delete_intent FROM applications WHERE id=?5)=1) AND ((SELECT delete_intent FROM applications WHERE id=?5)=0 OR ?1 IN ('deleting','degraded','failed'))",
            to_state,
            observed_generation,
            message,
            now,
            id_value,
            from_state
        )
        .execute(&mut *connection)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if changed == 1 {
            Ok(())
        } else {
            Err(Self::transition_miss(&mut connection, "application_status", id_value).await?)
        }
    }
}
