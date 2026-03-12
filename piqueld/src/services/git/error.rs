use thiserror::Error;

use crate::services::ChannelError;

pub type Result<T> = std::result::Result<T, GitError>;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("repository '{0}' does not exist")]
    NotFound(String),

    #[error("invalid repository URL for '{0}'")]
    InvalidUrl(String),

    #[error("no repositories found")]
    NoReposFound,

    #[error("failed to create directory '{path}': {source}")]
    CreateDir {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to read state file '{path}': {source}")]
    ReadState {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write state file '{path}': {source}")]
    WriteState {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to serialize state: {0}")]
    Serialize(#[from] serde_json::Error),

    #[error("git clone failed for '{repo}': {source}")]
    CloneFailed {
        repo: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("failed to remove repository directory '{path}': {source}")]
    RemoveDir {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Channel(#[from] ChannelError),
}
