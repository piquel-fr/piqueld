use tokio::sync::{mpsc, oneshot};

use crate::config::ServerConfig;

use super::{GitService, RepositoryInfo};

enum GitCommand {
    GetRepository {
        owner: String,
        name: String,
        reply: oneshot::Sender<Option<RepositoryInfo>>,
    },
    ListRepositories {
        reply: oneshot::Sender<piquel::Result<Vec<RepositoryInfo>>>,
    },
    Clone {
        owner: String,
        name: String,
        reply: oneshot::Sender<piquel::Result<RepositoryInfo>>,
    },
    DeleteRepository {
        owner: String,
        name: String,
        reply: oneshot::Sender<piquel::Result<()>>,
    },
}

pub struct GitHandle {
    tx: mpsc::Sender<GitCommand>,
}

impl GitHandle {
    pub async fn get_repository(&self, owner: &str, repo: &str) -> piquel::Result<RepositoryInfo> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(GitCommand::GetRepository {
                owner: owner.to_string(),
                name: repo.to_string(),
                reply,
            })
            .await?;
        rx.await?
            .ok_or("Repository {owner}/{repo} not found".into())
    }
    pub async fn list_repositories(&self) -> piquel::Result<Vec<RepositoryInfo>> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(GitCommand::ListRepositories { reply }).await?;
        rx.await?
    }
    pub async fn clone(&self, owner: &str, repo: &str) -> piquel::Result<RepositoryInfo> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(GitCommand::Clone {
                owner: owner.into(),
                name: repo.into(),
                reply,
            })
            .await?;
        rx.await?
    }
    pub async fn delete(&self, owner: &str, repo: &str) -> piquel::Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(GitCommand::DeleteRepository {
                owner: owner.to_string(),
                name: repo.to_string(),
                reply,
            })
            .await?;
        rx.await?
    }
}

pub fn new_git_service(config: &ServerConfig) -> GitHandle {
    let (tx, mut rx) = mpsc::channel::<GitCommand>(32);

    // TODO: error
    let mut service = GitService::init(&config).unwrap();

    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                GitCommand::GetRepository { owner, name, reply } => {
                    let result = service.get_repository(&owner, &name);
                    let _ = reply.send(result);
                }
                GitCommand::ListRepositories { reply } => {
                    let result = service.list_repositories();
                    let _ = reply.send(Ok(result));
                }
                GitCommand::Clone { owner, name, reply } => {
                    let result = service.clone(&owner, &name);
                    let _ = reply.send(result);
                }
                GitCommand::DeleteRepository { owner, name, reply } => {
                    let result = service.delete(&owner, &name);
                    let _ = reply.send(result);
                }
            };
        }
    });

    GitHandle { tx }
}
