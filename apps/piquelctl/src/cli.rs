//! Clap command-line surface. `///` on derived items is user-facing help text, so
//! internal notes use `//`. Flattened `Args` structs keep `//` because their doc
//! comments would become the `about` text of commands that flatten them.
//!
//! The help text is also published as `docs/user/reference/cli-commands.md`, so
//! it follows `docs/dev/documentation.md`: the first paragraph is the `-h`
//! summary, later paragraphs only appear with `--help`.
use clap::{Args, Parser, Subcommand};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// Heading of the options every command accepts, which `--help` lists apart
/// from each command's own.
const GLOBAL: &str = "Global options";

/// Parses one of a fixed set of values, so clap lists them in `--help`.
/// `$values` yields the values, which have an `as_str` form that `$parse`
/// turns back into the value, and `$help`, when given, describes each one.
///
/// ```text
/// one_of!(Preset::ALL, Preset::parse)  →  `--preset <PRESET>  [possible values: read-only, …]`
/// ```
//
// Clap's `ValueEnum` would do this, but the orphan rule forbids implementing
// it here for `piqueld-core` types. Values go through their string form
// instead: clap only accepts the listed strings, each the `as_str` of a
// value, so `$parse` cannot fail on them.
macro_rules! one_of {
    ($values:expr, $parse:path $(, $help:expr)?) => {
        clap::builder::TypedValueParser::map(
            clap::builder::PossibleValuesParser::new($values.into_iter().map(|value| {
                let possible = clap::builder::PossibleValue::new(value.as_str());
                $(let possible = possible.help(($help)(value));)?
                possible
            })),
            |value: String| $parse(&value).expect("every listed value parses"),
        )
    };
}
pub(crate) use one_of;

/// Operate a piqueld control plane.
///
/// `piquelctl` connects to the daemon's Unix socket, `/run/piqueld/piqueld.sock`,
/// unless `--socket`, `--url`, the `PIQUELD_SOCKET` or `PIQUELD_URL` variable,
/// or a connection profile names another endpoint. It authenticates with
/// `PIQUELD_TOKEN`, or with the login that `piquelctl login` saved. On the
/// daemon host, root and the daemon's user need no login.
#[derive(Debug, Parser)]
#[command(name = "piquelctl", version)]
pub(crate) struct Cli {
    // Where the effective endpoint and timeout came from, filled in by
    // `Profiles::resolve` and shown in connection diagnostics.
    #[arg(skip)]
    pub(crate) connection_sources: crate::profiles::ConnectionSources,

    /// Connect with this profile from the profiles files.
    ///
    /// Defaults to `PIQUELD_PROFILE`, else to the profile named `default` if
    /// there is one.
    #[arg(long, global = true, value_name = "NAME", help_heading = GLOBAL)]
    pub(crate) profile: Option<String>,
    /// Read profiles only from this file.
    ///
    /// Defaults to `PIQUELD_PROFILES_FILE`. Without either, `piquelctl` reads
    /// `/etc/piqueld/profiles.toml`, then `~/.config/piqueld/profiles.toml`
    /// (in `$XDG_CONFIG_HOME` when it is set), whose profiles replace those of
    /// the same name.
    #[arg(long, global = true, value_name = "PATH", help_heading = GLOBAL)]
    pub(crate) profiles_file: Option<PathBuf>,

