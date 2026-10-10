//! Snapshots of local worktrees, uploaded by `preview deploy --local` as Git
//! bundles. Bundles are untrusted: each is streamed to disk within
//! `[previews] max_upload_bytes`, then received into a new private
//! repository of its own, never one shared with checkouts or other uploads:
//!
//! 1. its header must name one snapshot ref and at most one prerequisite,
//!    the base, which is fetched from the application's repository;
//! 2. `git bundle verify` checks it applies, and unbundling checks every
//!    object (`transfer.fsckObjects`);
//! 3. the snapshot commit must descend from the base.
//!
//! The repository then moves to `<commit>.git`, from which deployments of
//! that snapshot read their manifest and build `"self"` sources. Snapshots
//! no preview's latest deployment uses are removed (see [`Snapshots::prune`]).
use crate::command::LoggedCommand;
use crate::git::Checkout;
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
    /// The bundle is larger than `[previews] max_upload_bytes`.
    #[error("the snapshot bundle is larger than the {limit}-byte upload limit")]
    TooLarge {
        /// The limit, in bytes.
        limit: u64,
    },
    /// The upload is not a valid bundle of one snapshot commit.
    #[error("the upload is not a valid Git bundle of one snapshot commit: {0}")]
    Invalid(&'static str),
    /// The base the bundle needs could not be fetched.
    #[error("base commit {base} could not be fetched from the application's repository")]
    BaseUnavailable {
        /// The bundle's prerequisite.
        base: String,
    },
    /// The upload could not be read or stored.
    #[error("the snapshot could not be received")]
    Internal(#[source] anyhow::Error),
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
pub(crate) struct Received {
    /// The snapshot commit.
    pub(crate) commit: String,
    /// The prerequisite it was uploaded against; `None` for a complete bundle.
    pub(crate) base: Option<String>,
    /// Holds the bundle and the repository; removed when dropped.
    staging: tempfile::TempDir,
}

impl Snapshots {
    /// Most bytes a bundle header may take.
    const HEADER_LIMIT: u64 = 64 * 1024;
    /// Uploads received at once, bounding the disk a burst of them takes.
    const RECEIVING: usize = 2;

    /// Opens `root`, removing uploads a previous process left half-received.
    pub(crate) async fn open(root: PathBuf) -> std::io::Result<Self> {
        let staging = root.join("staging");
        match tokio::fs::remove_dir_all(&staging).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        tokio::fs::create_dir_all(&staging).await?;
        Ok(Self {
            root,
            lock: tokio::sync::Mutex::new(()),
            receiving: tokio::sync::Semaphore::new(Self::RECEIVING),
        })
    }

    /// Streams `body`, which announced `length` bytes, to a staging file of
    /// at most `limit` bytes, then receives it as described in the module
    /// documentation, fetching its base from `url`. At most
    /// [`Self::RECEIVING`] uploads are received at once; others wait.
    ///
    /// # Errors
    /// Returns [`SnapshotError`] for an oversized, malformed, or unverifiable
    /// bundle, an unavailable base, or a storage failure.
    pub(crate) async fn receive<E: std::error::Error + Send + Sync + 'static>(
        &self,
        body: impl Stream<Item = Result<Bytes, E>>,
        length: Option<u64>,
        limit: u64,
        url: &str,
    ) -> Result<Received, SnapshotError> {
        if length.is_some_and(|length| length > limit) {
            return Err(SnapshotError::TooLarge { limit });
        }
        let _receiving = self
            .receiving
            .acquire()
            .await
            .map_err(|error| SnapshotError::Internal(error.into()))?;
        let staging = tempfile::Builder::new()
            .prefix("upload-")
            .tempdir_in(self.root.join("staging"))
            .map_err(|error| SnapshotError::Internal(error.into()))?;
        let bundle = staging.path().join("snapshot.bundle");
        Self::store(body, &bundle, limit).await?;
        let header = BundleHeader::read(&bundle).await?;
        let repository = Staged(staging.path().join("repository.git"));
        repository.init().await?;
        if let Some(base) = &header.base {
            repository.fetch_base(url, base).await?;
        }
        repository.unbundle(&bundle, &header).await?;
        Ok(Received {
            commit: header.commit,
            base: header.base,
            staging,
        })
    }

    /// Streams `body` to `path`, refusing more than `limit` bytes.
    async fn store<E: std::error::Error + Send + Sync + 'static>(
        body: impl Stream<Item = Result<Bytes, E>>,
        path: &Path,
        limit: u64,
    ) -> Result<(), SnapshotError> {
        let internal = |error: std::io::Error| SnapshotError::Internal(error.into());
        let mut file = tokio::fs::File::create(path).await.map_err(internal)?;
        let mut written = 0_u64;
        let mut body = std::pin::pin!(body);
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|error| SnapshotError::Internal(error.into()))?;
            written = written.saturating_add(chunk.len() as u64);
            if written > limit {
                return Err(SnapshotError::TooLarge { limit });
            }
            file.write_all(&chunk).await.map_err(internal)?;
        }
        file.flush().await.map_err(internal)
    }

    /// Moves `received` into place, unless that commit already is, and runs
    /// `accept`, which deploys it, before any [`Self::prune`] can remove it.
    pub(crate) async fn keep<T, E: From<SnapshotError>>(
        &self,
        received: Received,
        accept: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        let _lock = self.lock.lock().await;
        let path = self.path(&received.commit);
        if !tokio::fs::try_exists(&path)
            .await
            .map_err(|error| SnapshotError::Internal(error.into()))?
        {
            tokio::fs::rename(received.staging.path().join("repository.git"), &path)
                .await
                .map_err(|error| SnapshotError::Internal(error.into()))?;
        }
        accept.await
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
    /// Longest the base fetch may take; its process group is then killed.
    const FETCH_TIMEOUT: Duration = Duration::from_secs(300);

    /// Runs Git in the repository.
    async fn git(&self, operation: &'static str, arguments: &[&str]) -> anyhow::Result<Vec<u8>> {
        let mut command = Checkout::command();
        command.arg("-C").arg(&self.0).args(arguments);
        LoggedCommand::output(&mut command, operation, 4096).await
    }

    /// Creates the bare repository.
    async fn init(&self) -> Result<(), SnapshotError> {
        let mut init = Checkout::command();
        init.args([
            "init",
            "--quiet",
            "--bare",
            "--initial-branch",
            SNAPSHOT_BRANCH,
        ])
        .arg(&self.0);
        LoggedCommand::output(&mut init, "create snapshot repository", 4096)
            .await
            .map(drop)
            .map_err(SnapshotError::Internal)
    }

    /// Fetches `base` and its history from `url`, under a ref so that
    /// `git bundle verify` finds it.
    async fn fetch_base(&self, url: &str, base: &str) -> Result<(), SnapshotError> {
        let refspec = format!("{base}:refs/base");
        let arguments = [
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--",
            url,
            &refspec,
        ];
        let fetched = tokio::time::timeout(
            Self::FETCH_TIMEOUT,
            self.git("fetch snapshot base", &arguments),
        )
        .await;
        if matches!(fetched, Ok(Ok(_))) {
            return Ok(());
        }
        // Git may echo the URL, credentials included, so details stay in logs.
        tracing::warn!(?fetched, "snapshot base fetch failed");
        Err(SnapshotError::BaseUnavailable {
            base: base.to_owned(),
        })
    }

    /// Verifies `bundle` against the repository, unbundles its snapshot ref
    /// with every object checked, and checks that `header`'s commit is what
    /// it unbundled and descends from its base.
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
        let unbundle = [
            "-c",
            "transfer.fsckObjects=true",
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
        if let Some(base) = &header.base {
            self.git(
                "check snapshot base",
                &["merge-base", "--is-ancestor", base, &header.commit],
            )
            .await
            .map_err(invalid("the snapshot does not descend from its base"))?;
        }
        Ok(())
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

/// What a bundle's header says it carries: one snapshot ref and at most one
/// prerequisite. Its pack is checked by Git.
#[derive(Debug, Eq, PartialEq)]
struct BundleHeader {
    /// The ref, under `refs/`.
    reference: String,
    /// The commit the ref points at.
    commit: String,
    /// The prerequisite commit.
    base: Option<String>,
}

impl BundleHeader {
    /// Reads and checks the header of the bundle at `path`:
    ///
    /// ```text
    /// # v2 git bundle                       (or v3, with only @object-format=sha1)
    /// -<base> <subject>                     at most one prerequisite
    /// <commit> refs/heads/snapshot          exactly one ref
    /// (blank line, then the pack)
    /// ```
    async fn read(path: &Path) -> Result<Self, SnapshotError> {
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|error| SnapshotError::Internal(error.into()))?;
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
        let (mut base, mut reference) = (None, None);
        for line in lines {
            if let Some(capability) = line.strip_prefix('@') {
                if !v3 || capability != "object-format=sha1" {
                    return None;
                }
            } else if let Some(prerequisite) = line.strip_prefix('-') {
                let commit = prerequisite.split(' ').next()?;
                if !valid_git_commit(commit) || base.replace(commit.to_owned()).is_some() {
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
            base,
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

    async fn receive(
        snapshots: &Snapshots,
        bytes: Vec<u8>,
        limit: u64,
        url: &Path,
    ) -> Result<Received, SnapshotError> {
        let url = url.display().to_string();
        snapshots.receive(body(&bytes), None, limit, &url).await
    }

    /// A thin bundle needs its base, which is fetched from the repository;
    /// a complete one needs nothing. Either is checked out from its own
    /// repository once kept.
    #[tokio::test]
    async fn receives_thin_and_complete_bundles() {
        let root = tempfile::tempdir().unwrap();
        let (base, commit) = fixture(root.path());
        let remote = root.path().join("remote");
        let snapshots = Snapshots::open(root.path().join("snapshots"))
            .await
            .unwrap();
        let thin = bundle(root.path(), &format!("^{base}"));
        let complete = bundle(root.path(), "");
        assert!(thin.len() < complete.len());
        for (bytes, expected) in [(thin, Some(base)), (complete, None)] {
            let received = receive(&snapshots, bytes, 1 << 20, &remote).await.unwrap();
            assert_eq!((&received.commit, &received.base), (&commit, &expected));
            let kept = snapshots
                .keep(received, async { Ok::<_, SnapshotError>(()) })
                .await;
            assert!(kept.is_ok());
        }
        let source = snapshots.source(&remote.display().to_string(), &commit);
        let checkout = Checkout::clone(&source.repository).await.unwrap();
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
    /// lacks, are refused with their own errors.
    #[tokio::test]
    async fn refuses_oversized_invalid_and_unfetchable_bundles() {
        let root = tempfile::tempdir().unwrap();
        let (base, _) = fixture(root.path());
        let remote = root.path().join("remote");
        let snapshots = Snapshots::open(root.path().join("snapshots"))
            .await
            .unwrap();
        let thin = bundle(root.path(), &format!("^{base}"));
        let length = thin.len() as u64;

        // Announced, and only found while streaming.
        let announced = snapshots
            .receive(body(&thin), Some(length), length - 1, "unused")
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

        sh(root.path(), "git init -q -b main empty");
        let unfetchable = receive(&snapshots, thin, 1 << 20, &root.path().join("empty")).await;
        assert!(
            matches!(unfetchable, Err(SnapshotError::BaseUnavailable { base: missing }) if missing == base)
        );
    }

    #[test]
    fn headers_name_one_snapshot_and_at_most_one_base() {
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
                &format!("-{a} subject"),
                &format!("{b} refs/heads/snapshot"),
            ])),
            Some(BundleHeader {
                reference: "refs/heads/snapshot".into(),
                commit: b.clone(),
                base: Some(a.clone()),
            })
        );
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
                &format!("-{b}"),
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
        let snapshots = Snapshots::open(root.path().join("snapshots"))
            .await
            .unwrap();
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
