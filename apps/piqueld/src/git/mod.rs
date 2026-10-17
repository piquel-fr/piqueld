//! Isolated, bounded Git checkouts (see [`Checkouts`]) and branch listings,
//! and the snapshots of local worktrees deployments check out instead (see
//! [`snapshot`]). No shared mutable repository or credential store.

pub(crate) mod snapshot;

use anyhow::{Context, bail};
use piqueld_core::api::BranchState;
use piqueld_core::manifest::{GitRepository, valid_git_commit, valid_repository_path};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{collections::BTreeMap, time::Duration};
use tokio::process::Command;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The branch heads of a remote repository, listed with `git ls-remote`.
pub(crate) struct Heads {
    /// The repository listed.
    url: String,
    /// Head commit of each branch.
    branches: BTreeMap<String, String>,
}

impl Heads {
    /// Longest a listing may take before it counts as failed, well within a
    /// client's 30-second request budget so it still gets `unknown` branch
    /// states. Its process group, Git's helpers included, is then killed.
    const TIMEOUT: Duration = Duration::from_secs(10);
    /// Most output a listing may produce: tens of thousands of branches.
    const OUTPUT_LIMIT: usize = 16 * 1024 * 1024;

    /// Lists every branch of `url` with its head commit. Any repository,
    /// network, or authentication failure is an error, never an empty
    /// listing, so a branch only counts as gone when the repository answered.
    pub(crate) async fn list(url: &str) -> anyhow::Result<Self> {
        let mut command = Checkout::command();
        command.args(["ls-remote", "--heads", "--"]).arg(url);
        let output = tokio::time::timeout(
            Self::TIMEOUT,
            crate::command::LoggedCommand::output(
                &mut command,
                "list Git branches",
                Self::OUTPUT_LIMIT,
            ),
        )
        .await
        .context("git ls-remote timed out")?
        .map_err(|error| {
            // Git may echo the URL, credentials included, so details stay in logs.
            tracing::warn!(?error, "git ls-remote failed");
            match error.downcast_ref::<crate::command::CommandFailure>() {
                Some(failure) => anyhow::anyhow!("git ls-remote failed ({})", failure.status),
                None => anyhow::anyhow!("git ls-remote could not run"),
            }
        })?;
        let mut heads = BTreeMap::new();
        for line in String::from_utf8(output)
            .context("decode git ls-remote output")?
            .lines()
        {
            let (commit, reference) = line
                .split_once('\t')
                .context("parse git ls-remote output")?;
            if let Some(branch) = reference.strip_prefix("refs/heads/") {
                heads.insert(branch.to_owned(), commit.to_owned());
            }
        }
        Ok(Self {
            url: url.to_owned(),
            branches: heads,
        })
    }

    /// Heads listed some other way, for tests.
    #[cfg(test)]
    pub(crate) fn new(url: &str, branches: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            url: url.to_owned(),
            branches: branches.into_iter().collect(),
        }
    }

    /// The repository listed, which a branch state only describes.
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// The commit at `branch`'s head, unless the repository has no such branch.
    pub(crate) fn head(&self, branch: &str) -> Option<&str> {
        self.branches.get(branch).map(String::as_str)
    }

    /// Where `branch` is compared with `fetched`, the commit a preview last
    /// fetched its manifest from.
    pub(crate) fn state(&self, branch: &str, fetched: Option<&str>) -> BranchState {
        match (self.head(branch), fetched) {
            (None, _) => BranchState::Gone,
            (Some(head), Some(deployed)) if head != deployed => BranchState::Moved {
                head: head.to_owned(),
                deployed: deployed.to_owned(),
            },
            (Some(head), _) => BranchState::Exists {
                head: head.to_owned(),
            },
        }
    }
}

/// Branch listings for reads: one `git ls-remote` per repository at a
/// time, shared by every read that arrived while it ran, and at most
/// [`Self::LISTING`] at once, so frequent reads never fork a process each.
/// A listing is never reused once a later read arrives, so it is never
/// staler than the read itself, and is dropped once no read waits for it.
pub(crate) struct Listings {
    /// Each repository being listed, with its last listing and when it
    /// finished.
    repositories: std::sync::Mutex<BTreeMap<String, Arc<Repository>>>,
    /// Permits to list.
    listing: Semaphore,
}

