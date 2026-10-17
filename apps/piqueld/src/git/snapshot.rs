//! Snapshots of local worktrees, uploaded by `preview deploy --local` as Git
//! bundles. Bundles are untrusted: each is streamed to disk within
//! `[snapshots] max_upload_bytes`, then received into a new private
//! repository of its own, never one shared with checkouts or other uploads:
//!
//! 1. its header must name one snapshot ref, and the commits it needs, its
//!    prerequisites, are fetched from the application's repository;
//! 2. `git bundle verify` checks it applies, and unbundling checks every
//!    object (`transfer.fsckObjects`);
//! 3. the prerequisites must all be ancestors of one of them, the base, and
//!    the snapshot commit must descend from it. A thin bundle of a history
//!    with merges needs the base and some of its ancestors;
//! 4. its working tree must fit `[checkouts] max_bytes` (see
//!    [`Checkouts::measure`]), or no deployment could check it out.
//!
//! Fetching the prerequisites is stopped once the staging directory takes
//! more than `[checkouts] max_bytes` (see [`Checkouts::bounded`]).
//!
//! Unbundling keeps the pack as uploaded, so it takes about as much disk as
//! the upload; Git's allocations are bounded (see [`Checkout`]), so a small
//! pack of huge objects cannot exhaust memory.
//! An upload holds its permit until it is accepted or refused, so at most
//! [`Snapshots::RECEIVING`] uploads are on disk unaccepted.
//!
//! The repository then moves to `<commit>.git`, from which deployments of
//! that snapshot read their manifest and build `"self"` sources. Snapshots
//! no preview's latest deployment uses are removed (see [`Snapshots::prune`]).
use super::{Checkout, Checkouts, TreeTooLarge};
use crate::command::LoggedCommand;
use crate::store::StoreError;
use axum::body::Bytes;
use futures_util::{Stream, StreamExt};
use piqueld_core::manifest::{GitRepository, valid_git_commit};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// Why an upload was refused. Details stay in the daemon's logs.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// The bundle is larger than `[snapshots] max_upload_bytes`.
    #[error("the snapshot bundle is larger than the {limit}-byte upload limit")]
    TooLarge {
        /// The limit, in bytes.
        limit: u64,
    },
    /// The snapshot, or the history it needs, would not fit
    /// `[checkouts] max_bytes`.
    #[error(
        "the snapshot, or the history it needs, takes more than the {limit}-byte checkout limit"
    )]
    TreeTooLarge {
        /// The limit, in bytes.
        limit: u64,
    },
    /// The upload is not a valid bundle of one snapshot commit.
    #[error("the upload is not a valid Git bundle of one snapshot commit: {0}")]
    Invalid(&'static str),
    /// The commits the bundle needs could not be fetched.
    #[error("the commits the bundle needs could not be fetched from the application's repository")]
    BaseUnavailable {
        /// The bundle's prerequisites.
        prerequisites: Vec<String>,
    },
    /// The body stalled, or receiving took too long.
    #[error("the snapshot upload took too long; retry it")]
    TimedOut,
    /// The upload could not be read or stored.
    #[error("the snapshot could not be received")]
    Internal(#[source] anyhow::Error),
}

impl From<TreeTooLarge> for SnapshotError {
    fn from(TreeTooLarge { limit }: TreeTooLarge) -> Self {
        Self::TreeTooLarge { limit }
    }
}

impl SnapshotError {
    /// An upload that could not be read or stored because of `error`.
    fn internal(error: impl Into<anyhow::Error>) -> Self {
        Self::Internal(error.into())
    }
}

/// The snapshot ref every bundle carries, and the branch its repository has.
const SNAPSHOT_BRANCH: &str = "snapshot";

/// Where uploaded snapshots are kept: `<data_dir>/snapshots`, holding one
/// bare repository per snapshot commit and `staging/` for uploads being
/// received.
pub(crate) struct Snapshots {
    root: PathBuf,
    /// Held while a received snapshot moves into place and is accepted, and
    /// while unused ones are removed, so removal never races an acceptance.
    lock: tokio::sync::Mutex<()>,
    /// Permits to receive an upload (see [`Self::RECEIVING`]).
    receiving: tokio::sync::Semaphore,
}

/// A snapshot received into a staging repository, not yet in place.
pub(crate) struct Received<'a> {
    /// The snapshot commit.
    pub(crate) commit: String,
    /// The commit it was uploaded against; `None` for a complete bundle.
    pub(crate) base: Option<String>,
    /// Holds the bundle and the repository; removed when dropped.
    staging: tempfile::TempDir,
    /// Its permit to receive, held until it is kept or refused.
    _receiving: tokio::sync::SemaphorePermit<'a>,
}

