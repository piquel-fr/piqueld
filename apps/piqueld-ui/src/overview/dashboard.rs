//! What the overview and the application directory show: each application's
//! health and latest activity, the newest deployments, and preview limits.
use crate::state::ApplicationHealth;
use piqueld_client::{
    ApplicationSummary, DeploymentView, EnvironmentStatusView, EnvironmentView, PreviewUsage,
};
use std::cmp::Reverse;

/// One application in the dashboard list, with its environments' statuses and
/// deployment history fetched alongside; per-application failures are kept as
/// messages so one broken application does not fail the whole refresh.
#[derive(Clone, Debug)]
pub struct ApplicationRow {
    /// The listed application.
    pub application: ApplicationSummary,
    /// Its environments, in name order.
    pub environments: Vec<EnvironmentRow>,
    /// Deployments of every environment.
    pub deployments: Vec<DeploymentView>,
    /// Why some environment's deployments could not be read.
    pub deployment_error: Option<String>,
}

/// One environment of a listed application with its status, or the reason it
/// could not be read.
#[derive(Clone, Debug)]
pub struct EnvironmentRow {
    /// The listed environment.
    pub environment: EnvironmentView,
    /// Its status, once read.
    pub status: Option<EnvironmentStatusView>,
    /// Why its status could not be read.
    pub status_error: Option<String>,
}

impl EnvironmentRow {
    /// Health from the environment's status, `Stopped` while it is; missing
    /// or failed reads show as pending.
    #[must_use]
    pub fn health(&self) -> ApplicationHealth {
        match (&self.status, &self.status_error) {
            (Some(status), None) => ApplicationHealth::from_server_state(status.state),
            _ => ApplicationHealth::Pending,
        }
        .unless_stopped(self.environment.stopped())
    }
}

impl ApplicationRow {
    /// Health badge for a row: the least healthy of its environments, or not
    /// deployed without any.
    #[must_use]
    pub fn health(&self) -> ApplicationHealth {
        self.environments
            .iter()
            .map(EnvironmentRow::health)
            .max_by_key(|health| health.severity())
            .unwrap_or(ApplicationHealth::NotDeployed)
    }

    /// The directory row's subtitle: a requested deletion, else the first
    /// status message among its environments, else its generation.
    #[must_use]
    pub fn subtitle(&self) -> String {
        if self.application.delete_intent {
            return "Deletion requested".to_owned();
        }
        self.environments
            .iter()
            .find_map(|row| {
                let message = row.status.as_ref()?.message.as_ref()?;
                Some(format!("{}: {message}", row.environment.name))
            })
            .unwrap_or_else(|| format!("Generation {}", self.application.generation))
    }

    /// When its newest deployment was requested, in Unix milliseconds.
    #[must_use]
    pub fn last_deployed_ms(&self) -> Option<i64> {
        self.deployments
            .iter()
            .map(|deployment| deployment.operation.created_at_ms)
            .max()
    }

    /// The `limit` newest deployments across `rows`, newest first, each with
    /// its application. Deployments requested in the same millisecond come
    /// in descending operation ID order, so the list never reshuffles.
    #[must_use]
    pub fn recent_deployments(
        rows: &[Self],
        limit: usize,
    ) -> Vec<(&ApplicationSummary, &DeploymentView)> {
        let mut deployments = rows
            .iter()
            .flat_map(|row| {
                row.deployments
                    .iter()
                    .map(move |deployment| (&row.application, deployment))
            })
            .collect::<Vec<_>>();
        deployments.sort_by_key(|(_, deployment)| {
            Reverse((
                deployment.operation.created_at_ms,
                deployment.operation.id.as_str(),
            ))
        });
        deployments.truncate(limit);
        deployments
    }
}

/// How previews use the daemon's `[previews]` limits, across the whole
/// installation and each application, from least to most constrained.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PreviewLoad {
    /// Every count is below its limit.
    Within,
    /// Some count reached its limit, so new previews there are refused.
    AtLimit,
    /// Some count exceeds its limit, which was lowered after they were
    /// created; they keep running.
    OverLimit,
}

impl PreviewLoad {
    /// The load of `usage`: the worst of its total and per-application counts.
    #[must_use]
    pub fn of(usage: &PreviewUsage) -> Self {
        let limits = &usage.limits;
        std::iter::once((usage.total, limits.max_total))
            .chain(
                usage
                    .applications
                    .iter()
                    .map(|application| (application.previews, limits.max_per_application)),
            )
            .map(|(count, max)| match count.cmp(&max) {
                std::cmp::Ordering::Less => Self::Within,
                std::cmp::Ordering::Equal => Self::AtLimit,
                std::cmp::Ordering::Greater => Self::OverLimit,
            })
            .max()
            .unwrap_or(Self::Within)
    }

