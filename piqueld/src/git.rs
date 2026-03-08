use std::collections::HashMap;
use std::io;
use std::{fs, path::PathBuf};

use log::info;
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

        Ok(Self {
            path,
            repo_path,
            // TODO: load the state from disk
            repositories: HashMap::new(),
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
        Ok(repository)
    }
    fn list_repositories(&self) -> Vec<RepositoryInfo> {
        self.repositories.values().map(Clone::clone).collect()
    }
}
