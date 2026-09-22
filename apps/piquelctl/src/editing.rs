//! Field editing commands share confirmation, revision protection, and deployment output.
use crate::{
    cli::{Cli, CreateArgs, DeploymentArgs},
    commands::{resolve_application, wait_for_operation},
    error::Result,
    output::{Console, reports::SavedDeploymentReport},
    support::{confirm, retry_transport},
};
use clap::{Args, Subcommand};
use piqueld_client::{
    Build, Client, GitRepository, HealthCheck, Mount, RepositoryManifest, SavedApplication,
    Service, Source, Volume,
    edit::{ApplicationEdit, EditOptions, ServiceEdit},
};

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
#[derive(Debug, Args)]
pub(crate) struct Target {
    /// Application name or stable ID.
    app: String,
    /// Logical service name.
    service: String,
    #[command(flatten)]
    flags: EditFlags,
}
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
#[derive(Debug, Args)]
pub(crate) struct GitBuildArgs {
    #[arg(long, requires = "git", default_value = "main")]
    branch: String,
    #[arg(long, requires = "git")]
    commit: Option<String>,
    #[arg(long, requires = "git", default_value = "Dockerfile")]
    dockerfile: String,
    #[arg(long, requires = "git", default_value = ".")]
    context: String,
}
impl GitBuildArgs {
    fn source(&self, url: &str) -> Source {
        Source::Git {
            repository: GitRepository {
                url: url.into(),
                branch: self.branch.clone(),
                commit: self.commit.clone(),
            },
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
        key: String,
        value: String,
    },
    /// Remove one variable.
    Remove {
        #[command(flatten)]
        target: Target,
        key: String,
    },
}
#[derive(Debug, Subcommand)]
pub(crate) enum MountCommand {
    /// Add or replace the mount at a container target path.
    Set {
        #[command(flatten)]
        target: Target,
        volume: String,
        path: String,
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
        port: u16,
        #[arg(long, default_value = "/health")]
        path: String,
        #[arg(long, default_value_t = 10)]
        interval: u32,
        #[arg(long, default_value_t = 3)]
        check_timeout: u32,
    },
    /// Configure a command health check; command elements follow --.
    Command {
        #[command(flatten)]
        target: Target,
        #[arg(long, default_value_t = 10)]
        interval: u32,
        #[arg(long, default_value_t = 3)]
        check_timeout: u32,
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
    app: String,
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
pub(crate) struct RepositoryTarget {
    app: String,
    #[command(flatten)]
    flags: EditFlags,
}
#[derive(Debug, Args)]
pub(crate) struct RepositoryText {
    #[command(flatten)]
    target: RepositoryTarget,
    value: String,
}
#[derive(Debug, Subcommand)]
pub(crate) enum RepositoryCommand {
    /// Enable repository ownership of services and volumes.
    Connect {
        #[command(flatten)]
        target: RepositoryTarget,
        url: String,
        path: String,
        #[arg(long, default_value = "main")]
        branch: String,
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
        #[arg(required_unless_present = "clear", conflicts_with = "clear")]
        value: Option<String>,
        #[arg(long)]
        clear: bool,
    },
    /// Change the manifest file path.
    Path(RepositoryText),
}

impl ServiceCommand {
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
                    healthcheck: None,
                    resources: None,
                };
                return save(
                    cli,
                    client,
                    console,
                    &args.target.app,
                    &args.target.flags,
                    &ApplicationEdit::AddService(service),
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
impl RepositoryCommand {
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

pub(crate) async fn save(
    cli: &Cli,
    client: &Client,
    console: &mut Console,
    app: &str,
    flags: &EditFlags,
    edit: &ApplicationEdit,
) -> Result<()> {
    let current = resolve_application(client, app).await?;
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
