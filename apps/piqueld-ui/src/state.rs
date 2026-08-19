//! Small, transport-independent state helpers for the read-only dashboard.

use std::time::Duration;

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
        self.next_cursor = next_cursor;
    }

    /// Returns the number of pages accepted by this refresh.
    #[must_use]
    pub const fn pages_loaded(&self) -> usize {
        self.pages_loaded
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
