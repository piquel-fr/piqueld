//! The daemon's own tailnet node. It serves the website over HTTPS with the
//! tailnet-issued certificate, independently of the host's Tailscale daemon.

#[cfg(feature = "tailscale")]
mod node;
#[cfg(feature = "tailscale")]
pub use node::{Node, NodeListener};

#[cfg(not(feature = "tailscale"))]
pub use disabled::Node;

/// Builds without the `tailscale` feature cannot run a node. Configuration
/// validation rejects `tailscale.enabled`, so the node is never constructed.
#[cfg(not(feature = "tailscale"))]
mod disabled {
    use crate::config::DaemonConfig;
    use piqueld_core::api::TailnetStatus;
    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;

    /// Uninhabited stand-in for the tsnet node.
    pub enum Node {}

    impl Node {
        /// Never joins: validated configuration cannot enable the node.
        pub fn join(
            _config: &mut DaemonConfig,
        ) -> impl Future<Output = anyhow::Result<Option<Self>>> {
            std::future::ready(Ok(None))
        }

        /// Unreachable status subscription.
        #[must_use]
        pub fn status(&self) -> watch::Receiver<TailnetStatus> {
            match *self {}
        }

        /// Unreachable node name.
        #[must_use]
        pub fn dns_name(&self) -> &str {
            match *self {}
        }

        /// Unreachable listener.
        #[must_use]
        pub fn listener(self, _cancellation: CancellationToken) -> tokio::net::TcpListener {
            match self {}
        }
    }
}
