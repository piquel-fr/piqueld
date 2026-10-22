//! Commands executed inside one running task of an owned service.
use super::{BollardDocker, BollardResult, DockerError};
use bollard::{
    container::LogOutput,
    exec::{StartExecOptions, StartExecResults},
    models::ExecConfig,
    query_parameters::{ListTasksOptionsBuilder, ResizeExecOptions},
};
use futures_util::StreamExt;
use piqueld_core::{
    EnvironmentId, InstanceId,
    exec::{ExecInput, ExecOutput, ExecRequest},
};
use std::{collections::HashMap, convert::Infallible};
use tokio::{io::AsyncWriteExt, sync::mpsc};
use tokio_util::sync::CancellationToken;

/// A created Docker exec instance that has not started yet.
#[derive(Clone, Debug)]
pub struct Exec {
    /// Docker exec identity.
    pub id: String,
    /// Swarm task running the command.
    pub task: String,
    /// Whether the command has a pseudo-terminal.
    pub tty: bool,
}

/// Channels connecting a running command to its client transport.
pub struct ExecIo {
    /// Client input. [`ExecInput::CloseStdin`] ends standard input.
    pub input: mpsc::Receiver<ExecInput>,
    /// Command output. Only [`ExecOutput::Stdout`] and [`ExecOutput::Stderr`] are sent.
    pub output: mpsc::Sender<ExecOutput>,
    /// Cancelled by the transport to stop streaming, e.g. when the client
    /// is gone or lost access. A command already stopped is never started.
    pub stop: CancellationToken,
}

impl BollardDocker {
    /// Creates `request` in a task of its service whose container runs, or
    /// returns `None` when the service has none.
    pub(super) async fn create_task_exec(
        &self,
        instance: &InstanceId,
        environment: &EnvironmentId,
        request: &ExecRequest,
    ) -> Result<Option<Exec>, DockerError> {
        let services = self
            .owned_services(instance, environment, Some(request.service.as_str()))
            .await?;
        let Some(service) = services.into_keys().next() else {
            return Ok(None);
        };
        let tasks = self
            .client()?
            .list_tasks(Some(
                ListTasksOptionsBuilder::default()
                    .filters(&HashMap::from([
                        ("service", vec![service]),
                        ("desired-state", vec!["running".to_owned()]),
                    ]))
                    .build(),
            ))
            .await
            .or_request("list exec tasks")?;
        // Swarm reports a task as starting until its health check passes, but
        // its container already runs; prefer healthy tasks.
        let Some((_, task, container)) = tasks
            .into_iter()
            .filter_map(|task| {
                let status = task.status?;
                let preference = match status.state? {
                    bollard::models::TaskState::RUNNING => 0,
                    bollard::models::TaskState::STARTING => 1,
                    _ => return None,
                };
                Some((preference, task.id?, status.container_status?.container_id?))
            })
            .min_by_key(|(preference, ..)| *preference)
        else {
            return Ok(None);
        };
        let tty = request.tty.is_some();
        let created = self
            .client()?
            .create_exec(
                &container,
                ExecConfig {
                    attach_stdin: Some(request.stdin),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    tty: Some(tty),
                    console_size: request
                        .tty
                        .map(|size| vec![usize::from(size.height), usize::from(size.width)]),
                    cmd: Some(request.command.as_slice().to_vec()),
                    ..ExecConfig::default()
                },
            )
            .await
            .or_request("create exec")?;
        Ok(Some(Exec {
            id: created.id,
            task,
            tty,
        }))
    }

    /// Streams a created command until it exits and returns its exit code.
    ///
    /// Input failures only close standard input: a command may exit before
    /// reading all of it. A stop or failed output delivery means the client
    /// is gone, and streaming stops with an error.
    pub(super) async fn run_task_exec(&self, exec: &Exec, io: ExecIo) -> Result<i64, DockerError> {
        let StartExecResults::Attached {
            mut output,
            mut input,
        } = self
            .client()?
            .start_exec(
                &exec.id,
                Some(StartExecOptions {
                    detach: false,
                    tty: exec.tty,
                    output_capacity: None,
                }),
            )
            .await
            .or_request("start exec")?
        else {
            return Err(DockerError::Request("attach exec"));
        };
        let ExecIo {
            input: mut frames,
            output: sink,
            stop,
        } = io;
        // Output is pumped on its own so a command that stops reading its
        // input never stops its output from draining.
        let forward_output = async {
            while let Some(item) = output.next().await {
                let frame = match item.or_request("read exec output")? {
                    LogOutput::StdErr { message } => ExecOutput::Stderr(message.into()),
                    LogOutput::StdOut { message } | LogOutput::Console { message } => {
                        ExecOutput::Stdout(message.into())
                    }
                    LogOutput::StdIn { .. } => continue,
                };
                sink.send(frame)
                    .await
                    .map_err(|_| DockerError::Request("deliver exec output"))?;
            }
            Ok::<_, DockerError>(())
        };
        // Never returns: a blocked stdin write must not hide a stop, so stops
        // are watched separately.
        let client = self.client()?;
        let forward_input = async {
            let mut input_open = true;
            while let Some(frame) = frames.recv().await {
                match frame {
                    ExecInput::Stdin(data) if input_open => {
                        if let Err(error) = input.write_all(&data).await {
                            tracing::debug!(?error, "exec standard input closed");
                            input_open = false;
                        }
                    }
                    ExecInput::Stdin(_) => {}
                    ExecInput::Resize(size) => {
                        let options = ResizeExecOptions {
                            h: size.height.into(),
                            w: size.width.into(),
                        };
                        if let Err(error) = client.resize_exec(&exec.id, options).await {
                            tracing::debug!(?error, "exec terminal resize failed");
                        }
                    }
                    // Closing a terminal's input makes Docker close its output
                    // too, detaching from the command. Only a disconnected
                    // client does that; a terminal has no end of input.
                    ExecInput::CloseStdin if exec.tty => {}
                    ExecInput::CloseStdin => {
                        input_open = false;
                        if let Err(error) = input.shutdown().await {
                            tracing::debug!(?error, "exec standard input shutdown failed");
                        }
                    }
                }
            }
            std::future::pending::<Infallible>().await
        };
        // A stop wins over input still queued, which must not reach the command.
        tokio::select! {
            biased;
            () = stop.cancelled() => return Err(DockerError::Request("exec stopped by its client transport")),
            result = forward_output => result?,
            never = forward_input => match never {},
        }
        self.client()?
            .inspect_exec(&exec.id)
            .await
            .or_request("inspect exec")?
            .exit_code
            .ok_or(DockerError::Request("read exec exit code"))
    }
}
