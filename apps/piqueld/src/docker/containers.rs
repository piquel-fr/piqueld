//! The containers piqueld runs, as container telemetry sees them: discovery,
//! followed output, one-shot resource usage, and start events. These back
//! the container methods of [`DockerApi`](super::DockerApi); `LimitedDocker`
//! puts discovery and usage polls under its observation limit.
use super::{
    BollardDocker, BollardResult, DockerError, HashMap, ListServicesOptionsBuilder, StreamExt,
};
use bollard::{
    container::LogOutput,
    models::{ContainerStatsResponse, ContainerSummary, ContainerSummaryStateEnum},
    query_parameters::{
        EventsOptionsBuilder, ListContainersOptionsBuilder, LogsOptionsBuilder, StatsOptionsBuilder,
    },
};
use futures_util::stream::BoxStream;
use piqueld_core::{
    EnvironmentId, InstanceId,
    api::LogStream,
    resource::{
        APPLICATION_LABEL, INGRESS_LABEL, INSTANCE_LABEL, JOB_LABEL, MANAGED_LABEL, SERVICE_LABEL,
        TELEMETRY_TARGET,
    },
};

/// Label Swarm puts on every task container, naming its service's ID.
const SWARM_SERVICE_LABEL: &str = "com.docker.swarm.service.id";
/// Label Swarm puts on every task container, naming its task's ID.
const SWARM_TASK_LABEL: &str = "com.docker.swarm.task.id";

/// A container piqueld runs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedContainer {
    /// Docker's container ID.
    pub id: String,
    /// Docker's name, without its leading `/`.
    pub name: String,
    /// Whether it has output to read.
    pub state: ContainerState,
    /// When Docker created it, in Unix seconds.
    pub created: i64,
    /// The Swarm task it runs; `None` for infrastructure containers.
    pub task: Option<String>,
    /// What it runs.
    pub workload: Workload,
    /// Whether it mounts the daemon's telemetry socket: a task of an
    /// environment's receiving service.
    pub receiver: bool,
}

/// Whether a container has output to read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContainerState {
    /// Not started yet: no output.
    Created,
    /// Running, paused or restarting: output may still come.
    Running,
    /// Exited or dead: its output is final.
    Stopped,
}

/// What a container runs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Workload {
    /// A task of a manifest service.
    Service {
        /// The environment deploying it.
        environment: EnvironmentId,
        /// The manifest service name.
        name: String,
    },
    /// A run of a manifest job.
    Job {
        /// The environment running it.
        environment: EnvironmentId,
        /// The manifest job name.
        name: String,
    },
    /// A container piqueld runs itself for ingress: `piqueld-gateway`,
    /// `piqueld-tailscale` or `piqueld-cloudflared`.
    Infrastructure(&'static str),
}

/// Lines of output Docker returned together.
#[derive(Debug, Default)]
pub struct LogLines {
    /// The stream they were written to; `None` for stdin, never exported.
    pub stream: Option<LogStream>,
    /// Each line's Docker timestamp, in Unix nanoseconds, and its text.
    pub lines: Vec<(i64, String)>,
}

/// One container's resource usage, as a one-shot stats read reports it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContainerUsage {
    /// CPU time used since it started, in nanoseconds.
    pub cpu_ns: Option<u64>,
    /// Memory in use, in bytes, page cache included.
    pub memory: Option<u64>,
    /// Its memory limit, or the host's memory without one, in bytes.
    pub memory_limit: Option<u64>,
    /// Bytes received and sent per network interface.
    pub networks: Vec<(String, u64, u64)>,
    /// Bytes read from and written to block devices.
    pub disk: Option<(u64, u64)>,
}

/// A container's followed output.
pub type ContainerLogs = BoxStream<'static, Result<LogLines, DockerError>>;
/// The IDs of containers starting, when Docker names them.
pub type ContainerStarts = BoxStream<'static, Result<Option<String>, DockerError>>;

