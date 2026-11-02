//! Changes the dashboard makes to an application's environments, and the
//! source form that chooses where an environment deploys from: building its
//! own source (the saved manifest, or a branch of the application's
//! repository) or receiving releases promoted from another environment.

use piqueld_client::{
    ApplicationSpec, ApplicationView, Client, ClientError, CreateEnvironmentRequest,
    EnvironmentBranchRequest, EnvironmentId, EnvironmentRequest, EnvironmentSource,
    EnvironmentSourceRequest, EnvironmentView, TrackedBranch,
};

/// Dirty group of the environment page's source form.
pub const SOURCE_FORM: &str = "environment-source";
/// Dirty group of the environment page's rename form.
pub const NAME_FORM: &str = "environment-name";

/// A change to one of the application's environments. All but `Sync` are
/// guarded by the saved generation.
#[derive(Clone, Debug, PartialEq)]
pub enum EnvironmentEdit {
    /// Creates an environment.
    Create {
        /// The new environment's name.
        name: String,
        /// The branch it follows, in a repository-backed application.
        branch: Option<TrackedBranch>,
        /// The environment it is promoted from; excludes `branch`.
        promote_from: Option<String>,
    },
    /// Renames environment `id`.
    Rename {
        /// The environment.
        id: String,
        /// Its new name.
        name: String,
    },
    /// Makes environment `id` follow `branch`.
    Branch {
        /// The environment.
        id: String,
        /// The branch, and the commit it is pinned to, if any.
        branch: TrackedBranch,
    },
    /// Opts in or out of the application's sync, without a revision.
    Sync {
        /// The environment.
        id: String,
        /// Whether pushes deploy it.
        enabled: bool,
    },
    /// Grants or revokes the daemon's telemetry, without a revision.
    Telemetry {
        /// The environment.
        id: String,
        /// Whether it receives the telemetry.
        granted: bool,
    },
    /// Promoted from the given environment, or `None` to track its own source.
    Source {
        /// The environment.
        id: String,
        /// The environment it receives releases from.
        promote_from: Option<String>,
    },
    /// Deletes the environment, retaining its volumes.
    Delete(String),
    /// Retries a deletion that did not finish.
    RetryDeletion(String),
}

impl EnvironmentEdit {
    /// Whether a replay after a transport failure is safe. Deletions are not
    /// retried, as they could fail once the first attempt completed.
    #[must_use]
    pub const fn replayable(&self) -> bool {
        !matches!(self, Self::Delete(_) | Self::RetryDeletion(_))
    }

    /// The form whose unsaved edits the change saves, if any.
    #[must_use]
    pub const fn form(&self) -> Option<&'static str> {
        match self {
            Self::Rename { .. } => Some(NAME_FORM),
            Self::Branch { .. } | Self::Source { .. } => Some(SOURCE_FORM),
            _ => None,
        }
    }

    /// Whether the change advances the application revision, which later
    /// guarded edits must then expect.
    const fn advances(&self) -> bool {
        !matches!(
            self,
            Self::RetryDeletion(_) | Self::Sync { .. } | Self::Telemetry { .. }
        )
    }

    /// Records the applied change in `saved`: the `changed` environment the
    /// daemon returned replaces its previous view, in name order, or without
    /// one a deleted environment is marked as being deleted.
    pub fn apply(&self, saved: &mut ApplicationView, changed: Option<EnvironmentView>) {
        if self.advances() {
            saved.generation += 1;
        }
        let environments = &mut saved.environments;
        if let Some(changed) = changed {
            environments.retain(|env| env.id != changed.id);
            environments.push(changed);
            environments.sort_by(|a, b| a.name.cmp(&b.name));
        } else if let Self::Delete(id) = self
            && let Some(env) = environments.iter_mut().find(|env| env.id.as_str() == id)
        {
            env.delete_intent = true;
        }
    }

    /// Sends the change, returning the changed environment unless it is
    /// being deleted.
    ///
    /// # Errors
    ///
    /// Returns the request's transport or API error.
    pub async fn send(
        &self,
        client: &Client,
        application: &str,
        generation: u64,
    ) -> Result<Option<EnvironmentView>, ClientError> {
        let expected_generation = Some(generation);
        match self {
            Self::Create {
                name,
                branch,
                promote_from,
            } => {
                let request = CreateEnvironmentRequest {
                    name: name.clone(),
                    branch: branch.as_ref().map(|branch| branch.branch().to_owned()),
                    commit: branch
                        .as_ref()
                        .and_then(|branch| branch.commit().map(str::to_owned)),
                    promote_from: promote_from.clone(),
                    expected_generation,
                };
                client
                    .create_environment(application, &request, false)
                    .await
                    .map(Some)
            }
            Self::Rename { id, name } => {
                let request = EnvironmentRequest {
                    name: name.clone(),
                    expected_generation,
                };
                client
                    .rename_environment(id, &request, false)
                    .await
                    .map(Some)
            }
            Self::Branch { id, branch } => {
                let request = EnvironmentBranchRequest {
                    branch: branch.branch().to_owned(),
                    commit: branch.commit().map(str::to_owned),
                    expected_generation,
                };
                client
                    .set_environment_branch(id, &request, false)
                    .await
                    .map(Some)
            }
            Self::Sync { id, enabled } => client.set_environment_sync(id, *enabled).await.map(Some),
            Self::Telemetry { id, granted } => client
                .set_environment_telemetry(id, *granted)
                .await
                .map(Some),
            Self::Source { id, promote_from } => {
                let request = EnvironmentSourceRequest {
                    promote_from: promote_from.clone(),
                    expected_generation,
                };
                client
                    .set_environment_source(id, &request, false)
                    .await
                    .map(Some)
            }
            Self::Delete(id) => client
                .delete_environment(id, Some(generation), false)
                .await
                .map(|_| None),
            Self::RetryDeletion(id) => client
                .reconcile_environment(id, Some(generation))
                .await
                .map(|_| None),
        }
    }
}