/// A finished listing.
type Listed = (std::time::Instant, Arc<anyhow::Result<Heads>>);

impl Default for Listings {
    fn default() -> Self {
        Self {
            repositories: std::sync::Mutex::default(),
            listing: Semaphore::new(Self::LISTING),
        }
    }
}

impl Listings {
    /// Repositories listed at once.
    const LISTING: usize = 4;

    /// Lists `url`'s branches (see [`Heads::list`]), or shares the listing
    /// of a read that was already listing them.
    pub(crate) async fn list(&self, url: &str) -> Arc<anyhow::Result<Heads>> {
        let requested = std::time::Instant::now();
        let reader = Reader {
            listings: self,
            url,
            repository: Some(Arc::clone(self.lock().entry(url.to_owned()).or_default())),
        };
        let mut last = reader.repository().lock().await;
        if let Some((finished, heads)) = &*last
            && *finished > requested
        {
            return Arc::clone(heads);
        }
        let heads = match self.listing.acquire().await {
            Ok(_listing) => Heads::list(url).await,
            Err(error) => Err(error.into()),
        };
        let heads = Arc::new(heads);
        *last = Some((std::time::Instant::now(), Arc::clone(&heads)));
        heads
    }

    /// The repositories being listed.
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Arc<Repository>>> {
        self.repositories
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A repository's last listing, and when it finished.
type Repository = tokio::sync::Mutex<Option<Listed>>;

/// A read of [`Listings`], which removes its repository's entry when it
/// is the last to finish, or is cancelled.
struct Reader<'a> {
    listings: &'a Listings,
    url: &'a str,
    /// Taken only when dropped.
    repository: Option<Arc<Repository>>,
}

impl Reader<'_> {
    /// The repository being read.
    fn repository(&self) -> &Repository {
        self.repository
            .as_deref()
            .expect("taken only when the reader is dropped")
    }
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        let mut repositories = self.listings.lock();
        // Readers take and drop their handle under this lock, so the last
        // to finish sees only the map's and its own, and removes it.
        if let Some(repository) = self.repository.take()
            && Arc::strong_count(&repository) == 2
        {
            drop(repository);
            repositories.remove(self.url);
        }
    }
}

/// Admission for Git checkouts, which every clone goes through: at most
/// [`Self::CLONING`] clones at once, and the files of every checkout held
/// at once, history and working tree, within `[checkouts] max_bytes`. A
/// clone is stopped once it downloads more than its share of the budget
/// (see [`Self::bounded`]); its tree is then measured before its files are
/// written. A checkout waits until it fits, and one that never could, or has
/// more than [`Self::FILES`] files, is refused. Clones in progress are not
/// counted, so checkouts take at most twice `max_bytes`.
pub(crate) struct Checkouts {
    /// Where checkouts are made: `<data_dir>/checkouts`, emptied at startup.
    root: PathBuf,
    /// Permits to clone, held until the clone is checked out, even while it
    /// waits for `disk`: its download isn't counted there yet, so these
    /// bound what waiting clones take.
    cloning: Semaphore,
    /// One permit per MiB of history and working tree, held by each
    /// [`Checkout`].
    disk: Arc<Semaphore>,
    /// The most bytes all checkouts may take at once.
    max_bytes: u64,
}

/// A checkout, or a download, larger than [`Checkouts`] admits.
#[derive(Debug, thiserror::Error)]
#[error("it takes more than {limit} bytes, or has more than {files} files", files = Checkouts::FILES)]
pub(crate) struct TreeTooLarge {
    /// `[checkouts] max_bytes`.
    pub(crate) limit: u64,
}