    /// Connect to the daemon's Unix socket at this path.
    ///
    /// It overrides `PIQUELD_SOCKET`, `PIQUELD_URL` and the profile's endpoint.
    /// Without any of them, `piquelctl` uses `/run/piqueld/piqueld.sock`.
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        conflicts_with = "url",
        help_heading = GLOBAL
    )]
    pub(crate) socket: Option<PathBuf>,

    #[command(flatten)]
    pub(crate) auth: AuthArgs,

    /// Connect to the daemon over HTTP or HTTPS at this address.
    ///
    /// Give an origin without a path, query or credentials, such as
    /// `http://127.0.0.1:7845` or `https://piqueld.example.ts.net`. It
    /// overrides `PIQUELD_URL`, `PIQUELD_SOCKET` and the profile's endpoint.
    #[arg(
        long,
        global = true,
        value_name = "URL",
        conflicts_with = "socket",
        help_heading = GLOBAL
    )]
    pub(crate) url: Option<String>,

    /// Give up on the command after this long, such as `500ms`, `30s`, `2m` or `1h`.
    ///
    /// A number without a unit is seconds. The limit also applies to each
    /// request. Confirmation prompts don't count towards it, and it doesn't
    /// limit the session of `app exec` and `preview exec`, or how long `login`
    /// waits for your approval. `PIQUELD_TIMEOUT` and a profile's `timeout`
    /// replace the default.
    #[arg(
        long,
        global = true,
        default_value = "30s",
        value_name = "DURATION",
        value_parser = parse_duration,
        help_heading = GLOBAL
    )]
    pub(crate) timeout: Duration,

    /// Print the result as JSON on standard output.
    ///
    /// Information, warnings and errors on standard error become one JSON
    /// object per line.
    #[arg(long, global = true, help_heading = GLOBAL)]
    pub(crate) json: bool,

    /// Hide human-readable results, information and progress.
    ///
    /// JSON results, warnings, errors and prompts still appear.
    #[arg(long, short, global = true, help_heading = GLOBAL)]
    pub(crate) quiet: bool,
    /// Never prompt, even on a terminal.
    ///
    /// Commands that ask for confirmation then fail unless you pass `--yes`.
    #[arg(long, global = true, help_heading = GLOBAL)]
    pub(crate) noninteractive: bool,

    #[command(subcommand)]
    pub(crate) command: Command,
}

impl Cli {
    /// The daemon socket: `--socket`, or the daemon's default one.
    pub(crate) fn socket_path(&self) -> &Path {
        self.socket
            .as_deref()
            .unwrap_or(Path::new(crate::support::DEFAULT_SOCKET))
    }

    /// Human description of the configured endpoint.
    ///
    /// ```text
    /// --url http://127.0.0.1:7845  →  TCP http://127.0.0.1:7845
    /// (no endpoint)                →  Unix socket /run/piqueld/piqueld.sock
    /// ```
    pub(crate) fn transport(&self) -> String {
        match &self.url {
            Some(url) => format!("TCP {url}"),
            None => format!("Unix socket {}", self.socket_path().to_string_lossy()),
        }
    }
}

// Account selection and authentication transport options.
#[derive(Debug, Args)]
pub(crate) struct AuthArgs {
    /// Act as this saved login, by username or account ID.
    ///
    /// Defaults to the login you saved last for this daemon. `PIQUELD_TOKEN`
    /// takes precedence over saved logins.
    #[arg(long, global = true, help_heading = GLOBAL)]
    pub(crate) account: Option<String>,

    /// Allow sending credentials over plain HTTP to a remote daemon.
    ///
    /// Only use it when something else encrypts the connection, such as
    /// Tailscale: `piquelctl` adds no encryption and doesn't check. HTTPS,
    /// loopback addresses and the Unix socket don't need it.
    #[arg(long, global = true, help_heading = GLOBAL)]
    pub(crate) allow_insecure_http: bool,
}

