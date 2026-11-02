//! Editing the exports an application offers and the imports it links to.

use super::EditFlags;
use crate::{
    commands::resolve_application,
    error::{CliError, ErrorKind, Result},
    output::Console,
};
use clap::{Args, Subcommand};
use piqueld_client::{Client, Export, Import, Template, edit::ApplicationEdit};

#[derive(Debug, Args)]
pub(crate) struct ExportTarget {
    /// Application name or stable ID.
    app: String,
    /// Service of the application to export.
    service: String,
    #[command(flatten)]
    flags: EditFlags,
}
#[derive(Debug, Args)]
pub(crate) struct SetExportArgs {
    #[command(flatten)]
    target: ExportTarget,
    /// Applications whose environments may import the service. Separate
    /// several with commas, or repeat the option.
    #[arg(long, value_delimiter = ',', required = true, value_name = "APPS")]
    applications: Vec<String>,
    /// Let previews of those applications import it too. Without it,
    /// previews can't, even if they could before.
    #[arg(long)]
    previews: bool,
}
#[derive(Debug, Subcommand)]
pub(crate) enum ExportCommand {
    /// List the services the application exports, as saved.
    List {
        /// Application name or stable ID.
        app: String,
    },
    /// Export a service to other applications, or replace who may import it.
    ///
    /// Their services reach it as `<service>.<application>` once both sides
    /// have deployed the link.
    Set(SetExportArgs),
    /// Stop exporting a service.
    Remove(ExportTarget),
}
#[derive(Debug, Args)]
pub(crate) struct ImportTarget {
    /// Application name or stable ID.
    app: String,
    /// Application that exports the service.
    application: String,
    /// Exported service of that application.
    service: String,
    #[command(flatten)]
    flags: EditFlags,
}
#[derive(Debug, Args)]
pub(crate) struct AddImportArgs {
    #[command(flatten)]
    target: ImportTarget,
    /// Environment of the exporting application to reach, such as `staging`
    /// or `${{ vars.auth_env }}`. By default, the one named like the
    /// importing environment.
    #[arg(long, value_name = "ENV")]
    environment: Option<Template>,
}
#[derive(Debug, Subcommand)]
pub(crate) enum ImportCommand {
    /// List the services the application imports, as saved.
    ///
    /// `env show` reports whether each one connects.
    List {
        /// Application name or stable ID.
        app: String,
    },
    /// Import another application's exported service, or change the
    /// environment it reaches.
    ///
    /// The application's services reach it as `<service>.<application>`
    /// once both sides have deployed the link.
    Add(AddImportArgs),
    /// Stop importing a service.
    Remove(ImportTarget),
}
impl ExportCommand {
    /// Lists saved exports, or edits them client-side and saves the whole
    /// list, like jobs: setting an exported service replaces its grants in place.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let target = match self {
            Self::List { app } => {
                let application = resolve_application(client, app).await?;
                return console.emit(&application.application.to_manifest().spec.exports);
            }
            Self::Set(args) => &args.target,
            Self::Remove(target) => target,
        };
        let current = resolve_application(client, &target.app).await?;
        let mut exports = current.application.to_manifest().spec.exports;
        let existing = exports
            .iter()
            .position(|export| export.service == target.service);
        match (self, existing) {
            (Self::Set(args), existing) => {
                let export = Export {
                    service: target.service.clone(),
                    applications: args.applications.clone(),
                    previews: args.previews,
                };
                match existing {
                    Some(index) => exports[index] = export,
                    None => exports.push(export),
                }
            }
            (Self::Remove(_), Some(index)) => {
                exports.remove(index);
            }
            (_, None) => {
                return Err(CliError::new(
                    ErrorKind::Input,
                    format!("service {:?} is not exported", target.service),
                ));
            }
            (Self::List { .. }, Some(_)) => unreachable!("listing returned above"),
        }
        let edit = ApplicationEdit::Exports(exports);
        target
            .flags
            .save_loaded(client, console, &current, &edit)
            .await
    }
}
impl ImportCommand {
    /// Lists saved imports, or edits them client-side and saves the whole
    /// list: adding an imported service again replaces its environment.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let target = match self {
            Self::List { app } => {
                let application = resolve_application(client, app).await?;
                return console.emit(&application.application.to_manifest().spec.imports);
            }
            Self::Add(args) => &args.target,
            Self::Remove(target) => target,
        };
        let current = resolve_application(client, &target.app).await?;
        let mut imports = current.application.to_manifest().spec.imports;
        let existing = imports.iter().position(|import| {
            import.application == target.application && import.service == target.service
        });
        match (self, existing) {
            (Self::Add(args), existing) => {
                let import = Import {
                    application: target.application.clone(),
                    service: target.service.clone(),
                    environment: args.environment.clone(),
                };
                match existing {
                    Some(index) => imports[index] = import,
                    None => imports.push(import),
                }
            }
            (Self::Remove(_), Some(index)) => {
                imports.remove(index);
            }
            (_, None) => {
                return Err(CliError::new(
                    ErrorKind::Input,
                    format!("{}.{} is not imported", target.service, target.application),
                ));
            }
            (Self::List { .. }, Some(_)) => unreachable!("listing returned above"),
        }
        let edit = ApplicationEdit::Imports(imports);
        target
            .flags
            .save_loaded(client, console, &current, &edit)
            .await
    }
}
