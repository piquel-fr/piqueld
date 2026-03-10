use std::collections::HashMap;
use std::{fs, path::PathBuf};

use log::{debug, info, trace};
use piquelmacros::service;
use serde::{Deserialize, Serialize};

use crate::config::ServerConfig;

pub mod repository;
use repository::RepositoryInfo;

pub mod error;
use error::{GitError, Result};

const PREFIX: &str = "[GIT]";

#[derive(Debug, Serialize, Deserialize)]
struct GitServiceImpl {
    path: PathBuf,
    repo_path: PathBuf,
    data_path: PathBuf,

    repositories: HashMap<String, RepositoryInfo>,
}

impl GitServiceImpl {
    /// Serializes current state to the data file.
    fn write_self(&self) -> Result<()> {
        let data = serde_json::to_string(self)?;
        fs::write(&self.data_path, &data).map_err(|source| GitError::WriteState {
            path: self.data_path.clone(),
            source,
        })?;
        Ok(())
    }
}

#[service(error = GitError)]
impl GitServiceImpl {
    fn init(config: &ServerConfig) -> Result<Self> {
        let mut path = config.data_dir.clone();
        path.push("git");

        let mut repo_path = path.clone();
        repo_path.push("repositories");

        fs::create_dir_all(&path).map_err(|source| GitError::CreateDir {
            path: path.clone(),
            source,
        })?;
        fs::create_dir_all(&repo_path).map_err(|source| GitError::CreateDir {
            path: repo_path.clone(),
            source,
        })?;

        let mut data_path = path.clone();
        data_path.push("git.json");

        if let Ok(data) = fs::read_to_string(&data_path) {
            match serde_json::from_str(&data) {
                Ok(service) => {
                    trace!("{PREFIX} Loaded state from {data_path:?}");
                    return Ok(service);
                }
                Err(e) => {
                    trace!("{PREFIX} Discarding unreadable state file ({e}), starting fresh");
                }
            }
        }

        debug!("{PREFIX} No existing state at {data_path:?}, initialising empty service");
        Ok(Self {
            path,
            repo_path,
            data_path,
            repositories: HashMap::new(),
        })
    }
    fn get_repository(&self, owner: String, repo: String) -> Result<RepositoryInfo> {
        self.repositories
            .get(&format!("{owner}/{repo}"))
            .cloned()
            .ok_or(GitError::NotFound(repo))
    }
    fn clone_repo(&mut self, owner: String, name: String) -> Result<RepositoryInfo> {
        let info = RepositoryInfo::new(owner, name, self.repo_path.clone());
        let full_name = info.full_name();

        let url = info
            .make_url()
            .map_err(|_| GitError::InvalidUrl(full_name.clone()))?;

        let clone_err = |source: Box<dyn std::error::Error + Send + Sync>| GitError::CloneFailed {
            repo: full_name.clone(),
            source,
        };

        let (mut checkout, _) = gix::prepare_clone(url, info.path())
            .map_err(|e| clone_err(Box::new(e)))?
            .fetch_then_checkout(gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)
            .map_err(|e| clone_err(Box::new(e)))?;

        checkout
            .main_worktree(gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)
            .map_err(|e| clone_err(Box::new(e)))?;

        info!("{PREFIX} Successfully cloned {full_name}");
        self.repositories.insert(full_name, info.clone());
        self.write_self()?;
        Ok(info)
    }
    fn list_repositories(&self) -> Result<Vec<RepositoryInfo>> {
        // TODO: return error if no repositories
        Ok(self.repositories.values().cloned().collect())
    }
    fn delete(&mut self, owner: String, repo: String) -> Result<()> {
        let info = self.get_repository(owner, repo)?;

        fs::remove_dir_all(info.path()).map_err(|source| GitError::RemoveDir {
            path: info.path().to_owned(),
            source,
        })?;

        self.repositories.remove(&info.full_name());
        self.write_self()?;
        info!("{PREFIX} Deleted repository {}", info.full_name());
        Ok(())
    }
}
