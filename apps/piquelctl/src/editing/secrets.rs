//! Editing the secrets the daemon generates for an application.

use super::EditFlags;
use crate::{
    commands::resolve_application,
    error::{CliError, ErrorKind, Result},
    output::Console,
};
use clap::{Args, Subcommand, builder::TypedValueParser};
use piqueld_client::{
    Client, SecretDeclaration, SecretEncoding, SecretGenerator, edit::ApplicationEdit,
};

/// Parses `hex` or `base64url`, listing them with their help in help and
/// completions.
fn secret_encoding() -> impl TypedValueParser<Value = SecretEncoding> {
    crate::cli::one_of!(SecretEncoding::ALL, SecretEncoding::parse, |encoding| {
        match encoding {
            SecretEncoding::Hex => "Lowercase hexadecimal, two characters per byte.",
            SecretEncoding::Base64url => "URL-safe base64 without padding.",
        }
    })
}
#[derive(Debug, Args)]
pub(crate) struct GeneratedSecretTarget {
    /// Application name or stable ID.
    app: String,
    /// Secret name, as services and jobs mount it.
    name: String,
    #[command(flatten)]
    flags: EditFlags,
}
#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("generator").required(true).args(["bytes", "rsa_bits"])))]
pub(crate) struct SetGeneratedSecretArgs {
    #[command(flatten)]
    target: GeneratedSecretTarget,
    /// Generate this many random bytes (16–512), encoded as text.
    #[arg(long)]
    bytes: Option<u16>,
    /// Text encoding of the random bytes.
    ///
    /// By default, `hex`.
    #[arg(long, conflicts_with = "rsa_bits", value_parser = secret_encoding())]
    encoding: Option<SecretEncoding>,
    /// Generate an RSA private key of this many bits (2048, 3072, or 4096) in PKCS#8 PEM.
    #[arg(long)]
    rsa_bits: Option<u16>,
}
impl SetGeneratedSecretArgs {
    /// The generator the flags select; clap requires exactly one of them.
    fn generator(&self) -> SecretGenerator {
        match (self.bytes, self.rsa_bits) {
            (_, Some(bits)) => SecretGenerator::Rsa { bits },
            (bytes, None) => SecretGenerator::Random {
                bytes: bytes.expect("the generator group requires --bytes or --rsa-bits"),
                encoding: self.encoding.unwrap_or_default(),
            },
        }
    }
}
#[derive(Debug, Subcommand)]
pub(crate) enum GeneratedSecretCommand {
    /// List the secrets the application declares for piqueld to generate, as
    /// saved.
    ///
    /// `env secret list` shows the values generated in each environment.
    List {
        /// Application name or stable ID.
        app: String,
    },
    /// Declare a secret piqueld generates, or replace how it is generated.
    ///
    /// Give exactly one of `--bytes` or `--rsa-bits`. Values already
    /// generated are kept; `env secret regenerate` replaces them.
    Set(SetGeneratedSecretArgs),
    /// Remove a declaration.
    ///
    /// Values already generated are kept. Removing a name the application
    /// doesn't declare fails.
    Remove(GeneratedSecretTarget),
}
impl GeneratedSecretCommand {
    /// Lists saved declarations, or edits them client-side and saves the whole
    /// list, like exports: setting a declared name replaces it in place.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let target = match self {
            Self::List { app } => {
                let application = resolve_application(client, app).await?;
                return console.emit(&application.application.to_manifest().spec.secrets);
            }
            Self::Set(args) => &args.target,
            Self::Remove(target) => target,
        };
        let current = resolve_application(client, &target.app).await?;
        let mut secrets = current.application.to_manifest().spec.secrets;
        let existing = secrets.iter().position(|secret| secret.name == target.name);
        match (self, existing) {
            (Self::Set(args), existing) => {
                let secret = SecretDeclaration {
                    name: target.name.clone(),
                    generate: args.generator(),
                };
                match existing {
                    Some(index) => secrets[index] = secret,
                    None => secrets.push(secret),
                }
            }
            (Self::Remove(_), Some(index)) => {
                secrets.remove(index);
            }
            (_, None) => {
                return Err(CliError::new(
                    ErrorKind::Input,
                    format!("secret {:?} is not declared", target.name),
                ));
            }
            (Self::List { .. }, Some(_)) => unreachable!("listing returned above"),
        }
        let edit = ApplicationEdit::GeneratedSecrets(secrets);
        target
            .flags
            .save_loaded(client, console, &current, &edit)
            .await
    }
}