impl Checkouts {
    /// Clones at once, each downloading at most this share of `max_bytes`.
    const CLONING: usize = 4;
    /// How often a download's size is checked.
    const WATCH: Duration = Duration::from_millis(100);
    /// Applied before checking out, overriding the repository's own
    /// attributes so files are checked out as committed: no filter driver,
    /// encoding, line-ending, or `ident` conversion runs or grows them.
    const ATTRIBUTES: &str = "* -text -filter -working-tree-encoding -ident\n";
    /// Most files one working tree may have.
    pub(crate) const FILES: usize = 500_000;
    /// Bytes per permit of `disk`.
    const MIB: u64 = 1024 * 1024;

    /// Opens `root`, removing checkouts a previous process left behind, with
    /// the default `[checkouts] max_bytes`.
    pub(crate) async fn open(root: PathBuf) -> std::io::Result<Self> {
        match tokio::fs::remove_dir_all(&root).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        tokio::fs::create_dir_all(&root).await?;
        Ok(Self::new(
            root,
            crate::config::CheckoutsConfig::default().max_bytes,
        ))
    }

    /// The same checkouts directory within `max_bytes` instead, before any
    /// checkout is made.
    #[must_use]
    pub(crate) fn with_max_bytes(&self, max_bytes: std::num::NonZeroU64) -> Self {
        Self::new(self.root.clone(), max_bytes)
    }

    /// Checkouts in `root` within `max_bytes`.
    fn new(root: PathBuf, max_bytes: std::num::NonZeroU64) -> Self {
        let max_bytes = max_bytes.get();
        let permits = usize::try_from(max_bytes.div_ceil(Self::MIB))
            .unwrap_or(usize::MAX)
            .min(Semaphore::MAX_PERMITS);
        Self {
            root,
            cloning: Semaphore::new(Self::CLONING),
            disk: Arc::new(Semaphore::new(permits)),
            max_bytes,
        }
    }

    /// The most bytes all checkouts may take at once.
    pub(crate) const fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Clones the repository shallowly into a new directory and checks out
    /// a detached commit, as committed, once it fits, streaming Git output
    /// into `log` when supplied.
    ///
    /// A pinned `commit` is fetched explicitly (the branch may no longer
    /// exist), with the default branch's whole history instead when the
    /// server serves no unadvertised commit; otherwise the tip of `branch`
    /// is used. The resolved `HEAD` must be a full commit hash.
    ///
    /// # Errors
    /// Returns a Git failure, or [`TreeTooLarge`].
    pub(crate) async fn checkout(
        &self,
        repository: &GitRepository,
        log: Option<&crate::build::BuildLog>,
    ) -> anyhow::Result<Checkout> {
        let mut errors = Vec::new();
        repository.validate("repository", &mut errors);
        if !errors.is_empty() {
            bail!("invalid Git repository configuration");
        }
        let _cloning = self.cloning.acquire().await?;
        let directory = tempfile::Builder::new()
            .prefix("checkout-")
            .tempdir_in(&self.root)
            .context("create Git checkout directory")?;
        let root = directory.path().join("repository");
        let download = Box::pin(Self::download(repository, &root, log));
        let share = self.max_bytes / Self::CLONING as u64;
        Self::bounded(directory.path(), share, download).await?;
        let revision = repository.commit.as_deref().unwrap_or("HEAD");
        let tree = self.measure(&root, revision).await?;
        let bytes = tree.saturating_add(Self::disk_usage(directory.path()).await?);
        if bytes > self.max_bytes {
            return Err(TreeTooLarge {
                limit: self.max_bytes,
            }
            .into());
        }
        let disk = Arc::clone(&self.disk)
            .acquire_many_owned(Self::permits(bytes)?)
            .await?;
        let info = root.join(".git/info");
        tokio::fs::create_dir_all(&info)
            .await
            .context("create Git info directory")?;
        tokio::fs::write(info.join("attributes"), Self::ATTRIBUTES)
            .await
            .context("write Git checkout attributes")?;
        crate::command::LoggedCommand::run_recorded(
            Checkout::command()
                .arg("-C")
                .arg(&root)
                .args(["checkout", "--detach", revision, "--"]),
            "checkout Git commit",
            log,
        )
        .await?;
        let output = Checkout::command()
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", "HEAD"])
            .kill_on_drop(true)
            .output()
            .await
            .context("resolve Git commit")?;
        let commit = String::from_utf8(output.stdout)
            .context("decode Git commit")?
            .trim()
            .to_owned();
        if !output.status.success() || !valid_git_commit(&commit) {
            bail!("Git did not resolve a full commit hash");
        }
        let disk = self.settle(directory.path(), disk).await?;
        Ok(Checkout {
            directory,
            _disk: disk,
            commit,
        })
    }

