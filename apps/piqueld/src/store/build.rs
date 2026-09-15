//! Build records outlive operation retention; output is chunked and bounded.
use super::{Store, StoreError, now_ms, page_limit};
use piqueld_core::{
    ApplicationId,
    api::{BuildLogPage, BuildRecord, BuildState, Page},
    manifest::Source,
};

impl Store {
    /// Configures output retention and the per-build byte cap.
    #[must_use]
    pub fn with_build_history(mut self, policy: crate::config::BuildHistoryConfig) -> Self {
        self.build_history = policy;
        self
    }
    pub(crate) async fn start_build(
        &self,
        application: &ApplicationId,
        operation: &str,
        service: &str,
        source: &Source,
    ) -> Result<i64, StoreError> {
        let source = serde_json::to_string(source).map_err(StoreError::invalid_input)?;
        let app = application.as_str();
        let now = now_ms();
        let _writer = self.writers.lock().await;
        Ok(sqlx::query!("INSERT INTO builds(application_id,operation_id,service,source_json,state,started_at_ms) VALUES(?1,?2,?3,?4,'running',?5)",app,operation,service,source,now).execute(&self.pool).await.map_err(StoreError::database)?.last_insert_rowid())
    }
    pub(crate) async fn append_build_log(&self, id: i64, bytes: &[u8]) -> Result<(), StoreError> {
        let _writer = self.writers.lock().await;
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        let row = sqlx::query!(
            "SELECT log_bytes,log_expired,state FROM builds WHERE id=?1",
            id
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        if row.log_expired != 0 || row.state != "running" {
            return Ok(());
        }
        let remaining = i64::from(self.build_history.log_max_bytes)
            .saturating_sub(row.log_bytes)
            .max(0);
        let count = bytes
            .len()
            .min(usize::try_from(remaining).map_err(StoreError::invalid_input)?);
        // Each chunk is bounded regardless of which executor records output.
        let mut offset = row.log_bytes;
        for chunk in bytes[..count].chunks(4096) {
            sqlx::query!(
                "INSERT INTO build_log_chunks(build_id,offset,data) VALUES(?1,?2,?3)",
                id,
                offset,
                chunk
            )
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
            offset += i64::try_from(chunk.len()).map_err(StoreError::invalid_input)?;
        }
        let truncated = count < bytes.len();
        sqlx::query!(
            "UPDATE builds SET log_bytes=?1,log_truncated=log_truncated OR ?2 WHERE id=?3",
            offset,
            truncated,
            id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
    pub(crate) async fn build_commit(&self, id: i64, commit: &str) -> Result<(), StoreError> {
        sqlx::query!("UPDATE builds SET commit_hash=?1 WHERE id=?2", commit, id)
            .execute(&self.pool)
            .await
            .map_err(StoreError::database)?;
        Ok(())
    }
    pub(crate) async fn finish_build(
        &self,
        id: i64,
        state: BuildState,
        image: Option<&str>,
    ) -> Result<(), StoreError> {
        let state = match state {
            BuildState::Running => return Err(StoreError::InvalidInput),
            BuildState::Succeeded => "succeeded",
            BuildState::Failed => "failed",
            BuildState::Interrupted => "interrupted",
        };
        let now = now_ms();
        sqlx::query!("UPDATE builds SET state=?1,finished_at_ms=?2,image_id=?3 WHERE id=?4 AND state='running'",state,now,image,id).execute(&self.pool).await.map_err(StoreError::database)?;
        Ok(())
    }
    pub(crate) async fn recover_builds(&self) -> Result<(), StoreError> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE builds SET state='interrupted',finished_at_ms=?1 WHERE state='running'",
            now
        )
        .execute(&self.pool)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }
    pub(crate) async fn prune_build_logs(&self) -> Result<(), StoreError> {
        let cutoff =
            now_ms().saturating_sub(i64::from(self.build_history.log_retention_days) * 86_400_000);
        let _writer = self.writers.lock().await;
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        sqlx::query!("DELETE FROM build_log_chunks WHERE build_id IN (SELECT id FROM builds WHERE finished_at_ms<?1)",cutoff).execute(&mut *tx).await.map_err(StoreError::database)?;
        sqlx::query!(
            "UPDATE builds SET log_expired=1,log_bytes=0 WHERE finished_at_ms<?1",
            cutoff
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
    /// Lists build attempts newest first, optionally scoped to an application.
    /// # Errors
    /// Returns invalid pagination or database errors.
    pub async fn builds(
        &self,
        application: Option<&ApplicationId>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<BuildRecord>, StoreError> {
        let fetch = page_limit(limit)? + 1;
        let before = cursor
            .map(|c| {
                c.strip_prefix("v1:")
                    .ok_or(StoreError::InvalidInput)?
                    .parse::<i64>()
                    .map_err(StoreError::invalid_input)
            })
            .transpose()?
            .unwrap_or(i64::MAX);
        if before < 1 {
            return Err(StoreError::InvalidInput);
        }
        let app = application.map(ApplicationId::as_str);
        let mut rows = sqlx::query!("SELECT id,application_id,operation_id,service,source_json,state,started_at_ms,finished_at_ms,commit_hash,image_id,log_bytes,log_truncated,log_expired FROM builds WHERE id<?1 AND (?2 IS NULL OR application_id=?2) ORDER BY id DESC LIMIT ?3",before,app,fetch).fetch_all(&self.pool).await.map_err(StoreError::database)?;
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = more
            .then(|| rows.last().map(|r| format!("v1:{}", r.id)))
            .flatten();
        let items = rows
            .into_iter()
            .map(|r| {
                Ok(BuildRecord {
                    id: r.id,
                    application_id: r.application_id,
                    operation_id: r.operation_id,
                    service: r.service,
                    source: serde_json::from_str(&r.source_json).map_err(StoreError::corrupt)?,
                    state: match r.state.as_str() {
                        "running" => BuildState::Running,
                        "succeeded" => BuildState::Succeeded,
                        "failed" => BuildState::Failed,
                        "interrupted" => BuildState::Interrupted,
                        _ => return Err(StoreError::Corrupt),
                    },
                    started_at_ms: r.started_at_ms,
                    finished_at_ms: r.finished_at_ms,
                    commit: r.commit_hash,
                    image_id: r.image_id,
                    log_bytes: r.log_bytes,
                    log_truncated: r.log_truncated != 0,
                    log_expired: r.log_expired != 0,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        Ok(Page { items, next_cursor })
    }
    /// Reads at most 64 KiB from a build's output using byte offsets.
    /// # Errors
    /// Returns not found, invalid offset, or storage errors.
    pub async fn build_logs(&self, id: i64, offset: i64) -> Result<BuildLogPage, StoreError> {
        if offset < 0 {
            return Err(StoreError::InvalidInput);
        }
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        let row = sqlx::query!(
            "SELECT log_bytes,log_truncated,log_expired FROM builds WHERE id=?1",
            id
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        let chunks=sqlx::query!("SELECT offset,data FROM build_log_chunks WHERE build_id=?1 AND offset+length(data)>?2 ORDER BY offset LIMIT 16",id,offset).fetch_all(&mut *tx).await.map_err(StoreError::database)?;
        let mut bytes = Vec::new();
        for chunk in chunks {
            let skip = usize::try_from(offset.saturating_sub(chunk.offset).max(0))
                .map_err(StoreError::invalid_input)?;
            bytes.extend_from_slice(&chunk.data[skip..]);
        }
        let end =
            offset.saturating_add(i64::try_from(bytes.len()).map_err(StoreError::invalid_input)?);
        Ok(BuildLogPage {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            next_offset: (end < row.log_bytes).then_some(end),
            truncated: row.log_truncated != 0,
            expired: row.log_expired != 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn output_is_bounded_paged_and_expired_without_losing_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("state.db"))
            .await
            .unwrap()
            .with_build_history(crate::config::BuildHistoryConfig {
                log_max_bytes: 70_000,
                log_retention_days: 1,
            });
        let app = piqueld_core::parse_toml(include_str!(
            "../../../../crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml"
        ))
        .unwrap()
        .normalize(ApplicationId::parse("app-build-test").unwrap());
        let operation = store.save_application(&app, None, None).await.unwrap();
        let id = store
            .start_build(
                app.id(),
                &operation.id,
                "web",
                &app.spec().services[0].source,
            )
            .await
            .unwrap();
        store
            .append_build_log(id, &vec![b'a'; 80_000])
            .await
            .unwrap();
        let first = store.build_logs(id, 0).await.unwrap();
        assert_eq!(first.text.len(), 65536);
        assert!(first.truncated);
        let second = store
            .build_logs(id, first.next_offset.unwrap())
            .await
            .unwrap();
        assert_eq!(second.text.len(), 4464);
        assert!(second.next_offset.is_none());
        store
            .finish_build(id, BuildState::Succeeded, Some("sha256:fixture"))
            .await
            .unwrap();
        store.append_build_log(id, b"late").await.unwrap();
        assert_eq!(
            store.builds(Some(app.id()), None, 50).await.unwrap().items[0].log_bytes,
            70_000
        );
        sqlx::query!("UPDATE builds SET finished_at_ms=0 WHERE id=?1", id)
            .execute(&store.pool)
            .await
            .unwrap();
        store.prune_build_logs().await.unwrap();
        assert!(store.build_logs(id, 0).await.unwrap().expired);
        let records = store.builds(None, None, 50).await.unwrap();
        assert_eq!(records.items.len(), 1);
        assert_eq!(records.items[0].state, BuildState::Succeeded);
        let interrupted = store
            .start_build(
                app.id(),
                &operation.id,
                "web",
                &app.spec().services[0].source,
            )
            .await
            .unwrap();
        store.recover_builds().await.unwrap();
        let page = store.builds(None, None, 1).await.unwrap();
        assert_eq!(page.items[0].id, interrupted);
        assert_eq!(page.items[0].state, BuildState::Interrupted);
        assert_eq!(
            store
                .builds(None, page.next_cursor.as_deref(), 1)
                .await
                .unwrap()
                .items[0]
                .id,
            id
        );
    }
}