impl Snapshots {
    /// Most bytes a bundle header may take.
    const HEADER_LIMIT: u64 = 64 * 1024;
    /// Uploads received or waiting to be accepted at once, bounding the
    /// disk a burst of them takes.
    const RECEIVING: usize = 2;
    /// Longest receiving one upload may take, from its permit on, so a
    /// stalled one never holds a permit for long.
    const RECEIVE_TIMEOUT: Duration = Duration::from_secs(600);
    /// Longest the body may go without sending anything.
    const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

    /// Opens `root`, removing uploads a previous process left half-received.
    pub(crate) async fn open(root: PathBuf) -> std::io::Result<Self> {
        Checkouts::recreate(&root.join("staging")).await?;
        Ok(Self {
            root,
            lock: tokio::sync::Mutex::new(()),
            receiving: tokio::sync::Semaphore::new(Self::RECEIVING),
        })
    }

    /// Streams `body`, which announced `length` bytes, to a staging file of
    /// at most `limit` bytes, then receives it as described in the module
    /// documentation, fetching its prerequisites from `url` and measuring
    /// its tree against `checkouts`. At most [`Self::RECEIVING`] uploads are
    /// received, or wait to be kept, at once; others wait. Each gets
    /// [`Self::RECEIVE_TIMEOUT`], and its body [`Self::IDLE_TIMEOUT`]
    /// between chunks.
    ///
    /// # Errors
    /// Returns [`SnapshotError`] for an oversized, malformed, or unverifiable
    /// bundle, an unavailable base, a tree too large to check out, a
    /// timeout, or a storage failure.
    pub(crate) async fn receive<E: std::error::Error + Send + Sync + 'static>(
        &self,
        body: impl Stream<Item = Result<Bytes, E>>,
        (length, limit): (Option<u64>, u64),
        url: &str,
        checkouts: &Checkouts,
    ) -> Result<Received<'_>, SnapshotError> {
        if length.is_some_and(|length| length > limit) {
            return Err(SnapshotError::TooLarge { limit });
        }
        let receiving = self
            .receiving
            .acquire()
            .await
            .map_err(SnapshotError::internal)?;
        let (commit, base, staging) = tokio::time::timeout(
            Self::RECEIVE_TIMEOUT,
            self.stage(body, limit, url, checkouts),
        )
        .await
        .map_err(|_| SnapshotError::TimedOut)??;
        Ok(Received {
            commit,
            base,
            staging,
            _receiving: receiving,
        })
    }

    /// Receives an upload into a new staging repository (see
    /// [`Self::receive`]), returning its commit, its base, and the staging
    /// directory.
    async fn stage<E: std::error::Error + Send + Sync + 'static>(
        &self,
        body: impl Stream<Item = Result<Bytes, E>>,
        limit: u64,
        url: &str,
        checkouts: &Checkouts,
    ) -> Result<(String, Option<String>, tempfile::TempDir), SnapshotError> {
        let staging = tempfile::Builder::new()
            .prefix("upload-")
            .tempdir_in(self.root.join("staging"))
            .map_err(SnapshotError::internal)?;
        let bundle = staging.path().join("snapshot.bundle");
        Self::store(body, &bundle, limit).await?;
        let header = BundleHeader::read(&bundle).await?;
        let repository = Staged(staging.path().join("repository.git"));
        repository.init().await?;
        if !header.prerequisites.is_empty() {
            let fetch = repository.fetch(url, &header.prerequisites);
            Checkouts::bounded(staging.path(), checkouts.max_bytes(), fetch).await?;
        }
        repository.unbundle(&bundle, &header).await?;
        let base = repository.base(&header).await?;
        checkouts
            .measure(&repository.0, SNAPSHOT_BRANCH)
            .await
            .map_err(|error| match error.downcast::<TreeTooLarge>() {
                Ok(too_large) => too_large.into(),
                Err(error) => SnapshotError::Internal(error),
            })?;
        Ok((header.commit, base, staging))
    }

    /// Streams `body` to `path`, refusing more than `limit` bytes.
    async fn store<E: std::error::Error + Send + Sync + 'static>(
        body: impl Stream<Item = Result<Bytes, E>>,
        path: &Path,
        limit: u64,
    ) -> Result<(), SnapshotError> {
        let mut file = tokio::fs::File::create(path)
            .await
            .map_err(SnapshotError::internal)?;
        let mut written = 0_u64;
        let mut body = std::pin::pin!(body);
        while let Some(chunk) = tokio::time::timeout(Self::IDLE_TIMEOUT, body.next())
            .await
            .map_err(|_| SnapshotError::TimedOut)?
        {
            let chunk = chunk.map_err(SnapshotError::internal)?;
            written = written.saturating_add(chunk.len() as u64);
            if written > limit {
                return Err(SnapshotError::TooLarge { limit });
            }
            file.write_all(&chunk)
                .await
                .map_err(SnapshotError::internal)?;
        }
        file.flush().await.map_err(SnapshotError::internal)
    }

    /// Moves `received` into place, unless that commit already is, and runs
    /// `accept`, which deploys it, before any [`Self::prune`] can remove it.
    /// When `accept` fails, a snapshot this call moved into place goes again,
    /// and `accept`'s error is returned even if removing it fails.
    pub(crate) async fn keep<T, E: From<SnapshotError>>(
        &self,
        received: Received<'_>,
        accept: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        let _lock = self.lock.lock().await;
        let path = self.path(&received.commit);
        let installed = !tokio::fs::try_exists(&path)
            .await
            .map_err(SnapshotError::internal)?;
        if installed {
            tokio::fs::rename(received.staging.path().join("repository.git"), &path)
                .await
                .map_err(SnapshotError::internal)?;
        }
        let accepted = accept.await;
        // The refusal is what the caller needs; pruning retries a failed removal.
        if accepted.is_err()
            && installed
            && let Err(error) = tokio::fs::remove_dir_all(&path).await
        {
            tracing::warn!(?error, "removing a refused snapshot failed");
        }
        accepted
    }

    /// Where a deployment of the kept snapshot `commit` of the repository
    /// at `url` checks it out from.
    pub(crate) fn source(&self, url: &str, commit: &str) -> SnapshotSource {
        SnapshotSource {
            url: url.to_owned(),
            repository: GitRepository {
                url: self.path(commit).display().to_string(),
                branch: SNAPSHOT_BRANCH.into(),
                commit: Some(commit.into()),
            },
        }
    }

    /// Removes every kept snapshot but the `needed` ones, which are read
    /// while no snapshot is being accepted.
    ///
    /// # Errors
    /// Returns `needed`'s error, or a file system error; a failed removal is
    /// retried next time.
    pub(crate) async fn prune(
        &self,
        needed: impl Future<Output = Result<Vec<String>, StoreError>>,
    ) -> Result<(), StoreError> {
        let _lock = self.lock.lock().await;
        let needed = needed.await?.into_iter().collect::<BTreeSet<_>>();
        let mut entries = tokio::fs::read_dir(&self.root)
            .await
            .map_err(StoreError::PathSource)?;
        while let Some(entry) = entries.next_entry().await.map_err(StoreError::PathSource)? {
            let name = entry.file_name();
            let Some(commit) = name.to_str().and_then(|name| name.strip_suffix(".git")) else {
                continue;
            };
            if valid_git_commit(commit) && !needed.contains(commit) {
                tokio::fs::remove_dir_all(entry.path())
                    .await
                    .map_err(StoreError::PathSource)?;
            }
        }
        Ok(())
    }

    /// Where the snapshot `commit` is kept.
    fn path(&self, commit: &str) -> PathBuf {
        self.root.join(format!("{commit}.git"))
    }
}