    /// Checks the disk a checkout in `directory` takes against `disk`, its
    /// reservation, which an estimate may fall short of: it is topped up
    /// without waiting, or the checkout is refused.
    async fn settle(
        &self,
        directory: &Path,
        mut disk: OwnedSemaphorePermit,
    ) -> anyhow::Result<OwnedSemaphorePermit> {
        let needed = Self::permits(Self::disk_usage(directory).await?)?;
        let held =
            u32::try_from(disk.num_permits()).context("count the checkout's disk permits")?;
        if let Some(more) = needed.checked_sub(held).filter(|more| *more > 0) {
            let more = Arc::clone(&self.disk)
                .try_acquire_many_owned(more)
                .map_err(|_| TreeTooLarge {
                    limit: self.max_bytes,
                })?;
            disk.merge(more);
        }
        Ok(disk)
    }

    /// The permits of `disk` that `bytes` take.
    fn permits(bytes: u64) -> anyhow::Result<u32> {
        u32::try_from(bytes.div_ceil(Self::MIB).max(1)).context("count the checkout's disk permits")
    }

    /// The bytes `revision`'s working tree takes in the repository at
    /// `repository`, each file and directory charged
    /// `SnapshotLimits::file_cost`, read from object headers without
    /// writing a file.
    ///
    /// # Errors
    /// Returns a Git failure, or [`TreeTooLarge`] when it takes more than
    /// `[checkouts] max_bytes` or has more than [`Self::FILES`] files.
    pub(crate) async fn measure(&self, repository: &Path, revision: &str) -> anyhow::Result<u64> {
        let too_large = || TreeTooLarge {
            limit: self.max_bytes,
        };
        // A line is at most a type, 20 digits, and two separators, so a
        // larger output has more than `FILES` lines.
        let mut command = Checkout::command();
        command.arg("-C").arg(repository).args([
            "ls-tree",
            "-r",
            "-t",
            "--format=%(objecttype) %(objectsize)",
            "--end-of-options",
            revision,
        ]);
        let output = crate::command::LoggedCommand::output(
            &mut command,
            "measure Git tree",
            Self::FILES * 28,
        )
        .await
        .map_err(
            |error| match error.downcast_ref::<crate::command::OutputLimit>() {
                Some(_) => too_large().into(),
                None => error,
            },
        )?;
        let output = String::from_utf8(output).context("decode Git tree sizes")?;
        let mut bytes = 0_u64;
        for (files, entry) in output.lines().enumerate() {
            // Directories and submodules take a block, whatever their size.
            let size = match entry.split_once(' ') {
                Some(("blob", size)) => size.parse().context("parse a Git blob size")?,
                _ => 0,
            };
            let cost = piqueld_core::api::SnapshotLimits::file_cost(size);
            bytes = bytes.saturating_add(cost);
            if files >= Self::FILES || bytes > self.max_bytes {
                return Err(too_large().into());
            }
        }
        Ok(bytes)
    }

