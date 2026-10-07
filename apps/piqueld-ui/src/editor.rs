//! Form drafts and section patches. Each save changes only its own settings group.
//!
//! Fields that accept `${{ }}` references keep them as text: typed fields
//! parse either a literal or a reference when saved.
use piqueld_client::{
    Build, GitRepository, HealthCheck, ManifestRepository, Mount, ResourceLimits, Rollout, Service,
    Source, SourceRepository, Template, Typed,
};

/// Independently saved service settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Section {
    /// Source and scaling.
    General,
    /// Environment entries.
    Environment,
    /// Entrypoint and arguments.
    Process,
    /// Persistent mounts.
    Storage,
    /// Container health check.
    Health,
    /// Services that must be healthy before this one rolls out.
    Dependencies,
    /// Update order and monitor window.
    Rollout,
    /// CPU and memory limits.
    Resources,
}
impl Section {
    /// All form groups in display order.
    pub const ALL: [Self; 8] = [
        Self::General,
        Self::Environment,
        Self::Process,
        Self::Storage,
        Self::Health,
        Self::Dependencies,
        Self::Rollout,
        Self::Resources,
    ];
    /// Human-readable form heading.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::General => "Source & scaling",
            Self::Environment => "Environment",
            Self::Process => "Command & arguments",
            Self::Storage => "Volume mounts",
            Self::Health => "Health check",
            Self::Dependencies => "Startup dependencies",
            Self::Rollout => "Rollout",
            Self::Resources => "Resource limits",
        }
    }
}

/// Form state retains incomplete text until explicit validation and saving.
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceForm {
    /// Explicit source kind, image or git.
    pub source_kind: String,
    /// Container image.
    pub image: String,
    /// Git repository URL or host path.
    pub repository: String,
    /// Git branch.
    pub branch: String,
    /// Optional pinned commit.
    pub commit: String,
    /// Dockerfile relative to the repository root.
    pub dockerfile: String,
    /// Build context relative to the repository root.
    pub context: String,
    /// Docker build argument rows; duplicate keys are rejected.
    pub build_args: Vec<(String, String)>,
    /// Optional multi-stage build target; blank builds the final stage.
    pub target: String,
    /// Replica count, before numeric validation.
    pub replicas: String,
    /// Environment rows; duplicate keys are rejected.
    pub environment: Vec<(String, String)>,
    /// Entrypoint elements.
    pub command: Vec<String>,
    /// Argument elements.
    pub arguments: Vec<String>,
    /// Volume mounts.
    pub mounts: Vec<Mount>,
    /// None, HTTP, or command.
    pub health_kind: String,
    /// HTTP port.
    pub port: String,
    /// HTTP path.
    pub path: String,
    /// Health check interval in seconds.
    pub interval: String,
    /// Health check timeout in seconds.
    pub timeout: String,
    /// Health command elements.
    pub health_command: Vec<String>,
    /// Sorted names of services that must be healthy first.
    pub depends_on: Vec<String>,
    /// `derived`, `stop-first`, or `start-first`.
    pub rollout_order: String,
    /// Optional rollout monitor window in seconds.
    pub monitor: String,
    /// Optional CPU limit in millicores.
    pub cpu: String,
    /// Optional memory limit in bytes.
    pub memory: String,
}
impl From<&Service> for ServiceForm {
    /// Seeds a draft from saved configuration. Fields for the unused source or
    /// health check kind get editable defaults (branch `main`, port `8080`, …).
    fn from(service: &Service) -> Self {
        let mut form = Self {
            source_kind: "image".into(),
            image: String::new(),
            repository: String::new(),
            branch: "main".into(),
            commit: String::new(),
            dockerfile: "Dockerfile".into(),
            context: ".".into(),
            build_args: Vec::new(),
            target: String::new(),
            replicas: service.replicas.to_string(),
            environment: service
                .environment
                .iter()
                .map(|(k, v)| (k.clone(), v.to_string()))
                .collect(),
            command: texts(&service.command),
            arguments: texts(&service.arguments),
            mounts: service.mounts.clone(),
            health_kind: "none".into(),
            port: "8080".into(),
            path: "/health".into(),
            interval: "10".into(),
            timeout: "3".into(),
            health_command: Vec::new(),
            depends_on: service.depends_on.clone(),
            rollout_order: service
                .rollout
                .order
                .as_ref()
                .map_or_else(|| "derived".into(), ToString::to_string),
            monitor: optional_text(service.rollout.monitor_seconds.as_ref()),
            cpu: optional_text(
                service
                    .resources
                    .as_ref()
                    .and_then(|r| r.cpu_millis.as_ref()),
            ),
            memory: optional_text(
                service
                    .resources
                    .as_ref()
                    .and_then(|r| r.memory_bytes.as_ref()),
            ),
        };
        match &service.source {
            Source::Image { image } => form.image = image.to_string(),
            Source::Git {
                repository,
                build:
                    Build::Docker {
                        dockerfile,
                        context,
                        args,
                        target,
                    },
            } => {
                form.source_kind = "self".into();
                if let SourceRepository::Git(repository) = repository {
                    form.source_kind = "git".into();
                    form.repository.clone_from(&repository.url);
                    form.branch.clone_from(&repository.branch);
                    form.commit = repository.commit.clone().unwrap_or_default();
                }
                form.dockerfile = dockerfile.to_string();
                form.context = context.to_string();
                form.build_args = args
                    .iter()
                    .map(|(key, value)| (key.clone(), value.to_string()))
                    .collect();
                form.target = target.as_ref().map(ToString::to_string).unwrap_or_default();
            }
        }
        match &service.healthcheck {
            None => {}
            Some(HealthCheck::Http {
                port,
                path,
                interval_seconds,
                timeout_seconds,
            }) => {
                form.health_kind = "http".into();
                form.port = port.to_string();
                form.path = path.to_string();
                form.interval = interval_seconds.to_string();
                form.timeout = timeout_seconds.to_string();
            }
            Some(HealthCheck::Command {
                command,
                interval_seconds,
                timeout_seconds,
            }) => {
                form.health_kind = "command".into();
                form.health_command = texts(command);
                form.interval = interval_seconds.to_string();
                form.timeout = timeout_seconds.to_string();
            }
        }
        form
    }
}
/// Template text of each element.
fn texts(values: &[Template]) -> Vec<String> {
    values.iter().map(ToString::to_string).collect()
}

