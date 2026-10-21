//! Field editing commands share confirmation, revision protection, and deployment output.
use crate::{
    cli::{CreateArgs, DeploymentArgs, expected_generation},
    commands::{resolve_application, wait_for_operation},
    error::{CliError, ErrorKind, Result},
    output::{
        Console,
        reports::{FileRow, RouteRow, SavedDeploymentReport},
    },
    support::{read_text, retry_transport},
};
use clap::{Args, Subcommand, builder::TypedValueParser};
use piqueld_client::{
    ApplicationView, Build, Client, Export, FileMode, FileSource, GitRepository, HealthCheck,
    Import, Job, JobRun, JobVolumeAccess, Mount, Redirect, RedirectStatus, RepositoryManifest,
    Rollout, RolloutOrder, Route, SavedApplication, Service, ServiceFile, Source, SourceRepository,
    Template, Typed, Variable, Visibility, Volume,
    edit::{ApplicationEdit, EditOptions, ServiceEdit, Variables},
    sync::RepositorySync,
};

/// How an application follows pushes, before `--interval` applies to `poll`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SyncMode {
    Off,
    Poll,
    Webhook,
}

impl SyncMode {
    /// Parses `off`, `poll`, or `webhook`, listing them in help and completions.
    fn parser() -> impl TypedValueParser<Value = Self> {
        clap::builder::PossibleValuesParser::new(["off", "poll", "webhook"]).map(
            |value| match value.as_str() {
                "poll" => Self::Poll,
                "webhook" => Self::Webhook,
                _ => Self::Off,
            },
        )
    }

    /// The setting, polling every `interval` seconds, by default 300.
    fn with_interval(self, interval: Option<u32>) -> Result<RepositorySync> {
        match (self, interval) {
            (Self::Poll, interval) => Ok(RepositorySync::Poll {
                interval_seconds: interval.unwrap_or(RepositorySync::DEFAULT_INTERVAL),
            }),
            (_, Some(_)) => Err(CliError::new(
                ErrorKind::Input,
                "--interval applies only to `poll`",
            )),
            (Self::Off, None) => Ok(RepositorySync::Off),
            (Self::Webhook, None) => Ok(RepositorySync::Webhook),
        }
    }
}

/// Parses `public` or `private`, listing both in help and completions.
pub(crate) fn visibility() -> impl TypedValueParser<Value = Visibility> {
    clap::builder::PossibleValuesParser::new(["public", "private"])
        .map(|value| value.parse().expect("every listed value parses"))
}

/// Parses `on` or `off`, listing both in help and completions.
pub(crate) fn on_off() -> impl TypedValueParser<Value = bool> {
    clap::builder::PossibleValuesParser::new(["on", "off"]).map(|value| value == "on")
}

fn volume_access() -> impl TypedValueParser<Value = JobVolumeAccess> {
    clap::builder::PossibleValuesParser::new(JobVolumeAccess::ALL.map(JobVolumeAccess::as_str))
        .map(|value| value.parse().expect("every listed value parses"))
}

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
pub(crate) struct ServiceTarget {
    /// Application name or stable ID.
    app: String,
    /// Logical service name.
    service: String,
    #[command(flatten)]
    flags: EditFlags,
}
impl ServiceTarget {
    /// The service's field-level `edit`, as the application edit saving it.
    fn field(&self, edit: ServiceEdit) -> (&Self, ApplicationEdit) {
        let name = self.service.clone();
        (self, ApplicationEdit::Service { name, edit })
    }
}
// Declares service-edit argument structs taking one required positional value.
macro_rules! service_value_args {
    ($($name:ident: $ty:ty;)*) => {$ (
        #[derive(Debug, Args)]
        pub(crate) struct $name {
            #[command(flatten)]
            target: ServiceTarget,
            /// New setting value.
            value: $ty,
        }
    )*};
}
// Values that accept `${{ }}` references take `Template` or `Typed` types.
service_value_args! {
    TextArgs: String;
    TemplateArgs: Template;
    ReplicasArgs: Typed<u16>;
    SecondsArgs: Typed<u32>;
}
#[derive(Debug, Args)]
pub(crate) struct StringsArgs {
    #[command(flatten)]
    target: ServiceTarget,
    /// Elements after --, preserving spaces. Omit to clear the array.
    #[arg(last = true)]
    value: Vec<String>,
}
#[derive(Debug, Args)]
pub(crate) struct TemplatesArgs {
    #[command(flatten)]
    target: ServiceTarget,
    /// Elements after --, preserving spaces. Omit to clear the array.
    #[arg(last = true)]
    value: Vec<Template>,
}
// Declares service-edit argument structs whose value is either given or `--clear`ed.
macro_rules! optional_args {
    ($($name:ident: $ty:ty;)*) => {$ (
        #[derive(Debug, Args)]
        pub(crate) struct $name {
            #[command(flatten)]
            target: ServiceTarget,
            /// New value, or use --clear to remove the setting.
            #[arg(required_unless_present = "clear", conflicts_with = "clear")]
            value: Option<$ty>,
            /// Remove the setting.
            #[arg(long)]
            clear: bool,
        }
    )*};
}
optional_args! { CommitArgs: String; CpuArgs: Typed<u32>; MemoryArgs: Typed<u64>; }