    /// Clones `repository` shallowly into `root` without checking it out,
    /// and fetches its pinned commit, if any (see [`Self::checkout`]).
    async fn download(
        repository: &GitRepository,
        root: &Path,
        log: Option<&crate::build::BuildLog>,
    ) -> anyhow::Result<()> {
        // `--no-local` keeps `--depth` for repository paths too.
        let mut clone = Checkout::command();
        clone.args([
            "clone",
            "--no-checkout",
            "--single-branch",
            "--depth=1",
            "--no-local",
        ]);
        if repository.commit.is_none() {
            clone.args(["--branch", &repository.branch]);
        }
        clone.arg("--").arg(&repository.url).arg(root);
        crate::command::LoggedCommand::run_recorded(&mut clone, "clone Git repository", log)
            .await?;
        let Some(commit) = &repository.commit else {
            return Ok(());
        };
        let fetch = |arguments: &[&str]| {
            let mut fetch = Checkout::command();
            fetch.arg("-C").arg(root).arg("fetch").args(arguments);
            fetch
        };
        let pinned = crate::command::LoggedCommand::run_recorded(
            &mut fetch(&["--depth=1", "origin", commit]),
            "fetch pinned Git commit",
            log,
        )
        .await;
        if pinned.is_err() {
            crate::command::LoggedCommand::run_recorded(
                &mut fetch(&[
                    "--unshallow",
                    "origin",
                    "+refs/heads/*:refs/remotes/origin/*",
                ]),
                "fetch Git history",
                log,
            )
            .await?;
        }
        Ok(())
    }

    /// Runs `transfer`, which downloads into `directory`, and stops it,
    /// with its process group, once the directory takes more than `limit`
    /// bytes; one that finishes larger fails too.
    ///
    /// # Errors
    /// Returns `transfer`'s error, or [`TreeTooLarge`].
    pub(crate) async fn bounded<T, E: From<TreeTooLarge>>(
        directory: &Path,
        limit: u64,
        transfer: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        let watch = async {
            loop {
                tokio::time::sleep(Self::WATCH).await;
                match Self::disk_usage(directory).await {
                    Ok(bytes) if bytes > limit => return TreeTooLarge { limit },
                    Ok(_) => {}
                    Err(error) => tracing::warn!(?error, "measuring a Git download failed"),
                }
            }
        };
        let transferred = tokio::select! {
            result = transfer => result?,
            too_large = watch => return Err(too_large.into()),
        };
        // A transfer may finish between checks.
        match Self::disk_usage(directory).await {
            Ok(bytes) if bytes <= limit => Ok(transferred),
            Ok(_) => Err(TreeTooLarge { limit }.into()),
            Err(error) => {
                tracing::warn!(?error, "measuring a Git download failed");
                Err(TreeTooLarge { limit }.into())
            }
        }
    }

    /// The disk allocated to `directory` and everything under it, which
    /// Git may be writing.
    async fn disk_usage(directory: &Path) -> anyhow::Result<u64> {
        let directory = directory.to_owned();
        tokio::task::spawn_blocking(move || {
            use std::os::unix::fs::MetadataExt;
            let gone = |error: &std::io::Error| error.kind() == std::io::ErrorKind::NotFound;
            let (mut bytes, mut directories) = (0_u64, vec![directory]);
            while let Some(directory) = directories.pop() {
                let entries = match std::fs::read_dir(&directory) {
                    Err(error) if gone(&error) => continue,
                    entries => entries?,
                };
                for entry in entries {
                    let metadata =
                        match entry.and_then(|entry| Ok((entry.path(), entry.metadata()?))) {
                            Err(error) if gone(&error) => continue,
                            metadata => metadata?,
                        };
                    let (path, metadata) = metadata;
                    // `blocks` counts 512-byte units, whatever the block size.
                    bytes = bytes.saturating_add(metadata.blocks().saturating_mul(512));
                    if metadata.is_dir() {
                        directories.push(path);
                    }
                }
            }
            Ok(bytes)
        })
        .await
        .context("measure Git checkout")?
    }

    /// Checks a repository out and builds its local image, labelled for
    /// `owner` and queued with `priority`, streaming Git and Docker output
    /// into `log` and recording the resolved commit on it when supplied.
    /// Returns the commit and built image ID; the checkout is removed once
    /// the build finishes.
    pub(crate) async fn prepare(
        &self,
        repository: &GitRepository,
        build: &piqueld_core::manifest::ValidatedBuild,
        (owner, priority): (&piqueld_core::InstanceId, crate::docker::BuildPriority),
        docker: &impl crate::docker::DockerApi,
        log: Option<&crate::build::BuildLog>,
    ) -> anyhow::Result<(String, piqueld_core::resource::Sha256Digest)> {
        let checkout = self.checkout(repository, log).await?;
        if let Some(log) = log {
            log.commit(&checkout.commit).await?;
        }
        let piqueld_core::manifest::ValidatedBuild::Docker {
            dockerfile,
            context,
            args,
            target,
        } = build;
        let build = crate::docker::ImageBuild {
            owner,
            dockerfile: checkout.path(dockerfile).await?,
            context: checkout.path(context).await?,
            args,
            target: target.as_deref(),
            priority,
        };
        let image = docker
            .build_image(&build, log)
            .await
            .context("build Git source image")?;
        Ok((checkout.commit, image))
    }
}

