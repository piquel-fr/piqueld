//! Tailnet identity for private routes with `identity = true`. The gateway
//! asks the daemon who each client is before proxying, and copies the answer
//! onto the request as `Piqueld-*` headers:
//!
//! ```text
//! tailnet client -> apps node -PROXY v2-> Caddy private listener
//!   forward-auth: GET /identity, Piqueld-Client: <client tailnet address>
//!     -> <data_dir>/ingress/control/identity.sock (the daemon)
//!        whois through the apps node's LocalAPI, cached per address for a minute
//!     <- 200 Piqueld-User-Login, Piqueld-User-Name, Piqueld-Node, Piqueld-Node-Tags
//!   -> backend, with those headers
//! ```
//!
//! The socket is in the gateway's control mount, so Caddy dials it without a
//! port being opened. Lookups fail closed: when the client cannot be
//! identified or the daemon does not answer, the route answers 503 instead of
//! proxying without identity.
//!
//! The client address comes from the PROXY header, which the private listener
//! accepts only from the apps node, and every listener strips client-supplied
//! `Piqueld-*` headers, so a backend can trust these headers from the gateway.
//! Anything else on the application's networks can still call the backend
//! directly, as with `PIQUELD_INGRESS_PROXIES`.

use super::Ingress;
use crate::tailnet::TailnetLookup;
use anyhow::{Context, Result};
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use piqueld_core::tailnet::TailnetPeer;
use serde_json::{Value, json};
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

/// The daemon's identity socket, relative to the ingress directory; Caddy
/// sees it as `/control/identity.sock`.
const SOCKET: &str = "control/identity.sock";
/// Names the client to identify on forward-auth requests.
const CLIENT_HEADER: &str = "Piqueld-Client";
/// The identity headers backends receive.
const USER_LOGIN: &str = "Piqueld-User-Login";
const USER_NAME: &str = "Piqueld-User-Name";
const NODE: &str = "Piqueld-Node";
const NODE_TAGS: &str = "Piqueld-Node-Tags";
/// Client-supplied headers every listener removes. Some backends (CGI, WSGI,
/// PHP) read `_` as `-`, so underscored spellings are removed too.
pub(super) const STRIPPED_HEADERS: [&str; 2] = ["Piqueld-*", "Piqueld_*"];

impl Ingress {
    /// Caddy handlers serving `handler` with the client's tailnet identity.
    /// A forward-auth request asks the daemon; on 2xx its identity headers
    /// are copied onto the request and `handler` runs, otherwise its answer
    /// is the response. Any error, such as the daemon not listening,
    /// answers 503.
    ///
    /// A proxy drops the headers a request's `Connection` header names, after
    /// they were copied, so such requests naming identity headers are refused
    /// rather than proxied without them.
    ///
    /// ```text
    /// subroute {
    ///   Connection names a Piqueld-* header: 400
    ///   reverse_proxy unix//control/identity.sock (GET /identity)
    ///     2xx: copy Piqueld-User-Login, ... -> handler
    ///   errors: 503
    /// }
    /// ```
    pub(super) fn with_identity(handler: &Value) -> Vec<Value> {
        let copy: Vec<Value> = [USER_LOGIN, USER_NAME, NODE, NODE_TAGS]
            .map(|header| {
                let value = format!("{{http.reverse_proxy.header.{header}}}");
                json!({
                    "match":[{"not":[{"vars":{&value:[""]}}]}],
                    "handle":[{"handler":"headers","request":{"set":{header:[value]}}}]
                })
            })
            .into();
        let forward_auth = json!({
            "handler":"reverse_proxy",
            "upstreams":[{"dial":format!("unix//{SOCKET}")}],
            "rewrite":{"method":"GET","uri":"/identity"},
            "headers":{"request":{"set":{CLIENT_HEADER:["{http.request.remote.host}"]}}},
            "handle_response":[{"match":{"status_code":[2]},"routes":copy}]
        });
        let hop_by_hop = json!({
            "match":[{"header_regexp":{"Connection":{"pattern":"(?i)piqueld"}}}],
            "handle":[{"handler":"static_response","status_code":400,"body":"Identity headers cannot be hop-by-hop."}],
            "terminal":true
        });
        vec![json!({
            "handler":"subroute",
            "routes":[hop_by_hop, {"handle":[forward_auth, handler]}],
            "errors":{"routes":[{"handle":[{
                "handler":"static_response","status_code":503,
                "body":"This route requires a tailnet identity, which is unavailable right now."
            }]}]}
        })]
    }

