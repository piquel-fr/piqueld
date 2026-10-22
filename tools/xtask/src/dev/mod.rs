//! `just dev`: one isolated development instance per worktree, with its own
//! configuration, data and runtime directories, localhost port, and Docker
//! engine, so worktrees never share state. Instances can also serve on the
//! tailnet through one shared node. See docs/dev/development.md.

mod supervisor;
mod tailnet;

use std::collections::HashMap;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail, ensure};
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::{
    InspectContainerOptions, ListContainersOptionsBuilder, RemoveVolumeOptions,
};
use clap::Subcommand;
use rustix::process::getuid;
use serde::{Deserialize, Serialize};

use crate::Workspace;
use crate::docker::Engine;
use supervisor::{Holder, Supervisor};
use tailnet::{Tailnet, TailnetCommand};

const CONFIG_FILE: &str = "piqueld.local.toml";
/// Held every user's runtime directories before [`Instance::runtime_root`];
/// `just dev config --force` moves a configuration that still uses it.
const LEGACY_RUNTIME_ROOT: &str = "/tmp/piqueld-dev";
/// Changes to these rebuild and restart the daemon. It embeds the dashboard and
/// the migrations at compile time, so they are watched too, as are the build
/// settings, and it reads its configuration at startup.
const SOURCES: [&str; 9] = [
    "apps/piqueld",
    "apps/piqueld-ui",
    "crates",
    "migrations",
    "Cargo.toml",
    "Cargo.lock",
    ".cargo/config.toml",
    "rust-toolchain.toml",
    CONFIG_FILE,
];
/// Labels on each instance's engine, which let `prune` find what to delete.
const WORKTREE_LABEL: &str = "io.piqueld.dev.worktree";
const DATA_LABEL: &str = "io.piqueld.dev.data";
const RUNTIME_LABEL: &str = "io.piqueld.dev.runtime";

#[derive(Subcommand)]
pub enum Command {
    /// Watch the sources, rebuilding and restarting the daemon on every change.
    Run,
    /// Run in the background, then wait until ready.
    Start {
        /// Seconds to wait.
        #[arg(default_value_t = 900)]
        timeout: u64,
    },
    /// Wait until the current sources are built and serving.
    Wait {
        /// Seconds to wait.
        #[arg(default_value_t = 900)]
        timeout: u64,
    },
    /// Print the instance's state, URL, and files.
    Status,
    /// Stop the daemon and the watcher.
    Stop,
    /// Stop, then delete the instance's Docker engine and state.
    Clean,
    /// Stop and clean the instances of worktrees that no longer exist.
    Prune,
    /// Write piqueld.local.toml unless it exists.
    Config {
        /// Replace an existing configuration.
        #[arg(long)]
        force: bool,
    },
    /// Manage the shared tailnet node that serves instances over HTTPS.
    Tailnet {
        #[command(subcommand)]
        command: TailnetCommand,
    },
    /// Run this worktree's piquelctl against the instance.
    #[command(disable_help_flag = true)]
    Ctl {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

impl Command {
    pub async fn run(self, workspace: Workspace) -> Result<ExitCode> {
        match self {
            Self::Config { force } => {
                if !Instance::configure(&workspace, force).await? {
                    eprintln!("kept {CONFIG_FILE}; pass --force to replace it");
                }
            }
            Self::Prune => Engine::host().await?.prune_dev_instances().await?,
            Self::Run => Instance::load_or_configure(workspace).await?.run().await?,
            Self::Start { timeout } => {
                let instance = Instance::load_or_configure(workspace).await?;
                instance.start().await?;
                instance.wait(timeout).await?;
            }
            Self::Wait { timeout } => Instance::load(workspace)?.wait(timeout).await?,
            Self::Status => Instance::load(workspace)?.status().await,
            Self::Stop => Instance::load(workspace)?.supervisor().stop().await?,
            Self::Clean => Instance::load(workspace)?.clean().await?,
            Self::Tailnet { command } => Tailnet::new()?.run(command).await?,
            Self::Ctl { args } => return Err(Instance::load(workspace)?.ctl(&args)),
        }
        Ok(ExitCode::SUCCESS)
    }
}

/// The parts of piqueld.local.toml the tasks read; the rest is the daemon's.
/// Edits to the file are respected.
#[derive(Deserialize)]
struct Config {
    server: ServerConfig,
    #[serde(default)]
    docker: DockerConfig,
    #[serde(default)]
    tailscale: TailscaleConfig,
}

#[derive(Deserialize)]
struct ServerConfig {
    data_dir: PathBuf,
    runtime_dir: PathBuf,
    port: Option<u16>,
}

#[derive(Default, Deserialize)]
struct DockerConfig {
    socket: Option<PathBuf>,
}

#[derive(Default, Deserialize)]
struct TailscaleConfig {
    socket: Option<PathBuf>,
}

impl Config {
    fn read(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }
}

/// How far the supervisor got with the current sources.
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Phase {
    Building,
    Failed,
    Running,
    Exited,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Building => "building",
            Self::Failed => "failed",
            Self::Running => "running",
            Self::Exited => "exited",
        }
    }
}

