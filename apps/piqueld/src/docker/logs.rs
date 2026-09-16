//! Read owned container output on demand. No daemon log persistence or followers.
use super::{BollardDocker, DockerError};
use bollard::{
    container::LogOutput,
    query_parameters::{ListServicesOptionsBuilder, ListTasksOptionsBuilder, LogsOptionsBuilder},
};
use futures_util::StreamExt;
use piqueld_core::{
    ApplicationId, InstanceId,
    api::{ApplicationLogs, LogRecord},
    resource::{APPLICATION_LABEL, INSTANCE_LABEL, MANAGED_LABEL, SERVICE_LABEL},
};
use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

impl BollardDocker {
    pub(super) async fn read_logs(
        &self,
        instance: &InstanceId,
        application: &ApplicationId,
        service: Option<&str>,
        tail: u16,
        since: u32,
        stream: Option<piqueld_core::api::LogStream>,
    ) -> Result<ApplicationLogs, DockerError> {
        let services = self.log_services(instance, application, service).await?;
        if services.is_empty() {
            return Ok(ApplicationLogs::default());
        }
        let tasks = self
            .docker
            .list_tasks(Some(
                ListTasksOptionsBuilder::default()
                    .filters(&HashMap::from([(
                        "service",
                        services.keys().cloned().collect::<Vec<_>>(),
                    )]))
                    .build(),
            ))
            .await
            .map_err(|e| DockerError::request("list log tasks", e))?;
        let mut result = ApplicationLogs::default();
        let mut bytes = 0usize;
        let since = Self::log_since(since);
        'tasks: for task in tasks.iter().take(256) {
            let Some(task_id) = task.id.as_ref() else {
                result.truncated = true;
                continue;
            };
            let Some(container) = task
                .status
                .as_ref()
                .and_then(|s| s.container_status.as_ref())
                .and_then(|s| s.container_id.as_ref())
            else {
                result.truncated = true;
                continue;
            };
            let Some(service) = task.service_id.as_ref().and_then(|id| services.get(id)) else {
                result.truncated = true;
                continue;
            };
            let mut logs = self.docker.logs(
                container,
                Some(
                    LogsOptionsBuilder::default()
                        .stdout(stream != Some(piqueld_core::api::LogStream::Stderr))
                        .stderr(stream != Some(piqueld_core::api::LogStream::Stdout))
                        .timestamps(true)
                        .since(since)
                        .tail(&(u32::from(tail) + 1).to_string())
                        .build(),
                ),
            );
            while let Some(item) = logs.next().await {
                let item = match item {
                    Ok(item) => item,
                    Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }) => {
                        result.truncated = true;
                        break;
                    }
                    Err(error) => return Err(DockerError::request("read container logs", error)),
                };
                let source = match &item {
                    LogOutput::StdOut { .. } => "stdout",
                    LogOutput::StdErr { .. } => "stderr",
                    _ => "console",
                };
                if stream.is_some_and(|filter| filter.as_str() != source) {
                    continue;
                }
                for line in item.to_string().lines() {
                    let (timestamp, message) = line.split_once(' ').unwrap_or(("", line));
                    let message = LogRecord::clean_message(message);
                    bytes = bytes.saturating_add(
                        message.len() + timestamp.len() + task_id.len() + service.len() + 128,
                    );
                    if bytes > 1024 * 1024 {
                        result.truncated = true;
                        break 'tasks;
                    }
                    result.items.push(LogRecord {
                        service: service.clone(),
                        task_id: task_id.clone(),
                        timestamp: timestamp.into(),
                        stream: source.into(),
                        message,
                    });
                }
            }
        }
        result.truncated |= tasks.len() > 256 || result.items.len() > usize::from(tail);
        result.items.sort_by(|a, b| {
            a.timestamp
                .cmp(&b.timestamp)
                .then(a.task_id.cmp(&b.task_id))
        });
        let excess = result.items.len().saturating_sub(usize::from(tail));
        result.items.drain(..excess);
        Ok(result)
    }

    fn log_since(window: u32) -> i32 {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_sub(u64::from(window));
        i32::try_from(timestamp).unwrap_or(i32::MAX)
    }

    async fn log_services(
        &self,
        instance: &InstanceId,
        application: &ApplicationId,
        service: Option<&str>,
    ) -> Result<HashMap<String, String>, DockerError> {
        Ok(self
            .docker
            .list_services(Some(
                ListServicesOptionsBuilder::default()
                    .filters(&HashMap::from([(
                        "label",
                        vec![
                            format!("{INSTANCE_LABEL}={instance}"),
                            format!("{APPLICATION_LABEL}={application}"),
                            format!("{MANAGED_LABEL}=true"),
                        ],
                    )]))
                    .build(),
            ))
            .await
            .map_err(|e| DockerError::request("list log services", e))?
            .into_iter()
            .filter_map(|s| {
                let id = s.id?;
                let name = s.spec?.labels?.get(SERVICE_LABEL)?.clone();
                service
                    .is_none_or(|wanted| wanted == name)
                    .then_some((id, name))
            })
            .collect::<HashMap<_, _>>())
    }
}