/// A private checkout pinned once, retained until preparation finishes.
pub(crate) struct Checkout {
    /// Temporary directory removed when the checkout is dropped.
    directory: tempfile::TempDir,
    /// Its share of [`Checkouts`]' disk, released once the directory is gone.
    _disk: OwnedSemaphorePermit,
    /// Full commit hash actually checked out.
    pub(crate) commit: String,
}

impl Checkout {
    /// Most memory one Git allocation may take, such as an object being
    /// inflated or resolved from its deltas: a small pack can describe huge
    /// objects. Larger files are streamed instead (`core.bigFileThreshold`).
    const ALLOC_LIMIT: &str = "1g";

    /// Builds a hardened `git` command: restricted transports, no interactive
    /// prompts, hooks disabled, system configuration ignored, allocations within
    /// [`Self::ALLOC_LIMIT`], and killed if the future is dropped.
    fn command() -> Command {
        let mut command = Command::new("git");
        // Inherit the daemon user's credentials while excluding executable transport
        // helpers, even when its configuration enables them or rewrites a URL to use
        // one. Ignore the host-wide `/etc/gitconfig`: it holds operator settings,
        // such as rewriting GitHub URLs to SSH, that the daemon user cannot satisfy.
        // Never hang waiting for a password prompt.
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ALLOW_PROTOCOL", "file:git:http:https:ssh")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .env("GIT_ALLOC_LIMIT", Self::ALLOC_LIMIT);
        command.args(["-c", "core.hooksPath=/dev/null"]);
        command.kill_on_drop(true);
        command
    }

    /// Working tree root inside the temporary directory.
    pub(crate) fn root(&self) -> PathBuf {
        self.directory.path().join("repository")
    }