/// The new repository an upload is received into.
struct Staged(PathBuf);

impl Staged {
    /// Longest fetching the prerequisites may take; its process group is
    /// then killed.
    const FETCH_TIMEOUT: Duration = Duration::from_secs(300);

    /// Runs Git in the repository.
    async fn git(&self, operation: &'static str, arguments: &[&str]) -> anyhow::Result<Vec<u8>> {
        let mut command = Checkout::command_in(&self.0);
        command.args(arguments);
        LoggedCommand::output(&mut command, operation, 4096).await
    }

    /// Creates the bare repository.
    async fn init(&self) -> Result<(), SnapshotError> {
        let mut init = Checkout::command();
        init.args([
            "init",
            "--quiet",
            "--bare",
            "--object-format=sha1",
            "--initial-branch",
            SNAPSHOT_BRANCH,
        ])
        .arg(&self.0);
        LoggedCommand::output(&mut init, "create snapshot repository", 4096)
            .await
            .map(drop)
            .map_err(SnapshotError::Internal)
    }

    /// Fetches `prerequisites` and their history from `url`, under refs so
    /// that `git bundle verify` finds them.
    async fn fetch(&self, url: &str, prerequisites: &[String]) -> Result<(), SnapshotError> {
        let refspecs = prerequisites
            .iter()
            .map(|commit| format!("{commit}:refs/prerequisites/{commit}"))
            .collect::<Vec<_>>();
        let mut arguments = vec![
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--",
            url,
        ];
        arguments.extend(refspecs.iter().map(String::as_str));
        let fetched = tokio::time::timeout(
            Self::FETCH_TIMEOUT,
            self.git("fetch snapshot prerequisites", &arguments),
        )
        .await;
        if matches!(fetched, Ok(Ok(_))) {
            return Ok(());
        }
        // Git may echo the URL, credentials included, so details stay in logs.
        tracing::warn!(?fetched, "snapshot prerequisite fetch failed");
        Err(SnapshotError::BaseUnavailable {
            prerequisites: prerequisites.to_vec(),
        })
    }

