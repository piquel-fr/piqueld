//! Environments an application's manifest declares, and how its environments
//! differ from them.
//!
//! Every `[spec.environments.<name>]` block declares the environment `<name>`
//! and where it deploys from. A manifest that declares environments is the
//! source of truth for which environments exist and how each is sourced:
//! saving it, or fetching it from the branch `spec.manifest` names, creates the
//! missing ones and changes the sources of the others. Nothing is deployed, and
//! nothing is ever deleted: environments it no longer declares are reported
//! until deleted explicitly. A manifest that declares none leaves environments
//! to `piquelctl env`, as before.

use super::{ApplicationSpec, EnvironmentConfig, ValidationError, ValidationErrors};
use crate::api::EnvironmentView;
use crate::{EnvironmentName, EnvironmentSource, TrackedBranch, codes};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

/// Where a declared environment deploys from, naming other environments by
/// name, as the manifest does.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeclaredSource {
    /// The application's saved manifest.
    Saved,
    /// The manifest file on a branch of the application's manifest repository.
    Branch(TrackedBranch),
    /// Releases promoted from another environment of the application.
    Promoted {
        /// The environment it receives releases from.
        from: EnvironmentName,
    },
}

/// Describes the source in a few words, as [`EnvironmentSource`] does.
///
/// ```text
/// the saved manifest    branch main    branch main@0123…    releases promoted from staging
/// ```
impl std::fmt::Display for DeclaredSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Saved => formatter.write_str("the saved manifest"),
            Self::Branch(branch) => write!(formatter, "branch {branch}"),
            Self::Promoted { from } => write!(formatter, "releases promoted from {from}"),
        }
    }
}

/// One environment as the manifest declares it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Declaration {
    /// The environment's name, the block's key.
    pub name: EnvironmentName,
    /// Where it deploys from.
    pub source: DeclaredSource,
    /// Whether pushes to its branch deploy it while the repository syncs.
    /// Always false for promoted environments, which never build.
    pub sync: bool,
    /// Whether converging changes an existing environment's source and sync
    /// to these: its block sets them, or the previous manifest's did, so that
    /// removing them returns it to the default. Otherwise they only create a
    /// missing environment, and an existing one keeps the source it has, such
    /// as one set with `piquelctl env source` before blocks declared sources.
    pub manages_source: bool,
}

impl Declaration {
    /// The environment a new application with `declarations` deploys first:
    /// the first declared one that builds, or `production`.
    #[must_use]
    pub fn first_building(declarations: &[Self]) -> EnvironmentName {
        declarations
            .iter()
            .find(|declaration| !matches!(declaration.source, DeclaredSource::Promoted { .. }))
            .map_or_else(EnvironmentName::default_name, |declaration| {
                declaration.name.clone()
            })
    }
}

impl ApplicationSpec {
    /// The environments this manifest declares, ordered so that every
    /// promotion source comes before the environments promoting from it.
    /// Tracking environments deploy the saved manifest, or follow a branch of
    /// `spec.manifest`'s repository: their own `branch`, or the one
    /// `spec.manifest` names, pinned at `commit` when set.
    ///
    /// # Errors
    ///
    /// Returns every problem at its field path: invalid names, branches and
    /// commits; a source on a promoted environment
    /// (`environment_source_conflict`); `branch`, `commit` or `sync` without
    /// a manifest repository (`manifest_repository_required`); `sync` with its
    /// own `commit` (`environment_sync_pinned`); and a `promote_from`
    /// naming an undeclared environment (`promotion_source_invalid`) or
    /// leading back to itself (`promotion_cycle`).
    pub fn declared_environments(&self) -> Result<Vec<Declaration>, ValidationErrors> {
        self.declared_environments_since(None)
    }

