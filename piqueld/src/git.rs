use std::{fs, path::PathBuf};

use gix::bstr::BString;
use log::{info, trace};

use crate::config::ServerConfig;

const PREFIX: &str = "[GIT]";

pub struct Git {
    path: PathBuf,
    repo_path: PathBuf,
}

impl Git {
    pub fn new(config: &ServerConfig) -> Result<Git, Box<dyn std::error::Error>> {
        let mut path = config.data_dir.clone();
        path.push("git");

        let mut repo_path = path.clone();
        repo_path.push("repositories");

        fs::create_dir_all(&path).expect("It don't know why this would fail");
        fs::create_dir_all(&repo_path).expect("It don't know why this would fail");

        Ok(Git { path, repo_path })
    }
    pub fn get_repository(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Repository, Box<dyn std::error::Error>> {
        let mut path = self.repo_path.clone();
        path.push(repo);

        let repository = match gix::open(path) {
            Ok(repository) => {
                trace!("{PREFIX} Found repository {owner}/{repo} on system");
                repository
            }
            Err(_) => {
                info!("{PREFIX} Couldn't load repository {owner}/{repo}. Attempting to clone...");
                self.clone(owner, repo)?
            }
        };

        Ok(Repository { repository })
    }
    fn clone(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<gix::Repository, Box<dyn std::error::Error>> {
        let mut path = self.repo_path.clone();
        path.push(repo);

        let mut prepare_checkout = gix::prepare_clone(Git::make_repo_url(owner, repo)?, path)?
            .fetch_then_checkout(gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)?
            .0;

        let repository = prepare_checkout
            .main_worktree(gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)?
            .0;
        info!("{PREFIX} Successfully cloned {owner}/{repo}");
        Ok(repository)
    }
    fn make_repo_url(owner: &str, repo: &str) -> Result<gix::Url, gix::url::parse::Error> {
        gix::Url::from_parts(
            gix::url::Scheme::Ssh,
            Some("git".into()),
            None,
            Some("github.com".into()),
            None,
            BString::from(format!("{owner}/{repo}")),
            false,
        )
    }
}

pub struct Repository {
    repository: gix::Repository,
}