    /// The label of its status badge.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Within => "Within limits",
            Self::AtLimit => "At limit",
            Self::OverLimit => "Over limit",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overview::fixtures::{application, environment};
    use piqueld_client::ApplicationState;
    use serde_json::json;

    #[test]
    fn an_application_shows_its_least_healthy_environment() {
        let row = |environments| application("app-shop0001", environments, &[]).health();
        assert_eq!(row(Vec::new()), ApplicationHealth::NotDeployed);
        assert_eq!(
            row(vec![
                environment("production", false, Ok((ApplicationState::Ready, None))),
                environment("staging", false, Ok((ApplicationState::Failed, None))),
                environment("qa", false, Ok((ApplicationState::Degraded, None))),
            ]),
            ApplicationHealth::Failed
        );
        // An unreadable status is pending, never healthy.
        assert_eq!(
            row(vec![
                environment("production", false, Ok((ApplicationState::Ready, None))),
                environment("staging", false, Err("timed out")),
            ]),
            ApplicationHealth::Pending
        );
        // A stopped environment reads as stopped whatever its last status,
        // which ranks above a healthy one but below any problem.
        assert_eq!(
            row(vec![
                environment("production", false, Ok((ApplicationState::Ready, None))),
                environment("staging", true, Ok((ApplicationState::Failed, None))),
            ]),
            ApplicationHealth::Stopped
        );
        assert_eq!(
            row(vec![
                environment("production", false, Ok((ApplicationState::Pending, None))),
                environment("staging", true, Err("timed out")),
            ]),
            ApplicationHealth::Pending
        );
    }

    #[test]
    fn the_subtitle_prefers_deletion_then_the_first_status_message() {
        let mut row = application(
            "app-shop0001",
            vec![
                environment("production", false, Ok((ApplicationState::Ready, None))),
                environment("staging", false, Err("timed out")),
                environment(
                    "qa",
                    false,
                    Ok((ApplicationState::Failed, Some("image pull failed"))),
                ),
                environment(
                    "dev",
                    false,
                    Ok((ApplicationState::Degraded, Some("1 of 2"))),
                ),
            ],
            &[],
        );
        assert_eq!(row.subtitle(), "qa: image pull failed");
        row.application.delete_intent = true;
        assert_eq!(row.subtitle(), "Deletion requested");
        let quiet = application(
            "app-shop0001",
            vec![environment("production", false, Err("timed out"))],
            &[],
        );
        assert_eq!(quiet.subtitle(), "Generation 4");
    }

    #[test]
    fn recent_deployments_are_the_newest_across_applications() {
        let rows = [
            application(
                "app-shop0001",
                Vec::new(),
                &[("op-a1", 10), ("op-a2", 50), ("op-a3", 30)],
            ),
            application("app-notes001", Vec::new(), &[("op-b1", 40), ("op-b2", 50)]),
            application("app-empty001", Vec::new(), &[]),
        ];
        let recent = ApplicationRow::recent_deployments(&rows, 4)
            .into_iter()
            .map(|(application, deployment)| {
                (application.id.as_str(), deployment.operation.id.as_str())
            })
            .collect::<Vec<_>>();
        assert_eq!(
            recent,
            [
                ("app-notes001", "op-b2"),
                ("app-shop0001", "op-a2"),
                ("app-notes001", "op-b1"),
                ("app-shop0001", "op-a3"),
            ]
        );
        assert_eq!(rows[0].last_deployed_ms(), Some(50));
        assert_eq!(rows[2].last_deployed_ms(), None);
    }

    #[test]
    fn previews_report_their_most_constrained_limit() {
        let usage = |total, applications: &[u32]| {
            serde_json::from_value::<PreviewUsage>(json!({
                "limits": { "max_total": 5, "max_per_application": 2 },
                "total": total,
                "applications": applications
                    .iter()
                    .enumerate()
                    .map(|(index, previews)| json!({
                        "id": format!("app-shop000{index}"),
                        "name": format!("shop{index}"),
                        "previews": previews,
                    }))
                    .collect::<Vec<_>>(),
                "cpu_millis": 0,
                "memory_bytes": 0,
            }))
            .expect("usage")
        };
        assert_eq!(PreviewLoad::of(&usage(3, &[1, 1])), PreviewLoad::Within);
        // One application at its own limit refuses its next preview while
        // the installation still has room.
        assert_eq!(PreviewLoad::of(&usage(3, &[2, 1])), PreviewLoad::AtLimit);
        assert_eq!(PreviewLoad::of(&usage(5, &[1])), PreviewLoad::AtLimit);
        // A count over a lowered limit outweighs one at its limit.
        assert_eq!(PreviewLoad::of(&usage(5, &[3, 2])), PreviewLoad::OverLimit);
    }
}