    /// Verifies `bundle` against the repository, unbundles its snapshot ref
    /// with every object checked, and checks that `header`'s commit is what
    /// it unbundled.
    async fn unbundle(&self, bundle: &Path, header: &BundleHeader) -> Result<(), SnapshotError> {
        let bundle = bundle.to_str().ok_or(SnapshotError::Invalid("path"))?;
        let invalid = |reason| {
            move |error: anyhow::Error| {
                tracing::warn!(?error, reason, "snapshot bundle refused");
                SnapshotError::Invalid(reason)
            }
        };
        self.git(
            "verify snapshot bundle",
            &["bundle", "verify", "--quiet", bundle],
        )
        .await
        .map_err(invalid("verification failed"))?;
        let refspec = format!("{}:refs/heads/{SNAPSHOT_BRANCH}", header.reference);
        // One thread resolves deltas, so one object at most is inflated.
        let unbundle = [
            "-c",
            "transfer.fsckObjects=true",
            "-c",
            "pack.threads=1",
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            bundle,
            &refspec,
        ];
        self.git("unbundle snapshot", &unbundle)
            .await
            .map_err(invalid("its objects are invalid"))?;
        let resolve = [
            "rev-parse",
            "--verify",
            "--end-of-options",
            "snapshot^{commit}",
        ];
        let commit = self
            .git("resolve snapshot commit", &resolve)
            .await
            .map_err(invalid("its ref is not a commit"))?;
        if String::from_utf8_lossy(&commit).trim() != header.commit {
            return Err(SnapshotError::Invalid("its ref is not a commit"));
        }
        Ok(())
    }

    /// The base of an unbundled snapshot: the one prerequisite every other
    /// is an ancestor of, which the snapshot commit descends from. `None`
    /// for a complete bundle.
    async fn base(&self, header: &BundleHeader) -> Result<Option<String>, SnapshotError> {
        let invalid = |error| {
            tracing::warn!(?error, "snapshot prerequisites refused");
            SnapshotError::Invalid("its prerequisites are not the history of one base")
        };
        if header.prerequisites.is_empty() {
            return Ok(None);
        }
        let mut independent = vec!["merge-base", "--independent"];
        independent.extend(header.prerequisites.iter().map(String::as_str));
        let bases = self
            .git("find snapshot base", &independent)
            .await
            .map_err(invalid)?;
        let bases = String::from_utf8_lossy(&bases);
        let [base] = bases.split_whitespace().collect::<Vec<_>>()[..] else {
            return Err(invalid(anyhow::anyhow!(
                "several independent prerequisites"
            )));
        };
        self.git(
            "check snapshot base",
            &["merge-base", "--is-ancestor", base, &header.commit],
        )
        .await
        .map_err(invalid)?;
        Ok(Some(base.to_owned()))
    }
}

