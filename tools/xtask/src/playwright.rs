//! `just test-playwright`: the browser suite, with Chromium in a pinned
//! container and the Rust fixture and test runner on the host.

use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use bollard::models::{ContainerCreateBody, HostConfig};
use tokio::process::Command;

use crate::Workspace;
use crate::docker::Engine;
use crate::process::{Job, Shutdown, exit_code};

pub async fn run(workspace: &Workspace, args: &[String]) -> Result<ExitCode> {
    let tests = workspace.root().join("tests/playwright");
    let package: serde_json::Value = serde_json::from_slice(
        &std::fs::read(tests.join("package.json")).context("read package.json")?,
    )?;
    let version = package["devDependencies"]["@playwright/test"]
        .as_str()
        .context("package.json does not pin @playwright/test")?;
    // Only the browser server listens here, on loopback.
    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    let engine = Engine::host().await?;
    let mut shutdown = Shutdown::listen()?;
    // The host network keeps WebAuthn on the secure-context localhost. There
    // is no implicit browser download: `just setup-playwright` pulls the image.
    let browser = ContainerCreateBody {
        image: Some(format!("mcr.microsoft.com/playwright:v{version}-noble")),
        cmd: Some(
            [
                "node",
                "node_modules/@playwright/test/cli.js",
                "run-server",
                "--host",
                "127.0.0.1",
                "--port",
                &port.to_string(),
            ]
            .map(str::to_owned)
            .to_vec(),
        ),
        working_dir: Some("/tests".to_owned()),
        host_config: Some(HostConfig {
            init: Some(true),
            network_mode: Some("host".to_owned()),
            ipc_mode: Some("host".to_owned()),
            binds: Some(vec![format!("{}:/tests:ro", tests.display())]),
            ..HostConfig::default()
        }),
        ..ContainerCreateBody::default()
    };
    let container = engine
        .launch(None, browser)
        .await
        .context("start the browser; `just setup-playwright` pulls its image")?;
    let result = async {
        engine
            .wait_for_output(&container, "Listening on", Duration::from_secs(10))
            .await?;
        let status = Job::spawn(
            Command::new("pnpm")
                .args(["exec", "playwright", "test"])
                .args(args)
                .env(
                    "PW_TEST_CONNECT_WS_ENDPOINT",
                    format!("ws://127.0.0.1:{port}/"),
                )
                .current_dir(&tests),
        )?
        .finish(None, &mut shutdown)
        .await?;
        Ok(exit_code(status))
    }
    .await;
    engine.remove(&container).await?;
    result
}
