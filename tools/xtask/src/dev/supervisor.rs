//! The instance's supervisor: it starts the instance's engine, then builds and
//! runs the daemon, rebuilding and restarting it whenever the sources change.

use std::collections::HashMap;
use std::fs;
use std::io::IsTerminal;
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::{
    InspectContainerOptions, RestartContainerOptions, StartContainerOptions,
};
use rustix::process::getpid;
use tokio::process::Command;

use super::{DATA_LABEL, Instance, Phase, RUNTIME_LABEL, State, WORKTREE_LABEL};
use crate::docker::{DIND_SOCKET_DIR, Engine};
use crate::process::{Job, Shutdown};

/// The daemon allows ten seconds to finish in-flight requests.
const GRACE: Duration = Duration::from_secs(11);

/// What ended a wait on the build, the daemon, or a fix.
enum Event {
    Exited(std::process::ExitStatus),
    Changed,
    Shutdown,
}

impl Instance {
    /// Supervises the instance in the foreground until a signal stops it.
    pub(super) async fn run(&self) -> Result<()> {
        if let Some(other) = self.supervisor()
            && other != getpid()
        {
            bail!("already running (pid {other:?}); see `just dev status`");
        }
        Self::create_private_dir(&self.config.server.runtime_dir)?;
        fs::write(self.file("dev.pid"), getpid().as_raw_nonzero().to_string())?;
        // Signals arriving while the engine starts stop the first build.
        let mut shutdown = Shutdown::listen()?;
        let result = async {
            self.start_engine().await?;
            self.supervise(&mut shutdown).await
        }
        .await;
        for name in ["dev.pid", "state.json"] {
            fs::remove_file(self.file(name)).ok();
        }
        result
    }

    /// Runs the supervisor in the background, unless it runs.
    pub(super) fn start(&self) -> Result<()> {
        if self.supervisor().is_some() {
            return Ok(());
        }
        Self::create_private_dir(&self.config.server.runtime_dir)?;
        let log = fs::File::create(self.file("dev.log"))?;
        let child = std::process::Command::new(std::env::current_exe()?)
            .args(["dev", "run"])
            .current_dir(self.workspace.root())
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .process_group(0)
            .spawn()
            .context("start the supervisor")?;
        // Recording its pid here lets `wait` start immediately.
        fs::write(self.file("dev.pid"), child.id().to_string())?;
        Ok(())
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
            () = self.source_change(since) => {
                eprintln!("[dev] sources changed; rebuilding");
                Event::Changed
            }
            () = shutdown.requested() => Event::Shutdown,
        };
        if let Some(job) = job {
            job.stop(GRACE).await?;
        }
        Ok(event)
    }

    /// Resolves once a source is modified after `since`.
    async fn source_change(&self, since: SystemTime) {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if self.sources_changed_since(since) {
                // Let editors finish saving related files.
                tokio::time::sleep(Duration::from_millis(300)).await;
                return;
            }
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
        let mut job = Job::spawn(
            Command::new("cargo")
                .args(["build", "--package", "piqueld", "--bin", "piqueld"])
                .args(["--features", "embedded-ui", "--color", color])
                .current_dir(self.workspace.root())
                .stderr(Stdio::piped()),
        )?;
        job.tee_stderr(fs::File::create(self.file("output.log"))?)?;
        Ok(job)
    }

    /// Runs the built daemon. Its logs go to daemon.log as JSON; its errors
    /// are appended to output.log.
    fn daemon(&self) -> Result<Job> {
        let root = self.workspace.root();
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map_or_else(|| root.join("target"), |dir| root.join(dir));
        let mut job = Job::spawn(
            Command::new(target.join("debug/piqueld"))
                .arg("--config")
                .arg(root.join(super::CONFIG_FILE))
                .arg("--log-file")
                .arg(self.file("daemon.log"))
                .current_dir(root)
                .stderr(Stdio::piped()),
        )?;
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.file("output.log"))?;
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