/// A kept snapshot that a deployment checks its manifest and `"self"`
/// sources out from, instead of its repository.
pub(crate) struct SnapshotSource {
    /// The application's repository the snapshot was uploaded for.
    url: String,
    /// The snapshot's own repository, at the snapshot commit.
    pub(crate) repository: GitRepository,
}

impl SnapshotSource {
    /// Where to check `repository` out from: the snapshot for its own
    /// commit of the application's repository, which is how deployments pin
    /// `"self"` sources, and `repository` itself otherwise.
    pub(crate) fn checkout<'a>(
        source: Option<&'a Self>,
        repository: &'a GitRepository,
    ) -> &'a GitRepository {
        match source {
            Some(source)
                if repository.url == source.url
                    && repository.commit == source.repository.commit =>
            {
                &source.repository
            }
            _ => repository,
        }
    }
}

/// What a bundle's header says it carries: one snapshot ref and the commits
/// it needs. Its pack is checked by Git.
#[derive(Debug, Eq, PartialEq)]
struct BundleHeader {
    /// The ref, under `refs/`.
    reference: String,
    /// The commit the ref points at.
    commit: String,
    /// The commits it needs, at most [`Self::PREREQUISITES`].
    prerequisites: Vec<String>,
}

impl BundleHeader {
    /// Most prerequisites a bundle may need. A thin bundle needs its base,
    /// and one more commit for each merge of a branch forked before it.
    const PREREQUISITES: usize = 64;