#[derive(Debug, Subcommand)]
pub(crate) enum ServiceCommand {
    /// Add a service using an image or --git URL.
    Add(AddServiceArgs),
    /// Remove a service from saved configuration.
    Remove(ServiceTarget),
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
    Command(TemplatesArgs),
    /// Set argument elements after --; omit elements to clear.
    Arguments(TemplatesArgs),
    /// Set services that must be healthy before this one rolls out, after --;
    /// omit services to clear.
    DependsOn(StringsArgs),
    /// Replace the rollout order and monitor window; omitted flags use their defaults.
    Rollout(RolloutArgs),
    /// Make the service receive the daemon's telemetry from every
    /// application, over the socket mounted at /run/piqueld/telemetry, in
    /// environments granted it with `env telemetry`. At most one service per
    /// application.
    Telemetry(ReceivesTelemetryArgs),
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
    Image(TemplateArgs),
    /// Switch to a Git/Docker build source.
    Git(GitSourceArgs),
    /// Change the Git clone URL.
    Url(TextArgs),
    /// Change the Git branch.
    Branch(TextArgs),
    /// Pin a commit, or --clear to follow the branch.
    Commit(CommitArgs),
    /// Change the Dockerfile path.
    Dockerfile(TemplateArgs),
    /// Change the build context path.
    Context(TemplateArgs),
}
#[derive(Debug, Args)]
pub(crate) struct RolloutArgs {
    #[command(flatten)]
    target: ServiceTarget,
    /// stop-first or start-first. Omit to stop first only when a volume is mounted writable.
    #[arg(long)]
    order: Option<Typed<RolloutOrder>>,
    /// Seconds to watch each replacement task for failure, 1-3600. Omit for the 30-second default.
    #[arg(long)]
    monitor_seconds: Option<Typed<u32>>,
}
#[derive(Debug, Args)]
pub(crate) struct ReceivesTelemetryArgs {
    #[command(flatten)]
    target: ServiceTarget,
    /// `on` receives telemetry once the environment is granted it; `off` never.
    #[arg(value_name = "on|off", action = clap::ArgAction::Set, value_parser = on_off())]
    enabled: bool,
}
#[derive(Debug, Args)]
pub(crate) struct AddServiceArgs {
    #[command(flatten)]
    target: ServiceTarget,
    /// Container image reference, or select --git instead.
    #[arg(required_unless_present = "git", conflicts_with = "git")]
    image: Option<Template>,
    /// Build this Git repository with Docker.
    #[arg(long)]
    git: Option<String>,
    // Boxed to keep the CLI command enum small.
    #[command(flatten)]
    build: Box<GitBuildArgs>,
}
#[derive(Debug, Args)]
pub(crate) struct GitSourceArgs {
    #[command(flatten)]
    target: ServiceTarget,
    /// Git clone URL or host path.
    #[arg(id = "git")]
    url: String,
    // Boxed to keep the CLI command enum small.
    #[command(flatten)]
    build: Box<GitBuildArgs>,
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
    dockerfile: Template,
    /// Build context relative to the repository root.
    #[arg(long, requires = "git", default_value = ".")]
    context: Template,
    /// Docker build argument; repeat for several. Values are not secret.
    #[arg(long = "build-arg", value_name = "KEY=VALUE", requires = "git", value_parser = Self::parse_arg)]
    build_args: Vec<(String, String)>,
    /// Multi-stage build target.
    #[arg(long, requires = "git")]
    target: Option<Template>,
}
impl GitBuildArgs {
    /// Parses a `--build-arg` value at its first `=`, so values may contain `=`.
    /// Fails when the value has no `=`; names are checked by manifest validation.
    fn parse_arg(value: &str) -> std::result::Result<(String, String), String> {
        value
            .split_once('=')
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .ok_or_else(|| "build arguments must use KEY=VALUE".to_owned())
    }
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
                args: self
                    .build_args
                    .iter()
                    .map(|(key, value)| (key.clone(), value.as_str().into()))
                    .collect(),
                target: self.target.clone(),
            },
        }
    }
}
#[derive(Debug, Subcommand)]
pub(crate) enum EnvironmentCommand {
    /// Set one variable without changing other entries.
    Set {
        #[command(flatten)]
        target: ServiceTarget,
        /// Variable name.
        key: String,
        /// Variable value; may reference manifest variables with `${{ vars.<name> }}`.
        value: Template,
    },
    /// Remove one variable.
    Remove {
        #[command(flatten)]
        target: ServiceTarget,
        /// Variable name.
        key: String,
    },
}
#[derive(Debug, Subcommand)]
pub(crate) enum MountCommand {
    /// Add or replace the mount at a container target path.
    Set {
        #[command(flatten)]
        target: ServiceTarget,
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
pub(crate) enum FileCommand {
    /// List the read-only files services mount, as saved.
    List {
        /// Application name or stable ID.
        app: String,
        /// Only this service's files.
        service: Option<String>,
    },
    /// Add a read-only file, or replace the file at the same target.
    Add(AddFileArgs),
    /// Remove the file at a container target path.
    Remove(TextArgs),
}
#[derive(Debug, Args)]
pub(crate) struct AddFileArgs {
    #[command(flatten)]
    target: ServiceTarget,
    /// Absolute container path of the file.
    #[arg(value_name = "TARGET")]
    file: String,
    #[command(flatten)]
    source: FileSourceArgs,
    /// Octal permissions of the root-owned file.
    #[arg(long, default_value_t = FileMode::default())]
    mode: FileMode,
}
// Exactly one source of a file's content.
#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
pub(crate) struct FileSourceArgs {
    /// The file's content. It is never rendered with variables.
    #[arg(long)]
    content: Option<String>,
    /// Read the content from this local file now, and save it inline.
    #[arg(long, value_name = "LOCAL_FILE")]
    from: Option<std::path::PathBuf>,
    /// Read the content from this file in the manifest's repository, relative
    /// to its root, each time a deployment fetches the manifest.
    #[arg(long, value_name = "REPOSITORY_PATH")]
    path: Option<String>,
}
impl FileSourceArgs {
    /// The selected source; `--from` reads its local file now.
    async fn source(&self) -> Result<FileSource> {
        Ok(match (&self.content, &self.from, &self.path) {
            (Some(content), ..) => FileSource::Content(content.clone()),
            (_, Some(local), _) => {
                FileSource::Content(read_text(local, "file", ServiceFile::MAX_BYTES as u64).await?)
            }
            (.., Some(path)) => FileSource::Path(path.clone()),
            (None, None, None) => unreachable!("Clap requires one file source"),
        })
    }
}
impl FileCommand {
    /// Lists saved files, or adds, replaces, or removes one service's file by
    /// target.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (target, edit) = match self {
            Self::List { app, service } => {
                let application = resolve_application(client, app).await?;
                let rows: Vec<_> = application
                    .application
                    .spec()
                    .services
                    .iter()
                    .filter(|candidate| service.as_ref().is_none_or(|name| candidate.name == *name))
                    .flat_map(|candidate| {
                        candidate.files.iter().map(|file| FileRow {
                            service: candidate.name.clone(),
                            file: file.clone(),
                        })
                    })
                    .collect();
                return console.emit(&rows);
            }
            Self::Add(args) => args.target.field(ServiceEdit::File(ServiceFile {
                target: args.file.clone(),
                source: args.source.source().await?,
                mode: args.mode,
            })),
            Self::Remove(args) => args
                .target
                .field(ServiceEdit::RemoveFile(args.value.clone())),
        };
        target.flags.save(client, console, &target.app, &edit).await
    }
}
#[derive(Debug, Subcommand)]
pub(crate) enum HealthCommand {
    /// Configure an HTTP health check.
    Http {
        #[command(flatten)]
        target: ServiceTarget,
        /// Container port to probe.
        port: Typed<u16>,
        /// HTTP request path.
        #[arg(long, default_value = "/health")]
        path: Template,
        /// Seconds between checks.
        #[arg(long, default_value = "10")]
        interval: Typed<u32>,
        /// Seconds before a check counts as failed.
        #[arg(long, default_value = "3")]
        check_timeout: Typed<u32>,
    },
    /// Configure a command health check; command elements follow --.
    Command {
        #[command(flatten)]
        target: ServiceTarget,
        /// Seconds between checks.
        #[arg(long, default_value = "10")]
        interval: Typed<u32>,
        /// Seconds before a check counts as failed.
        #[arg(long, default_value = "3")]
        check_timeout: Typed<u32>,
        /// Command elements after --, preserving spaces.
        #[arg(last = true, required = true)]
        elements: Vec<Template>,
    },
    /// Remove the configured health check.
    Clear(ServiceTarget),
    /// Set the HTTP port.
    Port(ReplicasArgs),
    /// Set the HTTP path.
    Path(TemplateArgs),
    /// Set command elements for an existing command health check.
    CommandElements(TemplatesArgs),
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
    /// Fully qualified DNS hostname, exact or a wildcard such as `*.example.com`.
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
    /// Who may connect: everyone, or only tailnet devices. The environment's
    /// ceiling can make it stricter.
    #[arg(long, default_value = "private", value_parser = visibility())]
    visibility: Visibility,
    /// Stable route name, unique within the application, e.g. for `env url --route`.
    #[arg(long)]
    name: Option<String>,
}
#[derive(Debug, Args)]
pub(crate) struct RedirectRouteArgs {
    #[command(flatten)]
    target: RouteTarget,
    /// Absolute http(s) destination URL.
    to: Template,
    /// HTTP status: 301, 302, 303, 307, or 308.
    #[arg(long, default_value_t = RedirectStatus::PermanentRedirect.into())]
    status: u16,
    /// Drop the request path and query instead of appending them to the destination.
    #[arg(long)]
    no_preserve_path: bool,
    /// Who may connect: everyone, or only tailnet devices.
    #[arg(long, default_value = "private", value_parser = visibility())]
    visibility: Visibility,
    /// Stable route name, unique within the application, e.g. for `env url --route`.
    #[arg(long)]
    name: Option<String>,
}
#[derive(Debug, Args)]
pub(crate) struct RouteNameArgs {
    #[command(flatten)]
    target: RouteTarget,
    /// New stable route name, unique within the application; omit to remove it.
    name: Option<String>,
}
#[derive(Debug, Args)]
pub(crate) struct RouteVisibilityArgs {
    #[command(flatten)]
    target: RouteTarget,
    /// `public` or `private` (tailnet only).
    #[arg(value_parser = visibility())]
    visibility: Visibility,
}
#[derive(Debug, Subcommand)]
pub(crate) enum RouteCommand {
    /// List an application's deployed routes: visibility, the DNS records
    /// their hostnames need, and their state.
    List {
        /// Application name or stable ID.
        app: String,
    },
    /// Add an HTTPS route to an application.
    Add(AddRouteArgs),
    /// Add an HTTPS route the gateway answers with a redirect.
    Redirect(RedirectRouteArgs),
    /// Change who may connect to a route. Changing it withdraws the route from
    /// its old listener as the next deployment starts.
    Visibility(RouteVisibilityArgs),
    /// Name a route, or remove its name. Names select the same route in
    /// every environment and preview, e.g. `env url --route NAME`.
    Name(RouteNameArgs),
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
    /// read-only mounts every volume read-only, so the job runs beside the
    /// services. exclusive keeps writable volumes writable and stops every
    /// service mounting them until the rollout. Defaults to the job's current
    /// access, or read-only for a new job.
    #[arg(long, value_parser = volume_access())]
    volume_access: Option<JobVolumeAccess>,
    /// Command elements after --, replacing the service's command and arguments.
    #[arg(last = true, required = true)]
    command: Vec<Template>,
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
pub(crate) struct ExportTarget {
    /// Application name or stable ID.
    app: String,
    /// Exported service of the application.
    service: String,
    #[command(flatten)]
    flags: EditFlags,
}
#[derive(Debug, Args)]
pub(crate) struct SetExportArgs {
    #[command(flatten)]
    target: ExportTarget,
    /// Applications whose environments may import the service, comma-separated.
    #[arg(long, value_delimiter = ',', required = true)]
    applications: Vec<String>,
    /// Let previews of those applications import it too.
    #[arg(long)]
    previews: bool,
}
#[derive(Debug, Subcommand)]
pub(crate) enum ExportCommand {
    /// List the services the application exports, as saved.
    List {
        /// Application name or stable ID.
        app: String,
    },
    /// Export a service to other applications, or replace who may import it.
    Set(SetExportArgs),
    /// Stop exporting a service.
    Remove(ExportTarget),
}
#[derive(Debug, Args)]
pub(crate) struct ImportTarget {
    /// Application name or stable ID.
    app: String,
    /// Application exporting the service.
    application: String,
    /// Exported service of that application.
    service: String,
    #[command(flatten)]
    flags: EditFlags,
}
#[derive(Debug, Args)]
pub(crate) struct AddImportArgs {
    #[command(flatten)]
    target: ImportTarget,
    /// Environment of that application to reach, e.g. `staging` or
    /// `${{ vars.auth_env }}`. By default, the one named like the importing
    /// environment.
    #[arg(long)]
    environment: Option<Template>,
}
#[derive(Debug, Subcommand)]
pub(crate) enum ImportCommand {
    /// List the services the application imports, as saved. `env show`
    /// reports whether each connects.
    List {
        /// Application name or stable ID.
        app: String,
    },
    /// Import a service of another application, or replace the environment
    /// it reaches.
    Add(AddImportArgs),
    /// Stop importing a service.
    Remove(ImportTarget),
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
    /// Enable repository ownership of services and volumes. Every environment
    /// follows `--branch`, as do environments created later without one; change
    /// an environment's branch with `env branch`.
    Connect {
        #[command(flatten)]
        target: RepositoryTarget,
        /// Git clone URL or host path.
        // A distinct id keeps clap from storing this in the global `--url` endpoint.
        #[arg(id = "repository_url", value_name = "URL")]
        url: String,
        /// Manifest file path relative to the repository root.
        path: String,
        /// Branch environments follow unless created with their own.
        #[arg(long, default_value = "main")]
        branch: String,
        /// Pin a full commit hash instead of following the branch.
        #[arg(long)]
        commit: Option<String>,
        /// Deploy on push: `poll` lists the branches every `--interval`
        /// seconds, `webhook` when GitHub reports a push.
        #[arg(long = "sync", value_parser = SyncMode::parser(), default_value = "off")]
        sync: SyncMode,
        /// Seconds between polls, from 60 to 86400; 300 by default.
        #[arg(long)]
        interval: Option<u32>,
    },
    /// Stop fetching from Git and retain saved configuration for local editing.
    Disconnect(RepositoryTarget),
    /// Change the repository URL every environment fetches from.
    Url(RepositoryText),
    /// Change the manifest file path every environment reads.
    Path(RepositoryText),
    /// Deploy on push: every preview, and every environment that opted in
    /// with `env sync` and follows an unpinned branch, redeploys when its
    /// branch moves past the head of its last deployment. `poll` lists the branches every `--interval`
    /// seconds; `webhook` lists them when GitHub reports a push (see
    /// `app repository webhook`); `off` deploys only when asked.
    Sync {
        #[command(flatten)]
        target: RepositoryTarget,
        #[arg(value_parser = SyncMode::parser())]
        mode: SyncMode,
        /// Seconds between polls, from 60 to 86400; 300 by default.
        #[arg(long)]
        interval: Option<u32>,
    },
    /// Show or generate the GitHub webhook that reports pushes to sync.
    Webhook {
        #[command(subcommand)]
        command: WebhookCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum WebhookCommand {
    /// Show the payload URL to configure in GitHub, and when the secret was
    /// generated.
    Show {
        /// Application name or stable ID.
        app: String,
    },
    /// Generate a new webhook secret, replacing the previous one, and print
    /// it. It is shown only once.
    Rotate {
        /// Application name or stable ID.
        app: String,
        /// Skip interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Args)]
pub(crate) struct VariableTarget {
    /// Application name or stable ID.
    app: String,
    /// Variable name, referenced as `${{ vars.<name> }}`.
    name: String,
    /// Environment name whose value changes; omit to change the default.
    #[arg(long = "env", value_name = "ENV")]
    environment: Option<String>,
    /// Change the value every preview uses, `[spec.previews.variables]`.
    #[arg(long, conflicts_with = "environment")]
    previews: bool,
    #[command(flatten)]
    flags: EditFlags,
}
#[derive(Debug, Subcommand)]
pub(crate) enum VariableCommand {
    /// Set a variable's default, or with --env its value in one environment,
    /// or with --previews its value in previews.
    Set {
        #[command(flatten)]
        target: VariableTarget,
        /// `true`, `false`, and integers keep their type, and anything else is
        /// text, which may reference system variables such as `${{ env.name }}`.
        value: String,
        /// Keep the value as text even when it looks like a boolean or integer.
        #[arg(long)]
        string: bool,
    },
    /// Remove a variable's default, or with --env its value in one
    /// environment, or with --previews its value in previews.
    Unset(VariableTarget),
}

impl VariableCommand {
    /// Edits variables client-side and saves them together, like routes.
    /// Unsetting an unknown variable is an input error.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (Self::Set { target, .. } | Self::Unset(target)) = self;
        let current = resolve_application(client, &target.app).await?;
        let mut variables = Variables::of(&current.application.to_manifest());
        let values = match (&target.environment, target.previews) {
            (Some(environment), _) => variables
                .environments
                .entry(environment.clone())
                .or_default(),
            (None, true) => &mut variables.previews,
            (None, false) => &mut variables.defaults,
        };
        match self {
            Self::Set { value, string, .. } => {
                let value = if *string {
                    Variable::String(value.as_str().into())
                } else {
                    Variable::from_text(value)
                };
                values.insert(target.name.clone(), value);
            }
            Self::Unset(_) => {
                if values.remove(&target.name).is_none() {
                    return Err(CliError::new(
                        ErrorKind::Input,
                        format!("variable {:?} was not found", target.name),
                    ));
                }
            }
        }
        let edit = ApplicationEdit::Variables(variables);
        target
            .flags
            .save_loaded(client, console, &current, &edit)
            .await
    }
}

impl AddServiceArgs {
    /// The new service: its image or Git source, one replica, and no
    /// optional settings.
    fn service(&self) -> Service {
        Service {
            name: self.target.service.clone(),
            source: self.git.as_ref().map_or_else(
                || Source::Image {
                    image: self.image.clone().expect("Clap requires image or Git"),
                },
                |url| self.build.source(url),
            ),
            replicas: Typed::Literal(1),
            environment: std::collections::BTreeMap::new(),
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
        }
    }
}

impl ServiceCommand {
    /// Saves one service edit. Adding and removing services are whole-application
    /// edits; every other variant becomes a field-level `ServiceEdit` on the target service.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (target, edit) = match self {
            Self::Add(args) => (
                &args.target,
                ApplicationEdit::AddService(Box::new(args.service())),
            ),
            Self::Remove(target) => (
                target,
                ApplicationEdit::RemoveService(target.service.clone()),
            ),
            Self::Rename(args) => args.target.field(ServiceEdit::Name(args.value.clone())),
            Self::Replicas(args) => args.target.field(ServiceEdit::Replicas(args.value.clone())),
            Self::Source { command } => command.edit(),
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
            Self::Health { command } => command.edit(),
            Self::Cpu(args) => args.target.field(ServiceEdit::Cpu(args.value.clone())),
            Self::Memory(args) => args.target.field(ServiceEdit::Memory(args.value.clone())),
        };
        target.flags.save(client, console, &target.app, &edit).await
    }
}
impl SourceCommand {
    /// Maps the subcommand to its target service and source edit.
    fn edit(&self) -> (&ServiceTarget, ApplicationEdit) {
        match self {
            Self::Image(args) => args.target.field(ServiceEdit::Image(args.value.clone())),
            Self::Git(args) => args
                .target
                .field(ServiceEdit::Source(args.build.source(&args.url))),
            Self::Url(args) => args.target.field(ServiceEdit::GitUrl(args.value.clone())),
            Self::Branch(args) => args
                .target
                .field(ServiceEdit::GitBranch(args.value.clone())),
            Self::Commit(args) => args
                .target
                .field(ServiceEdit::GitCommit(args.value.clone())),
            Self::Dockerfile(args) => args
                .target
                .field(ServiceEdit::Dockerfile(args.value.clone())),
            Self::Context(args) => args.target.field(ServiceEdit::Context(args.value.clone())),
        }
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
impl MountCommand {
    /// Maps to a mount edit keyed by container target path.
    fn edit(&self) -> (&ServiceTarget, ApplicationEdit) {
        match self {
            Self::Set {
                target,
                volume,
                path,
                read_only,
            } => target.field(ServiceEdit::Mount(Mount {
                volume: volume.clone(),
                target: path.clone(),
                read_only: *read_only,
            })),
            Self::Remove(args) => args
                .target
                .field(ServiceEdit::RemoveMount(args.value.clone())),
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
                interval,
                check_timeout,
            } => target.field(ServiceEdit::Healthcheck(Some(HealthCheck::Http {
                port: port.clone(),
                path: path.clone(),
                interval_seconds: interval.clone(),
                timeout_seconds: check_timeout.clone(),
            }))),
            Self::Command {
                target,
                interval,
                check_timeout,
                elements,
            } => target.field(ServiceEdit::Healthcheck(Some(HealthCheck::Command {
                command: elements.clone(),
                interval_seconds: interval.clone(),
                timeout_seconds: check_timeout.clone(),
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
        }
    }
}
impl VolumeCommand {
    /// Saves a named volume declaration or removal.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (args, edit) = match self {
            Self::Add(args) => (
                args,
                ApplicationEdit::AddVolume(Volume {
                    name: args.volume.clone(),
                }),
            ),
            Self::Remove(args) => (args, ApplicationEdit::RemoveVolume(args.volume.clone())),
        };
        args.flags.save(client, console, &args.app, &edit).await
    }
}
impl RouteCommand {
    /// Lists deployed routes, or edits routes client-side and saves the whole
    /// list: loads the application, appends a route or changes or removes one
    /// (by normalized hostname), then saves against the same loaded generation.
    /// Changing or removing an unknown hostname is an input error.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let target = match self {
            Self::List { app } => return list_routes(client, console, app).await,
            Self::Add(args) => &args.target,
            Self::Redirect(args) => &args.target,
            Self::Visibility(args) => &args.target,
            Self::Name(args) => &args.target,
            Self::Remove(target) => target,
        };
        let current = resolve_application(client, &target.app).await?;
        let mut routes = current.application.to_manifest().spec.routes;
        // Literal hostnames are saved canonically; references as written.
        let hostname = if Template::mentions_reference(&target.hostname) {
            target.hostname.clone()
        } else {
            target.hostname.trim_end_matches('.').to_ascii_lowercase()
        };
        let not_found = || {
            CliError::new(
                ErrorKind::Input,
                format!("route {:?} was not found", target.hostname),
            )
        };
        match self {
            Self::List { .. } => unreachable!("listing returned above"),
            Self::Add(args) => routes.push(
                Route::service(
                    target.hostname.clone(),
                    args.visibility,
                    args.service.clone(),
                    args.port,
                )
                .named(args.name.clone()),
            ),
            Self::Redirect(args) => routes.push(
                Route::redirect(
                    target.hostname.clone(),
                    args.visibility,
                    Redirect {
                        to: args.to.clone(),
                        status: args.status,
                        preserve_path: !args.no_preserve_path,
                    },
                )
                .named(args.name.clone()),
            ),
            Self::Visibility(args) => {
                routes
                    .iter_mut()
                    .find(|route| route.hostname.as_str() == hostname)
                    .ok_or_else(not_found)?
                    .visibility = args.visibility;
            }
            Self::Name(args) => {
                routes
                    .iter_mut()
                    .find(|route| route.hostname.as_str() == hostname)
                    .ok_or_else(not_found)?
                    .name
                    .clone_from(&args.name);
            }
            Self::Remove(_) => {
                let count = routes.len();
                routes.retain(|route| route.hostname.as_str() != hostname);
                if routes.len() == count {
                    return Err(not_found());
                }
            }
        }
        let edit = ApplicationEdit::Routes(routes);
        target
            .flags
            .save_loaded(client, console, &current, &edit)
            .await
    }
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
                let job = Job {
                    name: target.job.clone(),
                    service: args.service.clone(),
                    command: args.command.clone(),
                    run: JobRun::BeforeRollout,
                    timeout_seconds: args
                        .timeout_seconds
                        .or_else(|| existing.map(|index| jobs[index].timeout_seconds))
                        .unwrap_or(Job::DEFAULT_TIMEOUT_SECONDS),
                    volume_access: args
                        .volume_access
                        .or_else(|| existing.map(|index| jobs[index].volume_access))
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
impl ExportCommand {
    /// Lists saved exports, or edits them client-side and saves the whole
    /// list, like jobs: setting an exported service replaces its grants in place.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let target = match self {
            Self::List { app } => {
                let application = resolve_application(client, app).await?;
                return console.emit(&application.application.to_manifest().spec.exports);
            }
            Self::Set(args) => &args.target,
            Self::Remove(target) => target,
        };
        let current = resolve_application(client, &target.app).await?;
        let mut exports = current.application.to_manifest().spec.exports;
        let existing = exports
            .iter()
            .position(|export| export.service == target.service);
        match (self, existing) {
            (Self::Set(args), existing) => {
                let export = Export {
                    service: target.service.clone(),
                    applications: args.applications.clone(),
                    previews: args.previews,
                };
                match existing {
                    Some(index) => exports[index] = export,
                    None => exports.push(export),
                }
            }
            (Self::Remove(_), Some(index)) => {
                exports.remove(index);
            }
            (_, None) => {
                return Err(CliError::new(
                    ErrorKind::Input,
                    format!("service {:?} is not exported", target.service),
                ));
            }
            (Self::List { .. }, Some(_)) => unreachable!("listing returned above"),
        }
        let edit = ApplicationEdit::Exports(exports);
        target
            .flags
            .save_loaded(client, console, &current, &edit)
            .await
    }
}
impl ImportCommand {
    /// Lists saved imports, or edits them client-side and saves the whole
    /// list: adding an imported service again replaces its environment.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let target = match self {
            Self::List { app } => {
                let application = resolve_application(client, app).await?;
                return console.emit(&application.application.to_manifest().spec.imports);
            }
            Self::Add(args) => &args.target,
            Self::Remove(target) => target,
        };
        let current = resolve_application(client, &target.app).await?;
        let mut imports = current.application.to_manifest().spec.imports;
        let existing = imports.iter().position(|import| {
            import.application == target.application && import.service == target.service
        });
        match (self, existing) {
            (Self::Add(args), existing) => {
                let import = Import {
                    application: target.application.clone(),
                    service: target.service.clone(),
                    environment: args.environment.clone(),
                };
                match existing {
                    Some(index) => imports[index] = import,
                    None => imports.push(import),
                }
            }
            (Self::Remove(_), Some(index)) => {
                imports.remove(index);
            }
            (_, None) => {
                return Err(CliError::new(
                    ErrorKind::Input,
                    format!("{}.{} is not imported", target.service, target.application),
                ));
            }
            (Self::List { .. }, Some(_)) => unreachable!("listing returned above"),
        }
        let edit = ApplicationEdit::Imports(imports);
        target
            .flags
            .save_loaded(client, console, &current, &edit)
            .await
    }
}
impl RepositoryCommand {
    /// Saves a manifest repository connection, disconnection, or single-field change.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (target, edit) = match self {
            Self::Connect {
                target,
                url,
                path,
                branch,
                commit,
                sync,
                interval,
            } => (
                target,
                ApplicationEdit::Repository(Some(RepositoryManifest {
                    repository: GitRepository {
                        url: url.clone(),
                        branch: branch.clone(),
                        commit: commit.clone(),
                    },
                    path: path.clone(),
                    sync: sync.with_interval(*interval)?,
                })),
            ),
            Self::Sync {
                target,
                mode,
                interval,
            } => (
                target,
                ApplicationEdit::RepositorySync(mode.with_interval(*interval)?),
            ),
            Self::Webhook { command } => return command.run(client, console).await,
            Self::Disconnect(target) => (target, ApplicationEdit::Repository(None)),
            Self::Url(args) => (
                &args.target,
                ApplicationEdit::RepositoryUrl(args.value.clone()),
            ),
            Self::Path(args) => (
                &args.target,
                ApplicationEdit::RepositoryPath(args.value.clone()),
            ),
        };
        target.flags.save(client, console, &target.app, &edit).await
    }
}