/// A branch and optional commit as typed into a form.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BranchDraft {
    /// The branch name.
    pub branch: String,
    /// Blank to follow the branch head.
    pub commit: String,
}

impl BranchDraft {
    /// The branch and commit new environments of an application with `spec`
    /// follow by default: those its `spec.manifest` names, or blank when it
    /// has no repository.
    #[must_use]
    pub fn default_for(spec: &ApplicationSpec) -> Self {
        spec.manifest
            .as_ref()
            .map(|manifest| Self {
                branch: manifest.repository.branch.clone(),
                commit: manifest.repository.commit.clone().unwrap_or_default(),
            })
            .unwrap_or_default()
    }

    /// Validates the draft, ignoring surrounding whitespace; a blank commit
    /// follows the branch head.
    ///
    /// # Errors
    ///
    /// Returns the validation message of an invalid branch or commit.
    pub fn tracked(&self) -> Result<TrackedBranch, String> {
        let commit = self.commit.trim();
        let commit = (!commit.is_empty()).then(|| commit.to_owned());
        TrackedBranch::new(self.branch.trim().to_owned(), commit).map_err(|error| error.to_string())
    }
}

impl From<&TrackedBranch> for BranchDraft {
    fn from(branch: &TrackedBranch) -> Self {
        Self {
            branch: branch.branch().to_owned(),
            commit: branch.commit().unwrap_or_default().to_owned(),
        }
    }
}

/// What the environment page's Source form shows: the branch it builds and
/// the environment it is promoted from, empty to build its own source.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceDraft {
    /// The branch it builds, shown while it builds its own source in a
    /// repository-backed application.
    pub branch: BranchDraft,
    /// The environment it is promoted from; empty to build its own source.
    pub promote_from: String,
}

impl SourceDraft {
    /// The form showing `source` as saved; one that follows no branch shows
    /// `default`, the branch it would build.
    #[must_use]
    pub fn saved(source: Option<&EnvironmentSource>, default: BranchDraft) -> Self {
        Self {
            branch: source
                .and_then(EnvironmentSource::branch)
                .map_or(default, BranchDraft::from),
            promote_from: source
                .and_then(EnvironmentSource::promoted_from)
                .map(ToString::to_string)
                .unwrap_or_default(),
        }
    }

    /// Whether the form chooses building its own source over promotion.
    #[must_use]
    pub fn builds(&self) -> bool {
        self.promote_from.is_empty()
    }

