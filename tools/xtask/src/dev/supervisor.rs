//! The instance's supervisor: it starts the instance's engine, then builds and
//! runs the daemon, rebuilding and restarting it whenever the sources change.
//! The shared tailnet node runs under a supervisor of its own.

use std::collections::HashMap;
use std::fs::{self, TryLockError};
use std::io::{IsTerminal, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::{
    InspectContainerOptions, RestartContainerOptions, StartContainerOptions,
};
use rustix::process::{Pid, Signal, getpid, kill_process};
use tokio::process::Command;

use super::{DATA_LABEL, Instance, Phase, RUNTIME_LABEL, State, Tailnet, WORKTREE_LABEL};
use crate::docker::{DIND_SOCKET_DIR, Engine};
use crate::process::{Job, Shutdown};

/// The daemon allows ten seconds to finish in-flight requests.
const GRACE: Duration = Duration::from_secs(11);

/// A background xtask process, such as an instance's supervisor, found through
/// the dev.pid it locks while it runs, so a file left behind by a crash never
/// names an unrelated process that reuses its pid.
pub(super) struct Supervisor {
    pid_file: PathBuf,
}

impl Supervisor {
    /// The supervisor of the instance with this runtime directory.
    pub(super) fn in_dir(runtime_dir: &Path) -> Self {
        Self {
            pid_file: runtime_dir.join("dev.pid"),
        }
    }

    /// The supervisor's process, while it runs.
    pub(super) fn running(&self) -> Option<Pid> {
        let mut file = fs::File::open(&self.pid_file).ok()?;
        // Taking the lock means no supervisor holds it; dropping the file
        // releases it again.
        let Err(TryLockError::WouldBlock) = file.try_lock_shared() else {
            return None;
        };
        let mut pid = String::new();
        file.read_to_string(&mut pid).ok()?;
        Pid::from_raw(pid.trim().parse().ok()?)
    }

    /// Locks dev.pid and records our pid in it, for as long as the returned
    /// file stays open. Fails when another supervisor holds the lock.
    pub(super) async fn claim(&self) -> Result<fs::File> {
        let path = &self.pid_file;
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        // `running` holds the lock for a moment while it checks, so a few
        // attempts tell it apart from a running supervisor.
        for _ in 0..20 {
            match file.try_lock() {
                Ok(()) => {
                    file.set_len(0)?;
                    write!(file, "{}", getpid().as_raw_nonzero())?;
                    return Ok(file);
                }
                Err(TryLockError::WouldBlock) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(TryLockError::Error(error)) => {
                    return Err(error).with_context(|| format!("lock {}", path.display()));
                }
            }
        }
        bail!("already running; see `just dev status`")
    }

    /// Runs `xtask <args>` in the background, with its output in `log`,
    /// unless a supervisor runs, and returns once one holds dev.pid.
    pub(super) async fn launch(&self, args: &[&str], log: &Path) -> Result<()> {
        if self.running().is_some() {
            return Ok(());
        }
        let log = fs::File::create(log)?;
        let mut child = std::process::Command::new(std::env::current_exe()?)
            .args(args)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .process_group(0)
            .spawn()
            .context("start the supervisor")?;
        while self.running().is_none() {
            if let Some(status) = child.try_wait()? {
                // A concurrent launch's supervisor may hold dev.pid instead.
                if self.running().is_some() {
                    break;
                }
                bail!("the supervisor exited ({status})");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }

    /// Stops the supervisor, which stops what it supervises, if it runs.
    pub(super) async fn stop(&self) -> Result<()> {
        let Some(pid) = self.running() else {
            return Ok(());
        };
        kill_process(pid, Signal::TERM).context("stop the instance")?;
        for _ in 0..150 {
            if self.running().is_none() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!("the instance did not stop within 15 seconds")
    }
}

/// What ended a wait on the build, the daemon, or a fix.
enum Event {
    Exited(std::process::ExitStatus),
    Changed,
    Shutdown,
}

impl Instance {
    /// Supervises the instance in the foreground until a signal stops it.
    pub(super) async fn run(&self) -> Result<()> {
        Self::create_private_dir(&self.config.server.runtime_dir)?;
        let _claim = self.supervisor().claim().await?;
        // Signals arriving while the engine starts stop the first build.
        let mut shutdown = Shutdown::listen()?;
        let result = async {
            self.start_engine().await?;
            let tailnet = Tailnet::new()?;
            if self.config.tailscale.socket.as_deref() == Some(&tailnet.socket()) {
                tailnet.start().await?;
            }
            self.supervise(&mut shutdown).await
        }
        .await;
        fs::remove_file(self.file("state.json")).ok();
        result
    }

    /// Runs the supervisor in the background, unless it runs, and returns once
    /// it holds dev.pid.
    pub(super) async fn start(&self) -> Result<()> {
        Self::create_private_dir(&self.config.server.runtime_dir)?;
        self.supervisor()
            .launch(&["dev", "run"], &self.file("dev.log"))
            .await
            .inspect_err(|_| self.print_tail("dev.log", 40))
    }

    async fn supervise(&self, shutdown: &mut Shutdown) -> Result<()> {
        loop {
            let started = SystemTime::now();
            self.record(Phase::Building, started)?;
            let phase = match self.attend(Some(self.build()?), started, shutdown).await? {
                Event::Exited(status) if status.success() => {
                    self.record(Phase::Running, started)?;
                    match self.attend(Some(self.daemon()?), started, shutdown).await? {
                        Event::Exited(status) => {
                            eprintln!("[dev] the daemon exited ({status})");
                            Phase::Exited
                        }
                        Event::Changed => continue,
                        Event::Shutdown => return Ok(()),
                    }
                }
                Event::Exited(_) => Phase::Failed,
                Event::Changed => continue,
                Event::Shutdown => return Ok(()),
            };
            self.record(phase, started)?;
            eprintln!("[dev] waiting for a source change to rebuild");
            if let Event::Shutdown = self.attend(None, started, shutdown).await? {
                return Ok(());
            }
        }
    }

    /// Waits until `job` exits, a source changes, or a signal arrives, then
    /// stops the job.
    async fn attend(
        &self,
        mut job: Option<Job>,
        since: SystemTime,
        shutdown: &mut Shutdown,
    ) -> Result<Event> {
        let event = tokio::select! {
            status = async {
                match job.as_mut() {
                    Some(job) => job.wait().await,
                    None => std::future::pending().await,
                }
            } => Event::Exited(status?),
            event = self.watch(since) => event,
            () = shutdown.requested() => Event::Shutdown,
        };
        if let Some(job) = job {
            job.stop(GRACE).await?;
        }
        Ok(event)
    }

    /// Resolves with `Changed` once a source is modified after `since`, or
    /// with `Shutdown` once the worktree is removed, leaving nothing to run.
    async fn watch(&self, since: SystemTime) -> Event {
        let mut changed = false;
        loop {
            // After a change, let editors finish saving related files. Removing
            // the worktree changes its sources too, so check for that first.
            let pause = if changed { 300 } else { 500 };
            tokio::time::sleep(Duration::from_millis(pause)).await;
            if !self.workspace.root().join(".git").exists() {
                eprintln!("[dev] the worktree was removed; stopping");
                return Event::Shutdown;
            }
            if changed {
                eprintln!("[dev] sources changed; rebuilding");
                return Event::Changed;
            }
            changed = self.sources_changed_since(since);
        }
    }

    fn record(&self, phase: Phase, build_started: SystemTime) -> Result<()> {
        let temporary = self.file("state.json.tmp");
        fs::write(
            &temporary,
            serde_json::to_vec(&State {
                phase,
                build_started,
            })?,
        )?;
        fs::rename(temporary, self.file("state.json")).context("record the instance's state")
    }

    /// Builds the daemon with the embedded dashboard. output.log keeps the
    /// latest build's output.
    fn build(&self) -> Result<Job> {
        let color = if std::io::stderr().is_terminal() {
            "always"
        } else {
            "never"
        };
        let log = fs::File::create(self.file("output.log"))?;
        let mut job = Job::spawn(
            Command::new("cargo")
                .args(["build", "--package", "piqueld", "--bin", "piqueld"])
                .args(["--features", "embedded-ui", "--color", color])
                .current_dir(self.workspace.root())
                .stderr(Stdio::piped()),
        )?;
        job.tee_stderr(log)?;
        Ok(job)
    }

    /// Runs the built daemon. Its logs go to daemon.log as JSON; its errors
    /// are appended to output.log.
    fn daemon(&self) -> Result<Job> {
        let root = self.workspace.root();
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map_or_else(|| root.join("target"), |dir| root.join(dir));
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.file("output.log"))?;
        let mut job = Job::spawn(
            Command::new(target.join("debug/piqueld"))
                .arg("--config")
                .arg(root.join(super::CONFIG_FILE))
                .arg("--log-file")
                .arg(self.file("daemon.log"))
                .current_dir(root)
                .stderr(Stdio::piped()),
        )?;
        job.tee_stderr(log)?;
        Ok(job)
    }

    /// Starts this instance's Docker engine when the configuration points at
    /// it. The container and its image store persist across restarts; `clean`
    /// removes them.
    async fn start_engine(&self) -> Result<()> {
        if !self.private_engine() {
            return Ok(());
        }
        let data = &self.config.server.data_dir;
        Self::create_private_dir(&self.file("docker"))?;
        Self::create_private_dir(data)?;
        let engine = Engine::host().await?;
        let name = self.container();
        match engine
            .inspect_container(&name, None::<InspectContainerOptions>)
            .await
        {
            Ok(container) => {
                if container.state.and_then(|state| state.running) != Some(true) {
                    engine
                        .start_container(&name, None::<StartContainerOptions>)
                        .await?;
                } else if !self.engine_socket_exists() {
                    // A restart recreates the socket when its directory was
                    // replaced while the engine ran.
                    engine
                        .restart_container(&name, None::<RestartContainerOptions>)
                        .await?;
                }
            }
            Err(error) if Engine::is_not_found(&error) => {
                let image = Engine::dind_image();
                engine.pull_missing(&image).await?;
                let (root, data, runtime) = (
                    self.workspace.root().display().to_string(),
                    data.display().to_string(),
                    self.config.server.runtime_dir.display().to_string(),
                );
                // The data directory is mounted at the same path because the
                // daemon bind-mounts files from it, such as the ingress
                // configuration.
                let binds = vec![
                    format!("{name}:/var/lib/docker"),
                    format!("{runtime}/docker:{DIND_SOCKET_DIR}"),
                    format!("{data}:{data}"),
                ];
                let body = ContainerCreateBody {
                    image: Some(image),
                    cmd: Some(vec![
                        "dockerd".to_owned(),
                        format!("--host=unix://{DIND_SOCKET_DIR}/docker.sock"),
                    ]),
                    env: Some(vec!["DOCKER_TLS_CERTDIR=".to_owned()]),
                    labels: Some(HashMap::from([
                        (WORKTREE_LABEL.to_owned(), root),
                        (DATA_LABEL.to_owned(), data),
                        (RUNTIME_LABEL.to_owned(), runtime),
                    ])),
                    host_config: Some(HostConfig {
                        privileged: Some(true),
                        binds: Some(binds),
                        ..HostConfig::default()
                    }),
                    ..ContainerCreateBody::default()
                };
                engine.launch(Some(&name), body).await?;
            }
            Err(error) => return Err(error).context("inspect the instance's engine"),
        }
        engine.wait_for_dind(&name).await
    }
}
