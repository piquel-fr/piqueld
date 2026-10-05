//! Field editing commands share confirmation, revision protection, and deployment output.
use crate::{
    cli::{Cli, CreateArgs, DeploymentArgs},
    commands::{resolve_application, wait_for_operation},
    error::{CliError, ErrorKind, Result},
    output::{Console, reports::SavedDeploymentReport},
    support::{confirm, retry_transport},
};
use clap::{Args, Subcommand};
use piqueld_client::{
    ApplicationView, Build, Client, GitRepository, HealthCheck, Job, JobRun, Mount, Redirect,
    RedirectStatus, RepositoryManifest, Rollout, RolloutOrder, Route, SavedApplication, Service,
    Source, SourceRepository, Volume,
    edit::{ApplicationEdit, EditOptions, ServiceEdit},
};

// Flags shared by every edit: deployment, generation precondition, and confirmation.
// Flattened `Args` structs use `//`: their doc comments would override the `about`
// text of commands that flatten them. Field `///` comments are user-facing help.
#[derive(Debug, Args)]
pub(crate) struct EditFlags {
    #[command(flatten)]
    deployment: DeploymentArgs,
    /// Require the inspected saved generation.
    #[arg(long)]
    expected_generation: Option<u64>,
    /// Override the saved generation precondition.
    #[arg(long, conflicts_with = "expected_generation")]
    force: bool,
    /// Skip interactive confirmation.
    #[arg(long)]
    yes: bool,
}
// Positional `<app> <service>` pair addressed by service-level edits.
#[derive(Debug, Args)]
pub(crate) struct Target {
    /// Application name or stable ID.
    app: String,
    /// Logical service name.
    service: String,
    #[command(flatten)]
    flags: EditFlags,
}
// Declares service-edit argument structs taking one required positional value.
macro_rules! service_value_args {
    ($($name:ident: $ty:ty;)*) => {$ (
        #[derive(Debug, Args)]
        pub(crate) struct $name {
            #[command(flatten)]
            target: Target,
            /// New setting value.
            value: $ty,
        }
    )*};
}
service_value_args! {
    TextArgs: String;
    ReplicasArgs: u16;
    SecondsArgs: u32;
}
#[derive(Debug, Args)]
pub(crate) struct StringsArgs {
    #[command(flatten)]
    target: Target,
    /// Elements after --, preserving spaces. Omit to clear the array.
    #[arg(last = true)]
    value: Vec<String>,
}
// Declares service-edit argument structs whose value is either given or `--clear`ed.
macro_rules! optional_args {
    ($($name:ident: $ty:ty;)*) => {$ (
        #[derive(Debug, Args)]
        pub(crate) struct $name {
            #[command(flatten)]
            target: Target,
            /// New value, or use --clear to remove the setting.
            #[arg(required_unless_present = "clear", conflicts_with = "clear")]
            value: Option<$ty>,
            /// Remove the setting.
            #[arg(long)]
            clear: bool,
        }
    )*};
}
optional_args! { CommitArgs: String; CpuArgs: u32; MemoryArgs: u64; }

