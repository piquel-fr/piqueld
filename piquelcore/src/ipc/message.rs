use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub enum Command {
    /// A debug command
    Echo(String),
    /// Get status of the machine
    Status,
    /// Will list all the cloned repositories on the system
    ListRepositories,
    /// Delete repository
    DeleteRepository(String),
}

impl std::fmt::Display for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Echo(msg) => {
                write!(f, "Echo \"{msg}\"")
            }
            Self::Status => {
                write!(f, "Status")
            }
            Self::ListRepositories => {
                write!(f, "List Repositories")
            }
            Self::DeleteRepository(name) => {
                write!(f, "Delete repository {name}")
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Success,
    Message(String),
    Error(String),
    RepositoryList(Vec<String>),
}
