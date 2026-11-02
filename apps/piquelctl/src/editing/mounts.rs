//! Editing what services mount: volumes, secrets and files.

use super::{
    EditFlags, FilePathArgs, MountPathArgs, SecretPathArgs, ServiceListArgs, ServiceTarget,
};
use crate::{
    commands::resolve_application,
    error::{CliError, ErrorKind, Result},
    output::Console,
    support::read_text,
};
use clap::{Args, Subcommand};
use piqueld_client::{
    Client, FileMode, FileSource, Mount, SecretMount, ServiceFile, Template, Volume,
    edit::{ApplicationEdit, ServiceEdit},
};

#[derive(Debug, Subcommand)]
pub(crate) enum MountCommand {
    /// Mount a volume at a container path, replacing any mount at that path.
    Set {
        #[command(flatten)]
        target: ServiceTarget,
        /// Name of a volume the application declares, with `app volume add`.
        volume: String,
        /// Absolute container path to mount it at.
        path: String,
        /// Mount the volume read-only. A writable mount makes the service's
        /// rollouts stop the old task first.
        #[arg(long)]
        read_only: bool,
    },
    /// Remove the mount at a container path.
    Remove(MountPathArgs),
}
#[derive(Debug, Subcommand)]
pub(crate) enum SecretMountCommand {
    /// List the secrets the application's services mount, as saved.
    List(ServiceListArgs),
    /// Mount a secret as a read-only file, replacing any secret mounted at
    /// the same path.
    ///
    /// A deployment fails while the secret has no value, or when the
    /// environment may not mount it.
    Add(AddSecretMountArgs),
    /// Remove the secret mounted at a container path.
    ///
    /// Removing a path the service doesn't mount a secret at fails.
    Remove(SecretPathArgs),
}
#[derive(Debug, Args)]
pub(crate) struct AddSecretMountArgs {
    #[command(flatten)]
    target: ServiceTarget,
    /// Secret name: one set with `app secret`, or one the manifest declares
    /// to be generated. It may reference variables, such as
    /// `${{ vars.<name> }}`, so each environment can mount its own secret.
    secret: Template,
    /// Absolute container path of the file, under `/run/secrets`.
    #[arg(value_name = "TARGET")]
    file: String,
}
impl SecretMountCommand {
    /// Lists saved secret mounts, or edits one service's mounts client-side
    /// and saves them together, like jobs: adding at a mounted target replaces
    /// that mount in place, and removing an unknown target is an input error.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let target = match self {
            Self::List(args) => {
                return args.emit(client, console, |service| &service.secrets).await;
            }
            Self::Add(args) => &args.target,
            Self::Remove(args) => &args.target,
        };
        let current = resolve_application(client, &target.app).await?;
        let mut secrets = current
            .application
            .spec()
            .services
            .iter()
            .find(|service| service.name == target.service)
            .ok_or_else(|| {
                CliError::new(
                    ErrorKind::Input,
                    format!("service {:?} was not found", target.service),
                )
            })?
            .secrets
            .clone();
        match self {
            Self::List(_) => unreachable!("listing returned above"),
            Self::Add(args) => {
                let mount = SecretMount {
                    name: args.secret.clone(),
                    target: args.file.clone(),
                };
                match secrets
                    .iter_mut()
                    .find(|secret| secret.target == mount.target)
                {
                    Some(existing) => *existing = mount,
                    None => secrets.push(mount),
                }
            }
            Self::Remove(args) => {
                let count = secrets.len();
                secrets.retain(|secret| secret.target != args.value);
                if secrets.len() == count {
                    return Err(CliError::new(
                        ErrorKind::Input,
                        format!("no secret is mounted at {:?}", args.value),
                    ));
                }
            }
        }
        let (target, edit) = target.field(ServiceEdit::Secrets(secrets));
        target
            .flags
            .save_loaded(client, console, &current, &edit)
            .await
    }
}
#[derive(Debug, Subcommand)]
pub(crate) enum FileCommand {
    /// List the files the application's services mount, as saved.
    List(ServiceListArgs),
    /// Mount a read-only file into a service, replacing any file at the same
    /// path.
    ///
    /// Give its content with exactly one of `--content`, `--from` or `--path`.
    /// A file holds at most 500 KiB and can't be empty.
    Add(AddFileArgs),
    /// Remove the file at a container path.
    Remove(FilePathArgs),
}
#[derive(Debug, Args)]
pub(crate) struct AddFileArgs {
    #[command(flatten)]
    target: ServiceTarget,
    /// Absolute container path of the file.
    #[arg(value_name = "TARGET")]
    file: String,
    #[command(flatten)]
    source: FileSourceArgs,
    /// Octal permissions of the file, from `0000` to `0777`, such as `0440`.
    /// Root owns the file.
    #[arg(long, default_value_t = FileMode::default())]
    mode: FileMode,
}
// Exactly one source of a file's content.
#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
pub(crate) struct FileSourceArgs {
    /// The file's content. Variables in it are never rendered.
    #[arg(long, value_name = "TEXT")]
    content: Option<String>,
    /// Read the content from this local file now, and save it in the
    /// manifest.
    #[arg(long, value_name = "LOCAL_FILE")]
    from: Option<std::path::PathBuf>,
    /// Read the content from this file in the manifest's repository, relative
    /// to its root, each time a deployment fetches the manifest. Only
    /// repository-backed applications can use it.
    #[arg(long, value_name = "REPOSITORY_PATH")]
    path: Option<String>,
}
impl FileSourceArgs {
    /// The selected source; `--from` reads its local file now.
    async fn source(&self) -> Result<FileSource> {
        Ok(match (&self.content, &self.from, &self.path) {
            (Some(content), ..) => FileSource::Content(content.clone()),
            (_, Some(local), _) => {
                FileSource::Content(read_text(local, "file", ServiceFile::MAX_BYTES as u64).await?)
            }
            (.., Some(path)) => FileSource::Path(path.clone()),
            (None, None, None) => unreachable!("Clap requires one file source"),
        })
    }
}
impl FileCommand {
    /// Lists saved files, or adds, replaces, or removes one service's file by
    /// target.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (target, edit) = match self {
            Self::List(args) => return args.emit(client, console, |service| &service.files).await,
            Self::Add(args) => args.target.field(ServiceEdit::File(ServiceFile {
                target: args.file.clone(),
                source: args.source.source().await?,
                mode: args.mode,
            })),
            Self::Remove(args) => args
                .target
                .field(ServiceEdit::RemoveFile(args.value.clone())),
        };
        target.flags.save(client, console, &target.app, &edit).await
    }
}
#[derive(Debug, Args)]
pub(crate) struct VolumeArgs {
    /// Application name or stable ID.
    app: String,
    /// Volume name.
    volume: String,
    #[command(flatten)]
    flags: EditFlags,
}
#[derive(Debug, Subcommand)]
pub(crate) enum VolumeCommand {
    /// Declare a named volume that services can mount.
    ///
    /// Each environment and preview gets its own Docker volume when it next
    /// deploys.
    Add(VolumeArgs),
    /// Remove a volume declaration.
    ///
    /// Remove the volume's mounts first. Docker keeps the volume and its data.
    Remove(VolumeArgs),
}
impl MountCommand {
    /// Maps to a mount edit keyed by container target path.
    pub(super) fn edit(&self) -> (&ServiceTarget, ApplicationEdit) {
        match self {
            Self::Set {
                target,
                volume,
                path,
                read_only,
            } => target.field(ServiceEdit::Mount(Mount {
                volume: volume.clone(),
                target: path.clone(),
                read_only: *read_only,
            })),
            Self::Remove(args) => args
                .target
                .field(ServiceEdit::RemoveMount(args.value.clone())),
        }
    }
}
impl VolumeCommand {
    /// Saves a named volume declaration or removal.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (args, edit) = match self {
            Self::Add(args) => (
                args,
                ApplicationEdit::AddVolume(Volume {
                    name: args.volume.clone(),
                }),
            ),
            Self::Remove(args) => (args, ApplicationEdit::RemoveVolume(args.volume.clone())),
        };
        args.flags.save(client, console, &args.app, &edit).await
    }
}
