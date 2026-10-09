//! `just docker-test`: the ignored Docker integration tests, run against a
//! throwaway Docker-in-Docker engine instead of the host's.

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bollard::models::{ContainerCreateBody, HostConfig, PortBinding};
use bollard::query_parameters::InspectContainerOptions;
use tokio::process::Command;

use crate::Workspace;
use crate::docker::{DIND_SOCKET_DIR, Engine};
use crate::process::{Job, Shutdown, exit_code};

pub async fn run(workspace: &Workspace) -> Result<ExitCode> {
    ensure!(
        cfg!(target_os = "linux"),
        "docker-test requires a Linux host with a local Docker-compatible daemon"
    );
    let engine = Engine::host().await?;
    let mut shutdown = Shutdown::listen()?;
    let runtime = tempfile::Builder::new()
        .prefix("piqueld-dind.")
        .tempdir()
        .context("create the engine's directory")?;
    let image = Engine::dind_image();
    engine.pull_missing(&image).await?;
    let container = engine.launch(None, dind(&image, runtime.path())).await?;
    let result = test(
        workspace,
        &engine,
        &container,
        runtime.path(),
        &mut shutdown,
    )
    .await;
    engine.remove(&container).await?;
    result
}

/// Ports the ingress tests reach on random loopback ports: Caddy's HTTP,
/// HTTPS and private HTTPS listeners, then Pebble's ACME API and
/// challtestsrv's DNS (TCP) and management API, run inside the engine by the
/// DNS-01 certificate test.
const PUBLISHED: [&str; 6] = [
    "80/tcp",
    "443/tcp",
    "8443/tcp",
    "14000/tcp",
    "8053/tcp",
    "8055/tcp",
];

/// An engine with its socket, and the daemon's data, in `runtime`, publishing
/// [`PUBLISHED`].
#[allow(
    clippy::zero_sized_map_values,
    reason = "bollard's type for Docker's exposed ports"
)]
fn dind(image: &str, runtime: &Path) -> ContainerCreateBody {
    let runtime = runtime.display();
    let loopback = || {
        Some(vec![PortBinding {
            host_ip: Some("127.0.0.1".to_owned()),
            host_port: Some(String::new()),
        }])
    };
    ContainerCreateBody {
        image: Some(image.to_owned()),
        cmd: Some(vec![
            "dockerd".to_owned(),
            format!("--host=unix://{DIND_SOCKET_DIR}/docker.sock"),
            "--storage-driver=vfs".to_owned(),
        ]),
        env: Some(vec!["DOCKER_TLS_CERTDIR=".to_owned()]),
        exposed_ports: Some(
            PUBLISHED
                .map(|port| (port.to_owned(), HashMap::new()))
                .into(),
        ),
        host_config: Some(HostConfig {
            privileged: Some(true),
            // Pre-mount /tmp so the entrypoint does not hide the data bind.
            tmpfs: Some(HashMap::from([(
                "/tmp".to_owned(),
                "rw,exec,dev".to_owned(),
            )])),
            binds: Some(vec![
                format!("{runtime}:{DIND_SOCKET_DIR}"),
                format!("{runtime}:{runtime}"),
            ]),
            port_bindings: Some(PUBLISHED.map(|port| (port.to_owned(), loopback())).into()),
            ..HostConfig::default()
        }),
        ..ContainerCreateBody::default()
    }
}

async fn test(
    workspace: &Workspace,
    engine: &Engine,
    container: &str,
    runtime: &Path,
    shutdown: &mut Shutdown,
) -> Result<ExitCode> {
    engine.wait_for_dind(container).await?;
    let settings = engine
        .inspect_container(container, None::<InspectContainerOptions>)
        .await?
        .network_settings
        .unwrap_or_default();
    // Connections from this host reach the engine's containers from its
    // network's gateway; the ingress tests trust it as the apps node.
    let host = settings
        .networks
        .iter()
        .flat_map(HashMap::values)
        .find_map(|network| {
            network
                .gateway
                .clone()
                .filter(|gateway| !gateway.is_empty())
        })
        .context("the engine's network has no gateway")?;
    let ports = settings.ports.unwrap_or_default();
    let port = |name: &str| {
        ports
            .get(name)
            .and_then(|bindings| bindings.as_ref()?.first()?.host_port.clone())
            .with_context(|| format!("the engine publishes no {name} port"))
    };
    let socket = runtime.join("docker.sock");
    let nextest = |args: &[&str]| {
        let mut command = Command::new("cargo");
        command
            .args([
                "nextest",
                "run",
                "--locked",
                "-p",
                "piqueld",
                "--run-ignored",
                "only",
            ])
            .args(["--test-threads=1"])
            .args(args)
            .env("PIQUELD_DOCKER_ISOLATED", "1")
            .env("PIQUELD_DOCKER_SOCKET", &socket)
            .current_dir(workspace.root());
        command
    };
    let limit = Some(limit()?);
    // Tests share one engine and mutate its Swarm state, so they run serially.
    let status = Job::spawn(
        nextest(&["--lib", "--no-capture", "-E", "test(ingress_caddy)"])
            .env("PIQUELD_DOCKER_DATA_DIR", runtime)
            .env("PIQUELD_INGRESS_HTTP_PORT", port("80/tcp")?)
            .env("PIQUELD_INGRESS_HTTPS_PORT", port("443/tcp")?)
            .env("PIQUELD_INGRESS_PRIVATE_PORT", port("8443/tcp")?)
            .env("PIQUELD_DOCKER_HOST_ADDRESS", &host)
            .env("PIQUELD_PEBBLE_PORT", port("14000/tcp")?)
            .env("PIQUELD_CHALLTESTSRV_DNS_PORT", port("8053/tcp")?)
            .env("PIQUELD_CHALLTESTSRV_API_PORT", port("8055/tcp")?),
    )?
    .finish(limit, shutdown)
    .await?;
    if !status.success() {
        return Ok(exit_code(status));
    }
    let status = Job::spawn(&mut nextest(&["--test", "docker_integration"]))?
        .finish(limit, shutdown)
        .await?;
    Ok(exit_code(status))
}

/// How long each test run may take: `PIQUELD_DOCKER_TEST_TIMEOUT`, such as
/// `90s`, `30m`, or `1h`, 15 minutes by default.
fn limit() -> Result<Duration> {
    let Ok(value) = std::env::var("PIQUELD_DOCKER_TEST_TIMEOUT") else {
        return Ok(Duration::from_mins(15));
    };
    let (number, unit) = value.split_at(value.len().saturating_sub(1));
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => 0,
    };
    let number: u64 = number.parse().unwrap_or(0);
    ensure!(
        scale > 0 && number > 0,
        "PIQUELD_DOCKER_TEST_TIMEOUT must look like 90s, 30m, or 1h, not {value:?}"
    );
    Ok(Duration::from_secs(number * scale))
}
