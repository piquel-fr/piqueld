use std::collections::HashMap;
use std::{fs, path::PathBuf};

use log::{debug, info, trace};
use serde::{Deserialize, Serialize};

use crate::config::ServerConfig;

mod handle;
pub use handle::GitService;
pub use handle::new_git_service;

mod repository;
pub use repository::RepositoryInfo;

const PREFIX: &str = "[GIT]";

#[derive(Debug, Serialize, Deserialize)]
struct GitServiceImpl {
    path: PathBuf,
    repo_path: PathBuf,
    data_path: PathBuf,

    repositories: HashMap<String, RepositoryInfo>,
}

impl GitServiceImpl {
    fn init(config: &ServerConfig) -> Self {
        let mut path = config.data_dir.clone();
        path.push("git");

        let mut repo_path = path.clone();
        repo_path.push("repositories");

        fs::create_dir_all(&path).expect("It don't know why this would fail");
        fs::create_dir_all(&repo_path).expect("It don't know why this would fail");

        let mut data_path = path.clone();
        data_path.push("git.json");

        if let Ok(data) = fs::read_to_string(&data_path) {
            if let Ok(service) = serde_json::from_str(&data) {
                trace!("{PREFIX} Loaded from {data_path:?}");
                return service;
            }
        }

        debug!("{PREFIX} Failed to load {data_path:?}");
        Self {
            path,
            repo_path,
            data_path,
            repositories: HashMap::new(),
        }
    }
    fn get_repository(&self, owner: &str, repo: &str) -> Option<RepositoryInfo> {
        self.repositories.get(&format!("{owner}/{repo}")).cloned()
    }
    fn clone(&mut self, owner: &str, name: &str) -> piquel::Result<RepositoryInfo> {
        let info = RepositoryInfo::new(owner, name);

        let mut prepare_checkout =
            gix::prepare_clone(info.make_url()?, info.path(self.repo_path.clone()))?
                .fetch_then_checkout(gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)?
                .0;

        let _ = prepare_checkout
            .main_worktree(gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)?
            .0;
        info!("{PREFIX} Successfully cloned {}", info.full_name());

        self.repositories.insert(info.full_name(), info.clone());
        self.write_self()?;
        Ok(info)
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
    fn delete(&mut self, owner: &str, repo: &str) -> piquel::Result<()> {
        let info = match self.get_repository(owner, repo) {
            Some(info) => info,
            None => return Err("Repository {owner}/{repo} does not exist".into()),
        };

        fs::remove_dir_all(info.path(self.repo_path.clone()))?;
        self.repositories.remove(&info.full_name());
        self.write_self()?;
        info!("Deleted repository {}", &info.full_name());
        Ok(())
    }
}
