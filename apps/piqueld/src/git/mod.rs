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
/// staler than the read itself.
pub(crate) struct Listings {
    /// The last listing of each repository ever read, and when it finished.
    repositories: std::sync::Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<Option<Listed>>>>>,
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
        let repository = Arc::clone(
            self.repositories
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(url.to_owned())
                .or_default(),
        );
        let mut last = repository.lock().await;
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
}

/// Admission for Git checkouts, which every clone goes through: at most
/// [`Self::CLONING`] clones at once, and the working trees of every checkout
/// held at once within `[checkouts] max_bytes`. A tree is measured before
/// its files are written; a checkout waits until it fits, and one that never
/// could, or has more than [`Self::FILES`] files, is refused.
pub(crate) struct Checkouts {
    /// Where checkouts are made: `<data_dir>/checkouts`, emptied at startup.
    root: PathBuf,
    /// Permits to clone, held until the clone is measured and checked out.
    cloning: Semaphore,
    /// One permit per MiB of working tree, held by each [`Checkout`].
    disk: Arc<Semaphore>,
    /// The most bytes all working trees may take at once.
    max_bytes: u64,
}

/// A working tree larger than [`Checkouts`] admits.
#[derive(Debug, thiserror::Error)]
#[error("its files take more than {limit} bytes, or number more than {files}", files = Checkouts::FILES)]
pub(crate) struct TreeTooLarge {
    /// `[checkouts] max_bytes`.
    pub(crate) limit: u64,
}

impl Checkouts {
    /// Clones at once. Each clone's history is on disk before its tree is
    /// measured, which a shallow clone keeps to about one compressed tree.
    const CLONING: usize = 4;
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

    /// The most bytes all working trees may take at once.
    pub(crate) const fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Clones the repository shallowly into a new directory and checks out
    /// a detached commit once its tree fits, streaming Git output into `log`
    /// when supplied.
    ///
    /// A pinned `commit` is fetched explicitly (the branch may no longer exist);
    /// otherwise the tip of `branch` is used. The resolved `HEAD` must be a full
    /// commit hash.
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
        clone.arg("--").arg(&repository.url).arg(&root);
        crate::command::LoggedCommand::run_recorded(&mut clone, "clone Git repository", log)
            .await?;
        if let Some(commit) = &repository.commit {
            crate::command::LoggedCommand::run_recorded(
                Checkout::command().arg("-C").arg(&root).args([
                    "fetch",
                    "--depth=1",
                    "origin",
                    commit,
                ]),
                "fetch pinned Git commit",
                log,
            )
            .await?;
        }
        let revision = repository.commit.as_deref().unwrap_or("HEAD");
        let bytes = self.measure(&root, revision).await?;
        let permits = u32::try_from(bytes.div_ceil(Self::MIB).max(1))
            .context("count the checkout's disk permits")?;
        let disk = Arc::clone(&self.disk).acquire_many_owned(permits).await?;
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
        Ok(Checkout {
            directory,
            _disk: disk,
            commit,
        })
    }

    /// The bytes `revision`'s working tree takes in the repository at
    /// `repository`, read from object headers without writing a file.
    ///
    /// # Errors
    /// Returns a Git failure, or [`TreeTooLarge`] when it takes more than
    /// `[checkouts] max_bytes` or has more than [`Self::FILES`] files.
    pub(crate) async fn measure(&self, repository: &Path, revision: &str) -> anyhow::Result<u64> {
        let too_large = || TreeTooLarge {
            limit: self.max_bytes,
        };
        // A line is at most 20 digits and a newline, so a larger output
        // has more than `FILES` lines.
        let mut command = Checkout::command();
        command.arg("-C").arg(repository).args([
            "ls-tree",
            "-r",
            "--format=%(objectsize)",
            "--end-of-options",
            revision,
        ]);
        let output = crate::command::LoggedCommand::output(
            &mut command,
            "measure Git tree",
            Self::FILES * 21,
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
        for (files, size) in output.lines().enumerate() {
            // Submodules print `-`; they are never checked out.
            bytes = bytes.saturating_add(size.parse().unwrap_or(0));
            if files >= Self::FILES || bytes > self.max_bytes {
                return Err(too_large().into());
            }
        }
        Ok(bytes)
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
            .build_image_recorded(&build, log)
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
    /// prompts, hooks disabled, allocations within [`Self::ALLOC_LIMIT`], and
    /// killed if the future is dropped.
    fn command() -> Command {
        let mut command = Command::new("git");
        // Inherit host credentials while excluding executable transport helpers, even
        // when host configuration enables them or rewrites a URL to use one.
        // Never hang waiting for a password prompt.
        command
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
        assert!(first.as_ref().as_ref().unwrap().head("main").is_some());
        let later = listings.list(&url).await;
        assert!(!Arc::ptr_eq(&first, &later));
    }
}
