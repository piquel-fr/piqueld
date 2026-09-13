//! Resolve connection configuration once before starting the command deadline.
use crate::{
    cli::{Cli, parse_duration},
    error::{CliError, ErrorKind},
};
use clap::{ArgMatches, parser::ValueSource};
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf};

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
}
impl Profiles {
    pub(crate) fn resolve(cli: &mut Cli, matches: &ArgMatches) -> Result<(), CliError> {
        let selected = cli
            .profile
            .clone()
            .or_else(|| std::env::var("PIQUELD_PROFILE").ok());
        let explicit_file = cli
            .profiles_file
            .clone()
            .or_else(|| std::env::var_os("PIQUELD_PROFILES_FILE").map(PathBuf::from));
        let path = explicit_file.clone().or_else(|| {
            std::env::var_os("XDG_CONFIG_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
                })
                .map(|root| root.join("piqueld/profiles.toml"))
        });
        let profiles = match path {
            Some(path) => match std::fs::read_to_string(&path) {
                Ok(text) => toml::from_str::<Self>(&text).map_err(|error| {
                    Self::invalid(format!("Invalid profiles file {}: {error}", path.display()))
                })?,
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && explicit_file.is_none()
                        && selected.is_none() =>
                {
                    Self::default()
                }
                Err(error) => {
                    return Err(Self::invalid(format!(
                        "Read profiles file {}: {error}",
                        path.display()
                    )));
                }
            },
            None if selected.is_some() || explicit_file.is_some() => {
                return Err(Self::invalid(
                    "No profiles file location; set --profiles-file",
                ));
            }
            None => Self::default(),
        };
        let profile = match selected {
            Some(name) => Some(
                profiles
                    .profiles
                    .get(&name)
                    .ok_or_else(|| Self::invalid(format!("Unknown connection profile: {name}")))?,
            ),
            None => profiles.profiles.get("default"),
        };
        if let Some(profile) = profile
            && profile.socket.is_some() == profile.url.is_some()
        {
            return Err(Self::invalid(
                "A connection profile must contain exactly one socket or URL",
            ));
        }
        // Resolve a transport as a pair: a higher-priority URL replaces a lower socket.
        if cli.socket.is_none() && cli.url.is_none() {
            let socket = std::env::var_os("PIQUELD_SOCKET").map(PathBuf::from);
            let url = std::env::var("PIQUELD_URL").ok();
            if socket.is_some() && url.is_some() {
                return Err(Self::invalid(
                    "Set only one of PIQUELD_SOCKET and PIQUELD_URL",
                ));
            }
            if socket.is_some() || url.is_some() {
                cli.socket = socket;
                cli.url = url;
            } else if let Some(profile) = profile {
                cli.socket.clone_from(&profile.socket);
                cli.url.clone_from(&profile.url);
            }
        }
        if matches.value_source("timeout") != Some(ValueSource::CommandLine) {
            let timeout = std::env::var("PIQUELD_TIMEOUT").ok();
            if let Some(value) = timeout
                .as_deref()
                .or_else(|| profile.and_then(|p| p.timeout.as_deref()))
            {
                cli.timeout = parse_duration(value).map_err(Self::invalid)?;
            }
        }
        Ok(())
    }
    fn invalid(message: impl Into<String>) -> CliError {
        CliError::new(ErrorKind::Input, message)
    }
}