    /// Reads and checks the header of the bundle at `path`:
    ///
    /// ```text
    /// # v2 git bundle                       (or v3, with only @object-format=sha1)
    /// -<commit> <subject>                   prerequisites, at most 64
    /// <commit> refs/heads/snapshot          exactly one ref
    /// (blank line, then the pack)
    /// ```
    async fn read(path: &Path) -> Result<Self, SnapshotError> {
        let file = tokio::fs::File::open(path)
            .await
            .map_err(SnapshotError::internal)?;
        let mut lines =
            tokio::io::AsyncReadExt::take(tokio::io::BufReader::new(file), Snapshots::HEADER_LIMIT)
                .lines();
        let mut header = Vec::new();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) if line.is_empty() => break,
                Ok(Some(line)) => header.push(line),
                Ok(None) | Err(_) => return Err(SnapshotError::Invalid("its header is malformed")),
            }
        }
        Self::parse(&header).ok_or(SnapshotError::Invalid("its header is malformed"))
    }

    /// Parses header lines, `None` unless they describe one snapshot.
    fn parse(lines: &[String]) -> Option<Self> {
        let (signature, lines) = lines.split_first()?;
        let v3 = match signature.as_str() {
            "# v2 git bundle" => false,
            "# v3 git bundle" => true,
            _ => return None,
        };
        let (mut prerequisites, mut reference) = (BTreeSet::new(), None);
        for line in lines {
            if let Some(capability) = line.strip_prefix('@') {
                if !v3 || capability != "object-format=sha1" {
                    return None;
                }
            } else if let Some(prerequisite) = line.strip_prefix('-') {
                let commit = prerequisite.split(' ').next()?;
                if !valid_git_commit(commit)
                    || !prerequisites.insert(commit.to_owned())
                    || prerequisites.len() > Self::PREREQUISITES
                {
                    return None;
                }
            } else {
                let (commit, name) = line.split_once(' ')?;
                if !valid_git_commit(commit)
                    || !Self::valid_reference(name)
                    || reference.replace((name, commit)).is_some()
                {
                    return None;
                }
            }
        }
        let (reference, commit) = reference?;
        Some(Self {
            reference: reference.to_owned(),
            commit: commit.to_owned(),
            prerequisites: prerequisites.into_iter().collect(),
        })
    }

    /// Whether `name` is a plain ref under `refs/`, safe in a refspec.
    fn valid_reference(name: &str) -> bool {
        name.strip_prefix("refs/").is_some_and(|rest| {
            !rest.is_empty()
                && rest.split('/').all(|part| {
                    !part.is_empty()
                        && !part.starts_with('.')
                        && part
                            .rsplit_once('.')
                            .is_none_or(|(_, suffix)| suffix != "lock")
                        && part
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
                })
                && !rest.contains("..")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `script` in `directory` with a fixed Git identity, returning its
    /// trimmed output.
    fn sh(directory: &Path, script: &str) -> String {
        let output = std::process::Command::new("sh")
            .args(["-ec", script])
            .current_dir(directory)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    /// A repository `remote` with two commits, and a clone whose snapshot
    /// commit on top of them adds `local.txt`. Returns the base and the
    /// snapshot commits.
    fn fixture(root: &Path) -> (String, String) {
        sh(
            root,
            "git init -q -b main remote
             cd remote && echo one > app.toml && git add . && git commit -qm one
             echo two > app.toml && git commit -qam two && cd ..
             git clone -q remote work && cd work && echo local > local.txt && git add .
             git commit -qm snapshot && git update-ref refs/heads/snapshot HEAD",
        );
        (
            sh(&root.join("remote"), "git rev-parse HEAD"),
            sh(&root.join("work"), "git rev-parse HEAD"),
        )
    }

    /// The bytes of a bundle of `work`'s snapshot ref, with `arguments`.
    fn bundle(root: &Path, arguments: &str) -> Vec<u8> {
        sh(
            &root.join("work"),
            &format!("git bundle create -q ../x.bundle refs/heads/snapshot {arguments}"),
        );
        std::fs::read(root.join("x.bundle")).unwrap()
    }

    fn body(bytes: &[u8]) -> impl Stream<Item = Result<Bytes, std::io::Error>> {
        futures_util::stream::iter(
            bytes
                .chunks(1000)
                .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                .collect::<Vec<_>>(),
        )
    }

    /// Checkouts within `max_bytes`; only their limit is used.
    fn checkouts(max_bytes: u64) -> Checkouts {
        Checkouts::new(
            PathBuf::from("unused"),
            std::num::NonZeroU64::new(max_bytes).unwrap(),
        )
    }

    /// Snapshots kept in `root/snapshots`.
    async fn snapshots(root: &Path) -> Snapshots {
        Snapshots::open(root.join("snapshots")).await.unwrap()
    }

    async fn receive<'a>(
        snapshots: &'a Snapshots,
        bytes: Vec<u8>,
        limit: u64,
        url: &Path,
    ) -> Result<Received<'a>, SnapshotError> {
        let url = url.display().to_string();
        snapshots
            .receive(body(&bytes), (None, limit), &url, &checkouts(1 << 30))
            .await
    }

    /// A thin bundle needs its base, which is fetched from the repository;
    /// a complete one needs nothing. Either is checked out from its own
    /// repository once kept.
    #[tokio::test]
    async fn receives_thin_and_complete_bundles() {
        let root = tempfile::tempdir().unwrap();
        let (base, commit) = fixture(root.path());
        let remote = root.path().join("remote");
        let snapshots = snapshots(root.path()).await;
        let thin = bundle(root.path(), &format!("^{base}"));
        let complete = bundle(root.path(), "");
        assert!(thin.len() < complete.len());
        for (bytes, expected) in [(thin, Some(base)), (complete, None)] {
            let received = receive(&snapshots, bytes, 1 << 20, &remote).await.unwrap();
            assert_eq!((&received.commit, &received.base), (&commit, &expected));
            let commit = received.commit.clone();
            let kept = snapshots
                .keep(received, async { Ok::<_, SnapshotError>(()) })
                .await;
            assert!(kept.is_ok());
            assert!(snapshots.path(&commit).exists());
        }
        let source = snapshots.source(&remote.display().to_string(), &commit);
        let checkouts = Checkouts::open(root.path().join("checkouts"))
            .await
            .unwrap();
        let checkout = checkouts.checkout(&source.repository, None).await.unwrap();
        assert_eq!(checkout.commit, commit);
        assert_eq!(
            tokio::fs::read_to_string(checkout.path("local.txt").await.unwrap())
                .await
                .unwrap(),
            "local\n"
        );
        // Only the snapshot of the same repository and commit is replaced.
        let pinned = GitRepository {
            url: remote.display().to_string(),
            branch: "main".into(),
            commit: Some(commit.clone()),
        };
        let other = GitRepository {
            url: "https://example.com/other.git".into(),
            ..pinned.clone()
        };
        assert_eq!(
            SnapshotSource::checkout(Some(&source), &pinned),
            &source.repository
        );
        assert_eq!(SnapshotSource::checkout(Some(&source), &other), &other);
        // Nothing staged is left behind.
        let staged = std::fs::read_dir(root.path().join("snapshots/staging")).unwrap();
        assert_eq!(staged.count(), 0);
    }

    /// Oversized, malformed, and corrupted bundles, and bases the repository
    /// lacks, are refused with their own errors, and a snapshot whose
    /// deployment is refused is not kept.
    #[tokio::test]
    async fn refuses_oversized_invalid_and_unfetchable_bundles() {
        let root = tempfile::tempdir().unwrap();
        let (base, _) = fixture(root.path());
        let remote = root.path().join("remote");
        let snapshots = snapshots(root.path()).await;
        let thin = bundle(root.path(), &format!("^{base}"));
        let length = thin.len() as u64;

        // Announced, and only found while streaming.
        let announced = snapshots
            .receive(
                body(&thin),
                (Some(length), length - 1),
                "unused",
                &checkouts(1 << 30),
            )
            .await;
        assert!(matches!(announced, Err(SnapshotError::TooLarge { .. })));
        let streamed = receive(&snapshots, thin.clone(), length - 1, &remote).await;
        assert!(matches!(streamed, Err(SnapshotError::TooLarge { .. })));

        let garbage = receive(&snapshots, b"not a bundle\n\n".to_vec(), 1 << 20, &remote).await;
        assert!(matches!(garbage, Err(SnapshotError::Invalid(_))));
        // `git bundle verify` accepts a truncated pack; unbundling does not.
        let truncated = receive(
            &snapshots,
            thin[..thin.len() - 40].to_vec(),
            1 << 20,
            &remote,
        );
        assert!(matches!(truncated.await, Err(SnapshotError::Invalid(_))));

        // A snapshot whose acceptance is refused is not kept.
        let received = receive(&snapshots, thin.clone(), 1 << 20, &remote)
            .await
            .unwrap();
        let commit = received.commit.clone();
        let refused = snapshots
            .keep(received, async {
                Err::<(), _>(SnapshotError::Invalid("refused"))
            })
            .await;
        assert!(refused.is_err());
        assert!(!snapshots.path(&commit).exists());

        // Its files must fit a checkout: `local.txt` and `app.toml` take 10 bytes.
        let url = remote.display().to_string();
        let expands = snapshots
            .receive(body(&thin), (None, 1 << 20), &url, &checkouts(9))
            .await;
        assert!(matches!(
            expands,
            Err(SnapshotError::TreeTooLarge { limit: 9 })
        ));

        sh(root.path(), "git init -q -b main empty");
        let unfetchable = receive(&snapshots, thin, 1 << 20, &root.path().join("empty")).await;
        assert!(
            matches!(unfetchable, Err(SnapshotError::BaseUnavailable { prerequisites }) if prerequisites == [base])
        );
    }

    /// A thin bundle of a history that merged a branch forked before its
    /// base needs the base and that fork point, and its base is the one
    /// they share. Prerequisites that are not one history are refused.
    #[tokio::test]
    async fn thin_bundles_of_merges_need_their_base_and_its_ancestors() {
        let root = tempfile::tempdir().unwrap();
        let base = sh(
            root.path(),
            "git init -q -b main remote && cd remote
             echo a > a && git add . && git commit -qm a && git switch -qc side
             echo s > s && git add . && git commit -qm s && git switch -q main
             echo b > a && git commit -qam b && git rev-parse HEAD",
        );
        let commit = sh(
            root.path(),
            "git clone -q remote work && cd work && git merge -q --no-edit origin/side
             git update-ref refs/heads/snapshot HEAD && git rev-parse HEAD",
        );
        let remote = root.path().join("remote");
        let snapshots = snapshots(root.path()).await;
        let merged = bundle(root.path(), &format!("^{base}"));
        let received = receive(&snapshots, merged, 1 << 20, &remote).await.unwrap();
        assert_eq!((received.commit, received.base), (commit, Some(base)));
        // Excluding both parents of the merge leaves two unrelated bases.
        let unrelated = bundle(root.path(), "^HEAD^1 ^HEAD^2");
        let refused = receive(&snapshots, unrelated, 1 << 20, &remote).await;
        assert!(matches!(refused, Err(SnapshotError::Invalid(_))));
    }

    /// A received upload holds its permit until it is kept, so uploads
    /// waiting to be accepted never pile up on disk.
    #[tokio::test]
    async fn received_uploads_hold_their_permit_until_kept() {
        let root = tempfile::tempdir().unwrap();
        let (base, _) = fixture(root.path());
        let remote = root.path().join("remote");
        let snapshots = snapshots(root.path()).await;
        let thin = bundle(root.path(), &format!("^{base}"));
        let received = receive(&snapshots, thin, 1 << 20, &remote).await.unwrap();
        assert_eq!(
            snapshots.receiving.available_permits(),
            Snapshots::RECEIVING - 1
        );
        let kept = snapshots
            .keep(received, async { Ok::<_, SnapshotError>(()) })
            .await;
        assert!(kept.is_ok());
        assert_eq!(
            snapshots.receiving.available_permits(),
            Snapshots::RECEIVING
        );
    }

    /// A body that stops sending gives up its permit.
    #[tokio::test(start_paused = true)]
    async fn stalled_uploads_time_out() {
        let root = tempfile::tempdir().unwrap();
        let snapshots = snapshots(root.path()).await;
        let stalled = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();
        let received = snapshots
            .receive(stalled, (None, 1 << 20), "unused", &checkouts(1 << 30))
            .await;
        assert!(matches!(received, Err(SnapshotError::TimedOut)));
        assert_eq!(
            snapshots.receiving.available_permits(),
            Snapshots::RECEIVING
        );
    }

    #[test]
    fn headers_name_one_snapshot_and_its_prerequisites() {
        let (a, b) = ("a".repeat(40), "b".repeat(40));
        let parse = |lines: &[String]| BundleHeader::parse(lines);
        let header = |lines: &[&str]| {
            lines
                .iter()
                .map(|line| (*line).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            parse(&header(&[
                "# v3 git bundle",
                "@object-format=sha1",
                &format!("-{b} subject"),
                &format!("-{a}"),
                &format!("{b} refs/heads/snapshot"),
            ])),
            Some(BundleHeader {
                reference: "refs/heads/snapshot".into(),
                commit: b.clone(),
                prerequisites: vec![a.clone(), b.clone()],
            })
        );
        let mut many = vec!["# v2 git bundle".to_owned()];
        many.extend((0..=BundleHeader::PREREQUISITES).map(|n| format!("-{n:040x}")));
        many.push(format!("{b} refs/heads/snapshot"));
        assert_eq!(parse(&many), None);
        for invalid in [
            header(&["# v4 git bundle", &format!("{b} refs/heads/snapshot")]),
            header(&[
                "# v2 git bundle",
                "@object-format=sha1",
                &format!("{b} refs/heads/x"),
            ]),
            header(&[
                "# v3 git bundle",
                "@filter=blob:none",
                &format!("{b} refs/heads/x"),
            ]),
            header(&[
                "# v2 git bundle",
                &format!("{b} refs/heads/x"),
                &format!("{a} refs/heads/y"),
            ]),
            header(&[
                "# v2 git bundle",
                &format!("-{a}"),
                &format!("-{a}"),
                &format!("{b} refs/x"),
            ]),
            header(&["# v2 git bundle", &format!("{b} HEAD")]),
            header(&["# v2 git bundle", &format!("{b} refs/heads/a:refs/heads/b")]),
            header(&["# v2 git bundle", &format!("{b} refs/heads/../x")]),
            header(&["# v2 git bundle", "1234 refs/heads/x"]),
            header(&["# v2 git bundle"]),
        ] {
            assert_eq!(parse(&invalid), None, "{invalid:?}");
        }
    }

    /// Pruning keeps exactly the snapshots still needed.
    #[tokio::test]
    async fn prune_keeps_only_needed_snapshots() {
        let root = tempfile::tempdir().unwrap();
        let snapshots = snapshots(root.path()).await;
        let (kept, removed) = ("a".repeat(40), "b".repeat(40));
        for commit in [&kept, &removed] {
            std::fs::create_dir(snapshots.path(commit)).unwrap();
        }
        let needed = vec![kept.clone()];
        snapshots.prune(async { Ok(needed) }).await.unwrap();
        assert!(snapshots.path(&kept).exists());
        assert!(!snapshots.path(&removed).exists());
        assert!(root.path().join("snapshots/staging").exists());
    }
}