    /// Answers the gateway's identity requests on the socket until
    /// cancellation, while private ingress is enabled. Binding is retried
    /// every 10 s; until it succeeds, identity routes answer 503.
    pub(super) async fn serve_identity(&self, cancellation: &CancellationToken) {
        let Some(node) = &self.node else {
            return;
        };
        let router = Identify(Arc::clone(&node.whois)).router();
        loop {
            match self.bind_identity().await {
                Ok(listener) => {
                    let shutdown = cancellation.clone();
                    let served = axum::serve(listener, router.clone())
                        .with_graceful_shutdown(async move { shutdown.cancelled().await })
                        .await;
                    if cancellation.is_cancelled() {
                        return;
                    }
                    tracing::warn!(error=?served, "the tailnet identity socket stopped; identity routes answer 503 until it is back");
                }
                Err(error) => {
                    tracing::warn!(
                        ?error,
                        "could not serve the tailnet identity socket; identity routes answer 503 until it is back"
                    );
                }
            }
            tokio::select! {
                () = cancellation.cancelled() => return,
                () = tokio::time::sleep(Duration::from_secs(10)) => {}
            }
        }
    }

    /// Binds the identity socket, replacing one left by an earlier run.
    /// Only the daemon's user, which the gateway runs as, may connect.
    async fn bind_identity(&self) -> Result<tokio::net::UnixListener> {
        use std::os::unix::fs::PermissionsExt;
        let path = self.directory.join(SOCKET);
        let control = path.parent().context("identity socket has no directory")?;
        crate::prepare_data_dir(control)
            .await
            .with_context(|| format!("prepare {}", control.display()))?;
        match tokio::fs::remove_file(&path).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error).with_context(|| format!("remove stale {}", path.display()));
            }
            _ => {}
        }
        let listener = tokio::net::UnixListener::bind(&path)
            .with_context(|| format!("bind {}", path.display()))?;
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .await
            .with_context(|| format!("restrict {}", path.display()))?;
        Ok(listener)
    }
}

/// Answers forward-auth requests with the identity of the client they name.
#[derive(Clone)]
struct Identify(Arc<dyn TailnetLookup>);

impl Identify {
    fn router(self) -> Router {
        Router::new()
            .route("/identity", axum::routing::get(Self::answer))
            .with_state(self)
    }

    /// 200 with the identity headers of the client named by `Piqueld-Client`,
    /// or 503, which Caddy returns to the client, when it cannot be identified.
    async fn answer(State(Self(lookup)): State<Self>, headers: HeaderMap) -> Response {
        let client = headers
            .get(CLIENT_HEADER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<IpAddr>().ok());
        let peer = match client {
            Some(address) => lookup.whois(SocketAddr::new(address, 0), false).await,
            None => None,
        };
        match peer {
            Some(peer) => (StatusCode::OK, Self::headers(&peer)).into_response(),
            None => (
                StatusCode::SERVICE_UNAVAILABLE,
                "This device's tailnet identity could not be determined.",
            )
                .into_response(),
        }
    }

    /// A device's user gets their login and display name; tagged devices
    /// have no user and get their tags instead. Both get the device's name.
    fn headers(peer: &TailnetPeer) -> HeaderMap {
        let tags = (!peer.tags.is_empty()).then(|| peer.tags.join(","));
        [
            (USER_LOGIN, peer.login.as_deref()),
            (USER_NAME, peer.name.as_deref()),
            (NODE, Some(peer.node.as_str())),
            (NODE_TAGS, tags.as_deref()),
        ]
        .into_iter()
        .filter_map(|(name, value)| Some((name.parse().ok()?, Self::encode(value?))))
        .collect()
    }

    /// Printable ASCII as is; anything else, such as an accented display
    /// name, as an RFC 2047 encoded word, as Tailscale's own identity
    /// headers do:
    ///
    /// ```text
    /// Alice Martin -> Alice Martin
    /// Zoë          -> =?utf-8?b?Wm/Dqw==?=
    /// ```
    fn encode(value: &str) -> HeaderValue {
        use base64::Engine;
        if value.bytes().all(|byte| (b' '..=b'~').contains(&byte))
            && let Ok(value) = HeaderValue::from_str(value)
        {
            return value;
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(value);
        HeaderValue::from_str(&format!("=?utf-8?b?{encoded}?=")).expect("base64 is printable ASCII")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn users_get_their_names_and_tagged_devices_their_tags() {
        let header = |headers: &HeaderMap, name: &str| {
            headers
                .get(name)
                .map(|value| value.to_str().unwrap().to_owned())
        };
        let laptop = Identify::headers(&TailnetPeer {
            login: Some("zoe@example.com".into()),
            name: Some("Zoë".into()),
            node: "laptop".into(),
            tags: Vec::new(),
        });
        assert_eq!(
            header(&laptop, USER_LOGIN).as_deref(),
            Some("zoe@example.com")
        );
        assert_eq!(
            header(&laptop, USER_NAME).as_deref(),
            Some("=?utf-8?b?Wm/Dqw==?=")
        );
        assert_eq!(header(&laptop, NODE).as_deref(), Some("laptop"));
        assert_eq!(header(&laptop, NODE_TAGS), None);
        let runner = Identify::headers(&TailnetPeer {
            login: None,
            name: None,
            node: "runner".into(),
            tags: vec!["tag:ci".into(), "tag:prod".into()],
        });
        assert_eq!(header(&runner, USER_LOGIN), None);
        assert_eq!(header(&runner, USER_NAME), None);
        assert_eq!(
            header(&runner, NODE_TAGS).as_deref(),
            Some("tag:ci,tag:prod")
        );
    }
}