impl BollardDocker {
    /// Lists every container `instance` runs. Task containers carry Swarm's
    /// labels rather than piqueld's, so they take their service's labels.
    pub(super) async fn list_managed_containers(
        &self,
        instance: &InstanceId,
    ) -> Result<Vec<ManagedContainer>, DockerError> {
        let services: HashMap<_, _> = self
            .docker
            .list_services(Some(
                ListServicesOptionsBuilder::default()
                    .filters(&HashMap::from([(
                        "label",
                        vec![
                            format!("{INSTANCE_LABEL}={instance}"),
                            format!("{MANAGED_LABEL}=true"),
                        ],
                    )]))
                    .build(),
            ))
            .await
            .or_request("list managed services")?
            .into_iter()
            .filter_map(|service| Some((service.id?, service.spec?.labels?)))
            .collect();
        Ok(self
            .docker
            .list_containers(Some(
                ListContainersOptionsBuilder::default().all(true).build(),
            ))
            .await
            .or_request("list containers")?
            .into_iter()
            .filter_map(|container| Self::managed_container(container, &services, instance))
            .collect())
    }

    /// Describes `container` when `instance` runs it.
    fn managed_container(
        container: ContainerSummary,
        services: &HashMap<String, HashMap<String, String>>,
        instance: &InstanceId,
    ) -> Option<ManagedContainer> {
        let mut labels = container.labels.unwrap_or_default();
        if let Some(service) = labels
            .get(SWARM_SERVICE_LABEL)
            .and_then(|id| services.get(id))
        {
            for (key, value) in service {
                labels.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        let label = |key: &str| labels.get(key).map(String::as_str);
        if label(MANAGED_LABEL) != Some("true")
            || label(INSTANCE_LABEL) != Some(&instance.to_string())
        {
            return None;
        }
        let name = container.names?.first()?.trim_start_matches('/').to_owned();
        let environment = || EnvironmentId::parse(label(APPLICATION_LABEL)?).ok();
        let workload = if let Some(job) = label(JOB_LABEL) {
            Workload::Job {
                environment: environment()?,
                name: job.to_owned(),
            }
        } else if let Some(service) = label(SERVICE_LABEL) {
            Workload::Service {
                environment: environment()?,
                name: service.to_owned(),
            }
        } else if label(INGRESS_LABEL) == Some("true") {
            Workload::Infrastructure(if name.ends_with("-tailscale") {
                "piqueld-tailscale"
            } else if name.ends_with("-cloudflared") {
                "piqueld-cloudflared"
            } else {
                "piqueld-gateway"
            })
        } else {
            return None;
        };
        Some(ManagedContainer {
            id: container.id?,
            state: match container.state? {
                ContainerSummaryStateEnum::CREATED | ContainerSummaryStateEnum::EMPTY => {
                    ContainerState::Created
                }
                ContainerSummaryStateEnum::RUNNING
                | ContainerSummaryStateEnum::PAUSED
                | ContainerSummaryStateEnum::RESTARTING => ContainerState::Running,
                ContainerSummaryStateEnum::REMOVING
                | ContainerSummaryStateEnum::EXITED
                | ContainerSummaryStateEnum::DEAD => ContainerState::Stopped,
            },
            created: container.created.unwrap_or_default(),
            task: label(SWARM_TASK_LABEL).map(str::to_owned),
            receiver: container
                .mounts
                .iter()
                .flatten()
                .any(|mount| mount.destination.as_deref() == Some(TELEMETRY_TARGET)),
            name,
            workload,
        })
    }

    /// Follows a container's stdout and stderr from `since`, in Unix seconds,
    /// until it stops; a stopped container's ends at once.
    pub(super) fn follow_container_logs(&self, id: &str, since: i32) -> ContainerLogs {
        self.docker
            .logs(
                id,
                Some(
                    LogsOptionsBuilder::default()
                        .follow(true)
                        .stdout(true)
                        .stderr(true)
                        .timestamps(true)
                        .since(since)
                        .build(),
                ),
            )
            .map(|item| {
                item.map(|output| Self::log_lines(&output))
                    .or_request("follow container logs")
            })
            .boxed()
    }

    /// Yields the ID of each container that starts from `since`, in Unix
    /// seconds, until the stream fails. Docker replays starts since then, so
    /// none is missed while the stream connects.
    pub(super) fn watch_container_starts(&self, since: i64) -> ContainerStarts {
        self.docker
            .events(Some(
                EventsOptionsBuilder::default()
                    .since(&since.to_string())
                    .filters(&HashMap::from([
                        ("type", vec!["container"]),
                        ("event", vec!["start"]),
                    ]))
                    .build(),
            ))
            .map(|event| {
                event
                    .map(|event| event.actor.and_then(|actor| actor.id))
                    .or_request("watch container starts")
            })
            .boxed()
    }

    /// Splits one chunk of timestamped output into lines. A line whose
    /// timestamp cannot be read is stamped now.
    ///
    /// ```text
    /// StdErr "2026-01-01T00:00:00.000000001Z failed\n"
    ///   -> stderr [(1767225600000000001, "failed")]
    /// ```
    fn log_lines(output: &LogOutput) -> LogLines {
        let stream = match output {
            LogOutput::StdOut { .. } | LogOutput::Console { .. } => Some(LogStream::Stdout),
            LogOutput::StdErr { .. } => Some(LogStream::Stderr),
            LogOutput::StdIn { .. } => None,
        };
        let lines = output
            .to_string()
            .lines()
            .map(|line| {
                let (timestamp, message) = line.split_once(' ').unwrap_or(("", line));
                let timestamp = unix_nanos(timestamp).unwrap_or_else(crate::store::now_ns);
                (timestamp, message.to_owned())
            })
            .collect();
        LogLines { stream, lines }
    }

    /// Reads a container's resource usage once, without waiting for a
    /// second sample; `None` once it is gone.
    pub(super) async fn read_container_usage(
        &self,
        id: &str,
    ) -> Result<Option<ContainerUsage>, DockerError> {
        let mut stats = self.docker.stats(
            id,
            Some(
                StatsOptionsBuilder::default()
                    .stream(false)
                    .one_shot(true)
                    .build(),
            ),
        );
        let Some(stats) = stats.next().await else {
            return Ok(None);
        };
        Ok(stats
            .found()
            .or_request("read container stats")?
            .map(Self::usage))
    }

    /// Takes the counters container telemetry exports from Docker's stats.
    /// Block I/O operations are `read` and `write` on cgroup v2, `Read` and
    /// `Write` on v1.
    fn usage(stats: ContainerStatsResponse) -> ContainerUsage {
        let memory = stats.memory_stats.unwrap_or_default();
        let disk = stats
            .blkio_stats
            .and_then(|blkio| blkio.io_service_bytes_recursive)
            .map(|entries| {
                entries.iter().fold((0, 0), |(read, write), entry| {
                    let value = entry.value.unwrap_or_default();
                    match entry.op.as_deref().map(str::to_ascii_lowercase).as_deref() {
                        Some("read") => (read + value, write),
                        Some("write") => (read, write + value),
                        _ => (read, write),
                    }
                })
            });
        let mut networks: Vec<_> = stats
            .networks
            .unwrap_or_default()
            .into_iter()
            .map(|(interface, network)| {
                (
                    interface,
                    network.rx_bytes.unwrap_or_default(),
                    network.tx_bytes.unwrap_or_default(),
                )
            })
            .collect();
        networks.sort();
        ContainerUsage {
            cpu_ns: stats.cpu_stats.and_then(|cpu| cpu.cpu_usage?.total_usage),
            memory: memory.usage,
            memory_limit: memory.limit,
            networks,
            disk,
        }
    }
}

/// Reads one of Docker's UTC RFC 3339 timestamps, such as
/// `2026-01-01T00:00:00.000000001Z`, as Unix nanoseconds.
fn unix_nanos(timestamp: &str) -> Option<i64> {
    let (date, time) = timestamp.strip_suffix('Z')?.split_once('T')?;
    let (clock, fraction) = time.split_once('.').unwrap_or((time, ""));
    let numbers = |text: &str, separator| -> Option<Vec<i64>> {
        text.split(separator)
            .map(|part| part.parse().ok())
            .collect::<Option<Vec<_>>>()
            .filter(|parts| parts.len() == 3)
    };
    let (date, clock) = (numbers(date, '-')?, numbers(clock, ':')?);
    let (year, month, day) = (date[0], date[1], date[2]);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || fraction.len() > 9
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let nanos: i64 = format!("{fraction:0<9}").parse().ok()?;
    // Days since 1970-01-01 in the proleptic Gregorian calendar, after
    // Howard Hinnant's `days_from_civil`.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds = days * 86_400 + clock[0] * 3_600 + clock[1] * 60 + clock[2];
    seconds.checked_mul(1_000_000_000)?.checked_add(nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_timestamps_read_as_unix_nanoseconds() {
        for (timestamp, nanos) in [
            ("1970-01-01T00:00:00Z", Some(0)),
            (
                "2026-01-01T00:00:00.000000001Z",
                Some(1_767_225_600_000_000_001),
            ),
            ("2024-02-29T23:59:59.5Z", Some(1_709_251_199_500_000_000)),
            ("2026-01-01T00:00:00+01:00", None),
            ("2026-13-01T00:00:00Z", None),
            ("not a timestamp", None),
        ] {
            assert_eq!(unix_nanos(timestamp), nanos, "{timestamp}");
        }
    }

    /// Task containers take their service's labels; other installations'
    /// containers and unlabelled ones are not managed.
    #[test]
    fn managed_containers_are_classified_by_their_labels() {
        let instance = InstanceId::parse("instance-1").unwrap();
        let labels = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect()
        };
        let owned = [(MANAGED_LABEL, "true"), (INSTANCE_LABEL, "instance-1")];
        let services = HashMap::from([(
            "service-1".to_owned(),
            labels(&[
                owned[0],
                owned[1],
                (APPLICATION_LABEL, "env-web-0001"),
                (SERVICE_LABEL, "web"),
            ]),
        )]);
        let container = |name: &str, labels: HashMap<String, String>| ContainerSummary {
            id: Some(format!("{name}-id")),
            names: Some(vec![format!("/{name}")]),
            labels: Some(labels),
            state: Some(ContainerSummaryStateEnum::RUNNING),
            created: Some(7),
            ..Default::default()
        };
        let classify = |container| {
            BollardDocker::managed_container(container, &services, &instance)
                .map(|managed| (managed.workload, managed.task))
        };
        assert_eq!(
            classify(container(
                "web.1.abc",
                labels(&[
                    (SWARM_SERVICE_LABEL, "service-1"),
                    (SWARM_TASK_LABEL, "task-1")
                ]),
            )),
            Some((
                Workload::Service {
                    environment: EnvironmentId::parse("env-web-0001").unwrap(),
                    name: "web".into(),
                },
                Some("task-1".into()),
            ))
        );
        assert_eq!(
            classify(container(
                "piqueld-ingress-0123-tailscale",
                labels(&[owned[0], owned[1], (INGRESS_LABEL, "true")]),
            )),
            Some((Workload::Infrastructure("piqueld-tailscale"), None))
        );
        let other = [(MANAGED_LABEL, "true"), (INSTANCE_LABEL, "instance-2")];
        for labels in [
            labels(&[]),
            labels(&[other[0], other[1], (INGRESS_LABEL, "true")]),
        ] {
            assert_eq!(classify(container("other", labels)), None);
        }
    }
}
