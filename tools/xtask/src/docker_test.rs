//! `just docker-test`: the ignored Docker integration tests, run against a
//! throwaway Docker-in-Docker engine instead of the host's. CI runs the suites,
//! and shards of them, in parallel jobs, each with an engine of its own.

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bollard::models::{ContainerCreateBody, PortBinding};
use bollard::query_parameters::InspectContainerOptions;
use tokio::process::Command;

use crate::Workspace;
use crate::docker::Engine;
use crate::process::{Job, Shutdown, exit_code};

/// A group of Docker integration tests, run by one nextest invocation.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Suite {
    /// The daemon library's `ingress_caddy` tests, against Caddy, Pebble and
    /// challtestsrv in the engine.
    // They reach them on the ports of `PUBLISHED`.
    Ingress,
    /// `apps/piqueld/tests/docker_integration.rs`, against the engine's Swarm.
    Engine,
}

impl Suite {
    /// Every suite, in the order `docker-test` runs them without one named.
    const ALL: [Self; 2] = [Self::Ingress, Self::Engine];

    /// The nextest arguments that select the suite's tests.
    fn filter(self) -> &'static [&'static str] {
        match self {
            Self::Ingress => &["--lib", "--no-capture", "-E", "test(ingress_caddy)"],
            Self::Engine => &["--test", "docker_integration"],
        }
    }
}

/// Where the engine keeps images. Docker reports the same images
/// differently from each store, and hosts use both.
#[derive(Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Store {
    /// The classic image store, on the overlay2 graph driver: older engines'
    /// default, and engines upgraded from them.
    #[default]
    Classic,
    /// The containerd image store: the default of fresh Docker 29 installs.
    Containerd,
}

/// Runs `suite`, or every suite, against one engine keeping images in
/// `store`. `partition`, nextest's `--partition` such as `count:1/2`, selects
/// a shard of each suite's tests.
pub async fn run(
    workspace: &Workspace,
    suite: Option<Suite>,
    partition: Option<&str>,
    store: Store,
) -> Result<ExitCode> {
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
    let container = engine
        .launch(None, dind(image, runtime.path(), store))
        .await?;
    let suites = suite.as_ref().map_or(&Suite::ALL[..], std::slice::from_ref);
    let result = test(
        workspace,
        &engine,
        &container,
        runtime.path(),
        &mut shutdown,
        suites,
        partition,
    )
    .await;
    engine.remove(&container).await?;
    result
}

/// Ports the ingress tests reach on random loopback ports, with the variable
/// telling the tests each one: Caddy's HTTP, HTTPS and private HTTPS
/// listeners, then Pebble's ACME API and challtestsrv's DNS (TCP) and
/// management API, run inside the engine by the DNS-01 certificate test.
const PUBLISHED: [(&str, &str); 6] = [
    ("80/tcp", "PIQUELD_INGRESS_HTTP_PORT"),
    ("443/tcp", "PIQUELD_INGRESS_HTTPS_PORT"),
    ("8443/tcp", "PIQUELD_INGRESS_PRIVATE_PORT"),
    ("14000/tcp", "PIQUELD_PEBBLE_PORT"),
    ("8053/tcp", "PIQUELD_CHALLTESTSRV_DNS_PORT"),
    ("8055/tcp", "PIQUELD_CHALLTESTSRV_API_PORT"),
];

/// An engine with its socket, and the daemon's data, in `runtime`, keeping
/// images in `store` and publishing [`PUBLISHED`].
fn dind(image: String, runtime: &Path, store: Store) -> ContainerCreateBody {
    let runtime = runtime.display();
    let mut body = Engine::dind_container(image, &runtime, [format!("{runtime}:{runtime}")]);
    // The image keeps /var/lib/docker in a volume on the host's filesystem,
    // where overlay2 works. vfs copies every layer, which made the image-heavy
    // tests several times slower. Naming a graph driver selects the classic
    // image store; without one, the engine uses the containerd image store.
    if store == Store::Classic {
        body.cmd
            .get_or_insert_default()
            .push("--storage-driver=overlay2".to_owned());
    }
    #[expect(
        clippy::zero_sized_map_values,
        reason = "bollard requires HashMap<(), ()> for each exposed port"
    )]
    let exposed_ports = PUBLISHED
        .map(|(port, _)| (port.to_owned(), HashMap::new()))
        .into();
    body.exposed_ports = Some(exposed_ports);
    let host = body.host_config.get_or_insert_default();
    // Pre-mount /tmp so the entrypoint does not hide the data bind.
    host.tmpfs = Some(HashMap::from([(
        "/tmp".to_owned(),
        "rw,exec,dev".to_owned(),
    )]));
    host.port_bindings = Some(
        PUBLISHED
            .map(|(port, _)| {
                let loopback = PortBinding {
                    host_ip: Some("127.0.0.1".to_owned()),
                    host_port: Some(String::new()),
                };
                (port.to_owned(), Some(vec![loopback]))
            })
            .into(),
    );
    body
}

async fn test(
    workspace: &Workspace,
    engine: &Engine,
    container: &str,
    runtime: &Path,
    shutdown: &mut Shutdown,
    suites: &[Suite],
    partition: Option<&str>,
) -> Result<ExitCode> {
    engine.wait_for_dind(container).await?;
    let ports = engine
        .inspect_container(container, None::<InspectContainerOptions>)
        .await?
        .network_settings
        .and_then(|settings| settings.ports)
        .unwrap_or_default();
    let port = |name: &str| {
        ports
            .get(name)
            .and_then(|bindings| bindings.as_ref()?.first()?.host_port.clone())
            .with_context(|| format!("the engine publishes no {name} port"))
    };
    let socket = runtime.join("docker.sock");
    let nextest = |args: &[&str]| {
        let mut command = Command::from(workspace.cargo());
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
            .env("PIQUELD_DOCKER_SOCKET", &socket);
        command
    };
    let limit = Some(limit()?);
    // Tests share one engine and mutate its Swarm state, so they run serially.
    for &suite in suites {
        let mut command = nextest(suite.filter());
        if let Some(partition) = partition {
            command.args(["--partition", partition]);
        }
        if suite == Suite::Ingress {
            command.env("PIQUELD_DOCKER_DATA_DIR", runtime);
            for (name, variable) in PUBLISHED {
                command.env(variable, port(name)?);
            }
        }
        let status = Job::spawn(&mut command)?.finish(limit, shutdown).await?;
        if !status.success() {
            return Ok(exit_code(status));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// How long each test run may take: `PIQUELD_DOCKER_TEST_TIMEOUT`, such as
/// `90s`, `30m`, or `1h`, 15 minutes by default.
fn limit() -> Result<Duration> {
    let Ok(value) = std::env::var("PIQUELD_DOCKER_TEST_TIMEOUT") else {
        return Ok(Duration::from_mins(15));
    };
    let (number, unit) = value
        .split_at_checked(value.len().saturating_sub(1))
        .unwrap_or_default();
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