    /// [`Self::declared_environments`] when this manifest replaces `previous`:
    /// sources `previous` set and this one removed are managed too, so they
    /// return to the default.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Self::declared_environments`].
    pub fn declared_environments_since(
        &self,
        previous: Option<&Self>,
    ) -> Result<Vec<Declaration>, ValidationErrors> {
        let mut errors = Vec::new();
        let mut declared = BTreeMap::new();
        for (name, config) in &self.environments {
            let path = format!("spec.environments.{name}");
            match EnvironmentName::parse(name.as_str()) {
                Ok(parsed) => match self.declare(parsed, config, &path) {
                    Ok(mut declaration) => {
                        declaration.manages_source |= previous
                            .and_then(|previous| previous.environments.get(name))
                            .is_some_and(EnvironmentConfig::sets_source);
                        declared.insert(name.as_str(), declaration);
                    }
                    Err(invalid) => errors.extend(invalid),
                },
                Err(source) => errors.push(ValidationError::new(
                    codes::NAME_INVALID,
                    "spec.environments",
                    format!(
                        "[spec.environments.{}]: {source}",
                        super::variables::echo(name)
                    ),
                )),
            }
        }
        let mut ordered = Vec::with_capacity(declared.len());
        let mut placed = std::collections::BTreeSet::new();
        for name in declared.keys() {
            if placed.contains(name) {
                continue;
            }
            // Walks up the chain of sources, then places it from the top down.
            let mut chain = vec![*name];
            while let Some(DeclaredSource::Promoted { from }) = declared
                .get(chain[chain.len() - 1])
                .map(|declaration| &declaration.source)
            {
                let from = from.as_str();
                if placed.contains(from) {
                    break;
                }
                if !declared.contains_key(from) {
                    // An invalid declaration already has its own errors.
                    if self.environments.contains_key(from) {
                        break;
                    }
                    errors.push(ValidationError::new(
                        codes::PROMOTION_SOURCE_INVALID,
                        format!("spec.environments.{}.promote_from", chain[chain.len() - 1]),
                        format!(
                            "{from} is not declared: promotion sources are environments of spec.environments"
                        ),
                    ));
                    break;
                }
                if let Some(start) = chain.iter().position(|name| *name == from) {
                    let mut cycle = chain[start..].to_vec();
                    cycle.push(from);
                    errors.push(ValidationError::new(
                        codes::PROMOTION_CYCLE,
                        format!("spec.environments.{}.promote_from", chain[chain.len() - 1]),
                        format!(
                            "an environment cannot promote from itself: {}",
                            cycle.join(" ← ")
                        ),
                    ));
                    break;
                }
                chain.push(from);
            }
            for name in chain.into_iter().rev() {
                if placed.insert(name) {
                    ordered.push(declared[name].clone());
                }
            }
        }
        if errors.is_empty() {
            Ok(ordered)
        } else {
            Err(ValidationErrors::sorted(errors))
        }
    }

    /// The declaration of environment `name` by `config`, whose block is at
    /// `path`, or the errors at its fields.
    fn declare(
        &self,
        name: EnvironmentName,
        config: &EnvironmentConfig,
        path: &str,
    ) -> Result<Declaration, Vec<ValidationError>> {
        let fail = |field: &str, code: &str, message: String| {
            ValidationError::new(code, format!("{path}.{field}"), message)
        };
        if let Some(from) = &config.promote_from {
            if config.branch.is_some() || config.commit.is_some() || config.sync.is_some() {
                return Err(vec![fail(
                    "promote_from",
                    codes::ENVIRONMENT_SOURCE_CONFLICT,
                    "a promoted environment never builds: remove branch, commit and sync".into(),
                )]);
            }
            let from = EnvironmentName::parse(from.as_str()).map_err(|_| {
                vec![fail(
                    "promote_from",
                    codes::PROMOTION_SOURCE_INVALID,
                    format!(
                        "{} is not an environment name",
                        super::variables::echo(from)
                    ),
                )]
            })?;
            return Ok(Declaration {
                name,
                source: DeclaredSource::Promoted { from },
                sync: false,
                manages_source: true,
            });
        }
        let Some(connection) = &self.manifest else {
            let errors = [
                ("branch", config.branch.is_some()),
                ("commit", config.commit.is_some()),
                ("sync", config.sync == Some(true)),
            ]
            .into_iter()
            .filter(|(_, set)| *set)
            .map(|(field, _)| {
                fail(
                    field,
                    codes::MANIFEST_REPOSITORY_REQUIRED,
                    "only environments of a repository-backed application follow a branch; add spec.manifest first".into(),
                )
            })
            .collect::<Vec<_>>();
            return if errors.is_empty() {
                Ok(Declaration {
                    name,
                    source: DeclaredSource::Saved,
                    sync: false,
                    manages_source: config.sets_source(),
                })
            } else {
                Err(errors)
            };
        };
        // Its own branch is unpinned unless it pins a commit; otherwise it
        // follows the branch and commit `spec.manifest` names.
        let (branch, commit) = match &config.branch {
            Some(branch) => (branch.clone(), config.commit.clone()),
            None => (
                connection.repository.branch.clone(),
                config
                    .commit
                    .clone()
                    .or_else(|| connection.repository.commit.clone()),
            ),
        };
        let branch = TrackedBranch::new(branch, commit).map_err(|invalid| {
            invalid
                .0
                .into_iter()
                // The connection's own fields are checked at spec.manifest.
                .filter(|error| match error.path.as_str() {
                    "branch" => config.branch.is_some(),
                    _ => config.commit.is_some(),
                })
                .map(|error| fail(&error.path, &error.code, error.message))
                .collect::<Vec<_>>()
        })?;
        // Only its own pin: one inherited from `spec.manifest`, or from the
        // revision a deployment fetched, keeps it opted in but not following,
        // as `piquelctl env sync` does for a pinned environment.
        let sync = config.sync.unwrap_or(false);
        if sync && config.commit.is_some() {
            return Err(vec![fail(
                "sync",
                codes::ENVIRONMENT_SYNC_PINNED,
                "pinned environments never sync: remove commit, or sync".into(),
            )]);
        }
        Ok(Declaration {
            name,
            source: DeclaredSource::Branch(branch),
            sync,
            manages_source: config.sets_source(),
        })
    }
}

