//! Secret values come from a file or stdin, never shell arguments or output.
use crate::{
    commands::resolve_application,
    error::{CliError, ErrorKind, Result},
    output::{
        Console,
        reports::{NameDeletionReport, StoredSecretReport, StoredSecretsReport},
    },
    support::{TerminalMode, read_input},
};
use clap::{Args, Subcommand};
use piqueld_client::{
    ApplicationView, Client, EnvironmentAccess, MAX_SECRET_VALUE_BYTES, SecretAccess,
};
use rustix::termios::{LocalModes, Termios};
use std::{
    io::{self, BufRead, IsTerminal, Read},
    path::PathBuf,
};

// `app secret` subcommands; `///` on variants and fields is user-facing help.
#[derive(Debug, Subcommand)]
pub(crate) enum SecretAction {
    /// List the application's stored secrets and who may mount them; values cannot be read back.
    List,
    /// Create or rotate a stored secret for the next explicit deployment.
    Set {
        /// Secret name.
        name: String,
        /// Read the value from this file (1–512000 bytes).
        #[arg(long, conflicts_with = "stdin", required_unless_present = "stdin")]
        file: Option<PathBuf>,
        /// Read the value from stdin (1–512000 bytes), without one trailing newline; a terminal is prompted for it without echo.
        #[arg(long, conflicts_with = "file")]
        stdin: bool,
        #[command(flatten)]
        access: AccessFlags,
        /// Require this secret generation; zero requires an absent secret.
        #[arg(long,value_parser=clap::value_parser!(i64).range(0..))]
        expected_generation: Option<i64>,
        /// Skip interactive confirmation.
        #[arg(long, short)]
        yes: bool,
    },
    /// Change which environments, and whether previews, may mount a stored secret.
    Access {
        /// Secret name.
        name: String,
        #[command(flatten)]
        access: AccessFlags,
    },
    /// Delete a stored secret no environment uses, and its Docker versions.
    Delete(SecretChange),
}

// A secret to delete or regenerate, guarded by its generation.
#[derive(Debug, Args)]
pub(crate) struct SecretChange {
    /// Secret name.
    name: String,
    /// Require this secret generation.
    #[arg(long,value_parser=clap::value_parser!(i64).range(1..))]
    expected_generation: Option<i64>,
    /// Skip interactive confirmation.
    #[arg(long, short)]
    yes: bool,
}

impl SecretChange {
    /// `--expected-generation`, else the `current` generation of the secret,
    /// which must exist.
    fn generation(&self, current: Option<i64>) -> Result<i64> {
        self.expected_generation
            .or(current)
            .ok_or_else(|| CliError::new(ErrorKind::Input, "secret does not exist"))
    }
}

/// Access list changes. Unset flags keep the current setting, or the default
/// of every environment and no previews for a new secret.
#[derive(Debug, Args)]
pub(crate) struct AccessFlags {
    /// Only these environments may mount the secret (comma-separated names or IDs; '' for none).
    #[arg(long, value_delimiter = ',', conflicts_with = "all_environments")]
    environments: Option<Vec<String>>,
    /// Every environment may mount the secret, including ones created later.
    #[arg(long)]
    all_environments: bool,
    /// Previews may mount the secret.
    #[arg(long, conflicts_with = "no_previews")]
    previews: bool,
    /// Previews may not mount the secret.
    #[arg(long)]
    no_previews: bool,
}

impl AccessFlags {
    /// `current` access with the flags applied, or `None` when no flag is set.
    /// Environment names resolve to IDs, so renames keep their access.
    fn apply(
        &self,
        current: Option<&SecretAccess>,
        application: &ApplicationView,
    ) -> Result<Option<SecretAccess>> {
        if self.environments.is_none()
            && !self.all_environments
            && !self.previews
            && !self.no_previews
        {
            return Ok(None);
        }
        let mut access = current.cloned().unwrap_or_default();
        if let Some(environments) = &self.environments {
            access.environments = EnvironmentAccess::Only(
                environments
                    .iter()
                    .filter(|environment| !environment.is_empty())
                    .map(|environment| {
                        application
                            .environment(environment)
                            .map(|environment| environment.id.clone())
                            .ok_or_else(|| {
                                CliError::new(
                                    ErrorKind::Input,
                                    format!("environment {environment:?} was not found"),
                                )
                            })
                    })
                    .collect::<Result<_>>()?,
            );
        }
        if self.all_environments {
            access.environments = EnvironmentAccess::All;
        }
        if self.previews || self.no_previews {
            access.previews = self.previews;
        }
        Ok(Some(access))
    }
}

