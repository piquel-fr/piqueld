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
    /// Whether this profile is used when neither `--profile` nor `PIQUELD_PROFILE` names one.
    pub(crate) default: bool,
}

/// Sources are tracked independently: endpoint and timeout may use different overrides.
#[derive(Debug, Default)]
pub(crate) struct ConnectionSources {
    pub(crate) endpoint: Source,
    pub(crate) timeout: Source,
}

/// Where one connection setting came from, shown in diagnostics.
///
/// ```text
/// built-in default
/// flag --url
/// environment variable PIQUELD_URL
/// profile "prod" in /etc/piqueld/profiles.toml
/// ```
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

/// Merged profiles files: an optional `default` profile name and a
/// `[profiles.<name>]` table per named connection.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Profiles {
    default: Option<DefaultProfile>,
    profiles: BTreeMap<String, Profile>,
}
/// The top-level `default = "<name>"`, used when no profile is selected.
#[derive(Deserialize)]
#[serde(transparent)]
struct DefaultProfile {
    name: String,
    /// File that set the default, filled in after parsing.
    #[serde(skip)]
    source: PathBuf,
}
/// One named connection: exactly one of `socket` or `url`, plus an optional timeout.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    socket: Option<PathBuf>,
    url: Option<String>,
    /// Unparsed duration, validated with `parse_duration`.
    timeout: Option<String>,
    /// File that defined this profile, filled in after parsing.
    #[serde(skip)]
    source: PathBuf,
}
impl Profiles {
    /// Loads profiles. An explicit `--profiles-file` or `PIQUELD_PROFILES_FILE` must exist
    /// and is used alone; otherwise `/etc/piqueld/profiles.toml` and then the user file
    /// (`$XDG_CONFIG_HOME` or `$HOME/.config`) are merged, each optional.
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
        let user_path =
            crate::support::config_dir().map(|directory| directory.join("profiles.toml"));
        Self::load_files(
            std::iter::once(PathBuf::from("/etc/piqueld/profiles.toml"))
                .chain(user_path)
                .map(|path| (path, false)),
        )
    }

    /// Merge entire profiles before validating, so replacements never inherit fields.
    /// Later files replace same-named profiles and the `default`; `required` makes a
    /// missing file an error.
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
            if let Some(mut default) = loaded.default {
                default.source.clone_from(&path);
                result.default = Some(default);
            }
        }
        // The default may name a profile from another file, so check it after merging.
        if let Some(default) = &result.default
            && !result.profiles.contains_key(&default.name)
        {
            return Err(Self::invalid(format!(
                "Unknown default connection profile {} in {}",
                default.name,
                default.source.display()
            )));
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

    /// Name, endpoint and default flag of each profile, sorted by name.
    pub(crate) fn summaries(&self) -> Vec<ProfileSummary<'_>> {
        let default = self.default_name();
        self.profiles
            .iter()
            .map(|(name, profile)| ProfileSummary {
                name,
                endpoint: profile.endpoint(),
                default: name == default,
            })
            .collect()
    }

    /// Profile used when none is selected: the `default` setting, else the profile
    /// named `default`. Only a configured `default` is guaranteed to exist.
    fn default_name(&self) -> &str {
        self.default
            .as_ref()
            .map_or("default", |default| &default.name)
    }

    /// Applies the selected profile and environment to `cli`, recording each setting's
    /// source. The profile is `--profile`, then `PIQUELD_PROFILE`, then the default
    /// (see `default_name`) if present.
    ///
    /// Precedence, highest first:
    /// - endpoint: `--socket`/`--url`, then `PIQUELD_SOCKET`/`PIQUELD_URL` (only one may be
    ///   set), then the profile.
    /// - timeout: `--timeout`, then `PIQUELD_TIMEOUT`, then the profile, then the default.
    pub(crate) fn resolve(&self, cli: &mut Cli, matches: &ArgMatches) -> Result<()> {
        let selected = cli
            .profile
            .clone()
            .map(|name| (name, Source::Flag("profile")))
            .or_else(|| {
                std::env::var("PIQUELD_PROFILE")
                    .ok()
                    .map(|name| (name, Source::Environment("PIQUELD_PROFILE")))
            });
        let (name, profile) = if let Some((name, source)) = selected {
            let Some(profile) = self.profiles.get(&name) else {
                return Err(Self::invalid(format!("Unknown connection profile: {name}"))
                    .configuration(source.to_string()));
            };
            (name, Some(profile))
        } else {
            let name = self.default_name();
            (name.to_owned(), self.profiles.get(name))
        };
        let profile_source = profile.map_or(Source::Default, |profile| Source::Profile {
            name,
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
                    Self::invalid(format!("invalid timeout: {message}"))
                        .configuration(cli.connection_sources.timeout.to_string())
                })?;
            }
        }
        Ok(())
    }
    /// Parses one profiles file. Errors report only a line, column, and generic reason,
    /// since source excerpts and offending values are not safe to print.
    fn parse_file(text: &str, path: &std::path::Path) -> Result<Self> {
        toml::from_str(text).map_err(|error: toml::de::Error| {
            let mut diagnostic = piqueld_client::TomlDiagnostic::new(text, &error);
            // Schema messages can quote values, which are not safe to print.
            diagnostic.message = if diagnostic.message.starts_with("unknown field") {
                "unknown field (expected default, or profiles containing socket, url, or timeout)"
            } else if diagnostic.message.starts_with("invalid type") {
                "invalid field type (profiles must be tables; default, socket, url, and timeout must be strings)"
            } else {
                "invalid TOML or profile schema"
            }
            .into();
            Self::invalid(format!("Invalid profiles file {}: {diagnostic}", path.display()))
                .configuration(format!("profiles file {}", path.display()))
        })
    }
    /// Input error tagged as a connection configuration problem.
    fn invalid(message: impl Into<String>) -> CliError {
        CliError::new(ErrorKind::Input, message)
            .configuration("connection configuration".to_owned())
    }
}

impl Profile {
    /// Checks transport exclusivity, URL syntax, and the timeout format.
    fn validate(&self) -> std::result::Result<(), String> {
        if self.socket.is_some() == self.url.is_some() {
            return Err("A connection profile must contain exactly one socket or URL".into());
        }
        if let Some(url) = &self.url {
            piqueld_client::Client::tcp(url).map_err(|error| error.to_string())?;
        }
        if let Some(timeout) = &self.timeout {
            parse_duration(timeout).map_err(|message| format!("invalid timeout: {message}"))?;
        }
        Ok(())
    }

    /// Socket path or URL, whichever is configured.
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
    fn later_files_replace_the_default_which_must_name_a_merged_profile() {
        let files = Files::new();
        // The system default names a profile defined only in the user file.
        let system = files.write(
            "system.toml",
            "default = 'dev'\n[profiles.prod]\nurl = 'http://127.0.0.1:7845'",
        );
        let user = files.write("user.toml", "[profiles.dev]\nsocket = '/tmp/dev.sock'");
        let profiles =
            Profiles::load_files([(system.clone(), false), (user.clone(), false)]).unwrap();
        assert_eq!(profiles.default_name(), "dev");
        let later = files.write("later.toml", "default = 'prod'");
        let profiles =
            Profiles::load_files([(system.clone(), false), (user, false), (later, false)]).unwrap();
        assert_eq!(profiles.default_name(), "prod");
        let error = Profiles::load_files([(system.clone(), false)])
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains(&format!(
                "Unknown default connection profile dev in {}",
                system.display()
            )),
            "{error}"
        );
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
                "invalid timeout",
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