impl ApplicationSpec {
    /// What saving this manifest, which declares `declarations`, changes in
    /// an application's `environments`, or creates for a new application
    /// (`None`): one that declares none gets `production`.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`TrackedBranch::of`] for `spec.manifest`.
    pub fn environment_changes(
        &self,
        declarations: &[Declaration],
        environments: Option<&[EnvironmentView]>,
    ) -> Result<Vec<EnvironmentChange>, ValidationErrors> {
        Ok(match environments {
            Some(environments) => EnvironmentChange::between(declarations, environments),
            None if declarations.is_empty() => vec![EnvironmentChange::Create {
                environment: EnvironmentName::default_name(),
                source: match &self.manifest {
                    Some(connection) => {
                        DeclaredSource::Branch(TrackedBranch::of(&connection.repository)?)
                    }
                    None => DeclaredSource::Saved,
                },
                sync: false,
            }],
            None => EnvironmentChange::between(declarations, &[]),
        })
    }
}

/// A difference between the environments a manifest declares and the
/// application's environments, as converging them would resolve it, or, for
/// `undeclared`, report it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum EnvironmentChange {
    /// The environment is created, not deployed.
    Create {
        /// The declared environment.
        environment: EnvironmentName,
        /// Where it deploys from.
        source: DeclaredSource,
        /// Whether pushes deploy it, from its first deployment.
        sync: bool,
    },
    /// The environment deploys from `to` instead of `from`; nothing is
    /// deployed. Waits while a deployment of it runs when it becomes promoted.
    Source {
        /// The declared environment.
        environment: EnvironmentName,
        /// Where it deploys from now.
        from: DeclaredSource,
        /// Where it is declared to deploy from.
        to: DeclaredSource,
    },
    /// Pushes start or stop deploying the environment.
    Sync {
        /// The declared environment.
        environment: EnvironmentName,
        /// Whether pushes deploy it.
        sync: bool,
    },
    /// The manifest no longer declares the environment. It keeps running,
    /// with its volumes and secrets, until deleted explicitly.
    Undeclared {
        /// The environment.
        environment: EnvironmentName,
    },
}