impl SecretAction {
    /// Runs a secret action on `application`'s store. Current metadata is
    /// fetched first so `set` and `delete` can default the expected generation
    /// (`0` creates a new secret; deleting an unknown secret fails), and access
    /// flags apply to the current access list. Values are read only after
    /// confirmation.
    pub(crate) async fn run(
        &self,
        client: &Client,
        console: &mut Console,
        application: &str,
    ) -> Result<()> {
        let application = resolve_application(client, application).await?;
        let id = application.application.id().as_str();
        let name_of_application = application.application.metadata().name.as_str();
        let stored = client.stored_secrets(id).await?;
        let current = |name: &str| stored.iter().find(|secret| secret.metadata.name == name);
        let environments = &application.environments;
        match self {
            Self::List => console.emit(&StoredSecretsReport {
                secrets: &stored,
                environments,
            })?,
            Self::Set {
                name,
                file,
                stdin: _,
                access,
                expected_generation,
                yes,
            } => {
                let existing = current(name);
                let generation = expected_generation
                    .unwrap_or_else(|| existing.map_or(0, |s| s.metadata.generation));
                let access = access.apply(existing.map(|s| &s.access), &application)?;
                let shown = access
                    .as_ref()
                    .or(existing.map(|s| &s.access))
                    .cloned()
                    .unwrap_or_default()
                    .describe(environments);
                console
                    .confirm(
                        *yes,
                        &format!(
                            "Set secret {name:?} of {name_of_application:?}, mountable by {shown}? Takes effect on a later Deploy."
                        ),
                    )
                    .await?;
                let value = Self::read_value(console, file.clone()).await?;
                if value.is_empty() || value.len() > MAX_SECRET_VALUE_BYTES {
                    return Err(CliError::new(
                        ErrorKind::Input,
                        format!("secret must contain 1–{MAX_SECRET_VALUE_BYTES} bytes"),
                    ));
                }
                let secret = client
                    .put_stored_secret(id, name, generation, value, access.as_ref())
                    .await?;
                console.emit(&StoredSecretReport {
                    secret: &secret,
                    environments,
                })?;
            }
            Self::Access { name, access } => {
                let existing = current(name)
                    .ok_or_else(|| CliError::new(ErrorKind::Input, "secret does not exist"))?;
                let access = access
                    .apply(Some(&existing.access), &application)?
                    .ok_or_else(|| {
                        CliError::new(
                            ErrorKind::Input,
                            "supply --environments, --all-environments, --previews or --no-previews",
                        )
                    })?;
                let secret = client.set_secret_access(id, name, &access).await?;
                console.emit(&StoredSecretReport {
                    secret: &secret,
                    environments,
                })?;
            }
            Self::Delete(change) => {
                let name = &change.name;
                let generation = change.generation(current(name).map(|s| s.metadata.generation))?;
                console
                    .confirm(
                        change.yes,
                        &format!(
                            "Delete secret {name:?} of {name_of_application:?} from every environment?"
                        ),
                    )
                    .await?;
                client.delete_stored_secret(id, name, generation).await?;
                console.emit(&NameDeletionReport { deleted: name })?;
            }
        }
        Ok(())
    }
    /// Reads the value from `file` verbatim, or from stdin without one
    /// trailing newline, so `echo value |` stores `value`. A terminal is
    /// asked for one line, without echoing it. Reads at most the limit plus a
    /// CRLF and one byte, so input still too long once the newline is
    /// dropped is detected rather than cut short. Reads run off the runtime,
    /// since stdin can block indefinitely and Ctrl-C must stay responsive.
    async fn read_value(console: &mut Console, file: Option<PathBuf>) -> Result<Vec<u8>> {
        let stdin = file.is_none();
        let terminal = stdin && io::stdin().is_terminal();
        // Dropped with this future, so echo also returns when Ctrl-C
        // interrupts the read.
        let _hidden = if terminal {
            console.prompt("Secret value (hidden): ")?;
            let hide = |mode: &mut Termios| {
                mode.local_modes.remove(LocalModes::ECHO);
                mode.local_modes.insert(LocalModes::ECHONL);
            };
            Some(TerminalMode::enable(hide).map_err(|error| {
                CliError::new(
                    ErrorKind::General,
                    format!("could not hide terminal input: {error}"),
                )
            })?)
        } else {
            None
        };
        let mut value = read_input("secret", move || {
            let input: Box<dyn Read> = match file {
                Some(path) => Box::new(std::fs::File::open(path)?),
                None => Box::new(io::stdin()),
            };
            let mut input = io::BufReader::new(input.take(MAX_SECRET_VALUE_BYTES as u64 + 3));
            let mut bytes = Vec::new();
            if terminal {
                input.read_until(b'\n', &mut bytes)?;
            } else {
                input.read_to_end(&mut bytes)?;
            }
            Ok(bytes)
        })
        .await?;
        if stdin && value.pop_if(|byte| *byte == b'\n').is_some() {
            value.pop_if(|byte| *byte == b'\r');
        }
        Ok(value)
    }
}