/// Text of an optional typed value; empty when unset.
fn optional_text<T: std::fmt::Display>(value: Option<&Typed<T>>) -> String {
    value.map_or_else(String::new, ToString::to_string)
}

/// Templates for each element.
fn templates(values: &[String]) -> Vec<Template> {
    values.iter().map(|value| value.as_str().into()).collect()
}

/// Parses an optional typed field: empty text is `None`, `${{ }}` text a
/// reference, anything else a literal; `error` explains the expected literal.
fn optional<T: std::str::FromStr>(value: &str, error: &str) -> Result<Option<Typed<T>>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    value.parse().map(Some).map_err(|_| error.into())
}

impl ServiceForm {
    /// Builds the edited source.
    fn source(&self) -> Result<Source, String> {
        let repository = match self.source_kind.as_str() {
            "image" => {
                return Ok(Source::Image {
                    image: self.image.as_str().into(),
                });
            }
            "git" => SourceRepository::Git(GitRepository {
                url: self.repository.clone(),
                branch: self.branch.clone(),
                commit: (!self.commit.is_empty()).then(|| self.commit.clone()),
            }),
            "self" => SourceRepository::Manifest(ManifestRepository::Manifest),
            _ => return Err("Choose image or Git as the source.".into()),
        };
        Ok(Source::Git {
            repository,
            build: Build::Docker {
                dockerfile: self.dockerfile.as_str().into(),
                context: self.context.as_str().into(),
                args: Self::unique_keys("Build argument", &self.build_args)?,
                target: (!self.target.is_empty()).then(|| self.target.as_str().into()),
            },
        })
    }

    /// Collects key/value rows into a map, rejecting keys that appear twice.
    /// `noun` names the rows in the error.
    fn unique_keys(
        noun: &str,
        rows: &[(String, String)],
    ) -> Result<std::collections::BTreeMap<String, Template>, String> {
        let mut values = std::collections::BTreeMap::new();
        for (key, value) in rows {
            if values
                .insert(key.clone(), Template::from(value.as_str()))
                .is_some()
            {
                return Err(format!("{noun} key {key:?} appears more than once."));
            }
        }
        Ok(values)
    }

    /// Parses the selected health check kind and its numeric fields.
    fn healthcheck(&self) -> Result<Option<HealthCheck>, String> {
        let interval_seconds = || {
            self.interval
                .parse()
                .map_err(|_| "Health interval must be a positive integer or a ${{ }} reference.")
        };
        let timeout_seconds = || {
            self.timeout
                .parse()
                .map_err(|_| "Health timeout must be a positive integer or a ${{ }} reference.")
        };
        Ok(match self.health_kind.as_str() {
            "none" => None,
            "http" => Some(HealthCheck::Http {
                port: self.port.parse().map_err(
                    |_| "Health port must be an integer between 1 and 65535 or a ${{ }} reference.",
                )?,
                path: self.path.as_str().into(),
                interval_seconds: interval_seconds()?,
                timeout_seconds: timeout_seconds()?,
            }),
            "command" => Some(HealthCheck::Command {
                command: templates(&self.health_command),
                interval_seconds: interval_seconds()?,
                timeout_seconds: timeout_seconds()?,
            }),
            _ => return Err("Choose a supported health check type.".into()),
        })
    }

    /// Parses the selected rollout order and the optional monitor window.
    fn rollout(&self) -> Result<Rollout, String> {
        Ok(Rollout {
            order: match self.rollout_order.as_str() {
                "derived" => None,
                order => Some(order.parse()?),
            },
            monitor_seconds: optional(
                &self.monitor,
                "Monitor window must be a positive integer in seconds or a ${{ }} reference.",
            )?,
        })
    }

