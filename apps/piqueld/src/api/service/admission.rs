//! Admission of expensive API work: each kind runs a bounded number of times
//! at once across every listener, and more is refused at once rather than
//! queued.
use super::{ApplicationError, ApplicationService};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Kinds of expensive work, each with its own budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expensive {
    /// Deployment analytics, aggregated over a time window.
    Analytics,
    /// Recent container output, read from Docker.
    Logs,
    /// Commands run in a container, for as long as they run.
    Exec,
    /// Event streams, for as long as they stay open.
    EventStream,
    /// Installation checks, which probe the network and Docker.
    Checks,
}
impl Expensive {
    /// How many of this kind may run at once.
    const fn limit(self) -> usize {
        match self {
            Self::Analytics => 4,
            Self::Logs => 8,
            Self::Exec => 32,
            Self::EventStream => 128,
            Self::Checks => 2,
        }
    }

    /// A fresh budget for this kind.
    fn budget(self) -> Arc<Semaphore> {
        Arc::new(Semaphore::new(self.limit()))
    }
}

/// One budget per [`Expensive`] kind, shared by clones of the service.
pub(super) struct Admission {
    analytics: Arc<Semaphore>,
    logs: Arc<Semaphore>,
    exec: Arc<Semaphore>,
    event_streams: Arc<Semaphore>,
    checks: Arc<Semaphore>,
}

impl Default for Admission {
    fn default() -> Self {
        Self {
            analytics: Expensive::Analytics.budget(),
            logs: Expensive::Logs.budget(),
            exec: Expensive::Exec.budget(),
            event_streams: Expensive::EventStream.budget(),
            checks: Expensive::Checks.budget(),
        }
    }
}

impl Admission {
    /// The budget of `kind`.
    fn of(&self, kind: Expensive) -> &Arc<Semaphore> {
        match kind {
            Expensive::Analytics => &self.analytics,
            Expensive::Logs => &self.logs,
            Expensive::Exec => &self.exec,
            Expensive::EventStream => &self.event_streams,
            Expensive::Checks => &self.checks,
        }
    }
}

impl ApplicationService {
    /// Admits one `kind` of expensive work, for as long as the returned
    /// permit is held.
    ///
    /// # Errors
    /// Returns [`ApplicationError::Busy`] when as many already run as `kind`
    /// allows.
    pub fn admit(&self, kind: Expensive) -> Result<OwnedSemaphorePermit, ApplicationError> {
        Arc::clone(self.admission.of(kind))
            .try_acquire_owned()
            .map_err(|_| ApplicationError::Busy(kind))
    }
}