// Top-level commands. `Profiles` and `Login` are dispatched by `main` before the
// timeout-bounded `commands::run`; everything else goes through it.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Sign in with a passkey through your browser, and save the login.
    ///
    /// `login` prints a link and a code. Open the link in any browser, even on
    /// another machine, and approve the code with your passkey. It waits until
    /// the code expires, at most 15 minutes, whatever `--timeout` says. It then
    /// saves the login for this daemon in `~/.config/piqueld/credentials.json`
    /// (in `$XDG_CONFIG_HOME` when it is set), or in the file
    /// `PIQUELD_CREDENTIALS_FILE` names.
    ///
    /// Grant options limit what the login may do; without them, it has all your
    /// account's access. Here `--app` takes application IDs, not names, which
    /// cannot be looked up before signing in.
    #[command(mut_arg("applications", |arg| arg.value_name("APPLICATION_ID")))]
    Login {
        #[command(flatten)]
        limit: crate::accounts::GrantArgs,
    },
    /// Sign out: revoke the current credential and delete its saved copy.
    ///
    /// With `PIQUELD_TOKEN` set, it revokes that token and leaves saved logins
    /// alone.
    Logout,
    /// Show who you are signed in as and what the current credential may do.
    Whoami,
    /// Manage accounts: list, edit and delete them, change their grants and passkeys, and create and revoke invitation and enrollment links.
    Account {
        #[command(subcommand)]
        command: crate::accounts::AccountCommand,
    },
    /// Create, list and revoke your API tokens and sessions, or revoke an account's.
    Token {
        #[command(subcommand)]
        command: crate::accounts::TokenCommand,
    },
    /// Read the audit trail of API requests, newest first.
    ///
    /// Each line is one request: when it was made, its outcome and status, what
    /// it did and to which application or environment, who made it with which
    /// credential, and from where. With `audit:read`, you see every account's
    /// requests. Without it, you see only your own account's, and an API token
    /// only its own.
    Audit(crate::accounts::AuditArgs),
    /// Print a one-time link that creates a new administrator account.
    ///
    /// Use it to regain access when no administrator can sign in. Run it on the
    /// daemon host as root or as the daemon's user, such as with `sudo`: the
    /// daemon serves it only over its Unix socket. The link works once within
    /// 24 hours and replaces any earlier one. Issuing it raises a security
    /// notification.
    RecoverAdmin,
    /// Print a one-time link that signs a browser in as the host operator.
    ///
    /// The browser gets full access for 12 hours. The link works once within 10
    /// minutes. Run it on the daemon host as root or as the daemon's user: the
    /// daemon serves it only over its Unix socket, to the host operator. It
    /// never sends `PIQUELD_TOKEN` or a saved login. Issuing it raises a
    /// security notification.
    SignInLink,
    /// Print the link that creates the first account.
    ///
    /// Run it on the daemon host: the daemon serves it only over its Unix
    /// socket, and only until the first account exists.
    SetupLink {
        /// Also open the link in your default browser.
        #[arg(long)]
        open: bool,
    },
    /// List your connection profiles and their endpoints.
    ///
    /// It reads the profiles files without contacting a daemon, and ignores
    /// `--profile` and the endpoint options.
    Profiles,
    /// Manage the daemon's encryption key for secrets.
    Secrets {
        #[command(subcommand)]
        action: crate::secrets::KeyAction,
    },
    /// Manage notification destinations and their deliveries.
    Notification {
        #[command(subcommand)]
        command: crate::notifications::NotificationCommand,
    },
    /// Show whether the daemon responds, its version, and the health of its subsystems.
    Status,
    /// Show the daemon process's resource usage, storage, retained history,
    /// and queues, measured at most every 5 seconds (requires system:read).
    Resources,
    /// Show the daemon's effective host settings, read-only (requires
    /// system:read). Change them in its TOML configuration and restart it.
    Settings,
    /// Check this CLI's connection and the installation's health.
    ///
    /// Shows the resolved endpoint and credential, then each daemon check as
    /// pass, warn, fail, or skip with a next step. Exits 6 when a check fails.
    Doctor,
    /// Inspect the daemon's DNS providers.
    Dns {
        #[command(subcommand)]
        command: DnsCommand,
    },
    /// Create, inspect, edit and deploy applications.
    App {
        #[command(subcommand)]
        command: AppCommand,
    },
    /// Create, inspect, deploy and delete an application's environments.
    Env {
        #[command(subcommand)]
        command: crate::environments::EnvCommand,
    },
    /// Create, inspect, deploy and delete previews: disposable deployments of a branch.
    Preview {
        #[command(subcommand)]
        command: crate::previews::PreviewCommand,
    },
    /// Inspect Git source builds and job runs, and read their output.
    Builds(BuildArgs),
    /// Show an operation, waiting until it ends.
    ///
    /// Deployments, deletions and other long tasks run as operations, whose IDs
    /// commands print. Unless you pass `--no-wait`, it exits with code 5 if the
    /// operation failed.
    Operation(OperationArgs),
    /// Summarize deployment outcomes, retries, durations, and failure codes
    /// over a time window, for every readable application or one environment.
    Analytics(crate::analytics::AnalyticsArgs),
    /// Read one page of the event history, oldest first.
    ///
    /// Events record what happened to applications, environments and the
    /// daemon, such as edits, deployments and failures.
    Events {
        /// Only show events of the application with this ID, and of all its
        /// environments, including deleted ones.
        #[arg(long, value_name = "APPLICATION_ID")]
        application: Option<String>,
        /// Only show events of the environment with this ID, even a deleted one.
        #[arg(long, value_name = "ENVIRONMENT_ID")]
        environment: Option<String>,
        #[command(flatten)]
        page: crate::support::PageArgs,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum DnsCommand {
    /// Check every DNS provider's credentials and zones now.
    ///
    /// The daemon otherwise checks them every hour. It reads credential files
    /// only when it starts, so restart it after changing one. Needs
    /// `system:operate`.
    Refresh,
}

#[derive(Debug, Subcommand)]
pub(crate) enum AppCommand {
    /// Create an empty application.
    ///
    /// Add services, volumes and routes to it with the other `app` commands.
    /// Needs `apps:create`.
    Create(CreateArgs),
    /// Add, change and remove an application's services.
    Service {
        #[command(subcommand)]
        command: crate::editing::ServiceCommand,
    },
    /// List, add and remove the read-only files services mount.
    File {
        #[command(subcommand)]
        command: crate::editing::FileCommand,
    },
    /// Declare and remove named volumes.
    Volume {
        #[command(subcommand)]
        command: crate::editing::VolumeCommand,
    },
    /// Add, change and remove HTTPS routes and redirects.
    Route {
        #[command(subcommand)]
        command: crate::editing::RouteCommand,
    },
    /// Add, reorder and remove the jobs that run before each rollout.
    Job {
        #[command(subcommand)]
        command: crate::editing::JobCommand,
    },
    /// Let other applications reach this application's services privately.
    Export {
        #[command(subcommand)]
        command: crate::editing::ExportCommand,
    },
    /// Reach the services other applications export, as `<service>.<application>`.
    Import {
        #[command(subcommand)]
        command: crate::editing::ImportCommand,
    },
    /// Set and remove manifest variables, and their values per environment and for previews.
    Variable {
        #[command(subcommand)]
        command: crate::editing::VariableCommand,
    },
    /// Connect the application to a manifest in Git, and manage that connection.
    Repository {
        #[command(subcommand)]
        command: crate::editing::RepositoryCommand,
    },
    /// List applications with their environments and status.
    List,
    /// Show an application and its environments.
    Show {
        /// Application name or stable ID.
        name_or_id: String,
    },
    /// Show recent Docker logs of the application's only environment.
    ///
    /// With several environments, use `env logs`. Needs `logs:read`.
    Logs {
        /// Application name or stable ID.
        name_or_id: String,
        #[command(flatten)]
        window: LogArgs,
    },
    /// Run a one-off command in a running task of a service.
    ///
    /// The command shares the task's image, environment, secrets, mounts and
    /// networks. Its output streams to your terminal, and `piquelctl` exits
    /// with its exit code. Stopping `piquelctl` early closes the stream, but the
    /// command may keep running. `--timeout` limits connecting, not the
    /// session. Needs `apps:exec`.
    Exec(crate::exec::ExecArgs<crate::exec::ApplicationTarget>),
    /// Declare the secrets piqueld generates for each environment.
    ///
    /// These commands edit `spec.secrets`. Changing or removing a declaration
    /// keeps values already generated; replace one with `env secret
    /// regenerate`.
    GeneratedSecret {
        #[command(subcommand)]
        command: crate::editing::GeneratedSecretCommand,
    },
    /// Manage the application's stored secrets and who may mount them.
    ///
    /// Values are write-only: no command prints them back.
    Secret {
        /// Application name or stable ID.
        application: String,
        #[command(subcommand)]
        action: crate::secrets::SecretAction,
    },
    /// Check a TOML manifest locally, without contacting a daemon.
    Validate {
        /// Manifest file: UTF-8 TOML of at most 2 MiB.
        #[arg(long, value_name = "PATH")]
        file: PathBuf,
    },
    /// Show what saving a TOML manifest would change, without saving it.
    ///
    /// It exits with code 3 when the plan is blocked.
    Plan(ManifestArgs),
    /// Save a TOML manifest, creating or replacing the application it names.
    ///
    /// Without `--deploy`, it only saves the manifest. With it, it also deploys
    /// every environment that builds, and waits for each deployment.
    Apply(ApplyArgs),
    /// Delete an application with all its environments and previews.
    ///
    /// The environments' named volumes are kept; the previews' volumes are
    /// deleted. Needs `apps:delete`.
    Delete(DeleteArgs),
    /// Retry the latest operation of the application's only environment.
    ///
    /// It runs the operation again with its saved inputs, without fetching the
    /// manifest or resolving sources again. If the operation is still in
    /// progress, it waits for it instead. With several environments, use
    /// `env reconcile`. A stopped environment starts. Needs `apps:deploy`.
    Reconcile(TargetArgs),
    /// Stop the application's only environment.
    ///
    /// Its routes are withdrawn and its services scale to zero, until it is
    /// started or deployed again. With several environments, use `env stop`.
    /// Needs `apps:deploy`.
    Stop(TargetArgs),
    /// Start the application's only environment again.
    ///
    /// Like `app reconcile`, it retries the latest operation with its saved
    /// inputs. With several environments, use `env start`. Needs
    /// `apps:deploy`.
    Start(TargetArgs),
    /// Deploy the application's only environment.
    ///
    /// It fetches the manifest again, from the environment's branch for a
    /// repository-backed application, and resolves images and Git sources
    /// again. With several environments, use `env deploy`. Needs `apps:deploy`.
    Deploy(DeployArgs),
    /// Rename an application.
    ///
    /// It fails while an operation of the application is running, and for
    /// repository-backed applications, whose manifests set their name. Update
    /// `metadata.name` in your manifest files afterwards.
    Rename(RenameArgs),
    /// Print the saved manifest as TOML.
    Manifest {
        /// Application name or stable ID.
        name_or_id: String,
    },
    /// List the application's releases, newest first.
    ///
    /// A release records what a deployment prepared: the manifest, and each
    /// service's image and where it came from.
    Releases {
        /// Application name or stable ID.
        name_or_id: String,
        /// Read the page after this cursor, which the previous page printed.
        #[arg(long)]
        cursor: Option<String>,
    },
}

#[derive(Debug, Args)]
pub(crate) struct CreateArgs {
    /// Name of the new application, unused by any other.
    pub(crate) name: String,
    #[command(flatten)]
    pub(crate) deployment: DeploymentArgs,
    /// Skip the confirmation prompt.
    #[arg(long)]
    pub(crate) yes: bool,
}

#[derive(Debug, Args)]
pub(crate) struct BuildArgs {
    #[command(subcommand)]
    pub(crate) command: BuildCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum BuildCommand {
    /// List Git source builds and job runs, newest first.
    List {
        /// Only show the builds of the environments of the application with this ID.
        #[arg(long, value_name = "APPLICATION_ID")]
        application: Option<String>,
        /// Only show the builds of the environment with this ID.
        #[arg(long, value_name = "ENVIRONMENT_ID")]
        environment: Option<String>,
        /// Read the page after this cursor, which the previous page printed.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Read the newest page of a build's output.
    ///
    /// To read older output, pass the `--before` offset it prints. Needs
    /// `logs:read`.
    Logs {
        /// Build ID, from `builds list`.
        id: i64,
        /// Read the output before this byte offset, which the previous page printed.
        #[arg(long, value_name = "OFFSET")]
        before: Option<i64>,
    },
}

#[derive(Debug, Args)]
pub(crate) struct ManifestArgs {
    /// Manifest file: UTF-8 TOML of at most 2 MiB.
    #[arg(long, value_name = "PATH")]
    pub(crate) file: PathBuf,
    /// Fail unless the application is at this generation; `0` requires that it
    /// doesn't exist.
    #[arg(long, value_name = "GENERATION")]
    pub(crate) expected_generation: Option<u64>,
    /// Render the manifest for this environment, and compare it with that
    /// environment's deployment.
    ///
    /// Defaults to the application's only environment. With several, the plan
    /// compares the manifest as written.
    #[arg(long = "env", value_name = "ENV")]
    pub(crate) environment: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct ApplyArgs {
    #[command(flatten)]
    pub(crate) deployment: DeploymentArgs,
    /// Manifest file: UTF-8 TOML of at most 2 MiB.
    #[arg(long, value_name = "PATH")]
    pub(crate) file: PathBuf,
    /// Fail unless the application is at this generation; `0` requires that it
    /// doesn't exist.
    ///
    /// Defaults to the generation it inspects, or `0` when no application has
    /// the manifest's name.
    #[arg(long, value_name = "GENERATION")]
    pub(crate) expected_generation: Option<u64>,

    /// Skip the confirmation prompt.
    #[arg(long)]
    pub(crate) yes: bool,

    /// Replace whichever application has the manifest's name now, or create it.
    ///
    /// It skips the generation and identity checks, but still asks for
    /// confirmation.
    #[arg(long, conflicts_with = "expected_generation")]
    pub(crate) force: bool,
}

// Shared `--deploy` / `--no-wait` flags for commands that save configuration.
#[derive(Debug, Args)]
pub(crate) struct DeploymentArgs {
    /// Also deploy the saved configuration.
    #[arg(long)]
    pub(crate) deploy: bool,
    /// Return once the change is saved, or its deployment accepted, without
    /// waiting for the deployment to end.
    #[arg(long)]
    pub(crate) no_wait: bool,
}

#[derive(Debug, Args)]
pub(crate) struct DeleteArgs {
    /// Application name or stable ID.
    pub(crate) name_or_id: String,
    /// Names of all the application's environments, comma-separated.
    ///
    /// Required when it has several, to confirm that you delete them all.
    #[arg(long, value_delimiter = ',', value_name = "NAMES")]
    pub(crate) environments: Vec<String>,
    #[command(flatten)]
    pub(crate) deletion: DeletionFlags,
}

// Precondition, confirmation, and waiting flags shared by deletions.
#[derive(Debug, Args)]
pub(crate) struct DeletionFlags {
    /// Fail unless the application is at this generation.
    ///
    /// Defaults to the generation it inspects.
    #[arg(long, value_name = "GENERATION")]
    pub(crate) expected_generation: Option<u64>,

    /// Skip the confirmation prompt.
    #[arg(long)]
    pub(crate) yes: bool,

    /// Skip the generation check.
    ///
    /// It still asks for confirmation.
    #[arg(long, conflicts_with = "expected_generation")]
    pub(crate) force: bool,

    /// Return once the daemon accepts the deletion, without waiting for it to end.
    #[arg(long)]
    pub(crate) no_wait: bool,
}

#[derive(Debug, Args)]
pub(crate) struct OperationArgs {
    /// Operation ID.
    pub(crate) operation_id: String,

    /// Show the operation's current state instead of waiting for it to end.
    #[arg(long)]
    pub(crate) no_wait: bool,
}

/// Parses a positive integer duration with an optional unit (`ms`, `s`, `m`, `h`,
/// `d`; bare numbers are seconds). Used for `--timeout`, `PIQUELD_TIMEOUT`,
/// profiles, and the `analytics` window.
///
/// ```text
/// "500ms" → 500ms    "30" → 30s    "2m" → 120s    "7d" → 604800s    "0s" / "1.5s" / "-1" → error
/// ```
pub(crate) fn parse_duration(value: &str) -> std::result::Result<Duration, String> {
    /// Converts a number of the unit, `None` when it overflows.
    type Unit = fn(u64) -> Option<Duration>;
    // `ms` comes first because it ends with `s`.
    let units: [(&str, Unit); 5] = [
        ("ms", |number| Some(Duration::from_millis(number))),
        ("s", |number| Some(Duration::from_secs(number))),
        ("m", |number| Duration::from_secs(number).checked_mul(60)),
        ("h", |number| {
            Duration::from_secs(number).checked_mul(60 * 60)
        }),
        ("d", |number| {
            Duration::from_secs(number).checked_mul(24 * 60 * 60)
        }),
    ];
    let (number, unit) = units
        .into_iter()
        .find_map(|(suffix, unit)| value.strip_suffix(suffix).map(|number| (number, unit)))
        .unwrap_or((value, units[1].1));
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("expected an integer duration such as 500ms, 30s, 2m, or 7d".to_owned());
    }
    let duration = number
        .parse()
        .ok()
        .and_then(unit)
        .ok_or_else(|| "duration is too large".to_owned())?;
    if duration.is_zero() {
        return Err("duration must be greater than zero".to_owned());
    }
    Ok(duration)
}

/// The application revision a change is conditioned on: `--expected-generation`,
/// else the inspected `current` one; none with `--force`, which overrides it.
pub(crate) fn expected_generation(force: bool, expected: Option<u64>, current: u64) -> Option<u64> {
    (!force).then_some(expected.unwrap_or(current))
}

#[derive(Debug, Args)]
pub(crate) struct RenameArgs {
    /// Application name or stable ID.
    pub(crate) name_or_id: String,
    /// New name of the application, unused by any other.
    pub(crate) new_name: String,
    #[command(flatten)]
    pub(crate) edit: crate::editing::EditFlags,
}

#[derive(Debug, Args)]
pub(crate) struct DeployArgs {
    #[command(flatten)]
    pub(crate) target: TargetArgs,
    #[command(flatten)]
    pub(crate) revision: RevisionArgs,
}

// One-time manifest revision overrides for deployments.
#[derive(Debug, Args)]
pub(crate) struct RevisionArgs {
    /// Fetch the repository manifest from this branch, for this deployment only.
    ///
    /// The environment keeps following its own branch.
    #[arg(long, conflicts_with = "commit")]
    pub(crate) branch: Option<String>,
    /// Fetch the repository manifest at this full commit hash, for this
    /// deployment only.
    #[arg(long, value_name = "SHA")]
    pub(crate) commit: Option<String>,
}

impl RevisionArgs {
    /// The one-time manifest revision, if overridden.
    pub(crate) fn revision(&self) -> Option<piqueld_client::ManifestRevision> {
        self.branch
            .clone()
            .map(piqueld_client::ManifestRevision::Branch)
            .or_else(|| {
                self.commit
                    .clone()
                    .map(piqueld_client::ManifestRevision::Commit)
            })
    }
}

// Application selection, confirmation, and waiting shared by `app reconcile`,
// `app stop`, `app start` and `app deploy`.
#[derive(Debug, Args)]
pub(crate) struct TargetArgs {
    /// Application name or stable ID.
    pub(crate) name_or_id: String,
    #[command(flatten)]
    pub(crate) flags: OperationFlags,
}

// Precondition, confirmation, and waiting flags shared by deploy, reconcile,
// stop and start.
#[derive(Debug, Args)]
pub(crate) struct OperationFlags {
    /// Fail unless the application is at this generation.
    ///
    /// Without it, a deployment uses the generation it inspects, and a retry
    /// checks none.
    #[arg(long, value_name = "GENERATION")]
    pub(crate) expected_generation: Option<u64>,
    /// Skip the confirmation prompt.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Return once the daemon accepts the operation, without waiting for it to end.
    #[arg(long)]
    pub(crate) no_wait: bool,
}

// Log window shared by `app logs` and `env logs`.
#[derive(Debug, Args)]
pub(crate) struct LogArgs {
    /// Only show this service's output.
    ///
    /// Defaults to every service.
    #[arg(long)]
    pub(crate) service: Option<String>,
    /// Show at most this many of the most recent lines, from 1 to 1000, merged
    /// across services.
    #[arg(
        long,
        default_value_t = 200,
        value_parser = clap::value_parser!(u16).range(1..=1000)
    )]
    pub(crate) tail: u16,
    /// Only show output written within this many seconds, from 1 to 86400 (a day).
    #[arg(
        long,
        default_value_t = 3600,
        value_name = "SECONDS",
        value_parser = clap::value_parser!(u32).range(1..=86400)
    )]
    pub(crate) since_seconds: u32,
}
