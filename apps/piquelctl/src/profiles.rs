//! Resolve connection configuration once before starting the command deadline.
use crate::{
    cli::{Cli, parse_duration},
    error::{CliError, ErrorKind, Result},
};
use clap::{ArgMatches, parser::ValueSource};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, collections::BTreeMap, fmt, path::PathBuf};

/// Effective profile data, independent of its human or machine presentation.
#[derive(Serialize)]
pub(crate) struct ProfileSummary<'a> {
    pub(crate) name: &'a str,
    pub(crate) endpoint: Cow<'a, str>,
}

/// Sources are tracked independently: endpoint and timeout may use different overrides.
#[derive(Debug, Default)]
pub(crate) struct ConnectionSources {
    pub(crate) endpoint: Source,
    pub(crate) timeout: Source,
}

#[derive(Clone, Debug, Default)]
pub(crate) enum Source {
    #[default]
    Default,
    Flag(&'static str),
    Environment(&'static str),
    Profile {
        name: String,
        path: PathBuf,
    },
}

impl fmt::Display for Source {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => formatter.write_str("built-in default"),
            Self::Flag(name) => write!(formatter, "flag --{name}"),
            Self::Environment(name) => write!(formatter, "environment variable {name}"),
            Self::Profile { name, path } => {
                write!(formatter, "profile {name:?} in {}", path.display())
            }
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Profiles {
    profiles: BTreeMap<String, Profile>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    socket: Option<PathBuf>,
    url: Option<String>,
    timeout: Option<String>,
    #[serde(skip)]
    source: PathBuf,
}
impl Profiles {
    pub(crate) fn load(cli: &Cli) -> Result<Self> {
        let explicit_file = cli
            .profiles_file
            .clone()
            .or_else(|| std::env::var_os("PIQUELD_PROFILES_FILE").map(PathBuf::from));
        if let Some(path) = explicit_file {
            let source = if cli.profiles_file.is_some() {
                Source::Flag("profiles-file")
            } else {
                Source::Environment("PIQUELD_PROFILES_FILE")
            };
            let context = format!("profiles file {} ({source})", path.display());
            return Self::load_files([(path, true)]).map_err(|error| error.configuration(context));
        }
        let user_path = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|root| root.join("piqueld/profiles.toml"));
        Self::load_files(
            std::iter::once(PathBuf::from("/etc/piqueld/profiles.toml"))
                .chain(user_path)
                .map(|path| (path, false)),
        )
    }

    /// Merge entire profiles before validating, so replacements never inherit fields.
    fn load_files(files: impl IntoIterator<Item = (PathBuf, bool)>) -> Result<Self> {
        let mut result = Self::default();
        for (path, required) in files {
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => continue,
                Err(error) => {
                    return Err(Self::invalid(format!(
                        "Read profiles file {}: {error}",
                        path.display()
                    )));
                }
            };
            let mut loaded = Self::parse_file(&text, &path)?;
            for profile in loaded.profiles.values_mut() {
                profile.source.clone_from(&path);
            }
            result.profiles.extend(loaded.profiles);
        }
        for (name, profile) in &result.profiles {
            profile.validate().map_err(|error| {
                Self::invalid(format!(
                    "Invalid connection profile {name} in {}: {error}",
                    profile.source.display()
                ))
            })?;
        }
        Ok(result)
    }

    pub(crate) fn summaries(&self) -> Vec<ProfileSummary<'_>> {
        self.profiles
            .iter()
            .map(|(name, profile)| ProfileSummary {
                name,
                endpoint: profile.endpoint(),
            })
            .collect()
    }

    pub(crate) fn resolve(&self, cli: &mut Cli, matches: &ArgMatches) -> Result<()> {
        let selected = cli
            .profile
            .clone()
            .or_else(|| std::env::var("PIQUELD_PROFILE").ok());
        let profile = match selected.as_ref() {
            Some(name) => Some(self.profiles.get(name).ok_or_else(|| {
                Self::invalid(format!("Unknown connection profile: {name}")).configuration(
                    if cli.profile.is_some() {
                        Source::Flag("profile")
                    } else {
                        Source::Environment("PIQUELD_PROFILE")
                    }
                    .to_string(),
                )
            })?),
            None => self.profiles.get("default"),
        };
        let profile_source = profile.map_or(Source::Default, |profile| Source::Profile {
            name: selected.unwrap_or_else(|| "default".to_owned()),
            path: profile.source.clone(),
        });
        // Resolve a transport as a pair: a higher-priority URL replaces a lower socket.
        if cli.socket.is_some() {
            cli.connection_sources.endpoint = Source::Flag("socket");
        } else if cli.url.is_some() {
            cli.connection_sources.endpoint = Source::Flag("url");
        } else {
            let socket = std::env::var_os("PIQUELD_SOCKET").map(PathBuf::from);
            let url = std::env::var("PIQUELD_URL").ok();
            if socket.is_some() && url.is_some() {
                return Err(Self::invalid(
                    "Set only one of PIQUELD_SOCKET and PIQUELD_URL",
                ));
            }
            if socket.is_some() || url.is_some() {
                cli.connection_sources.endpoint = Source::Environment(if socket.is_some() {
                    "PIQUELD_SOCKET"
                } else {
                    "PIQUELD_URL"
                });
                cli.socket = socket;
                cli.url = url;
            } else if let Some(profile) = profile {
                cli.connection_sources.endpoint = profile_source.clone();
                cli.socket.clone_from(&profile.socket);
                cli.url.clone_from(&profile.url);
            }
        }
        if matches.value_source("timeout") == Some(ValueSource::CommandLine) {
            cli.connection_sources.timeout = Source::Flag("timeout");
        } else {
            let timeout = std::env::var("PIQUELD_TIMEOUT").ok();
            if let Some(value) = timeout
                .as_deref()
                .or_else(|| profile.and_then(|p| p.timeout.as_deref()))
            {
                cli.connection_sources.timeout = if timeout.is_some() {
                    Source::Environment("PIQUELD_TIMEOUT")
                } else {
                    profile_source
                };
                cli.timeout = parse_duration(value).map_err(|message| {
                    Self::invalid(message).configuration(cli.connection_sources.timeout.to_string())
                })?;
            }
        }
        Ok(())
    }
    fn parse_file(text: &str, path: &std::path::Path) -> Result<Self> {
        toml::from_str(text).map_err(|error: toml::de::Error| {
            // Neither source excerpts nor schema error values are safe to print.
            let prefix = text.get(..error.span().map_or(0, |span| span.start)).unwrap_or("");
            let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
            let column = prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1;
            let reason = if error.message().starts_with("unknown field") {
                "unknown field (expected profiles containing socket, url, or timeout)"
            } else if error.message().starts_with("invalid type") {
                "invalid field type (profiles must be tables; socket, url, and timeout must be strings)"
            } else {
                "invalid TOML or profile schema"
            };
            Self::invalid(format!("Invalid profiles file {}: {reason} at line {line}, column {column}", path.display()))
                .configuration(format!("profiles file {}", path.display()))
        })
    }
    fn invalid(message: impl Into<String>) -> CliError {
        CliError::new(ErrorKind::Input, message)
            .configuration("connection configuration".to_owned())
    }
}

