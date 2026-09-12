//! Form drafts and section patches. Each save changes only its own settings group.
use piqueld_client::{HealthCheck, Mount, ResourceLimits, Service, Source};

/// Independently saved service settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Section {
    /// Image and scaling.
    General,
    /// Environment entries.
    Environment,
    /// Entrypoint and arguments.
    Process,
    /// Persistent mounts.
    Storage,
    /// Container health check.
    Health,
    /// CPU and memory limits.
    Resources,
}
impl Section {
    /// All form groups in display order.
    pub const ALL: [Self; 6] = [
        Self::General,
        Self::Environment,
        Self::Process,
        Self::Storage,
        Self::Health,
        Self::Resources,
    ];
    /// Human-readable form heading.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::General => "Image & scaling",
            Self::Environment => "Environment",
            Self::Process => "Command & arguments",
            Self::Storage => "Volume mounts",
            Self::Health => "Health check",
            Self::Resources => "Resource limits",
        }
    }
}

/// Form state retains incomplete text until explicit validation and saving.
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceForm {
    /// Container image.
    pub image: String,
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
    /// Optional CPU limit in millicores.
    pub cpu: String,
    /// Optional memory limit in bytes.
    pub memory: String,
}
impl From<&Service> for ServiceForm {
    fn from(service: &Service) -> Self {
        let Source::Image { image } = &service.source;
        let mut form = Self {
            image: image.clone(),
            replicas: service.replicas.to_string(),
            environment: service
                .environment
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            command: service.command.clone(),
            arguments: service.arguments.clone(),
            mounts: service.mounts.clone(),
            health_kind: "none".into(),
            port: "8080".into(),
            path: "/health".into(),
            interval: "10".into(),
            timeout: "3".into(),
            health_command: Vec::new(),
            cpu: service
                .resources
                .as_ref()
                .and_then(|r| r.cpu_millis)
                .map_or_else(String::new, |v| v.to_string()),
            memory: service
                .resources
                .as_ref()
                .and_then(|r| r.memory_bytes)
                .map_or_else(String::new, |v| v.to_string()),
        };
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
                form.path.clone_from(path);
                form.interval = interval_seconds.to_string();
                form.timeout = timeout_seconds.to_string();
            }
            Some(HealthCheck::Command {
                command,
                interval_seconds,
                timeout_seconds,
            }) => {
                form.health_kind = "command".into();
                form.health_command.clone_from(command);
                form.interval = interval_seconds.to_string();
                form.timeout = timeout_seconds.to_string();
            }
        }
        form
    }
}
impl ServiceForm {
    /// Applies a single group to a fresh copy of saved configuration.
    /// # Errors
    /// Returns actionable errors for malformed numeric fields or duplicate environment keys.
    pub fn patch(&self, section: Section, service: &mut Service) -> Result<(), String> {
        match section {
            Section::General => {
                service.source = Source::Image {
                    image: self.image.clone(),
                };
                service.replicas = self
                    .replicas
                    .parse()
                    .map_err(|_| "Replicas must be an integer between 0 and 65535.")?;
            }
            Section::Environment => {
                let mut values = std::collections::BTreeMap::new();
                for (key, value) in &self.environment {
                    if values.insert(key.clone(), value.clone()).is_some() {
                        return Err(format!("Environment key {key:?} appears more than once."));
                    }
                }
                service.environment = values;
            }
            Section::Process => {
                service.command.clone_from(&self.command);
                service.arguments.clone_from(&self.arguments);
            }
            Section::Storage => service.mounts.clone_from(&self.mounts),
            Section::Health => {
                service.healthcheck = match self.health_kind.as_str() {
                    "none" => None,
                    "http" => Some(HealthCheck::Http {
                        port: self
                            .port
                            .parse()
                            .map_err(|_| "Health port must be an integer between 1 and 65535.")?,
                        path: self.path.clone(),
                        interval_seconds: self
                            .interval
                            .parse()
                            .map_err(|_| "Health interval must be a positive integer.")?,
                        timeout_seconds: self
                            .timeout
                            .parse()
                            .map_err(|_| "Health timeout must be a positive integer.")?,
                    }),
                    "command" => Some(HealthCheck::Command {
                        command: self.health_command.clone(),
                        interval_seconds: self
                            .interval
                            .parse()
                            .map_err(|_| "Health interval must be a positive integer.")?,
                        timeout_seconds: self
                            .timeout
                            .parse()
                            .map_err(|_| "Health timeout must be a positive integer.")?,
                    }),
                    _ => return Err("Choose a supported health check type.".into()),
                }
            }
            Section::Resources => {
                let cpu = if self.cpu.trim().is_empty() {
                    None
                } else {
                    Some(
                        self.cpu
                            .parse()
                            .map_err(|_| "CPU must be a positive integer in millicores.")?,
                    )
                };
                let memory = if self.memory.trim().is_empty() {
                    None
                } else {
                    Some(
                        self.memory
                            .parse()
                            .map_err(|_| "Memory must be a positive integer in bytes.")?,
                    )
                };
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
            name: "web".into(),
            source: Source::Image {
                image: "nginx:stable".into(),
            },
            replicas: 1,
            environment: std::collections::BTreeMap::new(),
            command: vec!["entrypoint".into()],
            arguments: vec!["argument with spaces".into()],
            mounts: Vec::new(),
            healthcheck: None,
            resources: None,
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
    fn invalid_text_and_duplicate_keys_are_not_silently_discarded() {
        let mut saved = service();
        let mut draft = ServiceForm::from(&saved);
        draft.replicas = "invalid".into();
        assert!(draft.patch(Section::General, &mut saved).is_err());
        draft.environment = vec![("A".into(), "one".into()), ("A".into(), "two".into())];
        assert!(draft.patch(Section::Environment, &mut saved).is_err());
    }
}
