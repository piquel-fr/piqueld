//! Editing deployment jobs.

use super::{EditFlags, key_value, on_off};
use crate::{
    commands::resolve_application,
    error::{CliError, ErrorKind, Result},
    output::Console,
};
use clap::{Args, Subcommand, builder::TypedValueParser};
use piqueld_client::{
    Client, Job, JobRun, JobVolumeAccess, SecretMount, Template, edit::ApplicationEdit,
};
use std::collections::BTreeMap;

/// Parses `read-only` or `exclusive`, listing them with their help in help
/// and completions.
fn volume_access() -> impl TypedValueParser<Value = JobVolumeAccess> {
    crate::cli::one_of!(JobVolumeAccess::ALL, JobVolumeAccess::parse, |access| {
        match access {
            JobVolumeAccess::ReadOnly => {
                "Mount every volume read-only, so the job runs beside the services."
            }
            JobVolumeAccess::Exclusive => {
                "Keep writable volumes writable, and stop every service that mounts one until the rollout."
            }
        }
    })
}
#[derive(Debug, Args)]
pub(crate) struct JobTarget {
    /// Application name or stable ID.
    app: String,
    /// Job name.
    job: String,
    #[command(flatten)]
    flags: EditFlags,
}
#[derive(Debug, Args)]
pub(crate) struct SetJobArgs {
    #[command(flatten)]
    target: JobTarget,
    /// Service whose image, environment, secrets, mounts and startup
    /// dependencies the job reuses.
    service: String,
    /// Seconds the job may run before it fails the deployment, from 1 to
    /// 86400. By default, the job's current timeout, or 300 for a new job.
    #[arg(long, value_name = "SECONDS")]
    timeout_seconds: Option<u32>,
    /// How the job mounts its service's volumes. By default, the job's current
    /// access, or `read-only` for a new job.
    #[arg(long, value_name = "ACCESS", value_parser = volume_access())]
    volume_access: Option<JobVolumeAccess>,
    /// `on` to stop the job's service before the job runs, once its startup
    /// dependencies are ready, until the rollout; `off` to run it beside the
    /// service. By default, the job's current setting, or `off` for a new job.
    #[arg(long, value_name = "on|off", value_parser = on_off())]
    stop_service: Option<bool>,
    /// Environment variable for the job only, added to its service's. Repeat
    /// it to set several. They replace the job's own variables, which it keeps
    /// when you pass none.
    #[arg(long = "env", value_name = "KEY=VALUE", value_parser = key_value)]
    environment: Vec<(String, String)>,
    /// Remove the job's own environment variables.
    #[arg(long, conflicts_with = "environment")]
    clear_env: bool,
    /// Secret for the job only, in addition to its service's, mounted at
    /// `TARGET`, an absolute path under `/run/secrets/` such as
    /// `/run/secrets/db`. Repeat it to mount several. They replace the job's
    /// own secrets, which it keeps when you pass none.
    #[arg(long = "secret", value_name = "NAME=TARGET", value_parser = key_value)]
    secrets: Vec<(String, String)>,
    /// Remove the job's own secrets.
    #[arg(long, conflicts_with = "secrets")]
    clear_secrets: bool,
    /// Command the job runs, after `--`. It replaces the service's command
    /// and arguments.
    #[arg(last = true, required = true, value_name = "COMMAND")]
    command: Vec<Template>,
}
impl SetJobArgs {
    /// The job's own environment: the `--env` entries, none with
    /// `--clear-env`, or otherwise those of `current`, the job it replaces.
    fn environment(&self, current: Option<&Job>) -> BTreeMap<String, Template> {
        if self.clear_env || !self.environment.is_empty() {
            return self
                .environment
                .iter()
                .map(|(key, value)| (key.clone(), value.as_str().into()))
                .collect();
        }
        current.map_or_default(|job| job.environment.clone())
    }
    /// The job's own secrets: the `--secret` mounts, none with
    /// `--clear-secrets`, or otherwise those of `current`, the job it replaces.
    fn secrets(&self, current: Option<&Job>) -> Vec<SecretMount> {
        if self.clear_secrets || !self.secrets.is_empty() {
            return self
                .secrets
                .iter()
                .map(|(name, target)| SecretMount {
                    name: name.as_str().into(),
                    target: target.clone(),
                })
                .collect();
        }
        current.map_or_default(|job| job.secrets.clone())
    }
}
#[derive(Debug, Args)]
pub(crate) struct MoveJobArgs {
    #[command(flatten)]
    target: JobTarget,
    /// New 1-based position in the run order. A position past the end moves
    /// the job last.
    #[arg(value_parser = clap::value_parser!(u32).range(1..))]
    position: u32,
}
#[derive(Debug, Subcommand)]
pub(crate) enum JobCommand {
    /// Add a job that runs before each rollout, or replace the job with this
    /// name.
    ///
    /// A new job runs after the existing ones, and a replaced job keeps its
    /// position. Jobs run in order, after their service's dependencies are
    /// healthy, and the other services wait until every job succeeds.
    Set(SetJobArgs),
    /// Move a job to another position in the run order.
    Move(MoveJobArgs),
    /// Remove a job.
    Remove(JobTarget),
}
impl JobCommand {
    /// Edits jobs client-side and saves the whole list, like routes: a new job
    /// runs after the existing ones, a replaced job keeps its position, and
    /// moving or removing an unknown job is an input error.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let target = match self {
            Self::Set(args) => &args.target,
            Self::Move(args) => &args.target,
            Self::Remove(target) => target,
        };
        let current = resolve_application(client, &target.app).await?;
        let mut jobs = current.application.to_manifest().spec.jobs;
        let existing = jobs.iter().position(|job| job.name == target.job);
        match (self, existing) {
            (Self::Set(args), existing) => {
                let current = existing.map(|index| &jobs[index]);
                let job = Job {
                    name: target.job.clone(),
                    service: args.service.clone(),
                    command: args.command.clone(),
                    environment: args.environment(current),
                    secrets: args.secrets(current),
                    run: JobRun::BeforeRollout,
                    timeout_seconds: args
                        .timeout_seconds
                        .or_else(|| current.map(|job| job.timeout_seconds))
                        .unwrap_or(Job::DEFAULT_TIMEOUT_SECONDS),
                    volume_access: args
                        .volume_access
                        .or_else(|| current.map(|job| job.volume_access))
                        .unwrap_or_default(),
                    stop_service: args
                        .stop_service
                        .or_else(|| current.map(|job| job.stop_service))
                        .unwrap_or_default(),
                };
                match existing {
                    Some(index) => jobs[index] = job,
                    None => jobs.push(job),
                }
            }
            (Self::Move(args), Some(index)) => {
                let job = jobs.remove(index);
                let position = usize::try_from(args.position).unwrap_or(usize::MAX);
                jobs.insert((position - 1).min(jobs.len()), job);
            }
            (Self::Remove(_), Some(index)) => {
                jobs.remove(index);
            }
            (Self::Move(_) | Self::Remove(_), None) => {
                return Err(CliError::new(
                    ErrorKind::Input,
                    format!("job {:?} was not found", target.job),
                ));
            }
        }
        let edit = ApplicationEdit::Jobs(jobs);
        target
            .flags
            .save_loaded(client, console, &current, &edit)
            .await
    }
}
