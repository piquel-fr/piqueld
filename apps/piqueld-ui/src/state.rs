//! Small, transport-independent state helpers for the operator dashboard.

use piqueld_core::manifest::{
    ApplicationManifest, ApplicationSpecInput, MetadataInput, ServiceInput, SourceInput,
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::Duration,
};
use zeroize::Zeroize;

/// Maximum number of log lines retained by one live view.
pub const MAX_LOG_LINES: usize = 1_000;

/// Stable API prefix used by browser event streams.
pub const API_PREFIX: &str = "/api/v1";

/// Field-level validation messages grouped by their safe manifest paths.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FieldErrors(BTreeMap<String, Vec<String>>);

impl FieldErrors {
    /// Extracts public validation paths from an API error details value.
    #[must_use]
    pub fn from_details(details: &serde_json::Value) -> Self {
        let mut fields = BTreeMap::new();
        let values = details
            .get("errors")
            .and_then(serde_json::Value::as_array)
            .or_else(|| details.as_array());
        for error in values.into_iter().flatten() {
            let Some(path) = error.get("path").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let message = error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("invalid value");
            fields
                .entry(path.to_owned())
                .or_insert_with(Vec::new)
                .push(message.to_owned());
        }
        Self(fields)
    }

    /// Returns validation messages for an exact field path.
    #[must_use]
    pub fn messages(&self, path: &str) -> &[String] {
        self.0.get(path).map_or(&[], Vec::as_slice)
    }

    /// Returns all messages below a service or collection path.
    #[must_use]
    pub fn under(&self, prefix: &str) -> Vec<(String, String)> {
        self.0
            .iter()
            .filter(|(path, _)| path.starts_with(prefix))
            .flat_map(|(path, messages)| {
                messages
                    .iter()
                    .map(|message| (path.clone(), message.clone()))
            })
            .collect()
    }
}

/// Local form state retained when an optimistic-concurrency write conflicts.
#[derive(Clone, Debug, PartialEq)]
pub struct ConflictState {
    /// Generation currently stored by the daemon.
    pub current_generation: u64,
    /// The complete unsaved form that must remain editable.
    pub local_manifest: ApplicationManifest,
}

/// Bounded reconnect cursor state for a browser event stream.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReconnectState {
    /// Number of consecutive reconnect attempts.
    pub attempts: u8,
    /// Last accepted event cursor.
    pub last_event_id: Option<String>,
}

impl ReconnectState {
    /// Records a disconnect and returns the next bounded backoff in seconds.
    #[must_use]
    pub fn disconnected(&mut self) -> Option<u32> {
        self.attempts = self.attempts.saturating_add(1);
        (self.attempts <= 10).then(|| 1_u32 << self.attempts.min(5))
    }

    /// Resets the backoff after a successful event.
    pub const fn connected(&mut self) {
        self.attempts = 0;
    }

    /// Stores a non-empty event cursor.
    pub fn observed(&mut self, id: Option<String>) {
        if id.is_some() {
            self.last_event_id = id;
        }
    }
}

/// Bounded live-log state that keeps the displayed snapshot stable while paused.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LogBuffer {
    lines: VecDeque<String>,
    paused_snapshot: Option<VecDeque<String>>,
    /// Whether newly received lines are hidden from the current display.
    pub paused: bool,
    /// Whether the event source should remain connected.
    pub follow: bool,
}

impl LogBuffer {
    /// Adds one already-sanitized line and evicts the oldest line at the bound.
    pub fn push(&mut self, line: String) {
        self.lines.push_back(line);
        while self.lines.len() > MAX_LOG_LINES {
            self.lines.pop_front();
        }
    }

    /// Changes pause state, snapshotting the visible lines at the pause edge.
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        self.paused_snapshot = paused.then(|| self.lines.clone());
    }

    /// Returns the currently displayed bounded lines.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.paused_snapshot
            .as_ref()
            .unwrap_or(&self.lines)
            .iter()
            .map(String::as_str)
    }
}

