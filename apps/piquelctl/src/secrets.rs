//! Secret values come from a file or stdin, never shell arguments or output.
use crate::{
    commands::resolve_application,
    error::{CliError, ErrorKind, Result},
    output::{
        Console,
        reports::{NameDeletionReport, StoredSecretReport, StoredSecretsReport},
    },
    support::read_input,
};
use clap::{Args, Subcommand};
use piqueld_client::{
    ApplicationView, Client, EnvironmentAccess, MAX_SECRET_VALUE_BYTES, SecretAccess,
};
use std::{
    io::{self, Read},
    path::{Path, PathBuf},
};

// `app secret` subcommands; `///` on variants and fields is user-facing help.
#[derive(Debug, Subcommand)]
pub(crate) enum SecretAction {
    /// List the application's stored secrets and who may mount them.
    ///
    /// It shows each secret's version, never its value.
    List,
    /// Create or replace a stored secret.
    ///
    /// The value, of 1 to 512000 bytes, comes from a file or standard input,
    /// never from an argument. Running services keep their value until the
    /// next deployment. A new secret may be mounted by every environment and no
    /// preview, unless you set its access. Needs `secrets:write`.
    Set {
        /// Secret name.
        name: String,
        /// Read the value from this file.
        #[arg(
            long,
            value_name = "PATH",
            conflicts_with = "stdin",
            required_unless_present = "stdin"
        )]
        file: Option<PathBuf>,
        /// Read the value from standard input.
        #[arg(long, conflicts_with = "file")]
        stdin: bool,
        #[command(flatten)]
        access: AccessFlags,
        /// Fail unless the secret is at this generation; `0` requires that it
        /// doesn't exist.
        ///
        /// Defaults to the generation it inspects, or `0` for a new secret.
        #[arg(long, value_name = "GENERATION", value_parser = clap::value_parser!(i64).range(0..))]
        expected_generation: Option<i64>,
        /// Skip the confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },
    /// Change which environments, and whether previews, may mount a stored secret.
    ///
    /// Options you omit keep the current access. Deployments that mount a
    /// secret they may no longer use fail with `secret_access_denied`; running
    /// ones keep their value. Needs `secrets:write`.
    Access {
        /// Secret name.
        name: String,
        #[command(flatten)]
        access: AccessFlags,
    },
    /// Delete a stored secret and the Docker secrets made from it.
    ///
    /// It fails while any environment's configuration or deployment still uses
    /// the secret. Needs `secrets:write`.
    Delete(SecretChange),
}

// A secret to delete or regenerate, guarded by its generation.
#[derive(Debug, Args)]
pub(crate) struct SecretChange {
    /// Secret name.
    name: String,
    /// Fail unless the secret is at this generation.
    ///
    /// Defaults to the generation it inspects.
    #[arg(long, value_name = "GENERATION", value_parser = clap::value_parser!(i64).range(1..))]
    expected_generation: Option<i64>,
    /// Skip the confirmation prompt.
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

// Access list changes. Unset flags keep the current setting, or the default
// of every environment and no previews for a new secret.
#[derive(Debug, Args)]
pub(crate) struct AccessFlags {
    /// Let only these environments mount the secret: comma-separated names or
    /// IDs, or `''` for none.
    #[arg(long, value_delimiter = ',', conflicts_with = "all_environments")]
    environments: Option<Vec<String>>,
    /// Let every environment mount the secret, including ones created later.
    ///
    /// It never includes previews.
    #[arg(long)]
    all_environments: bool,
    /// Let previews mount the secret.
    #[arg(long, conflicts_with = "no_previews")]
    previews: bool,
    /// Stop previews from mounting the secret.
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
                let file = file.clone();
                // Stdin can block indefinitely; read off the runtime so Ctrl-C stays responsive.
                let value = read_input("secret", move || Self::read_value(file.as_deref())).await?;
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
    /// Reads at most one byte past the limit, so oversized input is detected.
    fn read_value(file: Option<&Path>) -> io::Result<Vec<u8>> {
        let input: Box<dyn Read> = if let Some(path) = file {
            Box::new(std::fs::File::open(path)?)
        } else {
            Box::new(io::stdin())
        };
        let mut bytes = Vec::new();
        input
            .take(MAX_SECRET_VALUE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

// `env secret` subcommands; `///` on variants and fields is user-facing help.
#[derive(Debug, Subcommand)]
pub(crate) enum GeneratedSecretAction {
    /// List the environment's generated secrets.
    ///
    /// It shows each secret's version, never its value.
    List,
    /// Generate a new value for a generated secret.
    ///
    /// Use it to rotate a value, or to replace one that `secrets recover-key`
    /// discarded. The next deployment uses it; running services keep theirs
    /// until then. Needs `secrets:write`.
    Regenerate(SecretChange),
    /// Delete a generated secret that nothing uses.
    ///
    /// It fails while the configuration or a deployment still uses the secret.
    /// A later deployment that mounts it generates a new value. Needs
    /// `secrets:write`.
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

// Daemon-wide operations, distinct from environment-scoped secret values.
#[derive(Debug, Subcommand)]
pub(crate) enum KeyAction {
    /// Recover from a lost secret key by discarding every value it encrypted.
    ///
    /// Use it only when the daemon's `secrets.key` is lost and you have no
    /// backup: the daemon refuses while the key still works. It discards the
    /// stored and generated secret values of ALL applications and environments
    /// and the repository webhook secrets, and deletes every notification
    /// destination, since their URLs are encrypted. Running services keep their
    /// Docker secrets. Afterwards, set the stored secrets again and deploy,
    /// which also generates new values for generated secrets, give each
    /// repository a new webhook secret with `app repository webhook rotate`,
    /// and add the notification destinations again with `notification set`.
    /// Needs `system:operate`.
    RecoverKey {
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

impl KeyAction {
    /// Discards every stored secret value after printing the scope and confirming.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let Self::RecoverKey { yes } = self;
        let message = "Discard stored and generated secret values for ALL applications and environments, and repository webhook secrets, and delete every notification destination? Running services keep their Docker secrets. Afterwards, set the secrets again and deploy, rotate repository webhook secrets with `app repository webhook rotate`, and add destinations again with `notification set`.";
        // Print scope even with --yes; the confirmation skips its prompt then.
        console.warning(message)?;
        console.confirm(*yes, message).await?;
        console.emit(&client.recover_secret_key().await?)
    }
}