/// The supervisor's progress, in `state.json` for `wait` and `status`.
#[derive(Serialize, Deserialize)]
struct State {
    phase: Phase,
    /// When the build of the current sources began. Sources modified later
    /// are not built yet.
    build_started: SystemTime,
}

/// This worktree's instance, as configured by piqueld.local.toml.
pub struct Instance {
    workspace: Workspace,
    config: Config,
}

impl Instance {
    fn load(workspace: Workspace) -> Result<Self> {
        let path = workspace.root().join(CONFIG_FILE);
        if !path.exists() {
            bail!("{CONFIG_FILE} is missing; write it with `just dev config`");
        }
        let config = Config::read(&path)?;
        let instance = Self { workspace, config };
        instance.own_runtime_dir()?;
        Ok(instance)
    }

    /// Creates the runtime directory unless it exists, and checks that it and
    /// its parent are the user's alone, before any task touches the sockets,
    /// logs, or dev.pid in it.
    fn own_runtime_dir(&self) -> Result<()> {
        let dir = &self.config.server.runtime_dir;
        let parent = dir.parent().context("runtime_dir has no parent")?;
        if parent == Path::new(LEGACY_RUNTIME_ROOT) {
            eprintln!(
                "warning: runtime_dir is in {LEGACY_RUNTIME_ROOT}, which other users can claim; \
                 run `just dev stop`, then `just dev config --force`"
            );
        }
        Self::own_private_dir(parent.to_owned())?;
        Self::own_private_dir(dir.clone())?;
        Ok(())
    }

    /// Whether `workspace`'s own supervisor runs from `runtime_dir`. It would
    /// restart the daemon with a replaced configuration, out of reach of the
    /// tasks that read it. A configuration copied from another worktree names
    /// that worktree's instance instead, which is not ours to stop.
    /// Fails when the running supervisor's worktree is unknown.
    fn runs_from(runtime_dir: &Path, workspace: &Workspace) -> Result<bool> {
        match Supervisor::in_dir(runtime_dir).holder() {
            Holder::Nobody => Ok(false),
            Holder::Worktree(owner) => Ok(owner == workspace.root()),
            Holder::Unrecorded => bail!(
                "an instance that records no worktree, such as one started by an older xtask, \
                 is running from {}; stop it from the worktree that started it \
                 (`just dev stop`), then run `just dev config --force`",
                runtime_dir.display()
            ),
        }
    }

    /// Whether `dir` and its parent are directories only the current user can
    /// access, so that files in it, such as dev.pid, can be trusted.
    fn private_runtime_dir(dir: &Path) -> bool {
        dir.parent()
            .is_some_and(|parent| Self::check_private(parent).is_ok())
            && Self::check_private(dir).is_ok()
    }

