//! Editing services: adding, renaming and listing them, their environment variables, health checks and rollout settings.

use super::{
    ArgumentsArgs, BuildRepository, CommandArgs, CpuArgs, DependsOnArgs, EditFlags, GitBuildArgs,
    HealthCommandArgs, HealthIntervalArgs, HealthPathArgs, HealthPortArgs, HealthRetriesArgs,
    HealthStartPeriodArgs, HealthTimeoutArgs, MemoryArgs, MountCommand, RenameArgs, ReplicasArgs,
    SecretMountCommand, SourceCommand, on_off,
};
use crate::{
    commands::resolve_application,
    error::Result,
    output::{Console, reports::ServiceRow},
};
use clap::{Args, Subcommand};
use piqueld_client::{
    Client, HealthCheck, Rollout, RolloutOrder, Service, Source, Template, Typed,
    edit::{ApplicationEdit, ServiceEdit},
};
use std::collections::BTreeMap;

// Positional `<app> <service>` pair addressed by service-level edits.
#[derive(Debug, Args)]
pub(crate) struct ServiceTarget {
    /// Application name or stable ID.
    pub(super) app: String,
    /// Service name.
    pub(super) service: String,
    #[command(flatten)]
    pub(super) flags: EditFlags,
}
impl ServiceTarget {
    /// The service's field-level `edit`, as the application edit saving it.
    pub(super) fn field(&self, edit: ServiceEdit) -> (&Self, ApplicationEdit) {
        let name = self.service.clone();
        (self, ApplicationEdit::Service { name, edit })
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum ServiceCommand {
    /// Add a service that runs an image, or builds a Git repository with
    /// `--git`.
    ///
    /// The new service runs one replica and has no other settings. Adding a
    /// service whose name is taken fails.
    Add(AddServiceArgs),
    /// Remove a service.
    ///
    /// Its routes, jobs and export are removed with it, and other services
    /// stop depending on it. The volumes it mounts keep their data.
    Remove(ServiceTarget),
    /// Rename a service.
    ///
    /// Its routes, jobs, export and the services depending on it follow the
    /// new name. Applications importing it must import the new name. The next
    /// deployment replaces the running service.
    Rename(RenameArgs),
    /// Set the number of replicas the service runs.
    Replicas(ReplicasArgs),
    /// Set where the service's image comes from: a prebuilt image or a Git
    /// build.
    ///
    /// `image` and `git` replace the source. The other commands change one
    /// setting of the current Git source.
    Source {
        #[command(subcommand)]
        command: SourceCommand,
    },
    /// Set or remove one environment variable of a service.
    Env {
        #[command(subcommand)]
        command: EnvironmentCommand,
    },
    /// Replace the image's entrypoint with the elements after `--`.
    ///
    /// Put the command's options before `--`. Without elements, the service
    /// uses the image's own entrypoint.
    Command(CommandArgs),
    /// Replace the image's default arguments with the elements after `--`.
    ///
    /// Put the command's options before `--`. Without elements, the service
    /// uses the image's own arguments.
    Arguments(ArgumentsArgs),
    /// Set the services that must be healthy before this one rolls out.
    ///
    /// List them after `--`. Without any, the service depends on none. Jobs of
    /// the service wait for them too.
    DependsOn(DependsOnArgs),
    /// Set the order and monitor window of the service's rollouts.
    ///
    /// A flag you omit returns to its default, so running it without flags
    /// restores both defaults.
    Rollout(RolloutArgs),
    /// Choose whether the service receives the daemon's own telemetry.
    ///
    /// With `on`, the service receives the traces, metrics and logs of every
    /// application on a socket mounted at `/run/piqueld/telemetry`, in
    /// environments granted telemetry with `env telemetry`. At most one
    /// service per application can receive it, and it must run one replica.
    Telemetry(ReceivesTelemetryArgs),
    /// Mount a volume into a service, or remove a mount.
    Mount {
        #[command(subcommand)]
        command: MountCommand,
    },
    /// Mount a secret into a service as a file, or remove a mount.
    ///
    /// These commands choose where secrets appear. Set their values, and who
    /// may mount them, with `app secret`.
    Secret {
        #[command(subcommand)]
        command: SecretMountCommand,
    },
    /// Set, change, or remove a service's health check.
    ///
    /// The commands that change one setting need a health check of the
    /// matching kind first.
    Health {
        #[command(subcommand)]
        command: HealthCommand,
    },
    /// Set the service's CPU limit, or remove it with `--clear`.
    Cpu(CpuArgs),
    /// Set the service's memory limit, or remove it with `--clear`.
    Memory(MemoryArgs),
}
#[derive(Debug, Args)]
pub(crate) struct RolloutArgs {
    #[command(flatten)]
    target: ServiceTarget,
    /// `stop-first` stops the old task before starting its replacement, and
    /// `start-first` starts the replacement first. By default, the order is
    /// `stop-first` when the service mounts a volume writable, else
    /// `start-first`.
    #[arg(long, value_name = "ORDER")]
    order: Option<Typed<RolloutOrder>>,
    /// Seconds Docker watches each replacement task for failure, from 1 to
    /// 3600. By default, 30.
    #[arg(long, value_name = "SECONDS")]
    monitor_seconds: Option<Typed<u32>>,
}
#[derive(Debug, Args)]
pub(crate) struct ReceivesTelemetryArgs {
    #[command(flatten)]
    target: ServiceTarget,
    /// `on` to receive telemetry where the environment is granted it, `off`
    /// to never receive it.
    #[arg(value_name = "on|off", action = clap::ArgAction::Set, value_parser = on_off())]
    enabled: bool,
}
#[derive(Debug, Args)]
pub(crate) struct AddServiceArgs {
    #[command(flatten)]
    target: ServiceTarget,
    /// Image to run, such as `nginx:stable`. It may reference variables.
    /// Omit it when you pass `--git`.
    #[arg(required_unless_present = "git", conflicts_with = "git")]
    image: Option<Template>,
    /// Build this Git repository with Docker instead of running an image.
    /// It takes a clone URL, a path on the daemon's host, or `self`: the
    /// manifest's own repository, at the commit the manifest was read from.
    #[arg(long, value_name = "URL|self")]
    git: Option<BuildRepository>,
    // Boxed to keep the CLI command enum small.
    #[command(flatten)]
    build: Box<GitBuildArgs>,
}
#[derive(Debug, Subcommand)]
pub(crate) enum EnvironmentCommand {
    /// Set an environment variable, keeping the service's others.
    Set {
        #[command(flatten)]
        target: ServiceTarget,
        /// Variable name.
        key: String,
        /// Variable value. It may reference variables, such as
        /// `${{ vars.<name> }}`.
        value: Template,
    },
    /// Remove an environment variable.
    ///
    /// Removing one the service doesn't set fails.
    Remove {
        #[command(flatten)]
        target: ServiceTarget,
        /// Variable name.
        key: String,
    },
}
// Application and optional service whose saved declarations a `list` shows.
#[derive(Debug, Args)]
pub(crate) struct ServiceListArgs {
    /// Application name or stable ID.
    app: String,
    /// Service to list. Without it, every service is listed.
    service: Option<String>,
}
impl ServiceListArgs {
    /// Emits the `items` of each selected service, as saved.
    pub(super) async fn emit<T: Clone>(
        &self,
        client: &Client,
        console: &mut Console,
        items: fn(&Service) -> &[T],
    ) -> Result<()>
    where
        Vec<ServiceRow<T>>: crate::output::Report,
    {
        let application = resolve_application(client, &self.app).await?;
        let rows: Vec<_> = application
            .application
            .spec()
            .services
            .iter()
            .filter(|candidate| {
                self.service
                    .as_ref()
                    .is_none_or(|name| candidate.name == *name)
            })
            .flat_map(|candidate| {
                items(candidate).iter().map(|item| ServiceRow {
                    service: candidate.name.clone(),
                    item: item.clone(),
                })
            })
            .collect();
        console.emit(&rows)
    }
}
#[derive(Debug, Subcommand)]
pub(crate) enum HealthCommand {
    /// Replace the health check with an HTTP check.
    ///
    /// The check sends a `GET` request to the port inside the container and
    /// passes on a 2xx or 3xx status. It needs nothing in the image.
    Http {
        #[command(flatten)]
        target: ServiceTarget,
        /// Container port to probe, from 1 to 65535.
        port: Typed<u16>,
        /// Request path, starting with `/`.
        #[arg(long, default_value = "/health")]
        path: Template,
        #[command(flatten)]
        timing: HealthTimingArgs,
    },
    /// Replace the health check with a command, given after `--`.
    ///
    /// The command runs inside the container, without a shell, and passes when
    /// it exits with status 0. Put the command's options before `--`.
    Command {
        #[command(flatten)]
        target: ServiceTarget,
        #[command(flatten)]
        timing: HealthTimingArgs,
        /// Command elements, after `--`. Each one is passed as is, spaces
        /// included.
        #[arg(last = true, required = true, value_name = "COMMAND")]
        elements: Vec<Template>,
    },
    /// Remove the health check.
    Clear(ServiceTarget),
    /// Change the port of an HTTP health check.
    Port(HealthPortArgs),
    /// Change the request path of an HTTP health check.
    Path(HealthPathArgs),
    /// Change the command of a command health check, given after `--`.
    CommandElements(HealthCommandArgs),
    /// Change the seconds between health checks.
    Interval(HealthIntervalArgs),
    /// Change the seconds before a health check counts as failed.
    Timeout(HealthTimeoutArgs),
    /// Change the seconds after the container starts during which failed
    /// checks don't count.
    StartPeriod(HealthStartPeriodArgs),
    /// Change how many consecutive failed checks make the container unhealthy.
    Retries(HealthRetriesArgs),
}
// When a new health check probes and how many failures it tolerates.
#[derive(Debug, Args)]
pub(crate) struct HealthTimingArgs {
    /// Seconds between checks, from 1 to 3600.
    #[arg(long, value_name = "SECONDS", default_value = "10")]
    interval: Typed<u32>,
    /// Seconds before a check counts as failed, from 1 to the interval.
    #[arg(long, value_name = "SECONDS", default_value = "3")]
    check_timeout: Typed<u32>,
    /// Seconds after the container starts during which failed checks don't
    /// count, up to 3600. A passing check makes the container healthy at
    /// once.
    #[arg(long, value_name = "SECONDS", default_value = "0")]
    start_period: Typed<u32>,
    /// Consecutive failed checks that make the container unhealthy, from 1 to
    /// 100.
    #[arg(long, default_value = "3")]
    retries: Typed<u32>,
}

impl AddServiceArgs {
    /// The new service: its image or Git source, one replica, and no
    /// optional settings.
    fn service(&self) -> Result<Service> {
        Ok(Service {
            name: self.target.service.clone(),
            source: self.git.as_ref().map_or_else(
                || {
                    Ok(Source::Image {
                        image: self.image.clone().expect("Clap requires image or Git"),
                    })
                },
                |repository| self.build.source(repository),
            )?,
            replicas: Typed::Literal(1),
            environment: BTreeMap::new(),
            command: Vec::new(),
            arguments: Vec::new(),
            mounts: Vec::new(),
            secrets: Vec::new(),
            files: Vec::new(),
            healthcheck: None,
            resources: None,
            depends_on: Vec::new(),
            rollout: Rollout::default(),
            receives_telemetry: false,
        })
    }
}

impl ServiceCommand {
    /// Saves one service edit. Adding and removing services are whole-application
    /// edits; every other variant becomes a field-level `ServiceEdit` on the target service.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (target, edit) = match self {
            Self::Add(args) => (
                &args.target,
                ApplicationEdit::AddService(Box::new(args.service()?)),
            ),
            Self::Remove(target) => (
                target,
                ApplicationEdit::RemoveService(target.service.clone()),
            ),
            Self::Rename(args) => args.target.field(ServiceEdit::Name(args.value.clone())),
            Self::Replicas(args) => args.target.field(ServiceEdit::Replicas(args.value.clone())),
            Self::Source { command } => command.edit()?,
            Self::Env { command } => command.edit(),
            Self::Command(args) => args.target.field(ServiceEdit::Command(args.value.clone())),
            Self::Arguments(args) => args
                .target
                .field(ServiceEdit::Arguments(args.value.clone())),
            Self::DependsOn(args) => args
                .target
                .field(ServiceEdit::DependsOn(args.value.clone())),
            Self::Rollout(args) => args.target.field(ServiceEdit::Rollout(Rollout {
                order: args.order.clone(),
                monitor_seconds: args.monitor_seconds.clone(),
            })),
            Self::Telemetry(args) => args
                .target
                .field(ServiceEdit::ReceivesTelemetry(args.enabled)),
            Self::Mount { command } => command.edit(),
            Self::Secret { command } => return command.run(client, console).await,
            Self::Health { command } => command.edit(),
            Self::Cpu(args) => args.target.field(ServiceEdit::Cpu(args.value.clone())),
            Self::Memory(args) => args.target.field(ServiceEdit::Memory(args.value.clone())),
        };
        target.flags.save(client, console, &target.app, &edit).await
    }
}
impl EnvironmentCommand {
    /// Maps to a single-entry environment edit; `None` removes the key.
    fn edit(&self) -> (&ServiceTarget, ApplicationEdit) {
        match self {
            Self::Set { target, key, value } => target.field(ServiceEdit::EnvironmentEntry((
                key.clone(),
                Some(value.clone()),
            ))),
            Self::Remove { target, key } => {
                target.field(ServiceEdit::EnvironmentEntry((key.clone(), None)))
            }
        }
    }
}
impl HealthCommand {
    /// Maps to a health check edit: `Http`/`Command` replace the whole check, `Clear`
    /// removes it, and the rest adjust one field of the existing check.
    fn edit(&self) -> (&ServiceTarget, ApplicationEdit) {
        match self {
            Self::Http {
                target,
                port,
                path,
                timing,
            } => target.field(ServiceEdit::Healthcheck(Some(HealthCheck::Http {
                port: port.clone(),
                path: path.clone(),
                interval_seconds: timing.interval.clone(),
                timeout_seconds: timing.check_timeout.clone(),
                start_period_seconds: timing.start_period.clone(),
                retries: timing.retries.clone(),
            }))),
            Self::Command {
                target,
                timing,
                elements,
            } => target.field(ServiceEdit::Healthcheck(Some(HealthCheck::Command {
                command: elements.clone(),
                interval_seconds: timing.interval.clone(),
                timeout_seconds: timing.check_timeout.clone(),
                start_period_seconds: timing.start_period.clone(),
                retries: timing.retries.clone(),
            }))),
            Self::Clear(target) => target.field(ServiceEdit::Healthcheck(None)),
            Self::Port(args) => args
                .target
                .field(ServiceEdit::HealthPort(args.value.clone())),
            Self::Path(args) => args
                .target
                .field(ServiceEdit::HealthPath(args.value.clone())),
            Self::CommandElements(args) => args
                .target
                .field(ServiceEdit::HealthCommand(args.value.clone())),
            Self::Interval(args) => args
                .target
                .field(ServiceEdit::HealthInterval(args.value.clone())),
            Self::Timeout(args) => args
                .target
                .field(ServiceEdit::HealthTimeout(args.value.clone())),
            Self::StartPeriod(args) => args
                .target
                .field(ServiceEdit::HealthStartPeriod(args.value.clone())),
            Self::Retries(args) => args
                .target
                .field(ServiceEdit::HealthRetries(args.value.clone())),
        }
    }
}
