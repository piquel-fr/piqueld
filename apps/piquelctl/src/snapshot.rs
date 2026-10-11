//! Snapshots of the current Git worktree for `preview deploy --local`.
//!
//! A snapshot is a commit of the worktree's tracked and untracked files,
//! except ignored ones, on top of `HEAD`, uploaded as a Git bundle. It never
//! changes the user's repository: Git writes the snapshot's index, objects,
//! and ref into a private temporary directory instead.
//!
//! 1. `git add -A` stages the worktree into a copy of its index
//!    (`GIT_INDEX_FILE`), writing new objects into a temporary object
//!    directory that reads the repository's own as an alternate.
//! 2. `git write-tree` and `git commit-tree -p HEAD` make the commit.
//! 3. A temporary bare repository sharing both object directories names it
//!    with a ref, and bundles it, thin against a base the application's
//!    repository has, or complete.
use crate::error::{CliError, ErrorKind, Result};
use piqueld_client::LocalSnapshot;
use std::path::{Path, PathBuf};
use std::process::Output;

/// Variables that would point Git commands at another repository, index,
/// or object directory than the ones each command names.
const REPOSITORY_VARIABLES: [&str; 7] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
];

/// The Git worktree containing the current directory.
pub(crate) struct Worktree {
    /// Its top-level directory.
    root: PathBuf,
    /// Its index file.
    index: PathBuf,
    /// The repository's object directory, shared by its worktrees.
    objects: PathBuf,
}

/// A snapshot commit, written into its own temporary directory.
pub(crate) struct Snapshot<'a> {
    worktree: &'a Worktree,
    /// Holds the snapshot's index, objects, bare repository, and bundles.
    directory: tempfile::TempDir,
    /// The snapshot commit.
    pub(crate) commit: String,
}

/// A bundle of a snapshot, ready to upload.
pub(crate) struct Bundle {
    pub(crate) path: PathBuf,
    /// Its size in bytes.
    pub(crate) length: u64,
    /// The commit it carries and the base it needs.
    pub(crate) snapshot: LocalSnapshot,
}

impl Worktree {
    /// Finds the worktree containing the current directory.
    pub(crate) async fn current() -> Result<Self> {
        Self::at(Path::new(".")).await
    }

    /// Finds the worktree containing `directory`.
    pub(crate) async fn at(directory: &Path) -> Result<Self> {
        let output = Git::command(directory)
            .args([
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--git-path",
                "index",
                "--git-path",
                "objects",
            ])
            .output()
            .await;
        let output = Git::checked(output, "find the Git worktree").map_err(|_| {
            CliError::new(
                ErrorKind::Input,
                "--local snapshots the Git worktree of the current directory, but there is none",
            )
        })?;
        let mut paths = output.lines().map(PathBuf::from);
        let (Some(root), Some(index), Some(objects)) = (paths.next(), paths.next(), paths.next())
        else {
            return Err(Git::failure("find the Git worktree", "unexpected output"));
        };
        Ok(Self {
            root,
            index,
            objects,
        })
    }

    /// The branch `HEAD` is on; `None` when it is detached.
    pub(crate) async fn branch(&self) -> Result<Option<String>> {
        let output = Git::command(&self.root)
            .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
            .output()
            .await;
        match output {
            Ok(output) if output.status.code() == Some(1) => Ok(None),
            output => Git::checked(output, "read the current branch").map(Some),
        }
    }