// `env secret` subcommands; `///` on variants and fields is user-facing help.
#[derive(Debug, Subcommand)]
pub(crate) enum GeneratedSecretAction {
    /// List the environment's generated secrets; values cannot be read back.
    List,
    /// Generate a new value for the next deployment, to rotate it or replace one discarded by key recovery.
    Regenerate(SecretChange),
    /// Delete an unreferenced generated secret; a later deployment generates a new value.
    Delete(SecretChange),
}

impl GeneratedSecretAction {
    /// Runs a generated-secret action for the named environment of
    /// `application`, or its only environment.
    pub(crate) async fn run(
        &self,
        client: &Client,
        console: &mut Console,
        application: &str,
        environment: Option<&str>,
    ) -> Result<()> {
        let (application, environment) =
            crate::environments::select(client, application, environment).await?;
        let id = environment.id.as_str();
        let metadata = client.secrets(id).await?;
        let (verb, change) = match self {
            Self::List => return console.emit(&metadata),
            Self::Regenerate(change) => ("Regenerate", change),
            Self::Delete(change) => ("Delete", change),
        };
        let name = &change.name;
        let current = metadata.iter().find(|secret| secret.name == *name);
        let generation = change.generation(current.map(|secret| secret.generation))?;
        console
            .confirm(
                change.yes,
                &format!(
                    "{verb} generated secret {name:?} of {:?} in environment {:?}?",
                    application.application.metadata().name,
                    environment.name.as_str()
                ),
            )
            .await?;
        if let Self::Regenerate(_) = self {
            console.emit(&client.regenerate_secret(id, name, generation).await?)
        } else {
            client.delete_secret(id, name, generation).await?;
            console.emit(&NameDeletionReport { deleted: name })
        }
    }
}

/// Daemon-wide operations, distinct from environment-scoped secret values.
#[derive(Debug, Subcommand)]
pub(crate) enum KeyAction {
    /// Recover from a lost master key by discarding ALL stored and generated values.
    RecoverKey {
        /// Confirm recovery without an interactive prompt.
        #[arg(long)]
        yes: bool,
    },
}

impl KeyAction {
    /// Discards every stored secret value after printing the scope and confirming.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let Self::RecoverKey { yes } = self;
        let message = "Discard stored and generated secret values for ALL applications and environments? Running services keep their Docker secrets; replacement values and a new deploy are required.";
        // Print scope even with --yes; the confirmation skips its prompt then.
        console.warning(message)?;
        console.confirm(*yes, message).await?;
        console.emit(&client.recover_secret_key().await?)
    }
}
