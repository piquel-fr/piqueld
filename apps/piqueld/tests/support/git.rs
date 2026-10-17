//! A local Git repository that services build from.

/// A repository whose `Dockerfile` builds a long-running Alpine image and
/// whose `Failfile` fails to build, with the Git source building the former.
pub struct GitBuildFixture {
    _directory: tempfile::TempDir,
    pub source: piqueld_core::Source,
}

impl GitBuildFixture {
    pub fn new() -> Self {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(
            repo.path().join("Dockerfile"),
            "FROM alpine:3.20\nCMD [\"sleep\", \"3600\"]\n",
        )
        .unwrap();
        std::fs::write(repo.path().join("Failfile"), "build-fails\n").unwrap();
        for args in [
            vec!["init", "--initial-branch=main"],
            vec!["add", "."],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "fixture",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .current_dir(repo.path())
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let source = piqueld_core::Source::Git {
            repository: piqueld_core::manifest::SourceRepository::Git(
                piqueld_core::manifest::GitRepository {
                    url: repo.path().display().to_string(),
                    branch: "main".into(),
                    commit: None,
                },
            ),
            build: dockerfile_build(),
        };
        Self {
            _directory: repo,
            source,
        }
    }
}

/// Builds the repository root's `Dockerfile`, without arguments or a target.
pub fn dockerfile_build() -> piqueld_core::manifest::Build {
    piqueld_core::manifest::Build::Docker {
        dockerfile: "Dockerfile".into(),
        context: ".".into(),
        args: std::collections::BTreeMap::new(),
        target: None,
    }
}