    /// Commits the worktree's tracked and untracked files, except ignored
    /// ones, on top of `HEAD`, without changing the repository. Submodules,
    /// nested repositories, and what [`Self::check_supported`] refuses are
    /// refused.
    pub(crate) async fn snapshot(&self, branch: &str) -> Result<Snapshot<'_>> {
        self.check_supported().await?;
        let head = Git::checked(
            Git::command(&self.root)
                .args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
                .output()
                .await,
            "read HEAD",
        )
        .map_err(|_| {
            CliError::new(
                ErrorKind::Input,
                "HEAD has no commit yet; commit once first",
            )
        })?;
        let directory = tempfile::Builder::new()
            .prefix("piquelctl-snapshot-")
            .tempdir()
            .map_err(|error| Git::failure("create a temporary directory", error))?;
        let snapshot = Snapshot {
            worktree: self,
            directory,
            commit: String::new(),
        };
        // Git refuses an object directory that does not exist.
        tokio::fs::create_dir(snapshot.objects())
            .await
            .map_err(|error| Git::failure("create the object directory", error))?;
        let index = snapshot.directory.path().join("index");
        match tokio::fs::copy(&self.index, &index).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(Git::failure("copy the index", error));
            }
            _ => {}
        }
        let staged = |arguments: &[&str]| {
            let mut command = Git::command(&self.root);
            command
                .env("GIT_INDEX_FILE", &index)
                .env("GIT_OBJECT_DIRECTORY", snapshot.objects())
                .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &self.objects)
                .args(arguments);
            command
        };
        Git::checked(
            staged(&["add", "--all"]).output().await,
            "stage the worktree",
        )?;
        let tree = Git::checked(staged(&["write-tree"]).output().await, "write the tree")?;
        let submodules = Git::checked(
            staged(&["ls-files", "--stage", "-z"]).output().await,
            "list staged files",
        )?
        .split('\0')
        .filter_map(|entry| entry.strip_prefix("160000 "))
        .filter_map(|entry| entry.split_once('\t').map(|(_, path)| path.to_owned()))
        .collect::<Vec<_>>();
        if !submodules.is_empty() {
            return Err(CliError::new(
                ErrorKind::Input,
                format!(
                    "--local does not support submodules or nested repositories: {}",
                    submodules.join(", ")
                ),
            ));
        }
        let message = format!("piquelctl snapshot of {branch}");
        let commit = Git::checked(
            staged(&["commit-tree", &tree, "-p", &head, "-m", &message])
                .env("GIT_AUTHOR_NAME", "piquelctl")
                .env("GIT_AUTHOR_EMAIL", "snapshot@piquelctl.invalid")
                .env("GIT_COMMITTER_NAME", "piquelctl")
                .env("GIT_COMMITTER_EMAIL", "snapshot@piquelctl.invalid")
                .output()
                .await,
            "commit the snapshot",
        )?;
        Ok(Snapshot { commit, ..snapshot })
    }

    /// Refuses what a snapshot can't take without writing to the
    /// repository or the daemon can't receive: object formats other than
    /// SHA-1; partial clones, which lack objects a bundle needs and fetch
    /// them into the repository on demand; and files staging would pass
    /// through a configured filter driver, such as Git LFS's, which store
    /// their own data in the repository.
    async fn check_supported(&self) -> Result<()> {
        let format = Git::checked(
            Git::command(&self.root)
                .args(["rev-parse", "--show-object-format"])
                .output()
                .await,
            "read the object format",
        )?;
        if format != "sha1" {
            return Err(CliError::new(
                ErrorKind::Input,
                format!("--local supports SHA-1 repositories only, not {format}"),
            ));
        }
        // Older Git marks a partial clone with `extensions.partialClone`,
        // newer Git with its remote's `promisor` setting.
        let partial = Git::command(&self.root)
            .args([
                "config",
                "--name-only",
                "--get-regexp",
                r"^(extensions\.partialclone|remote\..*\.promisor)$",
            ])
            .output()
            .await;
        // Exit code 1: not a partial clone.
        match partial {
            Ok(output) if output.status.code() == Some(1) => {}
            output => {
                Git::checked(output, "read whether this is a partial clone")?;
                return Err(CliError::new(
                    ErrorKind::Input,
                    "--local does not support partial clones; clone without --filter",
                ));
            }
        }
        let configured = Git::command(&self.root)
            .args([
                "config",
                "--name-only",
                "--get-regexp",
                r"^filter\..*\.(clean|process)$",
            ])
            .output()
            .await;
        // Exit code 1: no filter driver is configured.
        let drivers = match configured {
            Ok(output) if output.status.code() == Some(1) => return Ok(()),
            output => Git::checked(output, "read the filter drivers")?,
        };
        let drivers = drivers
            .lines()
            .filter_map(|key| key.strip_prefix("filter.")?.rsplit_once('.'))
            .map(|(driver, _)| driver.to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        let files = Git::checked(
            Git::command(&self.root)
                .args([
                    "ls-files",
                    "-z",
                    "--cached",
                    "--others",
                    "--exclude-standard",
                ])
                .output()
                .await,
            "list the worktree's files",
        )?;
        let mut check = Git::command(&self.root);
        check
            .args(["check-attr", "--stdin", "-z", "filter"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let attributes = Git::checked(Git::with_input(check, files).await, "read the filters")?;
        // `<path>\0filter\0<driver>\0`, for every file.
        let filtered = attributes
            .split('\0')
            .collect::<Vec<_>>()
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|[_, _, driver]| drivers.contains(*driver))
            .map(|[path, _, _]| (*path).to_owned())
            .collect::<Vec<_>>();
        if filtered.is_empty() {
            return Ok(());
        }
        Err(CliError::new(
            ErrorKind::Input,
            format!(
                "--local does not support files passed through Git filters such as LFS: {}",
                filtered.join(", ")
            ),
        ))
    }

    /// The nearest ancestor of `HEAD` that a remote-tracking branch has,
    /// which the application's repository most likely has too.
    async fn base(&self) -> Result<Option<String>> {
        let remotes = Git::checked(
            Git::command(&self.root)
                .args(["for-each-ref", "--format=%(objectname)", "refs/remotes"])
                .output()
                .await,
            "list remote-tracking branches",
        )?;
        let mut remotes = remotes.lines().collect::<Vec<_>>();
        remotes.sort_unstable();
        remotes.dedup();
        if remotes.is_empty() {
            return Ok(None);
        }
        let output = Git::command(&self.root)
            .args(["merge-base", "HEAD"])
            .args(remotes)
            .output()
            .await;
        match output {
            // No common ancestor.
            Ok(output) if output.status.code() == Some(1) => Ok(None),
            output => Git::checked(output, "find the base commit").map(Some),
        }
    }
}

impl Bundle {
    /// What is being uploaded, for progress.
    ///
    /// ```text
    /// uploading 12 KiB against 0123456789ab
    /// uploading 3 MiB with its whole history
    /// ```
    pub(crate) fn describe(&self) -> String {
        let size = match self.length {
            length if length >= 1 << 20 => format!("{} MiB", length >> 20),
            length if length >= 1 << 10 => format!("{} KiB", length >> 10),
            length => format!("{length} bytes"),
        };
        match &self.snapshot.base {
            Some(base) => format!("uploading {size} against {}", &base[..base.len().min(12)]),
            None => format!("uploading {size} with its whole history"),
        }
    }
}

impl Snapshot<'_> {
    /// Bundles the snapshot, `thin` against the worktree's base when it has
    /// one (see [`Worktree::base`]), otherwise complete.
    pub(crate) async fn bundle(&self, thin: bool) -> Result<Bundle> {
        let base = if thin {
            self.worktree.base().await?
        } else {
            None
        };
        let repository = self.directory.path().join("bundle.git");
        if !repository.exists() {
            Git::checked(
                Git::command(self.directory.path())
                    .args(["init", "--quiet", "--bare", "--object-format=sha1"])
                    .arg(&repository)
                    .output()
                    .await,
                "create the bundle repository",
            )?;
            let alternates = format!(
                "{}\n{}\n",
                self.objects().display(),
                self.worktree.objects.display()
            );
            tokio::fs::write(repository.join("objects/info/alternates"), alternates)
                .await
                .map_err(|error| Git::failure("share the snapshot's objects", error))?;
            Git::checked(
                Git::command(&repository)
                    .args(["update-ref", "refs/heads/snapshot", &self.commit])
                    .output()
                    .await,
                "name the snapshot",
            )?;
        }
        let path = self.directory.path().join(if base.is_some() {
            "thin.bundle"
        } else {
            "complete.bundle"
        });
        let mut bundle = Git::command(&repository);
        bundle
            .args(["bundle", "create", "--quiet"])
            .arg(&path)
            .arg("refs/heads/snapshot");
        if let Some(base) = &base {
            bundle.arg(format!("^{base}"));
        }
        Git::checked(bundle.output().await, "create the bundle")?;
        let length = tokio::fs::metadata(&path)
            .await
            .map_err(|error| Git::failure("read the bundle", error))?
            .len();
        Ok(Bundle {
            path,
            length,
            snapshot: LocalSnapshot {
                commit: self.commit.clone(),
                base,
            },
        })
    }

    /// Where Git writes the snapshot's new objects.
    fn objects(&self) -> PathBuf {
        self.directory.path().join("objects")
    }
}

