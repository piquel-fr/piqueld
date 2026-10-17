//! Git repositories that tests commit to, fetch from, and bundle.

use std::path::Path;

/// A Git repository in a temporary directory of its own, committing as a
/// fixed test identity.
pub struct Repository {
    directory: tempfile::TempDir,
}

impl Repository {
    /// An empty repository on `main`.
    pub fn new() -> Self {
        let repository = Self::empty();
        repository.git(&["init", "--initial-branch=main"]);
        repository
    }

    fn empty() -> Self {
        Self {
            directory: tempfile::tempdir().expect("repository directory"),
        }
    }

    pub fn path(&self) -> &Path {
        self.directory.path()
    }

    /// Runs `git` with `args` in the repository and returns its trimmed output.
    pub fn git(&self, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(self.path())
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git output is UTF-8")
            .trim()
            .to_owned()
    }

    /// Writes `files` and commits everything, even nothing, returning the
    /// commit.
    pub fn commit(&self, files: &[(&str, &[u8])]) -> String {
        for (name, contents) in files {
            std::fs::write(self.path().join(name), contents).expect("fixture file is written");
        }
        self.git(&["add", "--all"]);
        self.git(&["commit", "--allow-empty", "-qm", "fixture"]);
        self.git(&["rev-parse", "HEAD"])
    }

    /// A clone of this repository, like a developer's working copy.
    pub fn working_copy(&self) -> Self {
        let clone = Self::empty();
        clone.git(&["clone", "-q", &self.path().display().to_string(), "."]);
        clone
    }

    /// A bundle of `branch`, without `excluded` and its history when given.
    pub fn bundle(&self, branch: &str, excluded: Option<&str>) -> Vec<u8> {
        let path = self.path().join(".git/fixture.bundle");
        let path_argument = path.display().to_string();
        let reference = format!("refs/heads/{branch}");
        let excluded = excluded.map(|commit| format!("^{commit}"));
        let mut arguments = vec!["bundle", "create", "-q", &path_argument, &reference];
        arguments.extend(excluded.as_deref());
        self.git(&arguments);
        std::fs::read(path).expect("bundle is written")
    }
}
