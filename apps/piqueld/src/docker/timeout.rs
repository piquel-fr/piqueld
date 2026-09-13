//! Complete Docker operation budgets, including queueing and response streams.

use super::DockerError;
use std::{future::Future, time::Duration};

#[derive(Clone, Copy)]
pub(crate) enum DockerTimeout {
    Request,
    ImageResolution,
}

impl DockerTimeout {
    pub(crate) const fn duration(self) -> Duration {
        match self {
            Self::Request => Duration::from_secs(30),
            Self::ImageResolution => Duration::from_mins(10),
        }
    }

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
