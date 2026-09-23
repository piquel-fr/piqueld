//! Build records outlive operation retention; output is chunked and bounded.
use super::{Store, StoreError, now_ms, page_limit};
use piqueld_core::{
    ApplicationId,
    api::{BuildLogChunk, BuildLogPage, BuildRecord, BuildState, LogStream, Page},
    manifest::Source,
};

struct BuildRow {
    id: i64,
    application_id: String,
    operation_id: String,
    service: String,
    source_json: String,
    state: String,
    started_at_ms: i64,
    finished_at_ms: Option<i64>,
    commit_hash: Option<String>,
    image_id: Option<String>,
    log_bytes: i64,
    log_truncated: i64,
    log_expired: i64,
}

struct BuildLogRow {
    offset: i64,
    data: Vec<u8>,
    stream: String,
    timestamp_ms: i64,
}

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
    pub(crate) async fn append_build_log(
        &self,
        id: i64,
        bytes: &[u8],
        stream: LogStream,
    ) -> Result<(), StoreError> {
        let stream = stream.as_str();
        let timestamp = now_ms();
        let (_writer, mut tx) = self.begin_immediate().await?;
        let row = sqlx::query!(
            "SELECT log_bytes,log_expired,log_truncated,state FROM builds WHERE id=?1",
            id
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        if row.log_expired != 0 || row.log_truncated != 0 || row.state != "running" {
            return Ok(());
        }
        let remaining = i64::from(self.build_history.log_max_bytes)
            .saturating_sub(row.log_bytes)
            .max(0);
        let mut count = bytes
            .len()
            .min(usize::try_from(remaining).map_err(StoreError::invalid_input)?);
        if count < bytes.len() {
            count = crate::build::BuildLog::complete_prefix(&bytes[..count]);
        }
        // Each chunk is bounded regardless of which executor records output.
        let mut offset = row.log_bytes;
        let mut remaining = &bytes[..count];
        while !remaining.is_empty() {
            let mut end = remaining.len().min(4096);
            if end < remaining.len() {
                end = crate::build::BuildLog::complete_prefix(&remaining[..end]);
            }
            let chunk = &remaining[..end];
            sqlx::query!(
                "INSERT INTO build_log_chunks(build_id,offset,data,stream,timestamp_ms) VALUES(?1,?2,?3,?4,?5)",
                id,
                offset,
                chunk,
                stream,
                timestamp
            )
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
            offset += i64::try_from(chunk.len()).map_err(StoreError::invalid_input)?;
            remaining = &remaining[end..];
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
        let _writer = self.writers.lock().await;
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
        let _writer = self.writers.lock().await;
        sqlx::query!("UPDATE builds SET state=?1,finished_at_ms=?2,image_id=?3 WHERE id=?4 AND state='running'",state,now,image,id).execute(&self.pool).await.map_err(StoreError::database)?;
        Ok(())
    }
    pub(crate) async fn recover_builds(&self) -> Result<(), StoreError> {
        let now = now_ms();
        let _writer = self.writers.lock().await;
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
        let (_writer, mut tx) = self.begin_immediate().await?;
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
        let mut rows = if let Some(application) = application {
            let app = application.as_str();
            sqlx::query_as!(
                BuildRow,
                "SELECT id AS \"id!\",application_id,operation_id,service,source_json,state,
                 started_at_ms,finished_at_ms,commit_hash,image_id,log_bytes,log_truncated,log_expired
                 FROM builds WHERE application_id=?1 AND id<?2 ORDER BY id DESC LIMIT ?3",
                app,
                before,
                fetch
            )
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as!(
                BuildRow,
                "SELECT id AS \"id!\",application_id,operation_id,service,source_json,state,
                 started_at_ms,finished_at_ms,commit_hash,image_id,log_bytes,log_truncated,log_expired
                 FROM builds WHERE id<?1 ORDER BY id DESC LIMIT ?2",
                before,
                fetch
            )
            .fetch_all(&self.pool)
            .await
        }
        .map_err(StoreError::database)?;
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
    /// Reads the newest output chunks before an optional exclusive cursor. Stream filtering happens before pagination.
    /// # Errors
    /// Returns not found, invalid cursors, or storage errors.
    pub async fn build_logs(
        &self,
        id: i64,
        before: Option<i64>,
        stream: Option<LogStream>,
    ) -> Result<BuildLogPage, StoreError> {
        if before.is_some_and(|value| value < 0) {
            return Err(StoreError::InvalidInput);
        }
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        let row = sqlx::query!(
            "SELECT log_truncated,log_expired FROM builds WHERE id=?1",
            id
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        let before = before.unwrap_or(i64::MAX);
        let mut chunks = if let Some(stream) = stream {
            let filter = stream.as_str();
            sqlx::query_as!(
                BuildLogRow,
                "SELECT offset,data,stream,timestamp_ms FROM build_log_chunks
                 WHERE build_id=?1 AND stream=?2 AND offset<?3
                 ORDER BY offset DESC LIMIT 17",
                id,
                filter,
                before
            )
            .fetch_all(&mut *tx)
            .await
        } else {
            sqlx::query_as!(
                BuildLogRow,
                "SELECT offset,data,stream,timestamp_ms FROM build_log_chunks
                 WHERE build_id=?1 AND offset<?2 ORDER BY offset DESC LIMIT 17",
                id,
                before
            )
            .fetch_all(&mut *tx)
            .await
        }
        .map_err(StoreError::database)?;
        let more = chunks.len() > 16;
        chunks.truncate(16);
        chunks.reverse();
        let previous_offset = more.then(|| chunks[0].offset);
        let mut items = Vec::new();
        for chunk in chunks {
            items.push(BuildLogChunk {
                offset: chunk.offset,
                timestamp_ms: chunk.timestamp_ms,
                stream: match chunk.stream.as_str() {
                    "stdout" => LogStream::Stdout,
                    "stderr" => LogStream::Stderr,
                    _ => return Err(StoreError::Corrupt),
                },
                text: String::from_utf8_lossy(&chunk.data).into_owned(),
            });
        }
        Ok(BuildLogPage {
            items,
            previous_offset,
            truncated: row.log_truncated != 0,
            expired: row.log_expired != 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        _temp: tempfile::TempDir,
        store: Store,
        app: piqueld_core::NormalizedApplication,
        operation: String,
        id: i64,
    }
    impl Fixture {
        async fn new() -> Self {
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
            Self {
                _temp: temp,
                store,
                app,
                operation: operation.id,
                id,
            }
        }
    }

    #[tokio::test]
    async fn build_metadata_obeys_the_writer_gate() {
        let Fixture {
            _temp, store, id, ..
        } = Fixture::new().await;
        let writer = store.writers.lock().await;
        let blocked = std::time::Duration::from_millis(50);
        let (commit, finish, recover) = tokio::join!(
            tokio::time::timeout(blocked, store.build_commit(id, "commit")),
            tokio::time::timeout(blocked, store.finish_build(id, BuildState::Failed, None)),
            tokio::time::timeout(blocked, store.recover_builds()),
        );
        assert!(commit.is_err(), "commit metadata bypassed the writer gate");
        assert!(finish.is_err(), "completion bypassed the writer gate");
        assert!(recover.is_err(), "recovery bypassed the writer gate");
        drop(writer);
        store.build_commit(id, "commit").await.unwrap();
        store
            .finish_build(id, BuildState::Failed, None)
            .await
            .unwrap();
        store.recover_builds().await.unwrap();
    }

    #[tokio::test]
    async fn output_survives_concurrent_metadata_writes_for_another_build() {
        let Fixture {
            _temp,
            store,
            app,
            operation,
            id,
        } = Fixture::new().await;
        let other = store
            .start_build(app.id(), &operation, "web", &app.spec().services[0].source)
            .await
            .unwrap();
        let output = async {
            for _ in 0..8 {
                store
                    .append_build_log(id, &[b'a'; 4096], LogStream::Stdout)
                    .await?;
            }
            Ok::<_, StoreError>(())
        };
        let metadata = async {
            for _ in 0..8 {
                store.build_commit(other, "commit").await?;
            }
            store
                .finish_build(other, BuildState::Succeeded, Some("sha256:fixture"))
                .await
        };
        tokio::try_join!(output, metadata).unwrap();
        let logs = store.build_logs(id, None, None).await.unwrap();
        assert_eq!(logs.items.len(), 8);
        for (index, chunk) in logs.items.iter().enumerate() {
            assert_eq!(chunk.offset, i64::try_from(index * 4096).unwrap());
            assert_eq!(chunk.text, "a".repeat(4096));
        }
        let records = store.builds(Some(app.id()), None, 50).await.unwrap();
        assert_eq!(records.items[0].state, BuildState::Succeeded);
        assert_eq!(records.items[0].commit.as_deref(), Some("commit"));
        assert_eq!(records.items[1].log_bytes, 32768);
    }

    #[tokio::test]
    async fn build_pages_filter_before_applying_the_limit() {
        let Fixture {
            _temp,
            store,
            app,
            operation,
            id,
        } = Fixture::new().await;
        let mut manifest = app.to_manifest();
        manifest.metadata.name = "other".into();
        let other = manifest
            .validate()
            .unwrap()
            .normalize(ApplicationId::parse("app-other").unwrap());
        let other_operation = store.save_application(&other, None, Some(0)).await.unwrap();
        let latest = store
            .start_build(app.id(), &operation, "web", &app.spec().services[0].source)
            .await
            .unwrap();
        for _ in 0..3 {
            store
                .start_build(
                    other.id(),
                    &other_operation.id,
                    "web",
                    &other.spec().services[0].source,
                )
                .await
                .unwrap();
        }
        let first = store.builds(Some(app.id()), None, 1).await.unwrap();
        assert_eq!(first.items[0].id, latest);
        let second = store
            .builds(Some(app.id()), first.next_cursor.as_deref(), 1)
            .await
            .unwrap();
        assert_eq!(second.items[0].id, id);
        assert!(second.next_cursor.is_none());
        let global = store.builds(None, None, 1).await.unwrap();
        assert_eq!(global.items[0].application_id, other.id().as_str());
        let absent = ApplicationId::parse("app-absent").unwrap();
        assert!(
            store
                .builds(Some(&absent), None, 1)
                .await
                .unwrap()
                .items
                .is_empty()
        );
    }

    #[tokio::test]
    async fn output_is_bounded_paged_and_expired_without_losing_metadata() {
        let Fixture {
            _temp,
            store,
            app,
            operation,
            id,
        } = Fixture::new().await;
        store
            .append_build_log(id, &vec![b'a'; 80_000], LogStream::Stdout)
            .await
            .unwrap();
        let first = store.build_logs(id, None, None).await.unwrap();
        assert_eq!(
            first
                .items
                .iter()
                .map(|chunk| chunk.text.len())
                .sum::<usize>(),
            61808
        );
        assert!(first.truncated);
        let second = store
            .build_logs(id, first.previous_offset, None)
            .await
            .unwrap();
        assert_eq!(
            second
                .items
                .iter()
                .map(|chunk| chunk.text.len())
                .sum::<usize>(),
            8192
        );
        assert!(second.previous_offset.is_none());
        store
            .finish_build(id, BuildState::Succeeded, Some("sha256:fixture"))
            .await
            .unwrap();
        store
            .append_build_log(id, b"late", LogStream::Stdout)
            .await
            .unwrap();
        assert_eq!(
            store.builds(Some(app.id()), None, 50).await.unwrap().items[0].log_bytes,
            70_000
        );
        sqlx::query!("UPDATE builds SET finished_at_ms=0 WHERE id=?1", id)
            .execute(&store.pool)
            .await
            .unwrap();
        store.prune_build_logs().await.unwrap();
        assert!(store.build_logs(id, None, None).await.unwrap().expired);
        let records = store.builds(None, None, 50).await.unwrap();
        assert_eq!(records.items.len(), 1);
        assert_eq!(records.items[0].state, BuildState::Succeeded);
        let interrupted = store
            .start_build(app.id(), &operation, "web", &app.spec().services[0].source)
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
    #[tokio::test]
    async fn output_filters_before_paging_backwards() {
        let Fixture {
            _temp, store, id, ..
        } = Fixture::new().await;
        store
            .append_build_log(id, &vec![b'a'; 80_000], LogStream::Stdout)
            .await
            .unwrap();
        let newest = store
            .build_logs(id, None, Some(LogStream::Stdout))
            .await
            .unwrap();
        assert_eq!(newest.items.len(), 16);
        assert!(
            newest
                .items
                .iter()
                .all(|chunk| chunk.stream == LogStream::Stdout && chunk.timestamp_ms > 0)
        );
        let older = store
            .build_logs(id, newest.previous_offset, Some(LogStream::Stdout))
            .await
            .unwrap();
        assert_eq!(
            older
                .items
                .iter()
                .chain(&newest.items)
                .map(|chunk| chunk.text.len())
                .sum::<usize>(),
            70_000
        );
        assert!(older.previous_offset.is_none());
        assert!(older.items.last().unwrap().offset < newest.items[0].offset);
        assert!(
            store
                .build_logs(id, None, Some(LogStream::Stderr))
                .await
                .unwrap()
                .items
                .is_empty()
        );

        let Fixture {
            _temp: _other_temp,
            store,
            id,
            ..
        } = Fixture::new().await;
        // A sparse stream must be filtered before the 16-chunk page limit.
        store
            .append_build_log(id, b"error output", LogStream::Stderr)
            .await
            .unwrap();
        store
            .append_build_log(id, &vec![b'x'; 68_000], LogStream::Stdout)
            .await
            .unwrap();
        let errors = store
            .build_logs(id, None, Some(LogStream::Stderr))
            .await
            .unwrap();
        assert_eq!(errors.items[0].text, "error output");
        assert!(errors.previous_offset.is_none());
    }
    #[tokio::test]
    async fn structured_chunks_preserve_multibyte_output() {
        for (repeats, retained, truncated) in [(3000, 9000, false), (25000, 69999, true)] {
            let Fixture {
                _temp, store, id, ..
            } = Fixture::new().await;
            let text = "€".repeat(repeats);
            store
                .append_build_log(id, text.as_bytes(), LogStream::Stdout)
                .await
                .unwrap();
            if truncated {
                // The unused byte at the cap must not admit output after a gap.
                store
                    .append_build_log(id, b"x", LogStream::Stdout)
                    .await
                    .unwrap();
            }
            let mut output = String::new();
            let mut before = None;
            loop {
                let page = store.build_logs(id, before, None).await.unwrap();
                assert_eq!(page.truncated, truncated);
                let text = page
                    .items
                    .iter()
                    .map(|chunk| chunk.text.as_str())
                    .collect::<String>();
                output.insert_str(0, &text);
                before = page.previous_offset;
                if before.is_none() {
                    break;
                }
            }
            assert_eq!(output, text[..retained]);
            assert!(!output.contains('�'));
        }
    }
}
