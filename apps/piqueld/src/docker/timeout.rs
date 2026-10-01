//! Complete Docker operation budgets, including queueing and response streams.

use super::DockerError;
use std::{future::Future, time::Duration};

#[derive(Clone, Copy)]
/// Named deadline classes applied to whole Docker operations.
pub(crate) enum DockerTimeout {
    /// Ordinary Engine requests and observations.
    Request,
    /// Image pulls, which may transfer large layers on a cold cache.
    ImageResolution,
}

impl DockerTimeout {
    /// Returns the total budget for this class.
    pub(crate) const fn duration(self) -> Duration {
        match self {
            Self::Request => Duration::from_secs(30),
            Self::ImageResolution => Duration::from_mins(10),
        }
    }

    /// Runs `future` under this budget; an elapsed deadline becomes
    /// [`DockerError::UnavailableSource`] labelled with `operation`.
    pub(crate) async fn run<T>(
        self,
        operation: &'static str,
        future: impl Future<Output = Result<T, DockerError>>,
    ) -> Result<T, DockerError> {
        tokio::time::timeout(self.duration(), future)
            .await
            .map_err(|source| DockerError::unavailable(operation, source))?
    }
}
