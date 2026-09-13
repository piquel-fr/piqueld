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
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_sub(u64::from(since));
        let since = i32::try_from(since).unwrap_or(i32::MAX);
        'tasks: for task in tasks.iter().take(256) {
            let Some(task_id) = task.id.as_ref() else {
                continue;
            };
            let Some(container) = task
                .status
                .as_ref()
                .and_then(|s| s.container_status.as_ref())
                .and_then(|s| s.container_id.as_ref())
            else {
                continue;
            };
            let Some(service) = task.service_id.as_ref().and_then(|id| services.get(id)) else {
                continue;
            };
            let mut logs = self.docker.logs(
                container,
                Some(
                    LogsOptionsBuilder::default()
                        .stdout(true)
                        .stderr(true)
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
                    }) => break,
                    Err(error) => return Err(DockerError::request("read container logs", error)),
                };
                let stream = match &item {
                    LogOutput::StdOut { .. } => "stdout",
                    LogOutput::StdErr { .. } => "stderr",
                    _ => "console",
                };
                for line in item.to_string().lines() {
                    let (timestamp, message) = line.split_once(' ').unwrap_or(("", line));
                    let message = Self::log_text(message);
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
                        stream: stream.into(),
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
    fn log_text(value: &str) -> String {
        let mut output = String::new();
        let mut escape = false;
        let mut csi = false;
        for ch in value.chars() {
            if ch == '\u{1b}' {
                escape = true;
                csi = false;
                continue;
            }
            if escape {
                if ch == '[' && !csi {
                    csi = true;
                    continue;
                }
                if !csi || ('@'..='~').contains(&ch) {
                    escape = false;
                }
                continue;
            }
            if !ch.is_control() || ch == '\t' {
                output.push(ch);
            }
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::BollardDocker;
    #[test]
    fn container_output_cannot_control_the_terminal() {
        assert_eq!(
            BollardDocker::log_text("\x1b[31mred\x1b[0m\r\0\ttext"),
            "red\ttext"
        );
    }
}