    async fn load_or_configure(workspace: Workspace) -> Result<Self> {
        Self::configure(&workspace, false).await?;
        Self::load(workspace)
    }

    /// Where instances keep their data. It stays out of /tmp, whose old files
    /// systemd may delete.
    fn state_root() -> Result<PathBuf> {
        let state_home = std::env::var_os("XDG_STATE_HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".local/state")))
            .context("HOME is not set")?;
        Ok(state_home.join("piqueld-dev"))
    }

    /// Holds every instance's sockets and logs, and the tailnet node's. It is
    /// in /tmp because every session sees it, unlike /run/user, so it is named
    /// after the user and must be theirs alone: another user may have created
    /// it first.
    fn runtime_root() -> Result<PathBuf> {
        Self::own_private_dir(format!("/tmp/piqueld-dev-{}", getuid().as_raw()).into())
    }

    /// Creates `path` unless it exists, then checks it with [`Self::check_private`].
    fn own_private_dir(path: PathBuf) -> Result<PathBuf> {
        if let Err(error) = fs::DirBuilder::new().mode(0o700).create(&path)
            && error.kind() != io::ErrorKind::AlreadyExists
        {
            return Err(error).with_context(|| format!("create {}", path.display()));
        }
        Self::check_private(&path)?;
        Ok(path)
    }

    /// Checks that `path` is a directory, not a symbolic link, that only the
    /// current user can access.
    fn check_private(path: &Path) -> Result<()> {
        let metadata =
            fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
        ensure!(
            metadata.is_dir()
                && metadata.uid() == getuid().as_raw()
                && metadata.mode() & 0o777 == 0o700,
            "{} is not a directory only you can access; remove it",
            path.display()
        );
        Ok(())
    }

    /// Writes the worktree's configuration unless it exists and `force` is
    /// unset. Returns whether it did. While the shared tailnet node is logged
    /// in, the instance also serves on it, at its localhost port.
    async fn configure(workspace: &Workspace, force: bool) -> Result<bool> {
        let path = workspace.root().join(CONFIG_FILE);
        if path.exists() {
            if !force {
                return Ok(false);
            }
            if let Ok(config) = Config::read(&path)
                && Self::runs_from(&config.server.runtime_dir, workspace)?
            {
                bail!(
                    "the instance is running; run `just dev stop`, then `just dev config --force`"
                );
            }
        }
        let name = Self::name_for(workspace)?;
        let data = Self::state_root()?.join(&name);
        let runtime = Self::runtime_root()?.join(&name);
        // Held until the configuration is written, so a concurrent setup of
        // another worktree cannot pick the same port.
        let _lock = Self::lock_ports()?;
        let port = Self::free_port(workspace)?;
        let quote = |path: &Path| toml::Value::from(path.display().to_string()).to_string();
        let tailnet = Tailnet::new()?;
        let origin = match tailnet.dns_name().await {
            Ok(Some(dns_name)) => format!(
                r"[tailscale]
# The shared development node serves this instance, with public_url
# https://{dns_name}:{port}.
# Without this section, it is only on localhost.
enabled = true
socket = {socket}
https_port = {port}",
                socket = quote(&tailnet.socket()),
            ),
            result => {
                if let Err(error) = result {
                    eprintln!("the tailnet node is unavailable: {error:#}");
                }
                eprintln!("`just dev tailnet up` serves new instances on the tailnet");
                format!("[auth]\npublic_url = \"http://{name}.localhost:{port}\"")
            }
        };
        let (data, socket, runtime) = (
            quote(&data),
            quote(&runtime.join("docker/docker.sock")),
            quote(&runtime),
        );
        fs::write(
            &path,
            format!(
                r#"# Development instance for this worktree, generated by `just dev config`.
# `just dev` reads the directories, port, and Docker and tailnet sockets from
# here. Unset settings use the daemon's defaults; see examples/piqueld.toml.

[server]
data_dir = {data}
runtime_dir = {runtime}
# The browser preview opens public_url; the Unix socket serves the CLI.
listen_mode = "localhost"
port = {port}
# The instance's own host on localhost: browsers send a host's cookies to
# all of its ports.
allowed_hosts = ["{name}.localhost"]

{origin}

[docker]
# A private Docker-in-Docker engine, started by `just dev`. Point this at
# /var/run/docker.sock to use the host engine instead.
socket = {socket}
auto_initialize_swarm = true
"#
            ),
        )
        .with_context(|| format!("write {}", path.display()))?;
        eprintln!("wrote {CONFIG_FILE} for instance {name}");
        Ok(true)
    }

    /// The instance is named after the worktree directory, or `main` for the
    /// main checkout, followed by a hash of its path that keeps checkouts with
    /// the same name apart, e.g. `t3code-b681a751-0c1d2e3f`. The configuration
    /// records the name, so the hash need not be stable across Rust versions.
    fn name_for(workspace: &Workspace) -> Result<String> {
        let main = workspace.worktrees()?.into_iter().next();
        let name = if main.as_deref() == Some(workspace.root()) {
            "main".to_owned()
        } else {
            workspace
                .root()
                .file_name()
                .context("the worktree has no directory name")?
                .to_string_lossy()
                .to_lowercase()
        };
        let mut slug = String::new();
        for character in name.chars() {
            if character.is_ascii_alphanumeric() {
                slug.push(character);
            } else if !slug.ends_with('-') {
                slug.push('-');
            }
        }
        slug.truncate(40);
        let mut hasher = DefaultHasher::new();
        workspace.root().hash(&mut hasher);
        // The low 32 bits are plenty to tell a few checkouts apart.
        let hash = hasher.finish() & 0xffff_ffff;
        // The name is also a DNS label, which cannot start with a hyphen.
        Ok(format!("{}-{hash:08x}", slug.trim_matches('-'))
            .trim_start_matches('-')
            .to_owned())
    }

    /// The lowest port from 7846 that nothing listens on and no other
    /// worktree's configuration claims.
    fn free_port(workspace: &Workspace) -> Result<u16> {
        let claimed: Vec<u16> = workspace
            .worktrees()?
            .iter()
            .filter(|worktree| worktree.as_path() != workspace.root())
            .filter_map(|worktree| Config::read(&worktree.join(CONFIG_FILE)).ok()?.server.port)
            .collect();
        (7846..=u16::MAX)
            .find(|port| {
                !claimed.contains(port) && std::net::TcpListener::bind(("127.0.0.1", *port)).is_ok()
            })
            .context("no free port")
    }

    /// Locks the choice of ports across worktrees until the returned file is
    /// dropped, waiting for any other worktree's setup to finish.
    fn lock_ports() -> Result<fs::File> {
        let root = Self::state_root()?;
        Self::create_private_dir(&root)?;
        let path = root.join("ports.lock");
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        file.lock()
            .with_context(|| format!("lock {}", path.display()))?;
        Ok(file)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.config.server.runtime_dir.join(name)
    }

    fn socket(&self) -> PathBuf {
        self.file("piqueld.sock")
    }

    fn name(&self) -> String {
        self.config
            .server
            .runtime_dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }

    fn container(&self) -> String {
        format!("piqueld-dev-{}", self.name())
    }

    fn engine_socket(&self) -> PathBuf {
        self.file("docker/docker.sock")
    }

    /// Whether the configuration points at this instance's own engine.
    fn private_engine(&self) -> bool {
        self.config.docker.socket.as_deref() == Some(&self.engine_socket())
    }

    fn engine_socket_exists(&self) -> bool {
        fs::metadata(self.engine_socket()).is_ok_and(|metadata| metadata.file_type().is_socket())
    }

    pub(super) fn create_private_dir(path: &Path) -> Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .with_context(|| format!("create {}", path.display()))
    }