/// Write-only browser secret draft that zeroizes its backing bytes on clear/drop.
#[derive(Default)]
pub struct SecretDraft(Vec<u8>);

impl SecretDraft {
    /// Replaces the draft and clears the previous bytes first.
    pub fn replace(&mut self, value: impl AsRef<[u8]>) {
        self.0.zeroize();
        self.0.extend_from_slice(value.as_ref());
    }

    /// Borrows the current plaintext only at the submission boundary.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }

    /// Clears the plaintext draft.
    pub fn clear(&mut self) {
        self.0.zeroize();
        self.0.clear();
    }
}

impl Drop for SecretDraft {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Creates the smallest valid editor form.
#[must_use]
pub fn blank_manifest() -> ApplicationManifest {
    ApplicationManifest {
        api_version: "piqueld.dev/v1alpha1".into(),
        kind: "Application".into(),
        metadata: MetadataInput {
            name: String::new(),
        },
        spec: ApplicationSpecInput {
            services: vec![blank_service()],
            volumes: Vec::new(),
            routes: Vec::new(),
        },
    }
}

/// Creates a service form with safe defaults and no inherited values.
#[must_use]
pub fn blank_service() -> ServiceInput {
    ServiceInput {
        name: "web".into(),
        source: SourceInput::Image {
            image: String::new(),
        },
        replicas: 1,
        environment: BTreeMap::new(),
        command: Vec::new(),
        arguments: Vec::new(),
        ports: Vec::new(),
        mounts: Vec::new(),
        secrets: Vec::new(),
        healthcheck: None,
        resources: None,
    }
}

/// Switches a service source and clears fields from the previous source kind.
pub fn set_source_kind(service: &mut ServiceInput, git: bool) {
    service.source = if git {
        SourceInput::Git {
            repository: String::new(),
            reference: "main".into(),
            context: ".".into(),
            dockerfile: "Dockerfile".into(),
        }
    } else {
        SourceInput::Image {
            image: String::new(),
        }
    };
}

/// Maximum number of applications requested per API page.
pub const PAGE_LIMIT: u16 = 20;
/// Maximum number of API pages the dashboard will load for one refresh.
pub const MAX_PAGES: usize = 20;
/// Normal delay between successful background refreshes.
pub const POLL_INTERVAL: Duration = Duration::from_secs(15);
/// Maximum delay after repeated failed background refreshes.
pub const MAX_POLL_INTERVAL: Duration = Duration::from_mins(2);

/// Connection state shown in the dashboard header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionState {
    /// The first request has not completed.
    Loading,
    /// The most recent refresh reached the daemon.
    Reachable,
    /// The daemon answered, but the requested resource failed.
    Failed,
    /// The browser could not reach the daemon.
    Unreachable,
}

/// Whether the last successful application view is still current.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataState {
    /// No successful refresh has completed.
    Loading,
    /// A successful refresh returned applications.
    Ready,
    /// A successful refresh returned no applications.
    Empty,
    /// The last successful view is being retained after a refresh error.
    Stale,
}

/// The small view model used to describe refresh and selection transitions.
/// Keeping these transitions pure makes the browser state behavior testable
/// without a DOM or a browser runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewState {
    data: DataState,
    selected_id: Option<String>,
}

impl Default for ViewState {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewState {
    /// Starts before the first successful refresh.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            data: DataState::Loading,
            selected_id: None,
        }
    }

    /// Records a successful application listing, including an empty result.
    pub fn record_success(&mut self, application_count: usize) {
        self.data = if application_count == 0 {
            DataState::Empty
        } else {
            DataState::Ready
        };
    }

    /// Retains a prior successful view when a later refresh fails.
    pub fn record_failure(&mut self) {
        if self.data != DataState::Loading {
            self.data = DataState::Stale;
        }
    }

    /// Selects one application for the detail flow.
    pub fn select(&mut self, application_id: impl Into<String>) {
        self.selected_id = Some(application_id.into());
    }

    /// Clears the detail selection.
    pub fn clear_selection(&mut self) {
        self.selected_id = None;
    }

    /// Returns the current refresh state.
    #[must_use]
    pub const fn data(&self) -> DataState {
        self.data
    }

    /// Returns the currently selected application ID.
    #[must_use]
    pub fn selected_id(&self) -> Option<&str> {
        self.selected_id.as_deref()
    }
}

