use std::path::PathBuf;

use crate::config::ServerConfig;

pub struct Git {
    path: PathBuf,
    repo_path: PathBuf,
}

impl Git {
    pub fn new(config: &ServerConfig) -> Result<Git, Box<dyn std::error::Error>> {
        let mut path = config.data_dir.clone();
        path.push("git");

        let mut repo_path = path.clone();
        repo_path.push("repository");

        Ok(Git { path, repo_path })
    }
    pub fn get_repository(&self) -> Result<Repository, Box<dyn std::error::Error>> {
        let repository = gix::open(&self.repo_path)?;

        Ok(Repository { repository })
    }
}

pub struct Repository {
    repository: gix::Repository,
}
