//! Field editing commands share confirmation, revision protection, and deployment output.
use crate::{
    cli::{CreateArgs, DeploymentArgs, expected_generation},
    commands::{resolve_application, wait_for_operation},
    error::{CliError, ErrorKind, Result},
    output::{Console, reports::SavedDeploymentReport},
    support::retry_transport,
};
use clap::{
    Args,
    builder::{PossibleValue, PossibleValuesParser, TypedValueParser},
};
use piqueld_client::{
    ApplicationView, ApplyApplicationRequest, ApplyOptions, Client, SavedApplication, Template,
    Typed, Visibility,
    edit::{ApplicationEdit, EditOptions},
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
    clap::builder::PossibleValuesParser::new(["public", "private"]).map(|value| {
        value.parse().expect(
            "PossibleValuesParser accepts only public and private, both valid Visibility values",
        )
    })
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
    #[arg(long, short)]
    yes: bool,
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

mod jobs;
mod links;
mod mounts;
mod repositories;
mod routes;
mod secrets;
mod services;
mod sources;
mod variables;
pub(crate) use jobs::*;
pub(crate) use links::*;
pub(crate) use mounts::*;
pub(crate) use repositories::*;
pub(crate) use routes::*;
pub(crate) use secrets::*;
pub(crate) use services::*;
pub(crate) use sources::*;
pub(crate) use variables::*;
