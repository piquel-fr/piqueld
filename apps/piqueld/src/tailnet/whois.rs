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
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

/// How long a looked-up identity is reused for its address.
const CACHE_FOR: Duration = Duration::from_mins(1);
/// Lookups run on the request path, so they give up quickly.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
/// Addresses remembered at once; the cache starts over beyond this.
const CACHE_LIMIT: usize = 1024;

/// A lookup that did not finish in time: lookups of the address were backed
/// up, or `tailscaled` did not answer. The peer is unknown rather than
/// mismatched, so callers report the lookup as unavailable.
#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("tailnet peer lookup timed out")]
pub struct LookupTimedOut;

/// Resolves the tailnet identity behind a peer address. Routers serving the
/// tailnet node or the dashboard socket carry one as an
/// `Arc<dyn TailnetLookup>` extension; requests on other listeners have no
/// tailnet identity.
#[async_trait]
pub trait TailnetLookup: Send + Sync {
    /// Who `peer` is, or `None` when `tailscaled` cannot tell. `fresh` skips
    /// any cache: authorization must not act on an identity that has since
    /// changed, such as a removed tag.
    ///
    /// # Errors
    /// Returns [`LookupTimedOut`] when the lookup did not finish in time.
    async fn whois(
        &self,
        peer: SocketAddr,
        fresh: bool,
    ) -> Result<Option<TailnetPeer>, LookupTimedOut>;
}

/// [`TailnetLookup`] through the node's `tailscaled`, with `tailscale whois`.
///
/// Identities are cached per address for [`CACHE_FOR`] to describe audited
/// requests; failed lookups are not cached. Each address is looked up by one
/// `tailscale` process at a time, and requests that waited for it reuse its
/// answer, fresh ones too when it started after they asked. A current cached
/// answer never waits for a lookup, and lookups of different addresses never
/// wait for each other.
pub struct Whois {
    cli: Cli,
    peers: parking_lot::Mutex<HashMap<IpAddr, Arc<Slot>>>,
}

/// One address's last successful lookup and its single-flight lock.
#[derive(Default)]
struct Slot {
    answer: parking_lot::Mutex<Option<Answer>>,
    /// Held while the address is looked up.
    querying: Mutex<()>,
}

/// One address's last successful lookup.
struct Answer {
    /// When the lookup started: the identity is current as of then.
    asked: Instant,
    peer: TailnetPeer,
}

impl Slot {
    /// The last answer, if current for a request made at `asked`: started
    /// since then when `fresh`, else younger than [`CACHE_FOR`].
    fn current(&self, fresh: bool, asked: Instant) -> Option<TailnetPeer> {
        let answer = self.answer.lock();
        let answer = answer.as_ref()?;
        let current = if fresh {
            answer.asked >= asked
        } else {
            answer.asked.elapsed() < CACHE_FOR
        };
        current.then(|| answer.peer.clone())
    }
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
            peers: parking_lot::Mutex::default(),
        }
    }

    /// Looks peers up through the `tailscaled` listening on `socket`, such as
    /// the apps node's.
    #[must_use]
    pub fn at(socket: std::path::PathBuf) -> Self {
        Self::new(Cli { socket })
    }

    /// The answer slot of `address`. The cache starts over once it holds
    /// [`CACHE_LIMIT`] addresses.
    fn slot(&self, address: IpAddr) -> Arc<Slot> {
        let mut peers = self.peers.lock();
        if peers.len() >= CACHE_LIMIT && !peers.contains_key(&address) {
            peers.clear();
        }
        Arc::clone(peers.entry(address).or_default())
    }

    /// Asks `tailscaled` who `address` is.
    async fn query(&self, address: IpAddr) -> Result<TailnetPeer> {
        let address = address.to_string();
        let output = self.cli.run(&["whois", "--json", &address]).await?;
        let response: Response =
            serde_json::from_slice(&output).context("invalid tailscale whois output")?;
        Ok(response.into_peer())
    }
}

#[async_trait]
impl TailnetLookup for Whois {
    async fn whois(
        &self,
        peer: SocketAddr,
        fresh: bool,
    ) -> Result<Option<TailnetPeer>, LookupTimedOut> {
        let (address, asked) = (peer.ip(), Instant::now());
        let slot = self.slot(address);
        if let Some(peer) = slot.current(fresh, asked) {
            return Ok(Some(peer));
        }
        let Ok(_turn) = tokio::time::timeout(LOOKUP_TIMEOUT, slot.querying.lock()).await else {
            tracing::warn!(%address, "tailnet peer lookups are backed up");
            return Err(LookupTimedOut);
        };
        // The lookup this waited for may have answered.
        if let Some(peer) = slot.current(fresh, asked) {
            return Ok(Some(peer));
        }
        let started = Instant::now();
        match tokio::time::timeout(LOOKUP_TIMEOUT, self.query(address)).await {
            Ok(Ok(peer)) => {
                *slot.answer.lock() = Some(Answer {
                    asked: started,
                    peer: peer.clone(),
                });
                Ok(Some(peer))
            }
            Ok(Err(error)) => {
                tracing::warn!(?error, %address, "could not identify tailnet peer");
                Ok(None)
            }
            Err(_) => {
                tracing::warn!(%address, "tailscale whois timed out");
                Err(LookupTimedOut)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A current cached identity is returned at once, even while a lookup
    /// of the same address is running.
    #[tokio::test]
    async fn cached_identities_never_wait_for_a_lookup() {
        let whois = Whois::at("/nonexistent/tailscaled.sock".into());
        let peer = SocketAddr::from(([100, 64, 0, 1], 40_000));
        let slot = whois.slot(peer.ip());
        let cached = TailnetPeer {
            login: Some("alice@example.com".into()),
            node: "laptop".into(),
            tags: Vec::new(),
        };
        *slot.answer.lock() = Some(Answer {
            asked: Instant::now(),
            peer: cached.clone(),
        });
        let _running = slot.querying.lock().await;
        let found = tokio::time::timeout(Duration::from_millis(100), whois.whois(peer, false))
            .await
            .expect("the cached identity waited for the running lookup");
        assert_eq!(found.unwrap(), Some(cached));
    }

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
