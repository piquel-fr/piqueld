use std::{fs, path::PathBuf};

use gix::bstr::BString;
use log::{info, trace};
use tokio::sync::{mpsc, oneshot};

use crate::config::ServerConfig;

const PREFIX: &str = "[GIT]";

pub enum GitCommand {
    GetRepository {
        owner: String,
        name: String,
        reply: oneshot::Sender<piquel::Result<Repository>>,
    },
    ListRepositories {
        reply: oneshot::Sender<piquel::Result<Vec<Repository>>>,
    },
}

pub struct GitHandle {
    tx: mpsc::Sender<GitCommand>,
}

impl GitHandle {
    pub async fn get_repository(&self, owner: &str, repo: &str) -> piquel::Result<Repository> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(GitCommand::GetRepository {
                owner: owner.to_string(),
                name: repo.to_string(),
                reply,
            })
            .await?;
        rx.await?
    }
    pub async fn list_repositories(&self) -> piquel::Result<Vec<Repository>> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(GitCommand::ListRepositories { reply }).await?;
        rx.await?
    }
}

pub fn new_git_service(config: &ServerConfig) -> GitHandle {
    let (tx, mut rx) = mpsc::channel::<GitCommand>(32);

    let service = GitService::new(&config);

    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                GitCommand::GetRepository { owner, name, reply } => {
                    let result = service.get_repository(&owner, &name);
                    let _ = reply.send(result);
                }
                GitCommand::ListRepositories { reply } => {
                    let result = service.list_repositories();
                    let _ = reply.send(result);
                }
            };
        }
    });

    GitHandle { tx }
}

struct GitService {
    path: PathBuf,
    repo_path: PathBuf,
}

impl GitService {
    fn new(config: &ServerConfig) -> Self {
        let mut path = config.data_dir.clone();
        path.push("git");

        let mut repo_path = path.clone();
        repo_path.push("repositories");

        fs::create_dir_all(&path).expect("It don't know why this would fail");
        fs::create_dir_all(&repo_path).expect("It don't know why this would fail");

        Self { path, repo_path }
    }
    fn get_repository(&self, owner: &str, repo: &str) -> piquel::Result<Repository> {
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
    fn clone(&self, owner: &str, repo: &str) -> piquel::Result<gix::Repository> {
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
    fn list_repositories(&self) -> piquel::Result<Vec<Repository>> {
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
