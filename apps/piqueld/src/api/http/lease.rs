//! Access of connections that outlive their request: event streams and
//! commands. They are authorized when they open; a [`Lease`] then rechecks the
//! caller's access while they last, and ends them when the daemon shuts down.
use super::{
    ApiError, ApiState,
    auth::{FreshTailnet, TailnetSource},
};
use crate::auth::Identity;
use std::time::Duration;
use tokio::{sync::watch, time::Instant};

/// Longest a connection keeps access it lost without the daemon being told,
/// through an expiry or a tailnet peer that no longer matches a token's
/// binding. Revocations and grant changes are rechecked at once.
pub(super) const RECHECK: Duration = Duration::from_secs(30);

/// How long-lived connections are paced. The daemon uses the defaults;
/// tests shorten them to see a connection end without waiting seconds.
#[derive(Clone, Copy, Debug)]
pub struct ConnectionTiming {
    /// How often a lease looks for a due recheck or shutdown.
    pub tick: Duration,
    /// How often a command's client is pinged, which notices one that left
    /// within two intervals even while its input is not read.
    pub ping: Duration,
    /// How long a stopped command gets to wind down, and then to deliver its
    /// final message and close.
    pub close: Duration,
}

impl Default for ConnectionTiming {
    fn default() -> Self {
        Self {
            tick: Duration::from_secs(1),
            ping: Duration::from_secs(5),
            close: Duration::from_secs(5),
        }
    }
}

/// Why a long-lived connection ended early.
pub(super) enum Ended {
    /// The daemon is shutting down; clients reconnect once it is back.
    Shutdown,
    /// The caller lost access: its credential was revoked or expired, its
    /// grants changed, or its tailnet peer no longer matches its binding.
    Refused(ApiError),
}

/// The authorization of one long-lived connection.
pub(super) struct Lease {
    state: ApiState,
    identity: Identity,
    /// Where the connection came from, to recheck a token's tailnet binding.
    source: Option<TailnetSource>,
    /// Marked changed by every change to accounts, credentials, or grants.
    changes: watch::Receiver<()>,
    /// When access is rechecked even without a change.
    next: Instant,
}

impl Lease {
    /// Starts the lease of a connection `identity` was authorized for. Its
    /// first recheck is due at once, so a change made since `identity` was
    /// authenticated, before this subscribed to changes, is not missed.
    pub(super) fn new(state: ApiState, identity: Identity, source: Option<TailnetSource>) -> Self {
        Self {
            changes: state.access_changes(),
            state,
            identity,
            source,
            next: Instant::now(),
        }
    }

    /// Whether [`Self::recheck`] must run: the daemon is shutting down,
    /// access changed, or [`RECHECK`] passed since the last check.
    pub(super) fn due(&self) -> bool {
        self.state.shutdown().is_cancelled()
            || self.changes.has_changed().unwrap_or(false)
            || Instant::now() >= self.next
    }

    /// Waits until `ready` resolves or a recheck is due: the daemon shuts
    /// down, access changes, or [`RECHECK`] passes.
    pub(super) async fn wait(&self, ready: impl Future) {
        // A clone, so the change stays unseen for `due`.
        let mut changes = self.changes.clone();
        tokio::select! {
            _ = ready => {}
            () = self.state.shutdown().cancelled() => {}
            _ = changes.changed() => {}
            () = tokio::time::sleep_until(self.next) => {}
        }
    }

    /// Rechecks the caller's access now: its tailnet binding, then `check`,
    /// which must re-read the caller's access through the store, refusing an
    /// ended credential.
    pub(super) async fn recheck<T, F: Future<Output = Result<T, ApiError>>>(
        &mut self,
        check: impl FnOnce(ApiState, Identity) -> F,
    ) -> Result<T, Ended> {
        if self.state.shutdown().is_cancelled() {
            return Err(Ended::Shutdown);
        }
        // Changes committed before this are seen by the check below.
        self.changes.borrow_and_update();
        self.next = Instant::now() + RECHECK;
        if let Some(fresh) = FreshTailnet::check(&self.identity, self.source.as_ref()).await {
            fresh.verify(&self.identity).map_err(Ended::Refused)?;
        }
        check(self.state.clone(), self.identity.clone())
            .await
            .map_err(Ended::Refused)
    }

    /// Resolves once `check` refuses the caller or the daemon shuts down,
    /// rechecking whenever [`Self::due`].
    pub(super) async fn ended<F: Future<Output = Result<(), ApiError>>>(
        mut self,
        mut check: impl FnMut(ApiState, Identity) -> F,
    ) -> Ended {
        loop {
            tokio::time::sleep(self.state.connection_timing().tick).await;
            if self.due()
                && let Err(ended) = self.recheck(&mut check).await
            {
                return ended;
            }
        }
    }
}