    /// This instance's supervisor.
    fn supervisor(&self) -> Supervisor {
        Supervisor::in_dir(&self.config.server.runtime_dir)
    }

    fn state(&self) -> Option<State> {
        serde_json::from_slice(&fs::read(self.file("state.json")).ok()?).ok()
    }

    /// Whether any source was modified after `time`.
    fn sources_changed_since(&self, time: SystemTime) -> bool {
        SOURCES
            .iter()
            .any(|source| Self::modified_since(&self.workspace.root().join(source), time))
    }

    /// Whether `path`, or anything visible below it, was modified after `time`.
    /// Directories count too, so deleted files are noticed.
    fn modified_since(path: &Path, time: SystemTime) -> bool {
        let Ok(metadata) = fs::symlink_metadata(path) else {
            return false;
        };
        if metadata.modified().is_ok_and(|modified| modified > time) {
            return true;
        }
        metadata.is_dir()
            && fs::read_dir(path)
                .into_iter()
                .flatten()
                .flatten()
                .any(|entry| {
                    !entry.file_name().to_string_lossy().starts_with('.')
                        && Self::modified_since(&entry.path(), time)
                })
    }

    /// The daemon's public URL, once it serves the API. The authentication
    /// status is served before initial setup, unlike the socket's /health.
    async fn public_url(&self) -> Option<String> {
        let client =
            piqueld_client::Client::unix(self.socket()).with_timeout(Duration::from_secs(2));
        Some(client.auth_status().await.ok()?.public_url)
    }