impl Profile {
    fn validate(&self) -> std::result::Result<(), String> {
        if self.socket.is_some() == self.url.is_some() {
            return Err("A connection profile must contain exactly one socket or URL".into());
        }
        if let Some(url) = &self.url {
            piqueld_client::Client::tcp(url).map_err(|error| error.to_string())?;
        }
        if let Some(timeout) = &self.timeout {
            parse_duration(timeout)?;
        }
        Ok(())
    }

    fn endpoint(&self) -> std::borrow::Cow<'_, str> {
        if let Some(socket) = &self.socket {
            socket.to_string_lossy()
        } else {
            self.url.as_deref().unwrap_or_default().into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Profiles;
    use std::path::PathBuf;

    struct Files {
        directory: tempfile::TempDir,
    }

    impl Files {
        fn new() -> Self {
            Self {
                directory: tempfile::tempdir().unwrap(),
            }
        }

        fn write(&self, name: &str, text: &str) -> PathBuf {
            let path = self.directory.path().join(name);
            std::fs::write(&path, text).unwrap();
            path
        }
    }

    #[test]
    fn user_profiles_replace_whole_system_entries_before_validation() {
        let files = Files::new();
        let system = files.write(
            "system.toml",
            r#"
[profiles.prod]
url = "http://127.0.0.1:7845"
timeout = "2m"
[profiles.dev]
socket = "/tmp/system.sock"
url = "invalid"
timeout = "invalid"
[profiles.system]
socket = "/tmp/system-only.sock"
"#,
        );
        let user = files.write(
            "user.toml",
            r#"
[profiles.prod]
socket = "/tmp/user.sock"
[profiles.dev]
url = "http://localhost:7846"
[profiles.user]
socket = "/tmp/user-only.sock"
"#,
        );
        let profiles =
            Profiles::load_files([(system.clone(), false), (user.clone(), false)]).unwrap();
        assert_eq!(
            profiles
                .profiles
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["dev", "prod", "system", "user"]
        );
        let prod = &profiles.profiles["prod"];
        assert_eq!(prod.endpoint(), "/tmp/user.sock");
        assert!(prod.url.is_none());
        assert!(prod.timeout.is_none());
        assert_eq!(prod.source, user);
        assert_eq!(profiles.profiles["system"].source, system);
    }

    #[test]
    fn missing_optional_files_are_ignored_but_required_files_fail_with_path() {
        let files = Files::new();
        let missing = files.directory.path().join("missing.toml");
        let valid = files.write("valid.toml", "[profiles.dev]\nsocket = '/tmp/dev.sock'");
        let profiles = Profiles::load_files([(missing.clone(), false), (valid, false)]).unwrap();
        assert!(profiles.profiles.contains_key("dev"));
        assert!(
            Profiles::load_files([(missing.clone(), false)])
                .unwrap()
                .profiles
                .is_empty()
        );
        let error = Profiles::load_files([(missing.clone(), true)])
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains(missing.to_str().unwrap()));
    }

    #[test]
    fn malformed_and_unreadable_optional_files_fail_with_path() {
        let files = Files::new();
        let malformed = files.write("malformed.toml", "[profiles.");
        // Reading a directory fails even when tests run as root.
        for path in [malformed, files.directory.path().to_owned()] {
            let error = Profiles::load_files([(path.clone(), false)])
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains(path.to_str().unwrap()), "{error}");
        }
    }

    #[test]
    fn invalid_effective_profiles_report_name_and_source() {
        let files = Files::new();
        for (settings, expected) in [
            (
                "socket = '/tmp/dev.sock'\nurl = 'http://localhost'",
                "exactly one socket or URL",
            ),
            ("timeout = '2s'", "exactly one socket or URL"),
            ("url = 'invalid'", "not a valid URL"),
            (
                "socket = '/tmp/dev.sock'\ntimeout = 'bad'",
                "timeout must be",
            ),
        ] {
            let path = files.write("invalid.toml", &format!("[profiles.broken]\n{settings}"));
            let error = Profiles::load_files([(path.clone(), false)])
                .err()
                .unwrap()
                .to_string();
            assert!(
                error.contains("broken")
                    && error.contains(path.to_str().unwrap())
                    && error.contains(expected),
                "{error}"
            );
        }
    }
}
