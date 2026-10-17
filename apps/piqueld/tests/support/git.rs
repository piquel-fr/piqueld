//! A local Git repository that services build from.

use super::repository::Repository;

/// A repository whose `Dockerfile` builds a long-running Alpine image and
/// whose `Failfile` fails to build, with the Git source building the former.
pub struct GitBuildFixture {
    _repository: Repository,
    pub source: piqueld_core::Source,
}

impl GitBuildFixture {
    pub fn new() -> Self {
        let repository = Repository::new();
        repository.commit(&[
            (
                "Dockerfile",
                b"FROM alpine:3.20\nCMD [\"sleep\", \"3600\"]\n",
            ),
            ("Failfile", b"build-fails\n"),
        ]);
        let source = piqueld_core::Source::Git {
            repository: piqueld_core::manifest::SourceRepository::Git(
                piqueld_core::manifest::GitRepository {
                    url: repository.path().display().to_string(),
                    branch: "main".into(),
                    commit: None,
                },
            ),
            build: dockerfile_build(),
        };
        Self {
            _repository: repository,
            source,
        }
    }

    /// [`Self::source`], building the `Failfile` instead.
    pub fn failing_source(&self) -> piqueld_core::Source {
        let mut source = self.source.clone();
        let piqueld_core::Source::Git { build, .. } = &mut source else {
            unreachable!("the fixture builds from Git")
        };
        let piqueld_core::manifest::Build::Docker { dockerfile, .. } = build;
        *dockerfile = "Failfile".into();
        source
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