#[derive(Debug, Subcommand)]
pub(crate) enum ServiceCommand {
    /// Add a service using an image or --git URL.
    Add(AddServiceArgs),
    /// Remove a service from saved configuration.
    Remove(Target),
    /// Rename a service declaration; deploy replaces its runtime service.
    Rename(TextArgs),
    /// Set desired replicas.
    Replicas(ReplicasArgs),
    /// Switch sources or edit individual Git/build settings.
    Source {
        #[command(subcommand)]
        command: SourceCommand,
    },
    /// Set or remove individual environment variables.
    Env {
        #[command(subcommand)]
        command: EnvironmentCommand,
    },
    /// Set entrypoint elements after --; omit elements to clear.
    Command(StringsArgs),
    /// Set argument elements after --; omit elements to clear.
    Arguments(StringsArgs),
    /// Set services that must be healthy before this one rolls out, after --;
    /// omit services to clear.
    DependsOn(StringsArgs),
    /// Replace the rollout order and monitor window; omitted flags use their defaults.
    Rollout(RolloutArgs),
    /// Add, replace, or remove a volume mount by target path.
    Mount {
        #[command(subcommand)]
        command: MountCommand,
    },
    /// Configure or edit a health check.
    Health {
        #[command(subcommand)]
        command: HealthCommand,
    },
    /// Set CPU millicores, or --clear.
    Cpu(CpuArgs),
    /// Set memory bytes, or --clear.
    Memory(MemoryArgs),
}
#[derive(Debug, Subcommand)]
pub(crate) enum SourceCommand {
    /// Switch to a prebuilt image.
    Image(TextArgs),
    /// Switch to a Git/Docker build source.
    Git(GitSourceArgs),
    /// Change the Git clone URL.
    Url(TextArgs),
    /// Change the Git branch.
    Branch(TextArgs),
    /// Pin a commit, or --clear to follow the branch.
    Commit(CommitArgs),
    /// Change the Dockerfile path.
    Dockerfile(TextArgs),
    /// Change the build context path.
    Context(TextArgs),
}
#[derive(Debug, Args)]
pub(crate) struct RolloutArgs {
    #[command(flatten)]
    target: Target,
    /// stop-first or start-first. Omit to stop first only when a volume is mounted writable.
    #[arg(long)]
    order: Option<RolloutOrder>,
    /// Seconds to watch each replacement task for failure. Omit for the 30-second default.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=i64::from(Rollout::MAX_MONITOR_SECONDS)))]
    monitor_seconds: Option<u32>,
}
#[derive(Debug, Args)]
pub(crate) struct AddServiceArgs {
    #[command(flatten)]
    target: Target,
    /// Container image reference, or select --git instead.
    #[arg(required_unless_present = "git", conflicts_with = "git")]
    image: Option<String>,
    /// Build this Git repository with Docker.
    #[arg(long)]
    git: Option<String>,
    #[command(flatten)]
    build: GitBuildArgs,
}
#[derive(Debug, Args)]
pub(crate) struct GitSourceArgs {
    #[command(flatten)]
    target: Target,
    /// Git clone URL or host path.
    #[arg(id = "git")]
    url: String,
    #[command(flatten)]
    build: GitBuildArgs,
}
// Git build settings; each flag requires the `git` argument (`--git` or the
// positional URL of `source git`).
#[derive(Debug, Args)]
pub(crate) struct GitBuildArgs {
    /// Branch to fetch when no commit is pinned.
    #[arg(long, requires = "git", default_value = "main")]
    branch: String,
    /// Pin a full commit hash instead of following the branch.
    #[arg(long, requires = "git")]
    commit: Option<String>,
    /// Dockerfile path relative to the repository root.
    #[arg(long, requires = "git", default_value = "Dockerfile")]
    dockerfile: String,
    /// Build context relative to the repository root.
    #[arg(long, requires = "git", default_value = ".")]
    context: String,
}
impl GitBuildArgs {
    /// Git source that builds `url` with Docker using these settings.
    fn source(&self, url: &str) -> Source {
        Source::Git {
            repository: SourceRepository::Git(GitRepository {
                url: url.into(),
                branch: self.branch.clone(),
                commit: self.commit.clone(),
            }),
            build: Build::Docker {
                dockerfile: self.dockerfile.clone(),
                context: self.context.clone(),
            },
        }
    }
}
#[derive(Debug, Subcommand)]
pub(crate) enum EnvironmentCommand {
    /// Set one variable without changing other entries.
    Set {
        #[command(flatten)]
        target: Target,
        /// Variable name.
        key: String,
        /// Variable value.
        value: String,
    },
    /// Remove one variable.
    Remove {
        #[command(flatten)]
        target: Target,
        /// Variable name.
        key: String,
    },
}
#[derive(Debug, Subcommand)]
pub(crate) enum MountCommand {
    /// Add or replace the mount at a container target path.
    Set {
        #[command(flatten)]
        target: Target,
        /// Declared named volume.
        volume: String,
        /// Container target path.
        path: String,
        /// Mount the volume read-only.
        #[arg(long)]
        read_only: bool,
    },
    /// Remove the mount at a container target path.
    Remove(TextArgs),
}
#[derive(Debug, Subcommand)]
pub(crate) enum HealthCommand {
    /// Configure an HTTP health check.
    Http {
        #[command(flatten)]
        target: Target,
        /// Container port to probe.
        port: u16,
        /// HTTP request path.
        #[arg(long, default_value = "/health")]
        path: String,
        /// Seconds between checks.
        #[arg(long, default_value_t = 10)]
        interval: u32,
        /// Seconds before a check counts as failed.
        #[arg(long, default_value_t = 3)]
        check_timeout: u32,
    },
    /// Configure a command health check; command elements follow --.
    Command {
        #[command(flatten)]
        target: Target,
        /// Seconds between checks.
        #[arg(long, default_value_t = 10)]
        interval: u32,
        /// Seconds before a check counts as failed.
        #[arg(long, default_value_t = 3)]
        check_timeout: u32,
        /// Command elements after --, preserving spaces.
        #[arg(last = true, required = true)]
        elements: Vec<String>,
    },
    /// Remove the configured health check.
    Clear(Target),
    /// Set the HTTP port.
    Port(ReplicasArgs),
    /// Set the HTTP path.
    Path(TextArgs),
    /// Set command elements for an existing command health check.
    CommandElements(StringsArgs),
    /// Set the check interval in seconds.
    Interval(SecondsArgs),
    /// Set the check timeout in seconds.
    Timeout(SecondsArgs),
}
#[derive(Debug, Args)]
pub(crate) struct VolumeArgs {
    /// Application name or stable ID.
    app: String,
    /// Named volume.
    volume: String,
    #[command(flatten)]
    flags: EditFlags,
}
#[derive(Debug, Subcommand)]
pub(crate) enum VolumeCommand {
    /// Declare a named volume. No Docker volume is created until deployment.
    Add(VolumeArgs),
    /// Remove a declaration; mounted volumes must first be unmounted. Data is retained.
    Remove(VolumeArgs),
}
#[derive(Debug, Args)]
pub(crate) struct RouteTarget {
    /// Application name or stable ID.
    app: String,
    /// Exact public DNS hostname.
    hostname: String,
    #[command(flatten)]
    flags: EditFlags,
}
#[derive(Debug, Args)]
pub(crate) struct AddRouteArgs {
    #[command(flatten)]
    target: RouteTarget,
    /// Backend service name.
    service: String,
    /// Internal HTTP port.
    port: u16,
}
#[derive(Debug, Args)]
pub(crate) struct RedirectRouteArgs {
    #[command(flatten)]
    target: RouteTarget,
    /// Absolute http(s) destination URL.
    to: String,
    /// HTTP status: 301, 302, 303, 307, or 308.
    #[arg(long, default_value_t = RedirectStatus::PermanentRedirect.into())]
    status: u16,
    /// Drop the request path and query instead of appending them to the destination.
    #[arg(long)]
    no_preserve_path: bool,
}
#[derive(Debug, Subcommand)]
pub(crate) enum RouteCommand {
    /// Add an HTTPS route to an application.
    Add(AddRouteArgs),
    /// Add an HTTPS route the gateway answers with a redirect.
    Redirect(RedirectRouteArgs),
    /// Remove an HTTPS route by hostname.
    Remove(RouteTarget),
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
    /// Service whose image, environment, secrets, mounts, and startup dependencies the job reuses.
    service: String,
    /// Seconds before the job fails the deployment. Defaults to the job's
    /// current timeout, or 300 for a new job.
    #[arg(long)]
    timeout_seconds: Option<u32>,
    /// Command elements after --, replacing the service's command and arguments.
    #[arg(last = true, required = true)]
    command: Vec<String>,
}
#[derive(Debug, Args)]
pub(crate) struct MoveJobArgs {
    #[command(flatten)]
    target: JobTarget,
    /// 1-based position in the run order; past the end moves the job last.
    #[arg(value_parser = clap::value_parser!(u32).range(1..))]
    position: u32,
}
#[derive(Debug, Subcommand)]
pub(crate) enum JobCommand {
    /// Add a job that runs before rollout, or replace the job with this name in place.
    Set(SetJobArgs),
    /// Move a job to another position in the run order.
    Move(MoveJobArgs),
    /// Remove a job by name.
    Remove(JobTarget),
}
#[derive(Debug, Args)]
pub(crate) struct RepositoryTarget {
    /// Application name or stable ID.
    app: String,
    #[command(flatten)]
    flags: EditFlags,
}
#[derive(Debug, Args)]
pub(crate) struct RepositoryText {
    #[command(flatten)]
    target: RepositoryTarget,
    /// New setting value.
    value: String,
}
#[derive(Debug, Subcommand)]
pub(crate) enum RepositoryCommand {
    /// Enable repository ownership of services and volumes.
    Connect {
        #[command(flatten)]
        target: RepositoryTarget,
        /// Git clone URL or host path.
        url: String,
        /// Manifest file path relative to the repository root.
        path: String,
        /// Branch to fetch when no commit is pinned.
        #[arg(long, default_value = "main")]
        branch: String,
        /// Pin a full commit hash instead of following the branch.
        #[arg(long)]
        commit: Option<String>,
    },
    /// Stop fetching from Git and retain saved configuration for local editing.
    Disconnect(RepositoryTarget),
    /// Change the repository URL.
    Url(RepositoryText),
    /// Change the branch.
    Branch(RepositoryText),
    /// Pin a commit, or --clear to follow the branch.
    Commit {
        #[command(flatten)]
        target: RepositoryTarget,
        /// Full commit hash, or use --clear to follow the branch.
        #[arg(required_unless_present = "clear", conflicts_with = "clear")]
        value: Option<String>,
        /// Follow the branch instead of a pinned commit.
        #[arg(long)]
        clear: bool,
    },
    /// Change the manifest file path.
    Path(RepositoryText),
}

