use crate::{config::ServerConfig, git::GitHandle};

/// This is the mains struct of the daemon.
/// It stores all the state, logic and configuration of the application.
/// An instance of this is created by main and lent to all consumers.
///
/// Consumers are objects that receive external information. They include:
/// - TCP/UDS server listening for piquelctl commands
/// - Webhook listener (WIP)
/// - Cron scheduler (WIP)
struct State {
    config: Box<ServerConfig>,
    git: GitHandle,
}

impl State {
    pub fn config(&self) -> &ServerConfig {
        &self.config
    }
}
