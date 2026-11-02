//! Editing manifest variables.

use super::EditFlags;
use crate::{
    commands::resolve_application,
    error::{CliError, ErrorKind, Result},
    output::Console,
};
use clap::{Args, Subcommand};
use piqueld_client::{
    Client, Variable,
    edit::{ApplicationEdit, Variables},
};

#[derive(Debug, Args)]
pub(crate) struct VariableTarget {
    /// Application name or stable ID.
    app: String,
    /// Variable name, referenced as `${{ vars.<name> }}`.
    name: String,
    /// Change the variable's value in this environment instead of its
    /// default. The environment must exist.
    #[arg(long = "env", value_name = "ENV")]
    environment: Option<String>,
    /// Change the variable's value in every preview,
    /// `[spec.previews.variables]`, instead of its default.
    #[arg(long, conflicts_with = "environment")]
    previews: bool,
    #[command(flatten)]
    flags: EditFlags,
}
#[derive(Debug, Subcommand)]
pub(crate) enum VariableCommand {
    /// Set a variable's default value, or its value in one environment or in
    /// previews.
    ///
    /// An environment's or the previews' value overrides the default.
    Set {
        #[command(flatten)]
        target: VariableTarget,
        /// `true`, `false` and integers keep their type, and anything else is
        /// text. Text may reference system variables, such as
        /// `${{ env.name }}`. Text in double quotes stays text, without the
        /// quotes.
        value: String,
        /// Keep the value as text even when it looks like a boolean or an
        /// integer.
        #[arg(long)]
        string: bool,
    },
    /// Remove a variable's default value, or its value in one environment or
    /// in previews.
    ///
    /// Removing a value that isn't set fails.
    Unset(VariableTarget),
}

impl VariableCommand {
    /// Edits variables client-side and saves them together, like routes.
    /// Unsetting an unknown variable is an input error.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (Self::Set { target, .. } | Self::Unset(target)) = self;
        let current = resolve_application(client, &target.app).await?;
        let mut variables = Variables::of(&current.application.to_manifest());
        let values = match (&target.environment, target.previews) {
            (Some(environment), _) => variables
                .environments
                .entry(environment.clone())
                .or_default(),
            (None, true) => &mut variables.previews,
            (None, false) => &mut variables.defaults,
        };
        match self {
            Self::Set { value, string, .. } => {
                let value = if *string {
                    Variable::String(value.as_str().into())
                } else {
                    Variable::from_text(value)
                };
                values.insert(target.name.clone(), value);
            }
            Self::Unset(_) => {
                if values.remove(&target.name).is_none() {
                    return Err(CliError::new(
                        ErrorKind::Input,
                        format!("variable {:?} was not found", target.name),
                    ));
                }
            }
        }
        let edit = ApplicationEdit::Variables(variables);
        target
            .flags
            .save_loaded(client, console, &current, &edit)
            .await
    }
}
