//! Secret values come from a file or stdin, never shell arguments or output.
use crate::{
    cli::Cli,
    error::{CliError, ErrorKind, Result},
    output::{Console, reports::SecretDeletionReport},
    support::{confirm, read_input},
};
use clap::Subcommand;
use piqueld_client::Client;
use std::{
    io::{self, Read},
    path::{Path, PathBuf},
};

const MAX_SECRET_BYTES: usize = 500 * 1024;

#[derive(Debug, Subcommand)]
pub(crate) enum SecretAction {
    /// List secret metadata; values cannot be read back.
    List,
    /// Create or rotate a secret for the next explicit deployment.
    Set {
        name: String,
        #[arg(long, conflicts_with = "stdin", required_unless_present = "stdin")]
        file: Option<PathBuf>,
        #[arg(long, conflicts_with = "file")]
        stdin: bool,
        #[arg(long,value_parser=clap::value_parser!(i64).range(0..))]
        expected_generation: Option<i64>,
        #[arg(long, short)]
        yes: bool,
    },
    /// Delete an unreferenced logical secret and its Docker versions.
    Delete {
        name: String,
        #[arg(long,value_parser=clap::value_parser!(i64).range(1..))]
        expected_generation: Option<i64>,
        #[arg(long, short)]
        yes: bool,
    },
}
impl SecretAction {
    pub(crate) async fn run(
        &self,
        cli: &Cli,
        client: &Client,
        console: &mut Console,
        application: &str,
    ) -> Result<()> {
        let app = crate::commands::resolve_application(client, application).await?;
        let id = app.application.id().as_str();
        let metadata = client.secrets(id).await?;
        match self {
            Self::List => console.emit(&metadata)?,
            Self::Set {
                name,
                file,
                stdin: _,
                expected_generation,
                yes,
            } => {
                let generation = expected_generation.unwrap_or_else(|| {
                    metadata
                        .iter()
                        .find(|s| s.name == *name)
                        .map_or(0, |s| s.generation)
                });
                confirm(
                    console,
                    cli.noninteractive,
                    *yes,
                    &format!(
                        "Set secret {name:?} for {application:?}? Takes effect on a later Deploy. [y/N] "
                    ),
                ).await?;
                let file = file.clone();
                // Stdin can block indefinitely; read off the runtime so Ctrl-C stays responsive.
                let value = read_input("secret", move || Self::read_value(file.as_deref())).await?;
                if value.is_empty() || value.len() > MAX_SECRET_BYTES {
                    return Err(CliError::new(
                        ErrorKind::Input,
                        "secret must contain 1–512000 bytes",
                    ));
                }
                let updated = client.put_secret(id, name, generation, value).await?;
                console.emit(&updated)?;
            }
            Self::Delete {
                name,
                expected_generation,
                yes,
            } => {
                let generation = expected_generation
                    .or_else(|| {
                        metadata
                            .iter()
                            .find(|s| s.name == *name)
                            .map(|s| s.generation)
                    })
                    .ok_or_else(|| CliError::new(ErrorKind::Input, "secret does not exist"))?;
                confirm(
                    console,
                    cli.noninteractive,
                    *yes,
                    &format!("Delete secret {name:?} from {application:?}? [y/N] "),
                )
                .await?;
                client.delete_secret(id, name, generation).await?;
                console.emit(&SecretDeletionReport { deleted: name })?;
            }
        }
        Ok(())
    }
    /// Reads at most one byte past the limit, so oversized input is detected.
    fn read_value(file: Option<&Path>) -> io::Result<Vec<u8>> {
        let input: Box<dyn Read> = if let Some(path) = file {
            Box::new(std::fs::File::open(path)?)
        } else {
            Box::new(io::stdin())
        };
        let mut bytes = Vec::new();
        input
            .take(MAX_SECRET_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

/// Daemon-wide operations, distinct from application-scoped secret values.
#[derive(Debug, Subcommand)]
pub(crate) enum KeyAction {
    /// Recover from a lost master key by discarding ALL stored values, for every application.
    RecoverKey {
        /// Confirm recovery without an interactive prompt.
        #[arg(long)]
        yes: bool,
    },
}

impl KeyAction {
    pub(crate) async fn run(
        &self,
        cli: &Cli,
        client: &Client,
        console: &mut Console,
    ) -> Result<()> {
        let Self::RecoverKey { yes } = self;
        let message = "Discard stored secret values for ALL applications? Running services keep their Docker secrets; replacement values and a new deploy are required. [y/N] ";
        // Print scope even with --yes; the confirmation helper skips its prompt then.
        console.warning(message.trim_end_matches(" [y/N] "))?;
        confirm(console, cli.noninteractive, *yes, message).await?;
        console.emit(&client.recover_secret_key().await?)
    }
}