    /// Prints the URL once the daemon serves the current sources. Fails with
    /// the relevant output when the build or startup fails.
    async fn wait(&self, timeout: u64) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(timeout);
        while Instant::now() < deadline {
            if self.supervisor().running().is_none() {
                self.print_tail("dev.log", 40);
                bail!("the instance is not running; start it with `just dev start`");
            }
            if let Some(state) = self.state()
                && !self.sources_changed_since(state.build_started)
            {
                match state.phase {
                    Phase::Building => {}
                    Phase::Failed => {
                        self.print_tail("output.log", 60);
                        bail!("the build failed; fix it and save to rebuild");
                    }
                    Phase::Exited => {
                        self.print_tail("output.log", 60);
                        bail!("the daemon exited; fix it and save to rebuild");
                    }
                    Phase::Running => {
                        if let Some(url) = self.public_url().await {
                            println!("ready: {url}/dashboard/");
                            return Ok(());
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        bail!("not ready after {timeout} seconds; see `just dev status`")
    }

    fn print_tail(&self, name: &str, count: usize) {
        let Ok(text) = fs::read_to_string(self.file(name)) else {
            return;
        };
        let lines: Vec<&str> = text.lines().collect();
        for line in &lines[lines.len().saturating_sub(count)..] {
            eprintln!("{line}");
        }
    }

    async fn status(&self) {
        let (phase, url) = if self.supervisor().running().is_some() {
            let phase = self
                .state()
                .map_or("starting", |state| state.phase.as_str());
            (phase, self.public_url().await)
        } else {
            ("stopped", None)
        };
        let docker = self.config.docker.socket.as_ref().map_or_else(
            || "the daemon's default".to_owned(),
            |socket| socket.display().to_string(),
        );
        println!("instance  {} ({phase})", self.name());
        println!(
            "url       {}",
            url.map_or_else(
                || "unavailable".to_owned(),
                |url| format!("{url}/dashboard/")
            )
        );
        println!("socket    {}", self.socket().display());
        println!("data      {}", self.config.server.data_dir.display());
        println!("docker    {docker}");
        println!("output    {}", self.file("output.log").display());
        println!("logs      {}", self.file("daemon.log").display());
    }

    async fn clean(&self) -> Result<()> {
        let engine = Engine::host().await?;
        let container = self.container();
        match engine
            .inspect_container(&container, None::<InspectContainerOptions>)
            .await
        {
            Ok(inspected) => self.check_owner(
                &container,
                inspected.config.and_then(|config| config.labels),
            )?,
            Err(error) if Engine::is_not_found(&error) => {}
            Err(error) => return Err(error).context("inspect the instance's engine"),
        }
        self.supervisor().stop().await?;
        let dirs = [
            &self.config.server.data_dir,
            &self.config.server.runtime_dir,
        ];
        engine.remove_dev_instance(&container, &dirs).await?;
        eprintln!("removed instance {}", self.name());
        Ok(())
    }

    /// Fails unless `container`, with these labels, is this worktree's engine:
    /// a configuration copied from another worktree names that worktree's
    /// engine, and a container that `just dev` did not create is not ours to
    /// delete.
    fn check_owner(&self, container: &str, labels: Option<HashMap<String, String>>) -> Result<()> {
        match labels.unwrap_or_default().get(WORKTREE_LABEL) {
            Some(owner) if Path::new(owner) == self.workspace.root() => Ok(()),
            Some(owner) => {
                bail!("{container} belongs to {owner}; run `just dev config --force` here")
            }
            None => bail!("{container} was not created by `just dev`; kept it"),
        }
    }

    /// Replaces xtask with this worktree's piquelctl; returns only on failure.
    /// Cargo's output stays visible: the build lock is shared with the dev
    /// daemon's rebuilds, so a wait for it would otherwise look like a hang.
    fn ctl(&self, args: &[String]) -> anyhow::Error {
        let error = self
            .workspace
            .cargo()
            .args(["run", "--package", "piquelctl", "--"])
            .args(args)
            .env("PIQUELD_SOCKET", self.socket())
            .exec();
        anyhow::Error::new(error).context("run piquelctl")
    }
}

impl Engine {
    /// Removes an instance's engine, its image store, and `dirs`. Only
    /// directories directly under a `piqueld-dev` directory, such as the state
    /// root, or under the runtime root are deleted, so a configuration pointing
    /// elsewhere never loses data.
    async fn remove_dev_instance(&self, container: &str, dirs: &[impl AsRef<Path>]) -> Result<()> {
        self.remove(container).await?;
        if let Err(error) = self
            .remove_volume(container, None::<RemoveVolumeOptions>)
            .await
            && !Self::is_not_found(&error)
        {
            return Err(error).context("remove the instance's image store");
        }
        let runtime_root =
            fs::canonicalize(Instance::runtime_root()?).context("resolve the runtime root")?;
        for dir in dirs {
            // Checking the resolved path keeps symbolic links from redirecting
            // the deletion. A missing directory has nothing to delete.
            let Ok(dir) = fs::canonicalize(dir) else {
                continue;
            };
            let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
                continue;
            };
            if parent.file_name() != Some("piqueld-dev".as_ref()) && parent != runtime_root {
                eprintln!("kept {}", dir.display());
            } else if fs::remove_dir_all(&dir).is_err() && dir.exists() {
                // Containers may have left root-owned files behind, which only
                // a container can remove.
                self.run_once(ContainerCreateBody {
                    image: Some(Self::dind_image()),
                    entrypoint: Some(vec!["rm".to_owned()]),
                    cmd: Some(vec![
                        "-rf".to_owned(),
                        format!("/parent/{}", name.display()),
                    ]),
                    host_config: Some(HostConfig {
                        binds: Some(vec![format!("{}:/parent", parent.display())]),
                        ..HostConfig::default()
                    }),
                    ..ContainerCreateBody::default()
                })
                .await?;
            }
        }
        Ok(())
    }

    /// Stops and cleans the instances of worktrees that no longer exist.
    async fn prune_dev_instances(&self) -> Result<()> {
        let filters = HashMap::from([("label", vec![WORKTREE_LABEL])]);
        let options = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&filters)
            .build();
        for container in self.list_containers(Some(options)).await? {
            let labels = container.labels.unwrap_or_default();
            let name = container.names.unwrap_or_default().into_iter().next();
            let (Some(name), Some(worktree)) = (name, labels.get(WORKTREE_LABEL)) else {
                continue;
            };
            if Path::new(worktree).is_dir() {
                continue;
            }
            // A supervisor from before its worktree was removed may still run.
            if let Some(runtime) = labels.get(RUNTIME_LABEL).map(Path::new)
                && Instance::private_runtime_dir(runtime)
            {
                Supervisor::in_dir(runtime).stop().await?;
            }
            let dirs: Vec<&String> = [DATA_LABEL, RUNTIME_LABEL]
                .iter()
                .filter_map(|label| labels.get(*label))
                .collect();
            self.remove_dev_instance(name.trim_start_matches('/'), &dirs)
                .await?;
            eprintln!("removed the instance of {worktree}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(root: &Path) -> Instance {
        instance_in(root, Path::new("/runtime"))
    }

    fn instance_in(root: &Path, runtime_dir: &Path) -> Instance {
        let config = format!(
            "[server]\ndata_dir = \"/data\"\nruntime_dir = {}",
            toml::Value::from(runtime_dir.display().to_string())
        );
        Instance {
            workspace: Workspace {
                root: root.to_owned(),
            },
            config: toml::from_str(&config).unwrap(),
        }
    }

    #[test]
    fn clean_only_removes_this_worktrees_engine() {
        let instance = instance(Path::new("/worktrees/a"));
        let owned = |labels| instance.check_owner("engine", labels).is_ok();
        let labelled = |worktree: &str| {
            Some(HashMap::from([(
                WORKTREE_LABEL.to_owned(),
                worktree.to_owned(),
            )]))
        };
        assert!(owned(labelled("/worktrees/a")));
        assert!(!owned(labelled("/worktrees/b")));
        assert!(!owned(Some(HashMap::new())));
        assert!(!owned(None));
    }

    #[test]
    fn runtime_root_is_a_private_directory() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("root");
        assert!(Instance::own_private_dir(root.clone()).is_ok());
        assert_eq!(fs::metadata(&root).unwrap().mode() & 0o777, 0o700);
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Instance::own_private_dir(root).is_err());
        let link = parent.path().join("link");
        std::os::unix::fs::symlink(parent.path(), &link).unwrap();
        assert!(Instance::own_private_dir(link).is_err());
    }

    #[tokio::test]
    async fn config_force_only_waits_for_this_worktrees_instance() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Instance::own_private_dir(temporary.path().join("root")).unwrap();
        // A custom runtime_dir, not named after the instance.
        let runtime = Instance::own_private_dir(root.join("rt")).unwrap();
        let mine = instance_in(Path::new("/worktrees/a"), &runtime);
        let runs_from = |root: &str| {
            let workspace = Workspace { root: root.into() };
            Instance::runs_from(&runtime, &workspace).unwrap()
        };
        assert!(!runs_from("/worktrees/a"));
        let claim = mine.claim().await.unwrap();
        assert!(runs_from("/worktrees/a"));
        // A configuration copied from another worktree names its instance.
        assert!(!runs_from("/worktrees/b"));
        drop(claim);
        // A supervisor that records no worktree, as older xtask builds did.
        let _claim = Supervisor::in_dir(&runtime).claim().await.unwrap();
        let workspace = Workspace {
            root: "/worktrees/a".into(),
        };
        assert!(Instance::runs_from(&runtime, &workspace).is_err());
    }

    #[test]
    fn migrations_and_configuration_are_sources() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("migrations")).unwrap();
        let instance = instance(root.path());
        let built = SystemTime::now();
        std::thread::sleep(Duration::from_millis(20));
        assert!(!instance.sources_changed_since(built));
        fs::write(root.path().join("migrations/0001_new.sql"), "").unwrap();
        assert!(instance.sources_changed_since(built));
        let built = SystemTime::now();
        std::thread::sleep(Duration::from_millis(20));
        fs::write(root.path().join(CONFIG_FILE), "").unwrap();
        assert!(instance.sources_changed_since(built));
    }
}
