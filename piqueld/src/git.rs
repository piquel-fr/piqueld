use std::{fs, io, path::PathBuf};

use gix::bstr::BString;
use log::{info, trace};
use tokio::sync::oneshot;

use crate::config::ServerConfig;

const PREFIX: &str = "[GIT]";

pub enum GitCommand {
    GetRepository {
        owner: String,
        name: String,
        reply: oneshot::Sender<io::Result<Repository>>,
    },
    ListRepositories {
        reply: oneshot::Sender<Result<(), Box<dyn std::error::Error>>>,
    },
}

pub struct GitService {
    path: PathBuf,
    repo_path: PathBuf,
}

impl GitService {
    pub fn new(config: &ServerConfig) -> Self {
        let mut path = config.data_dir.clone();
        path.push("git");

        let mut repo_path = path.clone();
        repo_path.push("repositories");

        fs::create_dir_all(&path).expect("It don't know why this would fail");
        fs::create_dir_all(&repo_path).expect("It don't know why this would fail");

        Self { path, repo_path }
    }
    fn get_repository(
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

        Ok(Repository {
            repository,
            owner: owner.to_string(),
            name: repo.to_string(),
        })
    }
    fn clone(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<gix::Repository, Box<dyn std::error::Error>> {
        let mut path = self.repo_path.clone();
        path.push(repo);

        let mut prepare_checkout = gix::prepare_clone(make_repo_url(owner, repo)?, path)?
            .fetch_then_checkout(gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)?
            .0;

        let repository = prepare_checkout
            .main_worktree(gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)?
            .0;
        info!("{PREFIX} Successfully cloned {owner}/{repo}");
        Ok(repository)
    }
    pub fn list_repositories(&self) -> Result<Vec<Repository>, Box<dyn std::error::Error>> {
        let dir = fs::read_dir(&self.repo_path)?;
        let repos = dir
            .filter_map(Result::ok)
            .filter_map(|entry| match gix::open(entry.path()) {
                Ok(repository) => Some(Repository {
                    repository,
                    name: entry.file_name().to_string_lossy().into(),
                    owner: "TBD".into(),
                }),
                Err(_) => None,
            })
            .collect();

        Ok(repos)
    }
}

pub struct Repository {
    repository: gix::Repository,
    owner: String,
    name: String,
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
