//! Identifies who a connection through a tailnet node came from: through the
//! daemon's own node, so the audit trail can name them and tokens can be
//! bound to them, and through the apps node, so private routes can pass their
//! identity to backends.
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

/// Asks a node's `tailscaled` who an address is.
#[async_trait]
pub trait WhoisSource: Send + Sync {
    /// The node's whois answer for `address`, the JSON `tailscale whois
    /// --json` prints and `LocalAPI` returns.
    async fn whois_json(&self, address: IpAddr) -> Result<Vec<u8>>;
}

/// The daemon's own node, through `tailscale whois`.
#[async_trait]
impl WhoisSource for Cli {
    async fn whois_json(&self, address: IpAddr) -> Result<Vec<u8>> {
        self.run(&["whois", "--json", &address.to_string()]).await
    }
}

/// [`TailnetLookup`] through a node's `tailscaled`.
///
/// Identities are cached per address for [`CACHE_FOR`]; failed lookups are
/// not cached. Lookups run one at a time, so a burst of requests spawns one
/// `tailscale` process at a time, and requests that waited reuse the identity
/// just looked up for their address.
pub struct Whois<S> {
    source: S,
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
    #[serde(rename = "DisplayName", default)]
    name: String,
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
        let user = self.user.filter(|_| tags.is_empty());
        let name = user
            .as_ref()
            .map(|user| user.name.clone())
            .filter(|name| !name.is_empty());
        TailnetPeer {
            login: user.map(|user| user.login),
            name,
            node,
            tags,
        }
    }
}

impl<S: WhoisSource> Whois<S> {
    /// Looks up identities through `source`.
    pub fn new(source: S) -> Self {
        Self {
            source,
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
        let output = tokio::time::timeout(LOOKUP_TIMEOUT, self.source.whois_json(address))
            .await
            .context("tailscale whois timed out")??;
        let response: Response =
            serde_json::from_slice(&output).context("invalid tailscale whois output")?;
        Ok(response.into_peer())
    }
}

#[async_trait]
impl<S: WhoisSource> TailnetLookup for Whois<S> {
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
            "UserProfile":{"LoginName":"alice@example.com","DisplayName":"Alice Martin"}}"#,
        );
        assert_eq!(laptop.login.as_deref(), Some("alice@example.com"));
        assert_eq!(laptop.name.as_deref(), Some("Alice Martin"));
        assert_eq!(laptop.node, "laptop");
        let runner = read(
            r#"{"Node":{"Name":"runner.tail1234.ts.net.","Tags":["tag:ci"]},
            "UserProfile":{"LoginName":"tagged-devices","DisplayName":"Tagged Devices"}}"#,
        );
        assert_eq!((runner.login, runner.name), (None, None));
        assert_eq!(runner.tags, ["tag:ci"]);
    }
}
