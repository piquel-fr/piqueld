//! Commands executed inside one running task of an owned service.
use super::{BollardDocker, DockerError};
use bollard::{
    container::LogOutput,
    exec::{StartExecOptions, StartExecResults},
    models::ExecConfig,
    query_parameters::{ListTasksOptionsBuilder, ResizeExecOptions},
};
use futures_util::StreamExt;
use piqueld_core::{
    ApplicationId, InstanceId,
    exec::{ExecInput, ExecOutput, ExecRequest},
};
use std::collections::HashMap;
use tokio::{io::AsyncWriteExt, sync::mpsc};

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
    /// Client input. Closing the channel closes the command's standard input.
    pub input: mpsc::Receiver<ExecInput>,
    /// Command output. Only [`ExecOutput::Stdout`] and [`ExecOutput::Stderr`] are sent.
    pub output: mpsc::Sender<ExecOutput>,
}

impl BollardDocker {
    /// Creates `request` in a task of its service whose container runs, or
    /// returns `None` when the service has none.
    pub(super) async fn create_task_exec(
        &self,
        instance: &InstanceId,
        application: &ApplicationId,
        request: &ExecRequest,
    ) -> Result<Option<Exec>, DockerError> {
        let services = self
            .owned_services(instance, application, Some(request.service.as_str()))
            .await?;
        let Some(service) = services.into_keys().next() else {
            return Ok(None);
        };
        let tasks = self
            .docker
            .list_tasks(Some(
                ListTasksOptionsBuilder::default()
                    .filters(&HashMap::from([
                        ("service", vec![service]),
                        ("desired-state", vec!["running".to_owned()]),
                    ]))
                    .build(),
            ))
            .await
            .map_err(|e| DockerError::request("list exec tasks", e))?;
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
            .docker
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
            .map_err(|e| DockerError::request("create exec", e))?;
        Ok(Some(Exec {
            id: created.id,
            task,
            tty,
        }))
    }

    /// Streams a created command until it exits and returns its exit code.
    ///
    /// Input failures only close standard input: a command may exit before
    /// reading all of it. Output delivery failures mean the client is gone.
    pub(super) async fn run_task_exec(
        &self,
        exec: &Exec,
        mut io: ExecIo,
    ) -> Result<i64, DockerError> {
        let StartExecResults::Attached {
            mut output,
            mut input,
        } = self
            .docker
            .start_exec(
                &exec.id,
                Some(StartExecOptions {
                    detach: false,
                    tty: exec.tty,
                    output_capacity: None,
                }),
            )
            .await
            .map_err(|e| DockerError::request("start exec", e))?
        else {
            return Err(DockerError::Request("attach exec"));
        };
        let mut input_open = true;
        loop {
            tokio::select! {
                item = output.next() => {
                    let frame = match item {
                        None => break,
                        Some(Err(error)) => return Err(DockerError::request("read exec output", error)),
                        Some(Ok(LogOutput::StdErr { message })) => ExecOutput::Stderr(message.into()),
                        Some(Ok(LogOutput::StdOut { message } | LogOutput::Console { message })) => {
                            ExecOutput::Stdout(message.into())
                        }
                        Some(Ok(LogOutput::StdIn { .. })) => continue,
                    };
                    io.output
                        .send(frame)
                        .await
                        .map_err(|_| DockerError::Request("deliver exec output"))?;
                }
                frame = io.input.recv(), if input_open => match frame {
                    Some(ExecInput::Stdin(data)) => {
                        if let Err(error) = input.write_all(&data).await {
                            tracing::debug!(?error, "exec standard input closed");
                            input_open = false;
                        }
                    }
                    Some(ExecInput::Resize(size)) => {
                        let options = ResizeExecOptions {
                            h: size.height.into(),
                            w: size.width.into(),
                        };
                        if let Err(error) = self.docker.resize_exec(&exec.id, options).await {
                            tracing::debug!(?error, "exec terminal resize failed");
                        }
                    }
                    // Closing a terminal's input makes Docker close its output
                    // too, detaching from the command. Only a disconnected
                    // client does that; a terminal has no end of input.
                    Some(ExecInput::CloseStdin) if exec.tty => {}
                    Some(ExecInput::CloseStdin) | None => {
                        input_open = false;
                        if let Err(error) = input.shutdown().await {
                            tracing::debug!(?error, "exec standard input shutdown failed");
                        }
                    }
                },
            }
        }
        self.docker
            .inspect_exec(&exec.id)
            .await
            .map_err(|e| DockerError::request("inspect exec", e))?
            .exit_code
            .ok_or(DockerError::Request("read exec exit code"))
    }
}