/// A bounded application health label used by the view layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplicationHealth {
    /// Desired and observed state agree.
    Converged,
    /// The application is reachable but not fully healthy yet.
    Degraded,
    /// The runtime reported a failed application.
    Failed,
    /// The daemon has not reported a terminal health category.
    Pending,
}

impl ApplicationHealth {
    /// Converts the server's stable state string to a bounded UI category.
    #[must_use]
    pub fn from_server_state(value: &str) -> Self {
        match value {
            "ready" | "converged" => Self::Converged,
            "degraded" => Self::Degraded,
            "failed" => Self::Failed,
            _ => Self::Pending,
        }
    }

    /// Returns the accessible label used in status badges.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Converged => "Converged",
            Self::Degraded => "Degraded",
            Self::Failed => "Failed",
            Self::Pending => "Pending",
        }
    }
}

/// Cursor state for a bounded paginated refresh.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PaginationState {
    pages_loaded: usize,
    next_cursor: Option<String>,
    seen_cursors: BTreeSet<String>,
    incomplete: bool,
}

impl PaginationState {
    /// Starts a refresh at the first API page.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the cursor for the next request, if another bounded page is allowed.
    #[must_use]
    pub fn next_cursor(&self) -> Option<&str> {
        (self.pages_loaded < MAX_PAGES)
            .then_some(self.next_cursor.as_deref())
            .flatten()
    }

    /// Records one response page and its opaque continuation cursor.
    pub fn record_page(&mut self, next_cursor: Option<String>) {
        self.pages_loaded = self.pages_loaded.saturating_add(1);
        let has_next = next_cursor.is_some();
        self.next_cursor = next_cursor.filter(|cursor| self.seen_cursors.insert(cursor.clone()));
        if has_next && self.next_cursor.is_none() {
            self.incomplete = true;
        }
        if self.pages_loaded >= MAX_PAGES && self.next_cursor.is_some() {
            self.incomplete = true;
        }
    }

    /// Returns the number of pages accepted by this refresh.
    #[must_use]
    pub const fn pages_loaded(&self) -> usize {
        self.pages_loaded
    }

    /// Returns whether pagination stopped before proving that the list ended.
    #[must_use]
    pub const fn incomplete(&self) -> bool {
        self.incomplete
    }
}

/// Single-flight polling controller with visibility and failure backoff.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PollController {
    in_flight: bool,
    hidden: bool,
    manual_pending: bool,
    failures: u8,
    delay: Duration,
}

impl Default for PollController {
    fn default() -> Self {
        Self::new()
    }
}

