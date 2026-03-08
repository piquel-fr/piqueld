use tokio::sync::{mpsc, oneshot};

use crate::config::ServerConfig;

use super::{GitService, Repository};

enum GitCommand {
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
