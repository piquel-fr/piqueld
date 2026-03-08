use std::collections::HashMap;
use std::io;
use std::{fs, path::PathBuf};

use log::{debug, info};
use serde::{Deserialize, Serialize};

use crate::config::ServerConfig;

mod handle;
pub use handle::GitHandle;
pub use handle::new_git_service;

mod repository;
pub use repository::RepositoryInfo;

const PREFIX: &str = "[GIT]";

#[derive(Debug, Serialize, Deserialize)]
struct GitService {
    #[serde(skip)]
    path: PathBuf,
    #[serde(skip)]
    repo_path: PathBuf,
    #[serde(skip)]
    data_path: PathBuf,

    repositories: HashMap<String, RepositoryInfo>,
}

impl GitService {
    fn init(config: &ServerConfig) -> io::Result<Self> {
        let mut path = config.data_dir.clone();
        path.push("git");

        let mut repo_path = path.clone();
        repo_path.push("repositories");

        fs::create_dir_all(&path).expect("It don't know why this would fail");
        fs::create_dir_all(&repo_path).expect("It don't know why this would fail");

        let mut data_path = path.clone();
        data_path.push("git.json");

        let data = fs::read_to_string(&data_path)?;

        Ok(match serde_json::from_str(&data) {
            Ok(service) => service,
            Err(err) => {
                debug!("{PREFIX} Failed to load {data_path:?}: {err:#}");
                Self {
                    path,
                    repo_path,
                    data_path,
                    repositories: HashMap::new(),
                }
            }
        })
    }
    fn get_repository(&self, (owner, repo): (&str, &str)) -> Option<&RepositoryInfo> {
        self.repositories.get(&format!("{owner}/{repo}"))
    }
    fn clone(&mut self, info: RepositoryInfo) -> piquel::Result<gix::Repository> {
        let mut path = self.repo_path.clone();
        path.push(info.name());

        let mut prepare_checkout = gix::prepare_clone(info.make_url()?, path)?
            .fetch_then_checkout(gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)?
            .0;

        let repository = prepare_checkout
            .main_worktree(gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)?
            .0;
        info!("{PREFIX} Successfully cloned {}", info.full_name());

        self.repositories.insert(info.full_name(), info);
        self.write_self()?;
        Ok(repository)
    }
    fn list_repositories(&self) -> Vec<RepositoryInfo> {
        self.repositories.values().map(Clone::clone).collect()
    }
    /// Will serialize this object to the data file.
    fn write_self(&self) -> piquel::Result<()> {
        let data = serde_json::to_string(&self)?;
        fs::write(&self.data_path, data)?;
        Ok(())
    }
}
