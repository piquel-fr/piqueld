//! Durable attempt lifecycle and output sink, independent of the executor.
use crate::store::{Store, StoreError};
use piqueld_core::{ApplicationId, api::BuildState, manifest::Source};
use std::sync::Arc;

/// A bounded persistent output sink usable by any build executor.
#[derive(Clone)]
pub struct BuildLog {
    store: Arc<Store>,
    id: i64,
}
impl BuildLog {
    /// Appends output, retaining the configured prefix and recording truncation.
    /// # Errors
    /// Returns a persistence error rather than silently losing output.
    pub async fn append(
        &self,
        bytes: &[u8],
        stream: piqueld_core::api::LogStream,
    ) -> Result<(), StoreError> {
        self.store.append_build_log(self.id, bytes, stream).await
    }
    /// Keeps an incomplete UTF-8 suffix for the next read without rejecting binary output.
    ///
    /// Returns how many leading bytes can be flushed now: everything except a
    /// trailing, possibly incomplete multi-byte character. Invalid bytes elsewhere
    /// are flushed as-is.
    ///
    /// ```text
    /// b"ok\xe2\x82"      -> 2  (the partial `€` waits for more bytes)
    /// b"\xffok\xe2\x82"  -> 3
    /// ```
    pub(crate) fn complete_prefix(bytes: &[u8]) -> usize {
        let mut end = 0;
        for chunk in bytes.utf8_chunks() {
            end += chunk.valid().len();
            let invalid = chunk.invalid();
            if std::str::from_utf8(invalid).is_err_and(|error| error.error_len().is_none()) {
                return end;
            }
            end += invalid.len();
        }
        end
    }
    /// Records the resolved Git commit on the build.
    pub(crate) async fn commit(&self, commit: &str) -> Result<(), StoreError> {
        self.store.build_commit(self.id, commit).await
    }
    /// Records a finished job run's exit code.
    pub(crate) async fn exit_code(&self, code: i64) -> Result<(), StoreError> {
        self.store.build_exit_code(self.id, code).await
    }
}

/// A persisted build record that is always given a terminal state.
///
/// Call [`BuildAttempt::finish`] on completion. If the attempt is dropped first
/// (e.g. the preparation future is cancelled) or finishing fails, `Drop` records
/// the outcome in the background.
pub(crate) struct BuildAttempt {
    pub(crate) log: BuildLog,
    completion: Completion,
}

/// How far a build's terminal state has been persisted.
enum Completion {
    /// The build has not finished; dropping it records `Interrupted`.
    Running,
    /// A terminal state was chosen but its write has not succeeded yet.
    Pending(BuildState, Option<String>),
    /// The terminal state is stored; nothing is left to record.
    Persisted,
}
impl Completion {
    /// Returns the terminal state `Drop` still needs to persist, if any.
    fn retry(self) -> Option<(BuildState, Option<String>)> {
        match self {
            Self::Running => Some((BuildState::Interrupted, None)),
            Self::Pending(state, image) => Some((state, image)),
            Self::Persisted => None,
        }
    }
}
impl BuildAttempt {
    /// Creates a running build record for one service of an operation, or a
    /// job run record when `job` is set.
    pub(crate) async fn start(
        store: Arc<Store>,
        application: &ApplicationId,
        operation: &str,
        service: &str,
        source: &Source,
        job: Option<&str>,
    ) -> Result<Self, StoreError> {
        let id = store
            .start_build(application, operation, service, source, job)
            .await?;
        Ok(Self {
            log: BuildLog { store, id },
            completion: Completion::Running,
        })
    }
    /// Persists the terminal state and, on success, the built image ID.
    pub(crate) async fn finish(
        mut self,
        state: BuildState,
        image: Option<&str>,
    ) -> Result<(), StoreError> {
        let image = image.map(str::to_owned);
        self.completion = Completion::Pending(state, image.clone());
        self.log
            .store
            .finish_build(self.log.id, state, image.as_deref())
            .await?;
        self.completion = Completion::Persisted;
        Ok(())
    }
}
impl Drop for BuildAttempt {
    /// Spawns a best-effort write of any unpersisted terminal state. If that also
    /// fails, startup recovery marks the build interrupted.
    fn drop(&mut self) {
        let Some((state, image)) =
            std::mem::replace(&mut self.completion, Completion::Persisted).retry()
        else {
            return;
        };
        let log = self.log.clone();
        tokio::spawn(async move {
            if let Err(error) = log
                .store
                .finish_build(log.id, state, image.as_deref())
                .await
            {
                tracing::error!(build_id=log.id,error=?error,"failed to record build completion; startup recovery will mark it interrupted");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_characters_wait_for_the_next_read() {
        assert_eq!(BuildLog::complete_prefix(b"ok\xe2\x82"), 2);
        assert_eq!(BuildLog::complete_prefix(b"\xffok\xe2\x82"), 3);
        assert_eq!(BuildLog::complete_prefix("ok€".as_bytes()), 5);
    }

    #[test]
    fn completion_retry_preserves_pending_terminal_result() {
        assert_eq!(
            Completion::Pending(BuildState::Succeeded, Some("sha256:image".into())).retry(),
            Some((BuildState::Succeeded, Some("sha256:image".into())))
        );
        assert_eq!(
            Completion::Running.retry(),
            Some((BuildState::Interrupted, None))
        );
        assert_eq!(Completion::Persisted.retry(), None);
    }
}