impl EnvironmentChange {
    /// What converging `environments` (never previews) to `declarations`
    /// changes, in the order of `declarations`, then the environments it
    /// leaves undeclared. Existing environments change only where their
    /// declaration manages their source (see [`Declaration::manages_source`]). Without declarations there is nothing to converge.
    /// Environments being deleted are gone: a declaration of one's name is
    /// created once it is.
    #[must_use]
    pub fn between(declarations: &[Declaration], environments: &[EnvironmentView]) -> Vec<Self> {
        if declarations.is_empty() {
            return Vec::new();
        }
        let live = environments
            .iter()
            .filter(|environment| !environment.delete_intent && environment.preview().is_none())
            .collect::<Vec<_>>();
        let mut changes = Vec::new();
        for declaration in declarations {
            let environment = declaration.name.clone();
            let Some(current) = live.iter().find(|current| current.name == environment) else {
                changes.push(Self::Create {
                    environment,
                    source: declaration.source.clone(),
                    sync: declaration.sync,
                });
                continue;
            };
            if !declaration.manages_source {
                continue;
            }
            let from = Self::named(&current.source, environments);
            if let Some(from) = from.filter(|from| *from != declaration.source) {
                changes.push(Self::Source {
                    environment: environment.clone(),
                    from,
                    to: declaration.source.clone(),
                });
            }
            // Promoted environments never build, so never sync.
            let promoted = matches!(declaration.source, DeclaredSource::Promoted { .. });
            if !promoted && current.sync != declaration.sync {
                changes.push(Self::Sync {
                    environment,
                    sync: declaration.sync,
                });
            }
        }
        changes.extend(
            live.iter()
                .filter(|current| {
                    !declarations
                        .iter()
                        .any(|declaration| declaration.name == current.name)
                })
                .map(|current| Self::Undeclared {
                    environment: current.name.clone(),
                }),
        );
        changes
    }

    /// `source`, naming its promotion source among `environments`, which
    /// always has it: sources are environments of the same application, and
    /// can't be deleted while environments promote from them.
    fn named(
        source: &EnvironmentSource,
        environments: &[EnvironmentView],
    ) -> Option<DeclaredSource> {
        Some(match source {
            EnvironmentSource::Saved => DeclaredSource::Saved,
            EnvironmentSource::Branch(branch) => DeclaredSource::Branch(branch.clone()),
            EnvironmentSource::Promoted(from) => DeclaredSource::Promoted {
                from: environments
                    .iter()
                    .find(|environment| environment.id == from.environment)?
                    .name
                    .clone(),
            },
        })
    }

    /// The environment it concerns.
    #[must_use]
    pub const fn environment(&self) -> &EnvironmentName {
        match self {
            Self::Create { environment, .. }
            | Self::Source { environment, .. }
            | Self::Sync { environment, .. }
            | Self::Undeclared { environment } => environment,
        }
    }
}

impl EnvironmentChange {
    /// Describes the change, without the environment's name.
    ///
    /// ```text
    /// created, deploys from branch main, follows pushes
    /// deploys from releases promoted from staging instead of branch main
    /// no longer declared; kept until deleted with `piquelctl env delete`
    /// ```
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::Create {
                source,
                sync: false,
                ..
            } => format!("created, deploys from {source}"),
            Self::Create {
                source, sync: true, ..
            } => format!("created, deploys from {source}, follows pushes"),
            Self::Source { from, to, .. } => format!("deploys from {to} instead of {from}"),
            Self::Sync { sync: true, .. } => "follows pushes".into(),
            Self::Sync { sync: false, .. } => "no longer follows pushes".into(),
            Self::Undeclared { .. } => {
                "no longer declared; kept until deleted with `piquelctl env delete`".into()
            }
        }
    }
}

