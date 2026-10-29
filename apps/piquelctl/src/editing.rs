//! Field editing commands share confirmation, revision protection, and deployment output.
use crate::{
    cli::{CreateArgs, DeploymentArgs, expected_generation},
    commands::{resolve_application, wait_for_operation},
    error::{CliError, ErrorKind, Result},
    output::{
        Console,
        reports::{RouteRow, SavedDeploymentReport, ServiceRow},
    },
    support::{read_text, retry_transport},
};
use clap::{
    Args, Subcommand,
    builder::{PossibleValue, PossibleValuesParser, TypedValueParser},
};
use piqueld_client::{
    ApplicationView, ApplyApplicationRequest, ApplyOptions, Build, Client, Export, FileMode,
    FileSource, GitRepository, HealthCheck, Import, Job, JobRun, JobVolumeAccess,
    ManifestRepository, Mount, Redirect, RedirectStatus, RepositoryManifest, Rollout, RolloutOrder,
    Route, SavedApplication, SecretMount, Service, ServiceFile, Source, SourceRepository, Template,
    Typed, Variable, Visibility, Volume,
    edit::{ApplicationEdit, EditOptions, ServiceEdit, Variables},
    sync::RepositorySync,
};
use std::collections::BTreeMap;

/// How an application follows pushes, before `--interval` applies to `poll`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SyncMode {
    Off,
    Poll,
    Webhook,
}