impl PollController {
    /// Creates a controller at the normal successful polling interval.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            in_flight: false,
            hidden: false,
            manual_pending: false,
            failures: 0,
            delay: POLL_INTERVAL,
        }
    }

    /// Marks the document visibility used to pause background polling.
    pub const fn set_hidden(&mut self, hidden: bool) {
        self.hidden = hidden;
    }

    /// Requests an immediate refresh, including while the page is hidden.
    pub const fn request_manual_refresh(&mut self) {
        self.manual_pending = true;
    }

    /// Attempts to claim the one in-flight request slot.
    pub const fn begin_request(&mut self) -> bool {
        if self.in_flight || (self.hidden && !self.manual_pending) {
            return false;
        }
        self.in_flight = true;
        self.manual_pending = false;
        true
    }

    /// Marks a successful request and returns to the normal interval.
    pub const fn record_success(&mut self) {
        self.in_flight = false;
        self.failures = 0;
        self.delay = POLL_INTERVAL;
    }

    /// Marks a failed request and exponentially backs off to a fixed bound.
    pub fn record_failure(&mut self) {
        self.in_flight = false;
        self.failures = self.failures.saturating_add(1);
        let multiplier = 1_u32 << self.failures.min(3);
        let seconds = POLL_INTERVAL
            .as_secs()
            .saturating_mul(u64::from(multiplier));
        self.delay = Duration::from_secs(seconds.min(MAX_POLL_INTERVAL.as_secs()));
    }

    /// Returns whether a background request may start now.
    #[must_use]
    pub const fn can_poll(&self) -> bool {
        !self.hidden && !self.in_flight
    }

    /// Returns the delay before the next background attempt.
    #[must_use]
    pub const fn delay(&self) -> Duration {
        self.delay
    }

    /// Returns whether a request is currently in flight.
    #[must_use]
    pub const fn in_flight(&self) -> bool {
        self.in_flight
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_health_covers_terminal_and_pending_states() {
        assert_eq!(
            ApplicationHealth::from_server_state("ready"),
            ApplicationHealth::Converged
        );
        assert_eq!(
            ApplicationHealth::from_server_state("degraded"),
            ApplicationHealth::Degraded
        );
        assert_eq!(
            ApplicationHealth::from_server_state("failed"),
            ApplicationHealth::Failed
        );
        assert_eq!(
            ApplicationHealth::from_server_state("pending"),
            ApplicationHealth::Pending
        );
    }

    #[test]
    fn view_state_covers_loading_empty_selected_and_stale_views() {
        let mut state = ViewState::new();
        assert_eq!(state.data(), DataState::Loading);
        assert_eq!(state.selected_id(), None);

        state.record_success(0);
        assert_eq!(state.data(), DataState::Empty);
        state.record_success(1);
        assert_eq!(state.data(), DataState::Ready);
        state.select("app-notes");
        assert_eq!(state.selected_id(), Some("app-notes"));
        state.record_failure();
        assert_eq!(state.data(), DataState::Stale);
        state.clear_selection();
        assert_eq!(state.selected_id(), None);
    }

    #[test]
    fn pagination_stops_after_the_bounded_page_count() {
        let mut pages = PaginationState::new();
        assert_eq!(pages.next_cursor(), None);
        pages.record_page(Some("v1:next".into()));
        assert_eq!(pages.next_cursor(), Some("v1:next"));
        for _ in 1..MAX_PAGES {
            pages.record_page(Some("v1:next".into()));
        }
        assert_eq!(pages.pages_loaded(), MAX_PAGES);
        assert_eq!(pages.next_cursor(), None);
        assert!(pages.incomplete());
    }

    #[test]
    fn pagination_stops_on_a_repeated_cursor_and_marks_the_list_incomplete() {
        let mut pages = PaginationState::new();
        pages.record_page(Some("same".into()));
        assert_eq!(pages.next_cursor(), Some("same"));
        pages.record_page(Some("same".into()));
        assert_eq!(pages.next_cursor(), None);
        assert!(pages.incomplete());
    }

    #[test]
    fn polling_is_single_flight_and_backed_off() {
        let mut poller = PollController::new();
        assert!(poller.begin_request());
        assert!(!poller.begin_request());
        poller.record_failure();
        assert!(!poller.in_flight());
        assert_eq!(poller.delay(), Duration::from_secs(30));
        poller.record_failure();
        poller.record_failure();
        poller.record_failure();
        assert_eq!(poller.delay(), MAX_POLL_INTERVAL);
        poller.record_success();
        assert_eq!(poller.delay(), POLL_INTERVAL);
    }

    #[test]
    fn hidden_pages_pause_background_work_but_allow_manual_refresh() {
        let mut poller = PollController::new();
        poller.set_hidden(true);
        assert!(!poller.can_poll());
        assert!(!poller.begin_request());
        poller.request_manual_refresh();
        assert!(poller.begin_request());
        poller.record_success();
        poller.set_hidden(false);
        assert!(poller.can_poll());
    }
}