impl WebhookCommand {
    /// Shows the webhook, or confirms and generates a new secret.
    async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        match self {
            Self::Show { app } => {
                let application = resolve_application(client, app).await?;
                console.emit(
                    &client
                        .webhook(application.application.id().as_str())
                        .await?,
                )
            }
            Self::Rotate { app, yes } => {
                let application = resolve_application(client, app).await?;
                console
                    .confirm(
                        *yes,
                        &format!(
                            "Generate a new webhook secret for application {:?}? The current one stops verifying at once.",
                            application.application.metadata().name
                        ),
                    )
                    .await?;
                let id = application.application.id().as_str();
                console.emit(&client.generate_webhook_secret(id).await?)
            }
        }
    }
}

/// Lists the deployed routes of every environment of `app`, from the
/// daemon's ingress status.
async fn list_routes(client: &Client, console: &mut Console, app: &str) -> Result<()> {
    let application = resolve_application(client, app).await?;
    let readiness = client.system_readiness().await?;
    let rows: Vec<_> = readiness
        .ingress
        .routes
        .into_iter()
        .filter_map(|route| {
            let environment = application.environment(&route.environment_id)?;
            Some(RouteRow {
                environment: environment.name.to_string(),
                route,
            })
        })
        .collect();
    console.emit(&rows)
}