impl SyncMode {
    /// Parses `off`, `poll`, or `webhook`, listing them with their help in
    /// help and completions.
    fn parser() -> impl TypedValueParser<Value = Self> {
        PossibleValuesParser::new([
            PossibleValue::new("off").help("Deploy only when asked."),
            PossibleValue::new("poll").help("List the branches every `--interval` seconds."),
            PossibleValue::new("webhook").help(
                "List the branches when GitHub reports a push (see `app repository webhook`).",
            ),
        ])
        .map(|value| match value.as_str() {
            "poll" => Self::Poll,
            "webhook" => Self::Webhook,
            _ => Self::Off,
        })
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

/// Parses a `KEY=VALUE` flag such as `--build-arg` at its first `=`, so values
/// may contain `=`. Fails when the value has no `=`; names are checked by
/// manifest validation.
fn key_value(value: &str) -> std::result::Result<(String, String), String> {
    value
        .split_once('=')
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .ok_or_else(|| "expected `=` between the two parts".to_owned())
}

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

// Flags shared by every edit: deployment, generation precondition, and confirmation.
// Flattened `Args` structs use `//`: their doc comments would override the `about`
// text of commands that flatten them. Field `///` comments are user-facing help.
#[derive(Debug, Args)]
pub(crate) struct EditFlags {
    #[command(flatten)]
    deployment: DeploymentArgs,
    /// Save only if the application's saved generation is still `GENERATION`.
    /// By default, it must still be the generation the command read, so a
    /// concurrent change fails the save instead of being overwritten.
    #[arg(long, value_name = "GENERATION")]
    expected_generation: Option<u64>,
    /// Save even if the application changed since the command read it. It
    /// still asks for confirmation.
    #[arg(long, conflicts_with = "expected_generation")]
    force: bool,
    /// Skip the confirmation prompt. You need it when not on a terminal.
    #[arg(long)]
    yes: bool,
}
// Positional `<app> <service>` pair addressed by service-level edits.
#[derive(Debug, Args)]
pub(crate) struct ServiceTarget {
    /// Application name or stable ID.
    app: String,
    /// Service name.
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
// Declares edit argument structs: a flattened target followed by one `value`,
// whose doc comment and `#[arg]` attributes each struct gives, so every
// command documents its own value. Values that accept `${{ }}` references
// take `Template` or `Typed` types.
macro_rules! value_args {
    ($($name:ident($target:ty) { $(#[$attr:meta])* value: $ty:ty $(,)? })*) => {$(
        #[derive(Debug, Args)]
        pub(crate) struct $name {
            #[command(flatten)]
            target: $target,
            $(#[$attr])*
            value: $ty,
        }
    )*};
}
value_args! {
    RenameArgs(ServiceTarget) {
        /// New service name.
        #[arg(value_name = "NEW_NAME")]
        value: String,
    }
    ReplicasArgs(ServiceTarget) {
        /// Number of replicas, from 1 to 100, or a variable reference such as
        /// `${{ vars.web_replicas }}`.
        #[arg(value_name = "REPLICAS")]
        value: Typed<u16>,
    }
    CommandArgs(ServiceTarget) {
        /// Entrypoint elements, after `--`. Each one is passed as is, spaces
        /// included, and may reference variables.
        #[arg(last = true, value_name = "COMMAND")]
        value: Vec<Template>,
    }
    ArgumentsArgs(ServiceTarget) {
        /// Argument elements, after `--`. Each one is passed as is, spaces
        /// included, and may reference variables.
        #[arg(last = true, value_name = "ARGUMENTS")]
        value: Vec<Template>,
    }
    DependsOnArgs(ServiceTarget) {
        /// Names of other services of the application, after `--`.
        #[arg(last = true, value_name = "SERVICES")]
        value: Vec<String>,
    }
    ImageArgs(ServiceTarget) {
        /// Image reference, such as `nginx:stable`. It may reference
        /// variables.
        #[arg(value_name = "IMAGE")]
        value: Template,
    }
    GitUrlArgs(ServiceTarget) {
        /// Git clone URL, or a path on the daemon's host.
        #[arg(value_name = "URL")]
        value: String,
    }
    GitBranchArgs(ServiceTarget) {
        /// Branch name.
        #[arg(value_name = "BRANCH")]
        value: String,
    }
    DockerfileArgs(ServiceTarget) {
        /// Dockerfile path, relative to the repository root.
        #[arg(value_name = "PATH")]
        value: Template,
    }
    ContextArgs(ServiceTarget) {
        /// Build context directory, relative to the repository root.
        #[arg(value_name = "PATH")]
        value: Template,
    }
    MountPathArgs(ServiceTarget) {
        /// Container path of the mount.
        #[arg(value_name = "PATH")]
        value: String,
    }
    FilePathArgs(ServiceTarget) {
        /// Container path of the file.
        #[arg(value_name = "TARGET")]
        value: String,
    }
    SecretPathArgs(ServiceTarget) {
        /// Container path of the secret file, under `/run/secrets`.
        #[arg(value_name = "TARGET")]
        value: String,
    }
    HealthPortArgs(ServiceTarget) {
        /// Container port, from 1 to 65535.
        #[arg(value_name = "PORT")]
        value: Typed<u16>,
    }
    HealthPathArgs(ServiceTarget) {
        /// Request path, starting with `/`.
        #[arg(value_name = "PATH")]
        value: Template,
    }
    HealthCommandArgs(ServiceTarget) {
        /// Command elements, after `--`.
        #[arg(last = true, value_name = "COMMAND")]
        value: Vec<Template>,
    }
    HealthIntervalArgs(ServiceTarget) {
        /// Seconds between checks, from 1 to 3600.
        #[arg(value_name = "SECONDS")]
        value: Typed<u32>,
    }
    HealthTimeoutArgs(ServiceTarget) {
        /// Seconds before a check counts as failed, from 1 to the interval.
        #[arg(value_name = "SECONDS")]
        value: Typed<u32>,
    }
    HealthStartPeriodArgs(ServiceTarget) {
        /// Seconds after the container starts, up to 3600.
        #[arg(value_name = "SECONDS")]
        value: Typed<u32>,
    }
    HealthRetriesArgs(ServiceTarget) {
        /// Consecutive failed checks, from 1 to 100.
        #[arg(value_name = "RETRIES")]
        value: Typed<u32>,
    }
    RepositoryUrlArgs(RepositoryTarget) {
        /// Git clone URL, or a path on the daemon's host.
        #[arg(value_name = "URL")]
        value: String,
    }
    RepositoryPathArgs(RepositoryTarget) {
        /// Path of the manifest file, relative to the repository root.
        #[arg(value_name = "PATH")]
        value: String,
    }
}
// Declares service-edit argument structs whose value is either given or
// removed with `--clear`; each struct documents both.
macro_rules! optional_args {
    ($($name:ident {
        $(#[$value_attr:meta])* value: $ty:ty,
        $(#[$clear_attr:meta])* clear $(,)?
    })*) => {$(
        #[derive(Debug, Args)]
        pub(crate) struct $name {
            #[command(flatten)]
            target: ServiceTarget,
            $(#[$value_attr])*
            #[arg(required_unless_present = "clear", conflicts_with = "clear")]
            value: Option<$ty>,
            $(#[$clear_attr])*
            #[arg(long)]
            clear: bool,
        }
    )*};
}
optional_args! {
    CommitArgs {
        /// Full commit hash to build.
        #[arg(value_name = "COMMIT")]
        value: String,
        /// Remove the pinned commit and build the branch head again.
        clear,
    }
    CpuArgs {
        /// CPU limit in millicores, from 1 to 1048576, where 1000 is one CPU.
        #[arg(value_name = "MILLICORES")]
        value: Typed<u32>,
        /// Remove the CPU limit.
        clear,
    }
    MemoryArgs {
        /// Memory limit in bytes, such as `268435456` for 256 MiB.
        #[arg(value_name = "BYTES")]
        value: Typed<u64>,
        /// Remove the memory limit.
        clear,
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
#[derive(Debug, Subcommand)]
pub(crate) enum SourceCommand {
    /// Run a prebuilt image, replacing the service's source.
    Image(ImageArgs),
    /// Build a Git repository with Docker, replacing the service's source.
    ///
    /// Build settings you don't pass get their defaults.
    Git(GitSourceArgs),
    /// Change the repository the service builds.
    Url(GitUrlArgs),
    /// Change the branch the service builds.
    Branch(GitBranchArgs),
    /// Pin the commit the service builds, or follow the branch again with
    /// `--clear`.
    Commit(CommitArgs),
    /// Change the Dockerfile the service builds.
    Dockerfile(DockerfileArgs),
    /// Change the directory the service's build uses as its context.
    Context(ContextArgs),
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
#[derive(Debug, Args)]
pub(crate) struct GitSourceArgs {
    #[command(flatten)]
    target: ServiceTarget,
    /// Git clone URL, a path on the daemon's host, or `self`: the manifest's
    /// own repository, at the commit the manifest was read from.
    #[arg(id = "git", value_name = "URL|self")]
    repository: BuildRepository,
    // Boxed to keep the CLI command enum small.
    #[command(flatten)]
    build: Box<GitBuildArgs>,
}
/// Repository a Git source builds, parsed like the manifest's `repository`:
/// `self` selects the manifest's own repository, anything else is a clone URL
/// or host path.
#[derive(Clone, Debug)]
pub(crate) enum BuildRepository {
    Manifest,
    Url(String),
}
impl From<String> for BuildRepository {
    fn from(value: String) -> Self {
        if value == "self" {
            Self::Manifest
        } else {
            Self::Url(value)
        }
    }
}
// Git build settings; each flag requires the `git` argument (`--git` or the
// positional repository of `source git`).
#[derive(Debug, Args)]
pub(crate) struct GitBuildArgs {
    /// Branch to build when no commit is pinned. Default: `main`. `self`
    /// doesn't take one.
    #[arg(long, requires = "git", value_name = "BRANCH")]
    branch: Option<String>,
    /// Build this full commit hash instead of the branch head. `self` doesn't
    /// take one.
    #[arg(long, requires = "git")]
    commit: Option<String>,
    /// Dockerfile path, relative to the repository root.
    #[arg(
        long,
        requires = "git",
        value_name = "PATH",
        default_value = "Dockerfile"
    )]
    dockerfile: Template,
    /// Build context directory, relative to the repository root.
    #[arg(long, requires = "git", value_name = "PATH", default_value = ".")]
    context: Template,
    /// Docker build argument. Repeat it to set several. Values aren't secret:
    /// Docker records them in the image.
    #[arg(long = "build-arg", value_name = "KEY=VALUE", requires = "git", value_parser = key_value)]
    build_args: Vec<(String, String)>,
    /// Dockerfile stage to build. By default, Docker builds the last stage.
    #[arg(long, requires = "git", value_name = "STAGE")]
    target: Option<Template>,
}
impl GitBuildArgs {
    /// Git source that builds `repository` with Docker using these settings.
    /// Fails when `self` is given a branch or commit: it always builds the
    /// manifest's commit.
    fn source(&self, repository: &BuildRepository) -> Result<Source> {
        let repository = match repository {
            BuildRepository::Url(url) => SourceRepository::Git(GitRepository {
                url: url.clone(),
                branch: self.branch.clone().unwrap_or_else(|| "main".into()),
                commit: self.commit.clone(),
            }),
            BuildRepository::Manifest if self.branch.is_some() || self.commit.is_some() => {
                return Err(CliError::new(
                    ErrorKind::Input,
                    "--branch and --commit do not apply to `self`, which builds the manifest's commit",
                ));
            }
            BuildRepository::Manifest => SourceRepository::Manifest(ManifestRepository::Manifest),
        };
        Ok(Source::Git {
            repository,
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
        })
    }
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
#[derive(Debug, Subcommand)]
pub(crate) enum MountCommand {
    /// Mount a volume at a container path, replacing any mount at that path.
    Set {
        #[command(flatten)]
        target: ServiceTarget,
        /// Name of a volume the application declares, with `app volume add`.
        volume: String,
        /// Absolute container path to mount it at.
        path: String,
        /// Mount the volume read-only. A writable mount makes the service's
        /// rollouts stop the old task first.
        #[arg(long)]
        read_only: bool,
    },
    /// Remove the mount at a container path.
    Remove(MountPathArgs),
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
    async fn emit<T: Clone>(
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
pub(crate) enum SecretMountCommand {
    /// List the secrets the application's services mount, as saved.
    List(ServiceListArgs),
    /// Mount a secret as a read-only file, replacing any secret mounted at
    /// the same path.
    ///
    /// A deployment fails while the secret has no value, or when the
    /// environment may not mount it.
    Add(AddSecretMountArgs),
    /// Remove the secret mounted at a container path.
    ///
    /// Removing a path the service doesn't mount a secret at fails.
    Remove(SecretPathArgs),
}
#[derive(Debug, Args)]
pub(crate) struct AddSecretMountArgs {
    #[command(flatten)]
    target: ServiceTarget,
    /// Secret name: one set with `app secret`, or one the manifest declares
    /// to be generated. It may reference variables, such as
    /// `${{ vars.<name> }}`, so each environment can mount its own secret.
    secret: Template,
    /// Absolute container path of the file, under `/run/secrets`.
    #[arg(value_name = "TARGET")]
    file: String,
}
impl SecretMountCommand {
    /// Lists saved secret mounts, or edits one service's mounts client-side
    /// and saves them together, like jobs: adding at a mounted target replaces
    /// that mount in place, and removing an unknown target is an input error.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let target = match self {
            Self::List(args) => {
                return args.emit(client, console, |service| &service.secrets).await;
            }
            Self::Add(args) => &args.target,
            Self::Remove(args) => &args.target,
        };
        let current = resolve_application(client, &target.app).await?;
        let mut secrets = current
            .application
            .spec()
            .services
            .iter()
            .find(|service| service.name == target.service)
            .ok_or_else(|| {
                CliError::new(
                    ErrorKind::Input,
                    format!("service {:?} was not found", target.service),
                )
            })?
            .secrets
            .clone();
        match self {
            Self::List(_) => unreachable!("listing returned above"),
            Self::Add(args) => {
                let mount = SecretMount {
                    name: args.secret.clone(),
                    target: args.file.clone(),
                };
                match secrets
                    .iter_mut()
                    .find(|secret| secret.target == mount.target)
                {
                    Some(existing) => *existing = mount,
                    None => secrets.push(mount),
                }
            }
            Self::Remove(args) => {
                let count = secrets.len();
                secrets.retain(|secret| secret.target != args.value);
                if secrets.len() == count {
                    return Err(CliError::new(
                        ErrorKind::Input,
                        format!("no secret is mounted at {:?}", args.value),
                    ));
                }
            }
        }
        let (target, edit) = target.field(ServiceEdit::Secrets(secrets));
        target
            .flags
            .save_loaded(client, console, &current, &edit)
            .await
    }
}
#[derive(Debug, Subcommand)]
pub(crate) enum FileCommand {
    /// List the files the application's services mount, as saved.
    List(ServiceListArgs),
    /// Mount a read-only file into a service, replacing any file at the same
    /// path.
    ///
    /// Give its content with exactly one of `--content`, `--from` or `--path`.
    /// A file holds at most 500 KiB and can't be empty.
    Add(AddFileArgs),
    /// Remove the file at a container path.
    Remove(FilePathArgs),
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
    /// Octal permissions of the file, from `0000` to `0777`, such as `0440`.
    /// Root owns the file.
    #[arg(long, default_value_t = FileMode::default())]
    mode: FileMode,
}
// Exactly one source of a file's content.
#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
pub(crate) struct FileSourceArgs {
    /// The file's content. Variables in it are never rendered.
    #[arg(long, value_name = "TEXT")]
    content: Option<String>,
    /// Read the content from this local file now, and save it in the
    /// manifest.
    #[arg(long, value_name = "LOCAL_FILE")]
    from: Option<std::path::PathBuf>,
    /// Read the content from this file in the manifest's repository, relative
    /// to its root, each time a deployment fetches the manifest. Only
    /// repository-backed applications can use it.
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
            Self::List(args) => return args.emit(client, console, |service| &service.files).await,
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
#[derive(Debug, Args)]
pub(crate) struct VolumeArgs {
    /// Application name or stable ID.
    app: String,
    /// Volume name.
    volume: String,
    #[command(flatten)]
    flags: EditFlags,
}
#[derive(Debug, Subcommand)]
pub(crate) enum VolumeCommand {
    /// Declare a named volume that services can mount.
    ///
    /// Each environment and preview gets its own Docker volume when it next
    /// deploys.
    Add(VolumeArgs),
    /// Remove a volume declaration.
    ///
    /// Remove the volume's mounts first. Docker keeps the volume and its data.
    Remove(VolumeArgs),
}
#[derive(Debug, Args)]
pub(crate) struct RouteTarget {
    /// Application name or stable ID.
    app: String,
    /// Hostname of the route: an exact name, or a wildcard such as
    /// `*.example.com`. It may reference variables, such as
    /// `${{ vars.domain }}`.
    hostname: String,
    #[command(flatten)]
    flags: EditFlags,
}
#[derive(Debug, Args)]
pub(crate) struct AddRouteArgs {
    #[command(flatten)]
    target: RouteTarget,
    /// Service that serves the route.
    service: String,
    /// HTTP port the service listens on inside its container.
    port: u16,
    /// `public` serves everyone, `private` only devices on the tailnet. The
    /// environment's visibility ceiling can make it stricter.
    #[arg(long, default_value = "private", value_parser = visibility())]
    visibility: Visibility,
    /// Stable route name, unique within the application. It selects the
    /// route in every environment and preview, such as with
    /// `env url --route`.
    #[arg(long)]
    name: Option<String>,
}
#[derive(Debug, Args)]
pub(crate) struct RedirectRouteArgs {
    #[command(flatten)]
    target: RouteTarget,
    /// Destination: an absolute `http` or `https` URL, without query or
    /// fragment. It may reference variables.
    #[arg(value_name = "URL")]
    to: Template,
    /// HTTP status of the redirect: `301`, `302`, `303`, `307` or `308`.
    #[arg(long, default_value_t = RedirectStatus::PermanentRedirect.into())]
    status: u16,
    /// Drop the request path and query instead of appending them to the
    /// destination.
    #[arg(long)]
    no_preserve_path: bool,
    /// `public` serves everyone, `private` only devices on the tailnet. The
    /// environment's visibility ceiling can make it stricter.
    #[arg(long, default_value = "private", value_parser = visibility())]
    visibility: Visibility,
    /// Stable route name, unique within the application. It selects the
    /// route in every environment and preview, such as with
    /// `env url --route`.
    #[arg(long)]
    name: Option<String>,
}
#[derive(Debug, Args)]
pub(crate) struct RouteNameArgs {
    #[command(flatten)]
    target: RouteTarget,
    /// New route name, unique within the application. Omit it to remove the
    /// name.
    name: Option<String>,
}
#[derive(Debug, Args)]
pub(crate) struct RouteVisibilityArgs {
    #[command(flatten)]
    target: RouteTarget,
    /// `public` serves everyone, `private` only devices on the tailnet.
    #[arg(value_parser = visibility())]
    visibility: Visibility,
}
#[derive(Debug, Subcommand)]
pub(crate) enum RouteCommand {
    /// List the application's deployed routes in every environment.
    ///
    /// Each route shows its visibility, its state, and the DNS records its
    /// hostname needs.
    List {
        /// Application name or stable ID.
        app: String,
    },
    /// Add an HTTPS route to a service.
    Add(AddRouteArgs),
    /// Add an HTTPS route that the gateway answers with a redirect.
    Redirect(RedirectRouteArgs),
    /// Change who can connect to a route.
    ///
    /// The next deployment withdraws the route from its old listener as it
    /// starts.
    Visibility(RouteVisibilityArgs),
    /// Name a route, or remove its name.
    ///
    /// A name selects the same route in every environment and preview, such
    /// as with `env url --route NAME`.
    Name(RouteNameArgs),
    /// Remove a route.
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
        current
            .map(|job| job.environment.clone())
            .unwrap_or_default()
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
        current.map(|job| job.secrets.clone()).unwrap_or_default()
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
#[derive(Debug, Args)]
pub(crate) struct ExportTarget {
    /// Application name or stable ID.
    app: String,
    /// Service of the application to export.
    service: String,
    #[command(flatten)]
    flags: EditFlags,
}
#[derive(Debug, Args)]
pub(crate) struct SetExportArgs {
    #[command(flatten)]
    target: ExportTarget,
    /// Applications whose environments may import the service. Separate
    /// several with commas, or repeat the option.
    #[arg(long, value_delimiter = ',', required = true, value_name = "APPS")]
    applications: Vec<String>,
    /// Let previews of those applications import it too. Without it,
    /// previews can't, even if they could before.
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
    ///
    /// Their services reach it as `<service>.<application>` once both sides
    /// have deployed the link.
    Set(SetExportArgs),
    /// Stop exporting a service.
    Remove(ExportTarget),
}
#[derive(Debug, Args)]
pub(crate) struct ImportTarget {
    /// Application name or stable ID.
    app: String,
    /// Application that exports the service.
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
    /// Environment of the exporting application to reach, such as `staging`
    /// or `${{ vars.auth_env }}`. By default, the one named like the
    /// importing environment.
    #[arg(long, value_name = "ENV")]
    environment: Option<Template>,
}
#[derive(Debug, Subcommand)]
pub(crate) enum ImportCommand {
    /// List the services the application imports, as saved.
    ///
    /// `env show` reports whether each one connects.
    List {
        /// Application name or stable ID.
        app: String,
    },
    /// Import another application's exported service, or change the
    /// environment it reaches.
    ///
    /// The application's services reach it as `<service>.<application>`
    /// once both sides have deployed the link.
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
#[derive(Debug, Subcommand)]
pub(crate) enum RepositoryCommand {
    /// Connect the application to a manifest file in a Git repository.
    ///
    /// Deployments then fetch the manifest from the repository. Every
    /// environment follows `--branch`, as do environments created later
    /// without their own, unless the manifest declares the environment's
    /// branch. Change an environment's branch with `env branch`. You can
    /// still edit the application: deployments of the connection's branch
    /// deploy your edits until a push changes the manifest file.
    Connect {
        #[command(flatten)]
        target: RepositoryTarget,
        /// Git clone URL, or a path on the daemon's host.
        // A distinct id keeps clap from storing this in the global `--url` endpoint.
        #[arg(id = "repository_url", value_name = "URL")]
        url: String,
        /// Path of the manifest file, relative to the repository root.
        path: String,
        /// Branch the environments follow.
        #[arg(long, default_value = "main")]
        branch: String,
        /// Pin this full commit hash instead of following the branch.
        #[arg(long)]
        commit: Option<String>,
        /// Whether to deploy pushes, as `app repository sync` sets it.
        #[arg(
            long = "sync",
            value_name = "MODE",
            value_parser = SyncMode::parser(),
            default_value = "off"
        )]
        sync: SyncMode,
        /// Seconds between polls with `--sync poll`, from 60 to 86400. By
        /// default, 300.
        #[arg(long, value_name = "SECONDS")]
        interval: Option<u32>,
    },
    /// Stop reading the manifest from Git, keeping the saved manifest.
    ///
    /// Edits are kept, and environments that followed a branch deploy the
    /// saved manifest. Declared environments lose their `branch`, `commit`
    /// and `sync`. It
    /// fails while services mount files from the repository: give them
    /// inline content first.
    Disconnect(RepositoryTarget),
    /// Change the repository every environment fetches the manifest from.
    Url(RepositoryUrlArgs),
    /// Change the manifest file every environment reads.
    Path(RepositoryPathArgs),
    /// Choose whether environments and previews deploy pushes.
    ///
    /// While sync is on, every preview, and every environment that opted in
    /// with `env sync` and follows an unpinned branch, redeploys when its
    /// branch moves past the head of its last deployment.
    Sync {
        #[command(flatten)]
        target: RepositoryTarget,
        /// How to detect pushes.
        #[arg(value_parser = SyncMode::parser())]
        mode: SyncMode,
        /// Seconds between polls with `poll`, from 60 to 86400. By default,
        /// 300.
        #[arg(long, value_name = "SECONDS")]
        interval: Option<u32>,
    },
    /// Discard the edits made since the manifest was last read from the
    /// repository, saving that manifest again.
    ///
    /// `app show` lists the edits. Deployments of the connection's branch
    /// then deploy the manifest as read, until a push changes it.
    Reset(RepositoryTarget),
    /// Show the GitHub webhook that reports pushes, or generate its secret.
    Webhook {
        #[command(subcommand)]
        command: WebhookCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum WebhookCommand {
    /// Show the payload URL to configure in GitHub, and when the secret was
    /// generated.
    ///
    /// The URL appears once the daemon sets `ingress.webhook_hostname`.
    Show {
        /// Application name or stable ID.
        app: String,
    },
    /// Generate a new webhook secret, replacing the previous one, and print
    /// it.
    ///
    /// The previous secret stops working at once. The new one is shown only
    /// this time.
    Rotate {
        /// Application name or stable ID.
        app: String,
        /// Skip the confirmation prompt. You need it when not on a terminal.
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
    /// Change the variable's value in this environment instead of its
    /// default. The environment must exist.
    #[arg(long = "env", value_name = "ENV")]
    environment: Option<String>,
    /// Change the variable's value in every preview,
    /// `[spec.previews.variables]`, instead of its default.
    #[arg(long, conflicts_with = "environment")]
    previews: bool,
    #[command(flatten)]
    flags: EditFlags,
}
#[derive(Debug, Subcommand)]
pub(crate) enum VariableCommand {
    /// Set a variable's default value, or its value in one environment or in
    /// previews.
    ///
    /// An environment's or the previews' value overrides the default.
    Set {
        #[command(flatten)]
        target: VariableTarget,
        /// `true`, `false` and integers keep their type, and anything else is
        /// text. Text may reference system variables, such as
        /// `${{ env.name }}`. Text in double quotes stays text, without the
        /// quotes.
        value: String,
        /// Keep the value as text even when it looks like a boolean or an
        /// integer.
        #[arg(long)]
        string: bool,
    },
    /// Remove a variable's default value, or its value in one environment or
    /// in previews.
    ///
    /// Removing a value that isn't set fails.
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
impl SourceCommand {
    /// Maps the subcommand to its target service and source edit.
    fn edit(&self) -> Result<(&ServiceTarget, ApplicationEdit)> {
        Ok(match self {
            Self::Image(args) => args.target.field(ServiceEdit::Image(args.value.clone())),
            Self::Git(args) => args
                .target
                .field(ServiceEdit::Source(args.build.source(&args.repository)?)),
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
        })
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
            Self::Reset(target) => return target.flags.reset(client, console, &target.app).await,
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

    /// Confirms and saves the manifest `app` last read from its repository,
    /// discarding its edits, under the same preconditions as `save_loaded`.
    async fn reset(&self, client: &Client, console: &mut Console, app: &str) -> Result<()> {
        let current = resolve_application(client, app).await?;
        let name = &current.application.metadata().name;
        let Some(repository) = &current.repository else {
            return Err(CliError::new(
                ErrorKind::Input,
                format!("application {name:?} is not repository-backed"),
            ));
        };
        if !current.edited() {
            return console.info(format_args!(
                "application {name:?} has no edits; it matches its repository manifest"
            ));
        }
        console
            .confirm(
                self.yes,
                &format!(
                    "Discard the edits to application {name:?} since its manifest was last read from the repository{}?",
                    if self.deployment.deploy {
                        " and deploy"
                    } else {
                        ""
                    }
                ),
            )
            .await?;
        let request = ApplyApplicationRequest {
            manifest: repository.to_manifest(),
            expected_generation: expected_generation(
                self.force,
                self.expected_generation,
                current.generation,
            ),
            expected_application_id: (!self.force).then(|| current.application.id().to_string()),
        };
        let options = ApplyOptions {
            force: self.force,
            deploy: self.deployment.deploy,
        };
        let saved =
            retry_transport(|| client.apply_application_with_options(&request, &options)).await?;
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