    /// Applies a single group to a fresh copy of saved configuration.
    /// # Errors
    /// Returns actionable errors for malformed numeric fields or duplicate
    /// environment or build argument keys.
    pub fn patch(&self, section: Section, service: &mut Service) -> Result<(), String> {
        match section {
            Section::General => {
                service.source = self.source()?;
                service.replicas = self.replicas.parse().map_err(
                    |_| "Replicas must be an integer between 0 and 65535 or a ${{ }} reference.",
                )?;
            }
            Section::Environment => {
                service.environment = Self::unique_keys("Environment", &self.environment)?;
            }
            Section::Process => {
                service.command = templates(&self.command);
                service.arguments = templates(&self.arguments);
            }
            Section::Storage => service.mounts.clone_from(&self.mounts),
            Section::Health => service.healthcheck = self.healthcheck()?,
            Section::Dependencies => service.depends_on.clone_from(&self.depends_on),
            Section::Rollout => service.rollout = self.rollout()?,
            Section::Resources => {
                let cpu = optional(
                    &self.cpu,
                    "CPU must be a positive integer in millicores or a ${{ }} reference.",
                )?;
                let memory = optional(
                    &self.memory,
                    "Memory must be a positive integer in bytes or a ${{ }} reference.",
                )?;
                service.resources = if cpu.is_none() && memory.is_none() {
                    None
                } else {
                    Some(ResourceLimits {
                        cpu_millis: cpu,
                        memory_bytes: memory,
                    })
                };
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn service() -> Service {
        Service {
            secrets: Vec::new(),
            name: "web".into(),
            source: Source::Image {
                image: "nginx:stable".into(),
            },
            replicas: 1.into(),
            environment: std::collections::BTreeMap::new(),
            command: vec!["entrypoint".into()],
            arguments: vec!["argument with spaces".into()],
            mounts: Vec::new(),
            healthcheck: None,
            resources: None,
            depends_on: Vec::new(),
            rollout: Rollout::default(),
        }
    }
    #[test]
    fn saving_one_group_preserves_other_saved_settings() {
        let mut saved = service();
        let mut draft = ServiceForm::from(&saved);
        draft.image = "nginx:new".into();
        draft.environment.push(("UNSAVED".into(), "value".into()));
        saved.arguments.push("saved elsewhere".into());
        draft.patch(Section::General, &mut saved).unwrap();
        assert!(saved.environment.is_empty());
        assert_eq!(
            saved.arguments,
            vec!["argument with spaces", "saved elsewhere"]
        );
        assert!(matches!(saved.source,Source::Image{image} if image=="nginx:new"));
    }
    #[test]
    fn health_sections_round_trip() {
        for check in [
            HealthCheck::Http {
                port: 9000.into(),
                path: "/live".into(),
                interval_seconds: 5.into(),
                timeout_seconds: 2.into(),
            },
            HealthCheck::Command {
                command: vec!["curl".into(), "-f".into(), "localhost".into()],
                interval_seconds: 7.into(),
                timeout_seconds: 4.into(),
            },
        ] {
            let mut saved = service();
            saved.healthcheck = Some(check.clone());
            let draft = ServiceForm::from(&saved);
            saved.healthcheck = None;
            draft.patch(Section::Health, &mut saved).unwrap();
            assert_eq!(saved.healthcheck, Some(check));
        }
    }
    #[test]
    fn git_source_round_trips_and_edits_build_settings() {
        let mut saved = service();
        saved.source = Source::Git {
            repository: SourceRepository::Git(GitRepository {
                url: "https://example.com/app.git".into(),
                branch: "release".into(),
                commit: Some("a".repeat(40)),
            }),
            build: Build::Docker {
                dockerfile: "infra/Dockerfile".into(),
                context: "app".into(),
                args: [("ORIGIN".into(), "https://example.com".into())].into(),
                target: Some("runtime".into()),
            },
        };
        let source = saved.source.clone();
        let mut draft = ServiceForm::from(&saved);
        draft.replicas = "3".into();
        draft.patch(Section::General, &mut saved).unwrap();
        assert_eq!(saved.source, source);
        assert_eq!(saved.replicas, 3.into());

        draft.build_args.clear();
        draft.target.clear();
        draft.patch(Section::General, &mut saved).unwrap();
        assert!(matches!(
            &saved.source,
            Source::Git { build: Build::Docker { args, target: None, .. }, .. } if args.is_empty()
        ));
        draft.build_args = vec![("A".into(), "1".into()), ("A".into(), "2".into())];
        assert!(draft.patch(Section::General, &mut saved).is_err());
    }
    #[test]
    fn invalid_text_and_duplicate_keys_are_not_silently_discarded() {
        let mut saved = service();
        let mut draft = ServiceForm::from(&saved);
        draft.replicas = "invalid".into();
        assert!(draft.patch(Section::General, &mut saved).is_err());
        draft.environment = vec![("A".into(), "one".into()), ("A".into(), "two".into())];
        assert!(draft.patch(Section::Environment, &mut saved).is_err());
    }
}
