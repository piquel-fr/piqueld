//! Logic of the dashboard shell and its read-only views, kept apart from
//! their rendering in `browser/` so it compiles and is tested natively:
//! relative times, application health summaries and recent deployments,
//! preview limits, notification destinations, and build history.

mod builds;
mod dashboard;
#[cfg(test)]
mod fixtures;
mod format;
mod notifications;

pub use builds::{BuildList, build_arguments};
pub use dashboard::{ApplicationRow, EnvironmentRow, PreviewLoad};
pub use format::Age;
pub use notifications::{DestinationDraft, Receives};
