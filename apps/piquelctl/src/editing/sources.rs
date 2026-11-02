//! Editing where an application comes from: images, Git sources and builds.

use super::{
    CommitArgs, ContextArgs, DockerfileArgs, GitBranchArgs, GitUrlArgs, ImageArgs, ServiceTarget,
    key_value,
};
use crate::error::{CliError, ErrorKind, Result};
use clap::{Args, Subcommand};
use piqueld_client::{
    Build, GitRepository, ManifestRepository, Source, SourceRepository, Template,
    edit::{ApplicationEdit, ServiceEdit},
};

#[derive(Debug, Subcommand)]
pub(crate) enum SourceCommand {
    /// Run a prebuilt image, replacing the service's source.
    Image(ImageArgs),
    /// Build a Git repository with Docker, replacing the service's source.
    ///
    /// Build settings you don't pass get their defaults.
    Git(GitSourceArgs),
    /// Change the repository the service builds.
    Url(GitUrlArgs),
    /// Change the branch the service builds.
    Branch(GitBranchArgs),
    /// Pin the commit the service builds, or follow the branch again with
    /// `--clear`.
    Commit(CommitArgs),
    /// Change the Dockerfile the service builds.
    Dockerfile(DockerfileArgs),
    /// Change the directory the service's build uses as its context.
    Context(ContextArgs),
}
#[derive(Debug, Args)]
pub(crate) struct GitSourceArgs {
    #[command(flatten)]
    target: ServiceTarget,
    /// Git clone URL, a path on the daemon's host, or `self`: the manifest's
    /// own repository, at the commit the manifest was read from.
    #[arg(id = "git", value_name = "URL|self")]
    repository: BuildRepository,
    // Boxed to keep the CLI command enum small.
    #[command(flatten)]
    build: Box<GitBuildArgs>,
}
/// Repository a Git source builds, parsed like the manifest's `repository`:
/// `self` selects the manifest's own repository, anything else is a clone URL
/// or host path.
#[derive(Clone, Debug)]
pub(crate) enum BuildRepository {
    Manifest,
    Url(String),
}
impl From<String> for BuildRepository {
    fn from(value: String) -> Self {
        if value == "self" {
            Self::Manifest
        } else {
            Self::Url(value)
        }
    }
}
// Git build settings; each flag requires the `git` argument (`--git` or the
// positional repository of `source git`).
#[derive(Debug, Args)]
pub(crate) struct GitBuildArgs {
    /// Branch to build when no commit is pinned. Default: `main`. `self`
    /// doesn't take one.
    #[arg(long, requires = "git", value_name = "BRANCH")]
    branch: Option<String>,
    /// Build this full commit hash instead of the branch head. `self` doesn't
    /// take one.
    #[arg(long, requires = "git")]
    commit: Option<String>,
    /// Dockerfile path, relative to the repository root.
    #[arg(
        long,
        requires = "git",
        value_name = "PATH",
        default_value = "Dockerfile"
    )]
    dockerfile: Template,
    /// Build context directory, relative to the repository root.
    #[arg(long, requires = "git", value_name = "PATH", default_value = ".")]
    context: Template,
    /// Docker build argument. Repeat it to set several. Values aren't secret:
    /// Docker records them in the image.
    #[arg(long = "build-arg", value_name = "KEY=VALUE", requires = "git", value_parser = key_value)]
    build_args: Vec<(String, String)>,
    /// Dockerfile stage to build. By default, Docker builds the last stage.
    #[arg(long, requires = "git", value_name = "STAGE")]
    target: Option<Template>,
}
impl GitBuildArgs {
    /// Git source that builds `repository` with Docker using these settings.
    /// Fails when `self` is given a branch or commit: it always builds the
    /// manifest's commit.
    pub(super) fn source(&self, repository: &BuildRepository) -> Result<Source> {
        let repository = match repository {
            BuildRepository::Url(url) => SourceRepository::Git(GitRepository {
                url: url.clone(),
                branch: self.branch.clone().unwrap_or_else(|| "main".into()),
                commit: self.commit.clone(),
            }),
            BuildRepository::Manifest if self.branch.is_some() || self.commit.is_some() => {
                return Err(CliError::new(
                    ErrorKind::Input,
                    "--branch and --commit do not apply to `self`, which builds the manifest's commit",
                ));
            }
            BuildRepository::Manifest => SourceRepository::Manifest(ManifestRepository::Manifest),
        };
        Ok(Source::Git {
            repository,
            build: Build::Docker {
                dockerfile: self.dockerfile.clone(),
                context: self.context.clone(),
                args: self
                    .build_args
                    .iter()
                    .map(|(key, value)| (key.clone(), value.as_str().into()))
                    .collect(),
                target: self.target.clone(),
            },
        })
    }
}
impl SourceCommand {
    /// Maps the subcommand to its target service and source edit.
    pub(super) fn edit(&self) -> Result<(&ServiceTarget, ApplicationEdit)> {
        Ok(match self {
            Self::Image(args) => args.target.field(ServiceEdit::Image(args.value.clone())),
            Self::Git(args) => args
                .target
                .field(ServiceEdit::Source(args.build.source(&args.repository)?)),
            Self::Url(args) => args.target.field(ServiceEdit::GitUrl(args.value.clone())),
            Self::Branch(args) => args
                .target
                .field(ServiceEdit::GitBranch(args.value.clone())),
            Self::Commit(args) => args
                .target
                .field(ServiceEdit::GitCommit(args.value.clone())),
            Self::Dockerfile(args) => args
                .target
                .field(ServiceEdit::Dockerfile(args.value.clone())),
            Self::Context(args) => args.target.field(ServiceEdit::Context(args.value.clone())),
        })
    }
}
