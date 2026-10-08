//! Identifies who a connection through the tailnet node came from, so the
//! audit trail can name them and tokens can be bound to them.
use super::Cli;
use anyhow::{Context, Result};
use async_trait::async_trait;
use piqueld_core::tailnet::TailnetPeer;
use serde::Deserialize;
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

/// How long a looked-up identity is reused for its address.
const CACHE_FOR: Duration = Duration::from_mins(1);
/// Lookups run on the request path, so they give up quickly.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
/// Addresses remembered at once; the cache starts over beyond this.
const CACHE_LIMIT: usize = 1024;

/// Resolves the tailnet identity behind a peer address. Routers serving the
/// tailnet node carry one as an `Arc<dyn TailnetLookup>` extension; requests
/// on other listeners have no tailnet identity.
#[async_trait]
pub trait TailnetLookup: Send + Sync {
    /// Who `peer` is, or `None` when it cannot be determined. `fresh` skips
    /// any cache: authorization must not act on an identity that has since
    /// changed, such as a removed tag.
    async fn whois(&self, peer: SocketAddr, fresh: bool) -> Option<TailnetPeer>;
}

/// [`TailnetLookup`] through the node's `tailscaled`, with `tailscale whois`.
///
/// Identities are cached per address for [`CACHE_FOR`] to describe audited
/// requests; failed lookups are not cached. Lookups run one at a time, so a
/// burst of requests spawns one `tailscale` process at a time, and requests
/// that waited reuse the identity just looked up for their address.
pub struct Whois {
    cli: Cli,
    cache: Mutex<HashMap<IpAddr, (Instant, TailnetPeer)>>,
    /// Held while a `tailscale whois` runs.
    querying: Mutex<()>,
}

/// The fields piqueld reads from `tailscale whois --json`.
#[derive(Deserialize)]
struct Response {
    #[serde(rename = "Node")]
    node: Node,
    #[serde(rename = "UserProfile")]
    user: Option<User>,
}

#[derive(Deserialize)]
struct Node {
    /// Fully qualified name, e.g. `laptop.tail1234.ts.net.`.
    #[serde(rename = "Name")]
    name: String,
    /// Absent or null for untagged nodes.
    #[serde(rename = "Tags", default)]
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct User {
    #[serde(rename = "LoginName")]
    login: String,
}

impl Response {
    /// Tagged nodes belong to no user, whatever profile `tailscaled` reports.
    fn into_peer(self) -> TailnetPeer {
        let node = self
            .node
            .name
            .split('.')
            .next()
            .unwrap_or_default()
            .to_owned();
        let tags = self.node.tags.unwrap_or_default();
        let login = self.user.filter(|_| tags.is_empty()).map(|user| user.login);
        TailnetPeer { login, node, tags }
    }
}

impl Whois {
    pub(super) fn new(cli: Cli) -> Self {
        Self {
            cli,
            cache: Mutex::new(HashMap::new()),
            querying: Mutex::new(()),
        }
    }

    /// A cached identity for `address`, if still current.
    async fn cached(&self, address: IpAddr) -> Option<TailnetPeer> {
        let cache = self.cache.lock().await;
        let (at, identity) = cache.get(&address)?;
        (at.elapsed() < CACHE_FOR).then(|| identity.clone())
    }

    /// Asks `tailscaled` who `address` is.
    async fn query(&self, address: IpAddr) -> Result<TailnetPeer> {
        let address = address.to_string();
        let output =
            tokio::time::timeout(LOOKUP_TIMEOUT, self.cli.run(&["whois", "--json", &address]))
                .await
                .context("tailscale whois timed out")??;
        let response: Response =
            serde_json::from_slice(&output).context("invalid tailscale whois output")?;
        Ok(response.into_peer())
    }
}

#[async_trait]
impl TailnetLookup for Whois {
    async fn whois(&self, peer: SocketAddr, fresh: bool) -> Option<TailnetPeer> {
        let address = peer.ip();
        if !fresh && let Some(identity) = self.cached(address).await {
            return Some(identity);
        }
        let Ok(_turn) = tokio::time::timeout(LOOKUP_TIMEOUT, self.querying.lock()).await else {
            tracing::warn!(%address, "tailnet peer lookups are backed up");
            return None;
        };
        if !fresh && let Some(identity) = self.cached(address).await {
            return Some(identity);
        }
        match self.query(address).await {
            Ok(identity) => {
                let mut cache = self.cache.lock().await;
                if cache.len() >= CACHE_LIMIT {
                    cache.clear();
                }
                cache.insert(address, (Instant::now(), identity.clone()));
                Some(identity)
            }
            Err(error) => {
                tracing::warn!(?error, %address, "could not identify tailnet peer");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whois_reads_users_and_treats_tagged_nodes_as_userless() {
        let read = |json: &str| serde_json::from_str::<Response>(json).unwrap().into_peer();
        let laptop = read(
            r#"{"Node":{"Name":"laptop.tail1234.ts.net.","Tags":null},
            "UserProfile":{"LoginName":"alice@example.com"}}"#,
        );
        assert_eq!(laptop.login.as_deref(), Some("alice@example.com"));
        assert_eq!(laptop.node, "laptop");
        let runner = read(
            r#"{"Node":{"Name":"runner.tail1234.ts.net.","Tags":["tag:ci"]},
            "UserProfile":{"LoginName":"tagged-devices"}}"#,
        );
        assert_eq!(runner.login, None);
        assert_eq!(runner.tags, ["tag:ci"]);
    }
}