impl ServiceCommand {
    /// Saves one service edit. Adding and removing services are whole-application
    /// edits; every other variant becomes a field-level `ServiceEdit` on the target service.
    /// New services start with one replica and no optional settings.
    pub(crate) async fn run(
        &self,
        cli: &Cli,
        client: &Client,
        console: &mut Console,
    ) -> Result<()> {
        let (target, edit) = match self {
            Self::Add(args) => {
                let service = Service {
                    name: args.target.service.clone(),
                    source: args.git.as_ref().map_or_else(
                        || Source::Image {
                            image: args.image.clone().expect("Clap requires image or Git"),
                        },
                        |url| args.build.source(url),
                    ),
                    replicas: 1,
                    environment: std::collections::BTreeMap::new(),
                    command: Vec::new(),
                    arguments: Vec::new(),
                    mounts: Vec::new(),
                    secrets: Vec::new(),
                    healthcheck: None,
                    resources: None,
                    depends_on: Vec::new(),
                    rollout: Rollout::default(),
                };
                return save(
                    cli,
                    client,
                    console,
                    &args.target.app,
                    &args.target.flags,
                    &ApplicationEdit::AddService(Box::new(service)),
                )
                .await;
            }
            Self::Remove(target) => {
                return save(
                    cli,
                    client,
                    console,
                    &target.app,
                    &target.flags,
                    &ApplicationEdit::RemoveService(target.service.clone()),
                )
                .await;
            }
            Self::Rename(args) => (&args.target, ServiceEdit::Name(args.value.clone())),
            Self::Replicas(args) => (&args.target, ServiceEdit::Replicas(args.value)),
            Self::Source { command } => command.edit(),
            Self::Env { command } => command.edit(),
            Self::Command(args) => (&args.target, ServiceEdit::Command(args.value.clone())),
            Self::Arguments(args) => (&args.target, ServiceEdit::Arguments(args.value.clone())),
            Self::DependsOn(args) => (&args.target, ServiceEdit::DependsOn(args.value.clone())),
            Self::Rollout(args) => (
                &args.target,
                ServiceEdit::Rollout(Rollout {
                    order: args.order,
                    monitor_seconds: args.monitor_seconds,
                }),
            ),
            Self::Mount { command } => command.edit(),
            Self::Health { command } => command.edit(),
            Self::Cpu(args) => (&args.target, ServiceEdit::Cpu(args.value)),
            Self::Memory(args) => (&args.target, ServiceEdit::Memory(args.value)),
        };
        save(
            cli,
            client,
            console,
            &target.app,
            &target.flags,
            &ApplicationEdit::Service {
                name: target.service.clone(),
                edit,
            },
        )
        .await
    }
}
impl SourceCommand {
    /// Maps the subcommand to its target service and source edit.
    fn edit(&self) -> (&Target, ServiceEdit) {
        match self {
            Self::Image(args) => (&args.target, ServiceEdit::Image(args.value.clone())),
            Self::Git(args) => (
                &args.target,
                ServiceEdit::Source(args.build.source(&args.url)),
            ),
            Self::Url(args) => (&args.target, ServiceEdit::GitUrl(args.value.clone())),
            Self::Branch(args) => (&args.target, ServiceEdit::GitBranch(args.value.clone())),
            Self::Commit(args) => (&args.target, ServiceEdit::GitCommit(args.value.clone())),
            Self::Dockerfile(args) => (&args.target, ServiceEdit::Dockerfile(args.value.clone())),
            Self::Context(args) => (&args.target, ServiceEdit::Context(args.value.clone())),
        }
    }
}
impl EnvironmentCommand {
    /// Maps to a single-entry environment edit; `None` removes the key.
    fn edit(&self) -> (&Target, ServiceEdit) {
        match self {
            Self::Set { target, key, value } => (
                target,
                ServiceEdit::EnvironmentEntry((key.clone(), Some(value.clone()))),
            ),
            Self::Remove { target, key } => {
                (target, ServiceEdit::EnvironmentEntry((key.clone(), None)))
            }
        }
    }
}
impl MountCommand {
    /// Maps to a mount edit keyed by container target path.
    fn edit(&self) -> (&Target, ServiceEdit) {
        match self {
            Self::Set {
                target,
                volume,
                path,
                read_only,
            } => (
                target,
                ServiceEdit::Mount(Mount {
                    volume: volume.clone(),
                    target: path.clone(),
                    read_only: *read_only,
                }),
            ),
            Self::Remove(args) => (&args.target, ServiceEdit::RemoveMount(args.value.clone())),
        }
    }
}
impl HealthCommand {
    /// Maps to a health check edit: `Http`/`Command` replace the whole check, `Clear`
    /// removes it, and the rest adjust one field of the existing check.
    fn edit(&self) -> (&Target, ServiceEdit) {
        match self {
            Self::Http {
                target,
                port,
                path,
                interval,
                check_timeout,
            } => (
                target,
                ServiceEdit::Healthcheck(Some(HealthCheck::Http {
                    port: *port,
                    path: path.clone(),
                    interval_seconds: *interval,
                    timeout_seconds: *check_timeout,
                })),
            ),
            Self::Command {
                target,
                interval,
                check_timeout,
                elements,
            } => (
                target,
                ServiceEdit::Healthcheck(Some(HealthCheck::Command {
                    command: elements.clone(),
                    interval_seconds: *interval,
                    timeout_seconds: *check_timeout,
                })),
            ),
            Self::Clear(target) => (target, ServiceEdit::Healthcheck(None)),
            Self::Port(args) => (&args.target, ServiceEdit::HealthPort(args.value)),
            Self::Path(args) => (&args.target, ServiceEdit::HealthPath(args.value.clone())),
            Self::CommandElements(args) => {
                (&args.target, ServiceEdit::HealthCommand(args.value.clone()))
            }
            Self::Interval(args) => (&args.target, ServiceEdit::HealthInterval(args.value)),
            Self::Timeout(args) => (&args.target, ServiceEdit::HealthTimeout(args.value)),
        }
    }
}
impl VolumeCommand {
    /// Saves a named volume declaration or removal.
    pub(crate) async fn run(
        &self,
        cli: &Cli,
        client: &Client,
        console: &mut Console,
    ) -> Result<()> {
        let (args, edit) = match self {
            Self::Add(args) => (
                args,
                ApplicationEdit::AddVolume(Volume {
                    name: args.volume.clone(),
                }),
            ),
            Self::Remove(args) => (args, ApplicationEdit::RemoveVolume(args.volume.clone())),
        };
        save(cli, client, console, &args.app, &args.flags, &edit).await
    }
}
impl RouteCommand {
    /// Edits routes client-side and saves the whole list: loads the application,
    /// appends or removes (by normalized hostname) a route, then saves against the
    /// same loaded generation. Removing an unknown hostname is an input error.
    pub(crate) async fn run(
        &self,
        cli: &Cli,
        client: &Client,
        console: &mut Console,
    ) -> Result<()> {
        let target = match self {
            Self::Add(args) => &args.target,
            Self::Redirect(args) => &args.target,
            Self::Remove(target) => target,
        };
        let current = resolve_application(client, &target.app).await?;
        let mut routes = current.application.to_manifest().spec.routes;
        match self {
            Self::Add(args) => routes.push(Route::service(
                target.hostname.clone(),
                args.service.clone(),
                args.port,
            )),
            Self::Redirect(args) => routes.push(Route::redirect(
                target.hostname.clone(),
                Redirect {
                    to: args.to.clone(),
                    status: args.status,
                    preserve_path: !args.no_preserve_path,
                },
            )),
            Self::Remove(_) => {
                let count = routes.len();
                let hostname = target.hostname.trim_end_matches('.').to_ascii_lowercase();
                routes.retain(|route| route.hostname != hostname);
                if routes.len() == count {
                    return Err(CliError::new(
                        ErrorKind::Input,
                        format!("route {:?} was not found", target.hostname),
                    ));
                }
            }
        }
        save_loaded(
            cli,
            client,
            console,
            current,
            &target.flags,
            &ApplicationEdit::Routes(routes),
        )
        .await
    }
}
impl JobCommand {
    /// Edits jobs client-side and saves the whole list, like routes: a new job
    /// runs after the existing ones, a replaced job keeps its position, and
    /// moving or removing an unknown job is an input error.
    pub(crate) async fn run(
        &self,
        cli: &Cli,
        client: &Client,
        console: &mut Console,
    ) -> Result<()> {
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
                let job = Job {
                    name: target.job.clone(),
                    service: args.service.clone(),
                    command: args.command.clone(),
                    run: JobRun::BeforeRollout,
                    timeout_seconds: args
                        .timeout_seconds
                        .or_else(|| existing.map(|index| jobs[index].timeout_seconds))
                        .unwrap_or(Job::DEFAULT_TIMEOUT_SECONDS),
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
        save_loaded(
            cli,
            client,
            console,
            current,
            &target.flags,
            &ApplicationEdit::Jobs(jobs),
        )
        .await
    }
}
impl RepositoryCommand {
    /// Saves a manifest repository connection, disconnection, or single-field change.
    pub(crate) async fn run(
        &self,
        cli: &Cli,
        client: &Client,
        console: &mut Console,
    ) -> Result<()> {
        let (target, edit) = match self {
            Self::Connect {
                target,
                url,
                path,
                branch,
                commit,
            } => (
                target,
                ApplicationEdit::Repository(Some(RepositoryManifest {
                    repository: GitRepository {
                        url: url.clone(),
                        branch: branch.clone(),
                        commit: commit.clone(),
                    },
                    path: path.clone(),
                })),
            ),
            Self::Disconnect(target) => (target, ApplicationEdit::Repository(None)),
            Self::Url(args) => (
                &args.target,
                ApplicationEdit::RepositoryUrl(args.value.clone()),
            ),
            Self::Branch(args) => (
                &args.target,
                ApplicationEdit::RepositoryBranch(args.value.clone()),
            ),
            Self::Commit { target, value, .. } => {
                (target, ApplicationEdit::RepositoryCommit(value.clone()))
            }
            Self::Path(args) => (
                &args.target,
                ApplicationEdit::RepositoryPath(args.value.clone()),
            ),
        };
        save(cli, client, console, &target.app, &target.flags, &edit).await
    }
}

/// Resolves `app` by name or ID, then saves `edit` against it.
pub(crate) async fn save(
    cli: &Cli,
    client: &Client,
    console: &mut Console,
    app: &str,
    flags: &EditFlags,
    edit: &ApplicationEdit,
) -> Result<()> {
    let current = resolve_application(client, app).await?;
    save_loaded(cli, client, console, current, flags, edit).await
}

/// Confirms and saves `edit` for an already loaded application. Unless `--force`,
/// the save requires `--expected-generation` or, by default, the loaded generation,
/// so concurrent edits are rejected instead of overwritten.
async fn save_loaded(
    cli: &Cli,
    client: &Client,
    console: &mut Console,
    current: ApplicationView,
    flags: &EditFlags,
    edit: &ApplicationEdit,
) -> Result<()> {
    let action = if flags.deployment.deploy {
        "Save and deploy changes to"
    } else {
        "Save changes to"
    };
    confirm(
        console,
        cli.noninteractive,
        flags.yes,
        &format!(
            "{action} application {:?}? [y/N] ",
            current.application.metadata().name
        ),
    )
    .await?;
    let options = EditOptions {
        expected_generation: (!flags.force)
            .then_some(flags.expected_generation.unwrap_or(current.generation)),
        force: flags.force,
        deploy: flags.deployment.deploy,
    };
    let saved = retry_transport(|| {
        client.edit_application(current.application.id().as_str(), edit, &options)
    })
    .await?;
    finish(console, client, &saved, &flags.deployment).await
}
/// Confirms and creates an empty application, optionally deploying it.
pub(crate) async fn create(
    cli: &Cli,
    client: &Client,
    console: &mut Console,
    args: &CreateArgs,
) -> Result<()> {
    confirm(
        console,
        cli.noninteractive,
        args.yes,
        &format!("Create application {:?}? [y/N] ", args.name),
    )
    .await?;
    let saved =
        retry_transport(|| client.create_application(&args.name, args.deployment.deploy)).await?;
    finish(console, client, &saved, &args.deployment).await
}
/// Emits the save result, first waiting for an accepted deployment unless `--no-wait`.
async fn finish(
    console: &mut Console,
    client: &Client,
    saved: &SavedApplication,
    deployment: &DeploymentArgs,
) -> Result<()> {
    if let Some(id) = &saved.operation_id
        && !deployment.no_wait
    {
        let operation = wait_for_operation(console, client, id).await?;
        return console.emit(&SavedDeploymentReport {
            saved,
            outcome: operation.state,
            operation: &operation,
        });
    }
    console.emit(saved)
}
