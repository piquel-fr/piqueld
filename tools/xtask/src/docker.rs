//! The host's Docker engine, and the Docker-in-Docker engines xtask runs on it
//! so tests and development instances never touch the host's own containers.

use std::ops::Deref;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use bollard::Docker;
use bollard::models::{ContainerCreateBody, ExecConfig};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, InspectContainerOptions,
    LogsOptionsBuilder, RemoveContainerOptionsBuilder, StartContainerOptions, WaitContainerOptions,
};
use futures_util::{StreamExt, TryStreamExt};

/// The pinned Docker-in-Docker image; `PIQUELD_DIND_IMAGE` overrides it.
const DIND_IMAGE: &str =
    "docker:29.6.2-dind@sha256:bfec1f5159c63a81ca6fdedbd81404d2c0e16378ed0feec3bb3fbf3998847659";

/// Where a Docker-in-Docker engine listens inside its container. Callers
/// bind-mount a private host directory here to reach the socket.
pub const DIND_SOCKET_DIR: &str = "/piqueld-socket";

pub struct Engine(Docker);

impl Deref for Engine {
    type Target = Docker;

    fn deref(&self) -> &Docker {
        &self.0
    }
}

impl Engine {
    /// Connects to the engine the Docker CLI uses: `DOCKER_HOST`, else the
    /// current context, else `/var/run/docker.sock`. It must be local, since
    /// xtask bind-mounts host directories into its containers.
    pub async fn host() -> Result<Self> {
        let endpoint = match std::env::var("DOCKER_HOST") {
            Ok(host) => host,
            Err(_) => Self::context_endpoint()?,
        };
        let path = endpoint.strip_prefix("unix://").with_context(|| {
            format!("xtask needs a local Unix-socket Docker endpoint, found {endpoint}")
        })?;
        let docker = Docker::connect_with_unix(path, 120, bollard::API_DEFAULT_VERSION)?;
        docker
            .ping()
            .await
            .with_context(|| format!("no running Docker engine at {endpoint}"))?;
        Ok(Self(docker))
    }

    fn context_endpoint() -> Result<String> {
        let output = std::process::Command::new("docker")
            .args([
                "context",
                "inspect",
                "--format",
                "{{.Endpoints.docker.Host}}",
            ])
            .output();
        match output {
            Ok(output) => {
                ensure!(
                    output.status.success(),
                    "could not resolve the Docker context: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
                Ok(String::from_utf8(output.stdout)?.trim().to_owned())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok("unix:///var/run/docker.sock".to_owned())
            }
            Err(error) => Err(error).context("run the Docker CLI"),
        }
    }

    pub fn dind_image() -> String {
        std::env::var("PIQUELD_DIND_IMAGE").unwrap_or_else(|_| DIND_IMAGE.to_owned())
    }

    /// Pulls `image` unless the engine has it.
    pub async fn pull_missing(&self, image: &str) -> Result<()> {
        if self.inspect_image(image).await.is_err() {
            let options = CreateImageOptionsBuilder::new().from_image(image).build();
            self.create_image(Some(options), None, None)
                .try_collect::<Vec<_>>()
                .await
                .with_context(|| format!("pull {image}"))?;
        }
        Ok(())
    }

    /// Creates and starts a container, returning its ID.
    pub async fn launch(&self, name: Option<&str>, body: ContainerCreateBody) -> Result<String> {
        let image = body.image.clone().unwrap_or_default();
        let options = name.map(|name| CreateContainerOptionsBuilder::new().name(name).build());
        let id = self
            .create_container(options, body)
            .await
            .with_context(|| format!("create a container from {image}"))?
            .id;
        self.start_container(&id, None::<StartContainerOptions>)
            .await
            .with_context(|| format!("start a container from {image}"))?;
        Ok(id)
    }

    /// Runs a container to completion, then removes it.
    pub async fn run_once(&self, body: ContainerCreateBody) -> Result<()> {
        let id = self.launch(None, body).await?;
        let result = self
            .wait_container(&id, None::<WaitContainerOptions>)
            .try_collect::<Vec<_>>()
            .await;
        self.remove(&id).await?;
        result.context("run a container")?;
        Ok(())
    }

    /// Removes a container and its anonymous volumes, if it exists.
    pub async fn remove(&self, container: &str) -> Result<()> {
        let options = RemoveContainerOptionsBuilder::new()
            .force(true)
            .v(true)
            .build();
        match self.remove_container(container, Some(options)).await {
            Err(error) if !Self::is_not_found(&error) => {
                Err(error).with_context(|| format!("remove container {container}"))
            }
            _ => Ok(()),
        }
    }

    pub fn is_not_found(error: &bollard::errors::Error) -> bool {
        matches!(
            error,
            bollard::errors::Error::DockerResponseServerError {
                status_code: 404,
                ..
            }
        )
    }

    pub async fn running(&self, container: &str) -> Result<bool> {
        let state = self
            .inspect_container(container, None::<InspectContainerOptions>)
            .await
            .with_context(|| format!("inspect container {container}"))?
            .state;
        Ok(state.and_then(|state| state.running).unwrap_or(false))
    }

    /// Runs `cmd` in a running container and returns its exit code.
    async fn exec(&self, container: &str, cmd: &[&str]) -> Result<i64> {
        let config = ExecConfig {
            cmd: Some(cmd.iter().map(ToString::to_string).collect()),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..ExecConfig::default()
        };
        let exec = self.create_exec(container, config).await?.id;
        if let bollard::exec::StartExecResults::Attached { output, .. } =
            self.start_exec(&exec, None).await?
        {
            output.for_each(|_| async {}).await;
        }
        Ok(self.inspect_exec(&exec).await?.exit_code.unwrap_or(-1))
    }

    /// Prints a container's output, for diagnosing startup failures.
    pub async fn print_logs(&self, container: &str) {
        let options = LogsOptionsBuilder::new().stdout(true).stderr(true).build();
        let mut logs = self.logs(container, Some(options));
        while let Some(Ok(output)) = logs.next().await {
            eprint!("{output}");
        }
    }

    /// Waits up to `limit` for `text` in a container's output.
    pub async fn wait_for_output(
        &self,
        container: &str,
        text: &str,
        limit: Duration,
    ) -> Result<()> {
        let options = LogsOptionsBuilder::new()
            .follow(true)
            .stdout(true)
            .stderr(true)
            .build();
        let mut logs = self.logs(container, Some(options));
        let found = tokio::time::timeout(limit, async {
            while let Some(Ok(output)) = logs.next().await {
                if output.to_string().contains(text) {
                    return true;
                }
            }
            false
        })
        .await;
        if found != Ok(true) {
            self.print_logs(container).await;
            bail!("the container never printed {text:?}");
        }
        Ok(())
    }

    /// Waits up to 60 seconds for the Docker-in-Docker engine in `container`,
    /// then opens its socket to the host user, whose private directory still
    /// guards it.
    pub async fn wait_for_dind(&self, container: &str) -> Result<()> {
        let ready = format!(
            "docker -H unix://{DIND_SOCKET_DIR}/docker.sock info >/dev/null 2>&1 \
             && chmod 666 {DIND_SOCKET_DIR}/docker.sock"
        );
        for _ in 0..60 {
            if !self.running(container).await? {
                self.print_logs(container).await;
                bail!("the isolated Docker daemon exited before becoming ready");
            }
            if self.exec(container, &["sh", "-c", &ready]).await.ok() == Some(0) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        self.print_logs(container).await;
        bail!("the isolated Docker daemon did not become ready within 60 seconds")
    }
}