/// Runs the Git commands of a snapshot.
struct Git;

impl Git {
    /// Settings that would otherwise make staging write outside the
    /// snapshot's directory: hooks such as `post-index-change`, a split
    /// index's shared files beside the repository's index, and an fsmonitor
    /// daemon.
    const ISOLATED: [&str; 6] = [
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.splitIndex=false",
        "-c",
        "core.fsmonitor=false",
    ];

    /// A Git command run in `directory`, unaffected by variables naming
    /// another repository, never prompting, and with [`Self::ISOLATED`].
    fn command(directory: &Path) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("git");
        for variable in REPOSITORY_VARIABLES {
            command.env_remove(variable);
        }
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(Self::ISOLATED)
            .arg("-C")
            .arg(directory)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        command
    }

    /// Runs `command`, whose standard streams are piped, with `input` on
    /// its standard input.
    async fn with_input(
        mut command: tokio::process::Command,
        input: String,
    ) -> std::io::Result<Output> {
        let mut child = command.spawn()?;
        let mut stdin = child.stdin.take().ok_or(std::io::ErrorKind::BrokenPipe)?;
        // Written concurrently with reading the output, so neither pipe fills.
        let write =
            async move { tokio::io::AsyncWriteExt::write_all(&mut stdin, input.as_bytes()).await };
        let (written, output) = tokio::join!(write, child.wait_with_output());
        written?;
        output
    }

    /// The standard output of a successful command, without its final
    /// newline, or a failure to `action` with what Git printed. Nothing
    /// else is trimmed: NUL-separated output keeps every path intact.
    fn checked(output: std::io::Result<Output>, action: &str) -> Result<String> {
        let output = output.map_err(|error| Self::failure(action, error))?;
        if !output.status.success() {
            return Err(Self::failure(
                action,
                String::from_utf8_lossy(&output.stderr).trim(),
            ));
        }
        String::from_utf8(output.stdout)
            .map(|stdout| stdout.strip_suffix('\n').unwrap_or(&stdout).to_owned())
            .map_err(|error| Self::failure(action, error))
    }

    /// A failure to `action` while snapshotting.
    fn failure(action: &str, cause: impl std::fmt::Display) -> CliError {
        CliError::new(
            ErrorKind::General,
            format!("could not {action} for the snapshot: {cause}"),
        )
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

    /// Every file of the worktree and its Git directory, with its hash: a
    /// snapshot must leave refs, `HEAD`, the index, the stash, objects,
    /// configuration, and the working tree as they were, and run no hook.
    /// No Git command runs, since one could refresh the index itself.
    fn state(work: &Path) -> String {
        sh(
            work,
            "find . -type f -print0 | sort -z | xargs -0 sha256sum",
        )
    }

    /// A clone of a two-commit repository, with an unpushed commit, a
    /// modified, a staged, an untracked, and an ignored file. It splits its
    /// index and has a hook that runs whenever the index changes.
    fn fixture(root: &Path) -> PathBuf {
        sh(
            root,
            "git init -q -b main remote
             cd remote && echo one > app.toml && git add . && git commit -qm one
             echo two > app.toml && git commit -qam two && cd ..
             git clone -q remote work && cd work && git switch -qc feat/x
             git config core.splitIndex true
             printf '#!/bin/sh\ntouch hook-ran\n' > .git/hooks/post-index-change
             chmod +x .git/hooks/post-index-change
             echo ignored > .gitignore && git add . && git commit -qm unpushed
             echo changed > app.toml && echo staged > staged.txt && git add staged.txt
             echo untracked > untracked.txt && echo secret > ignored && rm hook-ran",
        );
        root.join("work")
    }

    #[tokio::test]
    async fn snapshots_include_untracked_files_and_leave_the_repository_untouched() {
        let root = tempfile::tempdir().unwrap();
        let work = fixture(root.path());
        let before = state(&work);
        let worktree = Worktree::at(&work).await.unwrap();
        assert_eq!(worktree.branch().await.unwrap().as_deref(), Some("feat/x"));
        let snapshot = worktree.snapshot("feat/x").await.unwrap();
        let thin = snapshot.bundle(true).await.unwrap();
        let complete = snapshot.bundle(false).await.unwrap();
        assert_eq!(state(&work), before);
        // Read from the snapshot's own repository: the worktree's lacks it.
        let snapshot_git = format!(
            "git --git-dir={}",
            snapshot.directory.path().join("bundle.git").display()
        );
        let files = sh(
            &work,
            &format!("{snapshot_git} ls-tree -r --name-only {}", snapshot.commit),
        );
        assert_eq!(files, ".gitignore\napp.toml\nstaged.txt\nuntracked.txt");
        let parent = sh(
            &work,
            &format!("{snapshot_git} rev-parse {}^", snapshot.commit),
        );
        assert_eq!(parent, sh(&work, "git rev-parse HEAD"));

        // Thin against the nearest commit the remote has, which is not HEAD.
        let base = sh(&work, "git rev-parse origin/main");
        assert_eq!(thin.snapshot.base.as_deref(), Some(base.as_str()));
        assert_eq!(complete.snapshot.base, None);
        assert!(thin.length < complete.length);
        // Either applies to a clone of the remote, and carries the snapshot.
        for bundle in [&thin, &complete] {
            let heads = sh(
                &root.path().join("remote"),
                &format!(
                    "git bundle verify -q {0} 2>/dev/null; git bundle list-heads {0}",
                    bundle.path.display()
                ),
            );
            assert_eq!(heads, format!("{} refs/heads/snapshot", snapshot.commit));
        }
        // The objects of the snapshot were never written to the repository.
        let written = sh(
            &work,
            &format!("git cat-file -e {} && echo yes || echo no", snapshot.commit),
        );
        assert_eq!(written, "no");
    }

    /// Without remote-tracking branches, the whole history is bundled.
    #[tokio::test]
    async fn worktrees_without_remotes_bundle_their_whole_history() {
        let root = tempfile::tempdir().unwrap();
        sh(
            root.path(),
            "git init -q -b main work && cd work && echo a > a && git add . && git commit -qm a",
        );
        let worktree = Worktree::at(&root.path().join("work")).await.unwrap();
        let bundle = worktree
            .snapshot("main")
            .await
            .unwrap()
            .bundle(true)
            .await
            .unwrap();
        assert_eq!(bundle.snapshot.base, None);
    }

    /// Detached heads have no branch, and submodules and nested
    /// repositories are refused rather than half-supported.
    #[tokio::test]
    async fn detached_heads_have_no_branch_and_nested_repositories_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let work = fixture(root.path());
        sh(
            &work,
            "git switch -q --detach && git init -q nested && cd nested && echo n > n && git add . && git commit -qm n",
        );
        let worktree = Worktree::at(&work).await.unwrap();
        assert_eq!(worktree.branch().await.unwrap(), None);
        let Err(error) = worktree.snapshot("feat/x").await else {
            panic!("nested repositories must be refused");
        };
        assert!(error.to_string().contains("nested"), "{error}");
        assert!(Worktree::at(root.path()).await.is_err());
    }

    /// Files a configured filter driver would stage, such as LFS's, partial
    /// clones, and SHA-256 repositories are refused before anything is
    /// written; a filter attribute without a configured driver is harmless.
    #[tokio::test]
    async fn filtered_files_partial_clones_and_sha256_repositories_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let work = fixture(root.path());
        sh(
            &work,
            "echo '*.bin filter=cache' > .gitattributes && echo data > big.bin",
        );
        let worktree = Worktree::at(&work).await.unwrap();
        assert!(worktree.snapshot("feat/x").await.is_ok());
        sh(
            &work,
            "git config filter.cache.clean 'mkdir -p .git/filter-cache && cat'",
        );
        let before = state(&work);
        let Err(error) = worktree.snapshot("feat/x").await else {
            panic!("filtered files must be refused");
        };
        assert!(error.to_string().contains("big.bin"), "{error}");
        assert_eq!(state(&work), before);
        // A path starting with a space, listed first, is checked as is.
        sh(
            &work,
            "rm big.bin && echo '\" a.bin\" filter=cache' > .gitattributes && echo data > ' a.bin'",
        );
        let Err(error) = worktree.snapshot("feat/x").await else {
            panic!("filtered files must be refused");
        };
        assert!(error.to_string().contains(" a.bin"), "{error}");

        sh(
            root.path(),
            "git -C remote config uploadpack.allowFilter true && git clone -q --filter=blob:none \"file://$PWD/remote\" partial",
        );
        let worktree = Worktree::at(&root.path().join("partial")).await.unwrap();
        let Err(error) = worktree.snapshot("main").await else {
            panic!("partial clones must be refused");
        };
        assert!(error.to_string().contains("partial clone"), "{error}");

        sh(
            root.path(),
            "git init -q --object-format=sha256 -b main sha256 && cd sha256 && echo a > a && git add . && git commit -qm a",
        );
        let worktree = Worktree::at(&root.path().join("sha256")).await.unwrap();
        let Err(error) = worktree.snapshot("main").await else {
            panic!("SHA-256 repositories must be refused");
        };
        assert!(error.to_string().contains("sha256"), "{error}");
    }
}
