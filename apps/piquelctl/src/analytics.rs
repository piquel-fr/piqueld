//! `analytics`: deployment outcomes, retries, durations, and failure codes over
//! a time window, as the dashboard's Analytics page shows them.
use crate::{
    environments,
    error::{CliError, ErrorKind, Result},
    output::{Console, reports::report},
    support::now_ms,
};
use clap::Args;
use piqueld_client::{
    Client,
    observability::{DeploymentAnalytics, Elapsed},
};
use std::time::Duration;

// Analytics filters; `///` on fields is user-facing help.
#[derive(Debug, Args)]
pub(crate) struct AnalyticsArgs {
    /// Only deployments of this application's environment, by name or ID;
    /// every readable application's when omitted.
    #[arg(long)]
    app: Option<String>,
    /// The environment of --app, by name or ID; required when it has several.
    #[arg(long, requires = "app")]
    env: Option<String>,
    /// Start this long ago, such as 24h or 7d; defaults to 30 days before the end.
    #[arg(long, value_parser = crate::cli::parse_duration)]
    since: Option<Duration>,
    /// End this long ago; defaults to now.
    #[arg(long, value_parser = crate::cli::parse_duration)]
    until: Option<Duration>,
}

impl AnalyticsArgs {
    /// Prints the analytics, then warns when the window reaches before
    /// retained history.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (since, until) = self.window()?;
        let selected = match &self.app {
            Some(application) => {
                Some(environments::select(client, application, self.env.as_deref()).await?)
            }
            None => None,
        };
        let analytics = client
            .deployment_analytics(
                selected
                    .as_ref()
                    .map(|(_, environment)| environment.id.as_str()),
                since,
                until,
            )
            .await?;
        console.emit(&AnalyticsReport {
            analytics: &analytics,
            environment: selected.map(|(application, environment)| {
                format!(
                    "{} / {}",
                    application.application.metadata().name,
                    environment.name
                )
            }),
        })?;
        if analytics.incomplete {
            let pruned = analytics
                .pruned_through_ms
                .map(|at| format!(", pruned through Unix ms {at}"))
                .unwrap_or_default();
            console.warning(format_args!(
                "The window reaches before retained history (recorded from Unix ms {}{pruned}); counts cover retained applications and records only.",
                analytics.history_started_at_ms
            ))?;
        }
        Ok(())
    }

    /// The window's Unix millisecond bounds; `None` leaves the daemon's
    /// defaults, now and 30 days before the end.
    fn window(&self) -> Result<(Option<i64>, Option<i64>)> {
        if let (Some(since), Some(until)) = (self.since, self.until)
            && since < until
        {
            return Err(CliError::new(
                ErrorKind::Input,
                "--since must reach further back than --until",
            ));
        }
        let now = now_ms();
        let ago = |duration: Duration| {
            now.saturating_sub(i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
                .max(0)
        };
        Ok((self.since.map(ago), self.until.map(ago)))
    }
}

/// `analytics` result; human output also names the environment it covers.
struct AnalyticsReport<'a> {
    analytics: &'a DeploymentAnalytics,
    /// `shop / staging`, when limited to one environment.
    environment: Option<String>,
}
report!(AnalyticsReport<'_> as DeploymentAnalytics = analytics, self, out, {
    let a = self.analytics;
    if let Some(environment) = &self.environment {
        out.label("Environment", environment)?;
    }
    out.label("Window", format_args!("Unix ms {} to {}", a.since_ms, a.until_ms))?;
    out.label(
        "Deployments",
        format_args!(
            "{} ({} succeeded, {} failed by their latest attempt)",
            a.deployments, a.succeeded, a.failed
        ),
    )?;
    out.label(
        "Mean attempt duration",
        a.mean_duration_ms
            .map_or_else(|| "none".to_owned(), |ms| Elapsed::from_millis_f64(ms).to_string()),
    )?;
    out.label(
        "Failed attempts",
        format_args!("{} (including later recoveries)", a.failed_attempts),
    )?;
    out.label("Retry attempts", a.retry_attempts)?;
    out.label("Action retries", a.action_retries)?;
    out.blank()?;
    out.table(
        &a.actions,
        "No completed actions in this window.",
        "PHASE  ACTIONS  MEAN DURATION",
        |out, action| {
            out.line(format_args!(
                "{}  {}  {}",
                action.phase,
                action.count,
                Elapsed::from_millis_f64(action.mean_ms)
            ))
        },
    )?;
    out.blank()?;
    out.table(
        &a.failures,
        "No failures in this window.",
        "CODE  OCCURRENCES",
        |out, failure| out.line(format_args!("{}  {}", failure.code, failure.count)),
    )
});