    /// The change saving the form makes to environment `id`, whose `saved`
    /// source it edits, in an application that is `repository`-backed or
    /// deploys its saved manifest; `None` when it shows the saved source.
    /// Only repository-backed environments follow a branch.
    #[must_use]
    pub fn change(
        &self,
        id: String,
        saved: Option<&EnvironmentSource>,
        repository: bool,
    ) -> Option<Result<EnvironmentEdit, String>> {
        let promoted = saved.and_then(EnvironmentSource::promoted_from);
        if !self.builds() {
            return (promoted.map(EnvironmentId::as_str) != Some(self.promote_from.as_str())).then(
                || {
                    Ok(EnvironmentEdit::Source {
                        id,
                        promote_from: Some(self.promote_from.clone()),
                    })
                },
            );
        }
        if repository {
            return match self.branch.tracked() {
                Ok(tracked) if saved.and_then(EnvironmentSource::branch) == Some(&tracked) => None,
                Ok(branch) => Some(Ok(EnvironmentEdit::Branch { id, branch })),
                Err(error) => Some(Err(error)),
            };
        }
        promoted.map(|_| {
            Ok(EnvironmentEdit::Source {
                id,
                promote_from: None,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use piqueld_client::{
        ApplicationId, ApplicationManifest, EnvironmentKind, EnvironmentName, Metadata,
        PromotedFrom, RunState,
    };

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn environment(id: &str, name: &str, source: EnvironmentSource) -> EnvironmentView {
        EnvironmentView {
            id: EnvironmentId::parse(id).unwrap(),
            application_id: ApplicationId::parse("app-00000001").unwrap(),
            name: EnvironmentName::parse(name).unwrap(),
            source,
            kind: EnvironmentKind::Environment,
            sync: false,
            synced: None,
            telemetry: false,
            run_state: RunState::Running,
            resolved_generation: None,
            delete_intent: false,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    /// An application at generation 3 with `environments`.
    fn application(environments: Vec<EnvironmentView>) -> ApplicationView {
        let id = ApplicationId::parse("app-00000001").unwrap();
        let application = ApplicationManifest {
            api_version: "piqueld.dev/v1alpha1".into(),
            kind: "Application".into(),
            metadata: Metadata {
                name: "notes".into(),
            },
            spec: ApplicationSpec::default(),
        }
        .validate_template()
        .unwrap()
        .normalize(id);
        ApplicationView {
            spec_hash: application.spec_hash(),
            application,
            generation: 3,
            delete_intent: false,
            created_at_ms: 0,
            updated_at_ms: 0,
            environments,
            previews: Vec::new(),
            drift: Vec::new(),
            sync_check: None,
            repository: None,
        }
    }

    fn names(saved: &ApplicationView) -> Vec<&str> {
        saved
            .environments
            .iter()
            .map(|env| env.name.as_str())
            .collect()
    }

    fn branch(branch: &str, commit: Option<&str>) -> TrackedBranch {
        TrackedBranch::new(branch.into(), commit.map(Into::into)).unwrap()
    }

    fn promoted(from: &str) -> EnvironmentSource {
        EnvironmentSource::Promoted(PromotedFrom {
            environment: EnvironmentId::parse(from).unwrap(),
        })
    }

    /// The form showing `branch` and `commit`, building its own source.
    fn building(branch: &str, commit: &str) -> SourceDraft {
        SourceDraft {
            branch: BranchDraft {
                branch: branch.into(),
                commit: commit.into(),
            },
            promote_from: String::new(),
        }
    }

    #[test]
    fn a_renamed_environment_moves_to_its_name_order_and_advances_the_revision() {
        let mut saved = application(vec![
            environment("env-00000001", "alpha", EnvironmentSource::Saved),
            environment("env-00000002", "beta", EnvironmentSource::Saved),
        ]);
        let renamed = environment("env-00000001", "zeta", EnvironmentSource::Saved);
        let edit = EnvironmentEdit::Rename {
            id: "env-00000001".into(),
            name: "zeta".into(),
        };
        edit.apply(&mut saved, Some(renamed));
        assert_eq!(names(&saved), ["beta", "zeta"]);
        assert_eq!(saved.generation, 4);
    }

    #[test]
    fn unguarded_changes_keep_the_revision_later_edits_expect() {
        let mut saved = application(vec![environment(
            "env-00000001",
            "alpha",
            EnvironmentSource::Saved,
        )]);
        let mut changed = saved.environments[0].clone();
        changed.sync = true;
        let id = String::from("env-00000001");
        EnvironmentEdit::Sync {
            id: id.clone(),
            enabled: true,
        }
        .apply(&mut saved, Some(changed.clone()));
        changed.telemetry = true;
        EnvironmentEdit::Telemetry {
            id: id.clone(),
            granted: true,
        }
        .apply(&mut saved, Some(changed.clone()));
        EnvironmentEdit::RetryDeletion(id).apply(&mut saved, None);
        assert_eq!(saved.generation, 3);
        assert_eq!(saved.environments, [changed]);
    }

    #[test]
    fn an_accepted_deletion_marks_only_that_environment() {
        let mut saved = application(vec![
            environment("env-00000001", "alpha", EnvironmentSource::Saved),
            environment("env-00000002", "beta", EnvironmentSource::Saved),
        ]);
        EnvironmentEdit::Delete("env-00000002".into()).apply(&mut saved, None);
        let deleting: Vec<_> = saved
            .environments
            .iter()
            .map(|env| env.delete_intent)
            .collect();
        assert_eq!(deleting, [false, true]);
        assert_eq!(saved.generation, 4);
    }

    #[test]
    fn a_blank_commit_follows_the_branch_head_and_whitespace_is_ignored() {
        assert_eq!(
            building(" main ", "  ").branch.tracked(),
            Ok(branch("main", None))
        );
        assert_eq!(
            building("main", &format!(" {COMMIT}\n")).branch.tracked(),
            Ok(branch("main", Some(COMMIT)))
        );
        assert!(building("main", "abc").branch.tracked().is_err());
        assert!(building("", "").branch.tracked().is_err());
    }

    #[test]
    fn a_branch_form_changes_only_when_it_names_another_branch_or_commit() {
        let saved = EnvironmentSource::Branch(branch("release", None));
        let change = |draft: SourceDraft| draft.change("env-00000001".into(), Some(&saved), true);
        assert_eq!(change(building(" release", "")), None);
        assert_eq!(
            change(building("release", COMMIT)),
            Some(Ok(EnvironmentEdit::Branch {
                id: "env-00000001".into(),
                branch: branch("release", Some(COMMIT)),
            }))
        );
        assert!(matches!(change(building("release", "abc")), Some(Err(_))));
    }

    #[test]
    fn a_promoted_environment_changes_its_source_or_builds_again() {
        let saved = promoted("env-00000002");
        let id = || String::from("env-00000001");
        let promoting = |from: &str| SourceDraft {
            promote_from: from.into(),
            ..building("main", "")
        };
        assert_eq!(
            promoting("env-00000002").change(id(), Some(&saved), true),
            None
        );
        assert_eq!(
            promoting("env-00000003").change(id(), Some(&saved), false),
            Some(Ok(EnvironmentEdit::Source {
                id: id(),
                promote_from: Some("env-00000003".into()),
            }))
        );
        // Building again follows the shown branch, even the default one, in
        // a repository-backed application, and its saved manifest otherwise.
        assert_eq!(
            building("main", "").change(id(), Some(&saved), true),
            Some(Ok(EnvironmentEdit::Branch {
                id: id(),
                branch: branch("main", None),
            }))
        );
        assert_eq!(
            building("", "").change(id(), Some(&saved), false),
            Some(Ok(EnvironmentEdit::Source {
                id: id(),
                promote_from: None,
            }))
        );
    }

    #[test]
    fn an_untouched_form_reports_no_change_so_it_never_blocks_deploys() {
        let default = BranchDraft {
            branch: "main".into(),
            commit: String::new(),
        };
        // Repository-backed environments follow a branch unless promoted;
        // the others build the saved manifest.
        let sources = [
            (
                EnvironmentSource::Branch(branch("release", Some(COMMIT))),
                true,
            ),
            (promoted("env-00000002"), true),
            (EnvironmentSource::Saved, false),
            (promoted("env-00000002"), false),
        ];
        for (source, repository) in sources {
            let draft = SourceDraft::saved(Some(&source), default.clone());
            assert_eq!(
                draft.change("env-00000001".into(), Some(&source), repository),
                None,
                "{source}"
            );
        }
    }
}