    /// Resolves a manifest-relative path to an existing canonical path inside the
    /// working tree. Symlinks are followed, so anything escaping the checkout or
    /// pointing into `.git` is rejected.
    ///
    /// ```text
    /// "./Dockerfile"       -> <checkout>/repository/Dockerfile
    /// "../Dockerfile"      -> error (leaves the repository)
    /// "link-to-etc/passwd" -> error (symlink escapes the checkout)
    /// ```
    pub(crate) async fn path(&self, relative: &str) -> anyhow::Result<PathBuf> {
        if !valid_repository_path(relative) {
            bail!("path must remain within the repository root");
        }
        let root = tokio::fs::canonicalize(self.root())
            .await
            .context("locate checkout root")?;
        let path = tokio::fs::canonicalize(root.join(relative))
            .await
            .context("locate repository file")?;
        if !path.starts_with(&root) || path.starts_with(root.join(".git")) {
            bail!("repository path escapes the checkout");
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    async fn commit(root: &Path, contents: &str) -> String {
        tokio::fs::write(root.join("Dockerfile"), contents)
            .await
            .unwrap();
        crate::command::LoggedCommand::run(
            Checkout::command().arg("-C").arg(root).args(["add", "."]),
            "stage fixture",
        )
        .await
        .unwrap();
        crate::command::LoggedCommand::run(
            Checkout::command().arg("-C").arg(root).args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "fixture",
            ]),
            "commit fixture",
        )
        .await
        .unwrap();
        let output = Checkout::command()
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "HEAD"])
            .output()
            .await
            .unwrap();
        String::from_utf8(output.stdout).unwrap().trim().into()
    }

    #[tokio::test]
    async fn checkout_ignores_host_wide_git_configuration() {
        let root = tempfile::tempdir().unwrap();
        let system = root.path().join("gitconfig");
        tokio::fs::write(&system, "[piqueld]\n\tprobe = system\n")
            .await
            .unwrap();
        let output = Checkout::command()
            .env("GIT_CONFIG_SYSTEM", &system)
            .args(["config", "--get", "piqueld.probe"])
            .output()
            .await
            .unwrap();
        assert!(output.stdout.is_empty(), "{output:?}");
    }

    #[tokio::test]
    async fn checkout_rejects_executable_transports_even_when_host_allows_them() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("executed");
        let helper = format!("ext::touch {}", marker.display());
        for rewritten in [false, true] {
            let mut command = Checkout::command();
            command.args(["-c", "protocol.ext.allow=always"]);
            if rewritten {
                command.args([
                    "-c",
                    &format!("url.{helper}.insteadOf=https://example.com/repo"),
                ]);
            }
            command
                .args(["clone", "--"])
                .arg(if rewritten {
                    "https://example.com/repo"
                } else {
                    &helper
                })
                .arg(root.path().join("checkout"));
            let error = crate::command::LoggedCommand::run(&mut command, "clone fixture")
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("transport 'ext' not allowed"),
                "{error:#}"
            );
            assert!(!marker.exists());
        }
    }

    /// Checkouts in a temporary directory within `max_bytes`.
    async fn checkouts(root: &Path, max_bytes: u64) -> Checkouts {
        Checkouts::open(root.join("checkouts"))
            .await
            .unwrap()
            .with_max_bytes(std::num::NonZeroU64::new(max_bytes).unwrap())
    }

    /// A repository with one commit of `Dockerfile`, returning its source.
    async fn fixture(root: &Path) -> GitRepository {
        crate::command::LoggedCommand::run(
            Checkout::command()
                .args(["init", "--initial-branch=main"])
                .arg(root),
            "initialize fixture",
        )
        .await
        .unwrap();
        commit(root, "FROM scratch\n").await;
        GitRepository {
            url: root.display().to_string(),
            branch: "main".into(),
            commit: None,
        }
    }

    #[tokio::test]
    async fn checkout_pins_commits_and_confines_paths() {
        let repository = tempfile::tempdir().unwrap();
        let mut source = fixture(repository.path()).await;
        let checkouts = checkouts(repository.path(), 1 << 30).await;
        let checkout = checkouts.checkout(&source, None).await.unwrap();
        let first = checkout.commit.clone();
        let second = commit(repository.path(), "FROM scratch\nLABEL version=2\n").await;
        assert_eq!(
            checkouts.checkout(&source, None).await.unwrap().commit,
            second
        );
        source.commit = Some(first.clone());
        source.branch = "branch-no-longer-exists".into();
        assert_eq!(
            checkouts.checkout(&source, None).await.unwrap().commit,
            first
        );
        assert!(checkout.path("../Dockerfile").await.is_err());
        assert!(checkout.path("missing").await.is_err());
        std::os::unix::fs::symlink(repository.path(), checkout.root().join("escape")).unwrap();
        assert!(checkout.path("escape/Dockerfile").await.is_err());
        assert_eq!(
            tokio::fs::read_to_string(checkout.path("./Dockerfile").await.unwrap())
                .await
                .unwrap(),
            "FROM scratch\n"
        );
        // Clones are shallow, and their directory goes with them.
        let depth = Checkout::command()
            .arg("-C")
            .arg(checkout.root())
            .args(["rev-list", "--count", "HEAD"])
            .output()
            .await
            .unwrap();
        assert_eq!(String::from_utf8(depth.stdout).unwrap().trim(), "1");
        drop(checkout);
        let left = std::fs::read_dir(&checkouts.root).unwrap().count();
        assert_eq!(left, 0);
    }

    /// A tree that can never fit is refused before it is written; one that
    /// fits once another checkout is gone waits for it.
    #[tokio::test]
    async fn checkouts_wait_for_room_and_refuse_trees_that_never_fit() {
        let repository = tempfile::tempdir().unwrap();
        let source = fixture(repository.path()).await;
        // One MiB permit: the fixture's tree takes all of it.
        let checkouts = checkouts(repository.path(), 1 << 20).await;
        let held = checkouts.checkout(&source, None).await.unwrap();
        let waiting = checkouts.checkout(&source, None);
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(500), &mut waiting)
                .await
                .is_err()
        );
        drop(held);
        waiting.await.unwrap();

        let small = self::checkouts(repository.path(), 4).await;
        let error = small.checkout(&source, None).await.err().unwrap();
        assert!(error.downcast_ref::<TreeTooLarge>().is_some(), "{error:#}");
        assert_eq!(std::fs::read_dir(&small.root).unwrap().count(), 0);
    }

    /// Reads that arrive while a repository is being listed share that
    /// listing; a later read lists it again.
    #[tokio::test]
    async fn listings_are_shared_only_by_reads_that_overlap() {
        let repository = tempfile::tempdir().unwrap();
        let url = fixture(repository.path()).await.url;
        let listings = Listings::default();
        let [first, second] =
            futures_util::future::join_all([listings.list(&url), listings.list(&url)])
                .await
                .try_into()
                .ok()
                .unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(listings.repositories.lock().unwrap().is_empty());
        assert!(first.as_ref().as_ref().unwrap().head("main").is_some());
        let later = listings.list(&url).await;
        assert!(!Arc::ptr_eq(&first, &later));
        // Nothing is kept once no read waits, even a cancelled one.
        let cancelled = listings.list(&url);
        drop(tokio::time::timeout(Duration::ZERO, cancelled).await);
        assert!(listings.repositories.lock().unwrap().is_empty());
    }

    /// The repository's attributes cannot convert, and so grow, the files
    /// checked out: they are the committed bytes.
    #[tokio::test]
    async fn checkouts_are_the_committed_bytes() {
        let repository = tempfile::tempdir().unwrap();
        let source = fixture(repository.path()).await;
        tokio::fs::write(repository.path().join(".gitattributes"), "* ident\n")
            .await
            .unwrap();
        commit(repository.path(), "$Id$\n").await;
        let checkouts = checkouts(repository.path(), 1 << 30).await;
        let checkout = checkouts.checkout(&source, None).await.unwrap();
        let dockerfile = checkout.path("Dockerfile").await.unwrap();
        assert_eq!(
            tokio::fs::read_to_string(dockerfile).await.unwrap(),
            "$Id$\n"
        );
    }

    /// Directories are charged too: a deep tree of tiny files takes far
    /// more than its files' bytes.
    #[tokio::test]
    async fn checkouts_charge_their_directories() {
        let repository = tempfile::tempdir().unwrap();
        let source = fixture(repository.path()).await;
        for directory in 0..50 {
            let path = repository.path().join(format!("{directory}/a/b/c"));
            tokio::fs::create_dir_all(&path).await.unwrap();
            tokio::fs::write(path.join("f"), "x").await.unwrap();
        }
        let head = commit(repository.path(), "FROM scratch\n").await;
        let checkouts = checkouts(repository.path(), 1 << 30).await;
        let tree = checkouts
            .measure(Path::new(&source.url), &head)
            .await
            .unwrap();
        // 51 files and 200 directories, a block each and their index share.
        assert_eq!(tree, 251 * (4096 + 256));
    }

    /// A download that grows past its limit is stopped, and one that
    /// finishes past it, before it is checked, fails.
    #[tokio::test]
    async fn downloads_past_their_limit_are_stopped() {
        let directory = tempfile::tempdir().unwrap();
        let pack = directory.path().join("pack");
        let stalled = async {
            tokio::fs::write(&pack, vec![0; 8192]).await.unwrap();
            std::future::pending::<anyhow::Result<()>>().await
        };
        let finished = async {
            tokio::fs::write(&pack, vec![0; 8192]).await.unwrap();
            anyhow::Ok(())
        };
        for result in [
            Checkouts::bounded(directory.path(), 4096, stalled).await,
            Checkouts::bounded(directory.path(), 4096, finished).await,
        ] {
            let error = result.unwrap_err();
            assert!(error.downcast_ref::<TreeTooLarge>().is_some(), "{error:#}");
        }
    }
}