/// The environment's name, then [`EnvironmentChange::summary`].
impl std::fmt::Display for EnvironmentChange {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.environment(), self.summary())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::parse_template_toml;
    use crate::{ApplicationId, EnvironmentId, PromotedFrom};

    const CONNECTION: &str = "[spec.manifest]\npath='app.toml'\n[spec.manifest.repository]\nurl='https://example.com/notes.git'\nbranch='main'\n";

    fn spec(body: &str) -> Result<ApplicationSpec, ValidationErrors> {
        parse_template_toml(&format!(
            "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='notes'\n{body}"
        ))
        .map(|template| {
            template
                .normalize(ApplicationId::parse("app-notes").unwrap())
                .spec()
                .clone()
        })
    }

    fn codes(body: &str) -> Vec<(String, String)> {
        spec(body)
            .unwrap_err()
            .0
            .into_iter()
            .map(|error| (error.path, error.code))
            .collect()
    }

    fn name(name: &str) -> EnvironmentName {
        EnvironmentName::parse(name).unwrap()
    }

    fn branch(branch: &str, commit: Option<&str>) -> DeclaredSource {
        DeclaredSource::Branch(TrackedBranch::new(branch.into(), commit.map(Into::into)).unwrap())
    }

    fn environment(id: &str, source: EnvironmentSource, sync: bool) -> EnvironmentView {
        EnvironmentView {
            id: EnvironmentId::parse(format!("env-0000-{id}")).unwrap(),
            application_id: ApplicationId::parse("app-notes").unwrap(),
            name: name(id),
            source,
            kind: crate::EnvironmentKind::Environment,
            sync,
            synced: None,
            resolved_generation: None,
            delete_intent: false,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn declarations_resolve_sources_with_promotion_sources_first() {
        let spec = spec(&format!(
            "{CONNECTION}[spec.environments.production]\npromote_from='staging'\n[spec.environments.staging]\nsync=true\n[spec.environments.canary]\nbranch='next'\ncommit='{}'\n",
            "a".repeat(40)
        ))
        .unwrap();
        let declarations = spec.declared_environments().unwrap();
        assert_eq!(
            declarations,
            [
                Declaration {
                    name: name("canary"),
                    source: branch("next", Some(&"a".repeat(40))),
                    sync: false,
                    manages_source: true,
                },
                Declaration {
                    name: name("staging"),
                    source: branch("main", None),
                    sync: true,
                    manages_source: true,
                },
                Declaration {
                    name: name("production"),
                    source: DeclaredSource::Promoted {
                        from: name("staging")
                    },
                    sync: false,
                    manages_source: true,
                },
            ]
        );
        // Without a repository, environments deploy the saved manifest.
        let saved = self::spec("[spec.environments.staging.variables]\nx=1\n").unwrap();
        assert_eq!(
            saved.declared_environments().unwrap()[0].source,
            DeclaredSource::Saved
        );
    }

    #[test]
    fn the_staging_and_production_example_declares_both() {
        let example =
            parse_template_toml(include_str!("../../../../examples/staging-production.toml"))
                .unwrap()
                .normalize(ApplicationId::parse("app-notes").unwrap());
        assert_eq!(
            example
                .spec()
                .declared_environments()
                .unwrap()
                .into_iter()
                .map(|declaration| (
                    declaration.name.to_string(),
                    declaration.source.to_string(),
                    declaration.sync
                ))
                .collect::<Vec<_>>(),
            [
                ("staging".into(), "branch main".into(), true),
                (
                    "production".into(),
                    "releases promoted from staging".into(),
                    false
                ),
            ]
        );
    }

    /// Disconnecting the repository returns declared environments to the
    /// saved manifest instead of failing on fields that need one.
    #[test]
    fn disconnecting_drops_what_needs_a_repository() {
        let mut manifest =
            parse_template_toml(include_str!("../../../../examples/staging-production.toml"))
                .unwrap()
                .normalize(ApplicationId::parse("app-notes").unwrap())
                .to_manifest();
        manifest
            .spec
            .environments
            .get_mut("staging")
            .unwrap()
            .branch = Some("next".into());
        crate::edit::ApplicationEdit::Repository(None)
            .apply(&mut manifest)
            .unwrap();
        let sources = manifest
            .validate_template()
            .unwrap()
            .spec()
            .declared_environments()
            .unwrap()
            .into_iter()
            .map(|declaration| (declaration.source.to_string(), declaration.sync))
            .collect::<Vec<_>>();
        assert_eq!(
            sources,
            [
                ("the saved manifest".into(), false),
                ("releases promoted from staging".into(), false),
            ]
        );
    }

    /// Blocks that set no source, as every block did before blocks declared
    /// sources, leave existing environments' sources and sync alone; one that
    /// removes the source it set returns its environment to the default.
    #[test]
    fn only_declared_sources_change_existing_environments() {
        let staging = environment(
            "staging",
            EnvironmentSource::Branch(TrackedBranch::new("main".into(), None).unwrap()),
            true,
        );
        let production = environment(
            "production",
            EnvironmentSource::Promoted(PromotedFrom {
                environment: staging.id.clone(),
            }),
            false,
        );
        let environments = [production, staging];
        let legacy = spec(&format!(
            "{CONNECTION}[spec.environments.staging.variables]\nx=1\n[spec.environments.production]\nvisibility='private'\n"
        ))
        .unwrap();
        let declarations = legacy.declared_environments().unwrap();
        assert_eq!(EnvironmentChange::between(&declarations, &environments), []);

        let declared = spec(&format!(
            "{CONNECTION}[spec.environments.staging]\nsync=true\n[spec.environments.production]\npromote_from='staging'\n"
        ))
        .unwrap();
        let removed = legacy.declared_environments_since(Some(&declared)).unwrap();
        assert_eq!(
            EnvironmentChange::between(&removed, &environments)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [
                "production: deploys from branch main instead of releases promoted from staging",
                "staging: no longer follows pushes",
            ]
        );
    }

    #[test]
    fn impossible_sources_fail_at_their_field() {
        let field = |path: &str, code: &str| (path.to_owned(), code.to_owned());
        assert_eq!(
            codes("[spec.environments.staging]\nbranch='main'\nsync=true\n"),
            [
                field(
                    "spec.environments.staging.branch",
                    "manifest_repository_required"
                ),
                field(
                    "spec.environments.staging.sync",
                    "manifest_repository_required"
                ),
            ]
        );
        assert_eq!(
            codes(&format!(
                "{CONNECTION}[spec.environments.production]\npromote_from='staging'\nsync=true\n[spec.environments.staging]\n"
            )),
            [field(
                "spec.environments.production.promote_from",
                "environment_source_conflict"
            )]
        );
        assert_eq!(
            codes(&format!(
                "{CONNECTION}[spec.environments.staging]\ncommit='{}'\nsync=true\n",
                "a".repeat(40)
            )),
            [field(
                "spec.environments.staging.sync",
                "environment_sync_pinned"
            )]
        );
        assert_eq!(
            codes(&format!(
                "{CONNECTION}[spec.environments.staging]\nbranch='a..b'\n"
            )),
            [field(
                "spec.environments.staging.branch",
                "git_branch_invalid"
            )]
        );
        assert_eq!(
            codes("[spec.environments.production]\npromote_from='qa'\n"),
            [field(
                "spec.environments.production.promote_from",
                "promotion_source_invalid"
            )]
        );
        assert_eq!(
            codes(
                "[spec.environments.a]\npromote_from='b'\n[spec.environments.b]\npromote_from='c'\n[spec.environments.c]\npromote_from='a'\n"
            ),
            [field("spec.environments.c.promote_from", "promotion_cycle")]
        );
        assert_eq!(
            codes("[spec.environments.a]\npromote_from='a'\n"),
            [field("spec.environments.a.promote_from", "promotion_cycle")]
        );
    }

    #[test]
    fn changes_create_resource_and_report_what_the_manifest_dropped() {
        let staging = environment("staging", EnvironmentSource::Saved, false);
        let production = environment(
            "production",
            EnvironmentSource::Branch(TrackedBranch::new("main".into(), None).unwrap()),
            false,
        );
        let old = environment(
            "old",
            EnvironmentSource::Promoted(PromotedFrom {
                environment: staging.id.clone(),
            }),
            false,
        );
        let declarations = [
            Declaration {
                name: name("staging"),
                source: branch("main", None),
                sync: true,
                manages_source: true,
            },
            Declaration {
                name: name("production"),
                source: DeclaredSource::Promoted {
                    from: name("staging"),
                },
                sync: false,
                manages_source: true,
            },
            Declaration {
                name: name("qa"),
                source: branch("main", None),
                sync: false,
                manages_source: true,
            },
        ];
        let changes = EnvironmentChange::between(
            &declarations,
            &[old.clone(), production.clone(), staging.clone()],
        );
        assert_eq!(
            changes.iter().map(ToString::to_string).collect::<Vec<_>>(),
            [
                "staging: deploys from branch main instead of the saved manifest",
                "staging: follows pushes",
                "production: deploys from releases promoted from staging instead of branch main",
                "qa: created, deploys from branch main",
                "old: no longer declared; kept until deleted with `piquelctl env delete`",
            ]
        );
        // Nothing is managed without declarations.
        assert_eq!(
            EnvironmentChange::between(&[], &[old, production, staging]),
            []
        );
    }
}