impl EditFlags {
    /// Resolves `app` by name or ID, then saves `edit` against it.
    pub(crate) async fn save(
        &self,
        client: &Client,
        console: &mut Console,
        app: &str,
        edit: &ApplicationEdit,
    ) -> Result<()> {
        let current = resolve_application(client, app).await?;
        self.save_loaded(client, console, &current, edit).await
    }

    /// Confirms and saves `edit` for an already loaded application. Unless `--force`,
    /// the save requires `--expected-generation` or, by default, the loaded generation,
    /// so concurrent edits are rejected instead of overwritten.
    pub(crate) async fn save_loaded(
        &self,
        client: &Client,
        console: &mut Console,
        current: &ApplicationView,
        edit: &ApplicationEdit,
    ) -> Result<()> {
        let action = if self.deployment.deploy {
            "Save and deploy changes to"
        } else {
            "Save changes to"
        };
        console
            .confirm(
                self.yes,
                &format!(
                    "{action} application {:?}?",
                    current.application.metadata().name
                ),
            )
            .await?;
        let options = EditOptions {
            expected_generation: expected_generation(
                self.force,
                self.expected_generation,
                current.generation,
            ),
            force: self.force,
            deploy: self.deployment.deploy,
        };
        let saved = retry_transport(|| {
            client.edit_application(current.application.id().as_str(), edit, &options)
        })
        .await?;
        self.deployment.finish(client, console, &saved).await
    }
}

impl CreateArgs {
    /// Confirms and creates an empty application, optionally deploying it.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        console
            .confirm(self.yes, &format!("Create application {:?}?", self.name))
            .await?;
        let saved =
            retry_transport(|| client.create_application(&self.name, self.deployment.deploy))
                .await?;
        self.deployment.finish(client, console, &saved).await
    }
}

impl DeploymentArgs {
    /// Emits the save result, first waiting, in order, for every accepted
    /// deployment (one per environment that builds) unless `--no-wait`.
    pub(crate) async fn finish(
        &self,
        client: &Client,
        console: &mut Console,
        saved: &SavedApplication,
    ) -> Result<()> {
        let deployments = saved.deployments();
        if deployments.is_empty() || self.no_wait {
            return console.emit(saved);
        }
        let mut operations = Vec::with_capacity(deployments.len());
        for id in deployments {
            operations.push(wait_for_operation(console, client, id).await?);
        }
        console.emit(&SavedDeploymentReport {
            saved,
            outcome: operations[0].state,
            operation: &operations[0],
            operations: &operations,
        })
    }
}
