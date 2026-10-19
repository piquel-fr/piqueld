//! Where container log export stopped reading, kept across restarts.
use crate::store::{Store, StoreError};
use crate::telemetry::ContainerCursors;

impl Store {
    /// The cursors container log export last saved; empty before the first
    /// save or after [`Self::clear_container_cursors`].
    pub(crate) async fn container_cursors(&self) -> Result<ContainerCursors, StoreError> {
        let state =
            sqlx::query_scalar!("SELECT state_json FROM container_log_cursors WHERE singleton=1")
                .fetch_optional(&self.pool)
                .await
                .map_err(StoreError::database)?;
        state.map_or_else(
            || Ok(ContainerCursors::default()),
            |state| serde_json::from_str(&state).map_err(StoreError::corrupt),
        )
    }

    /// Replaces the saved cursors.
    pub(crate) async fn save_container_cursors(
        &self,
        cursors: &ContainerCursors,
    ) -> Result<(), StoreError> {
        let state = serde_json::to_string(cursors).map_err(StoreError::invalid_input)?;
        self.write(sqlx::query!(
            "INSERT INTO container_log_cursors(singleton,state_json) VALUES(1,?1)
            ON CONFLICT(singleton) DO UPDATE SET state_json=excluded.state_json",
            state,
        ))
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// Forgets the saved cursors, so export enabled again later starts from
    /// the present rather than replaying what ran while it was off.
    pub(crate) async fn clear_container_cursors(&self) -> Result<(), StoreError> {
        self.write(sqlx::query!("DELETE FROM container_log_cursors"))
            .await
            .map_err(StoreError::database)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::ContainerCursor;

    #[tokio::test]
    async fn container_cursors_are_replaced_and_cleared() {
        let (_directory, store) = Store::temporary().await;
        assert_eq!(
            store.container_cursors().await.unwrap(),
            ContainerCursors::default()
        );
        for at in [1, 2] {
            let cursors = ContainerCursors {
                watermark: Some(at),
                containers: [("container".to_owned(), ContainerCursor { at, done: false })].into(),
            };
            store.save_container_cursors(&cursors).await.unwrap();
            assert_eq!(store.container_cursors().await.unwrap(), cursors);
        }
        store.clear_container_cursors().await.unwrap();
        assert_eq!(
            store.container_cursors().await.unwrap(),
            ContainerCursors::default()
        );
    }
}
