//! Application editor logic: the page and tab a route opens, what the editor
//! derives from the saved application, and the Links tab's rows.

use piqueld_client::{
    ApplicationManifest, ApplicationView, DeploymentOrigin, EnvironmentSource, EnvironmentView,
    Export, Import, TrackedBranch,
    access::{AppPermission, Grants},
    sync::short_commit,
};

/// Tabs of the application and service pages.
pub const APPLICATION_TABS: [&str; 14] = [
    "Overview",
    "Environments",
    "Previews",
    "Services",
    "Source",
    "Variables",
    "Routes",
    "Volumes",
    "Jobs",
    "Links",
    "Secrets",
    "Releases",
    "Builds",
    "Events",
];

/// Tabs of environment and preview pages.
pub const ENVIRONMENT_TABS: [&str; 6] = [
    "Overview",
    "Deployments",
    "Secrets",
    "Logs",
    "Terminal",
    "Events",
];

/// Which page of an application the route shows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Page {
    /// Shared configuration, environments, and application-wide history.
    Application,
    /// One service's shared configuration.
    Service(String),
    /// One environment's runtime, deployments, secrets, logs, terminal, and history.
    Environment(String),
    /// One preview's branch, runtime, deployments, secrets, logs, terminal, and history.
    Preview(String),
}

impl Page {
    /// Tabs of the page; service pages have their own section tabs.
    #[must_use]
    pub const fn tabs(&self) -> &'static [&'static str] {
        match self {
            Self::Environment(_) | Self::Preview(_) => &ENVIRONMENT_TABS,
            Self::Application | Self::Service(_) => &APPLICATION_TABS,
        }
    }

    /// The accessible name of the page's tab list.
    #[must_use]
    pub const fn tabs_label(&self) -> &'static str {
        match self {
            Self::Environment(_) | Self::Preview(_) => "Environment sections",
            Self::Application | Self::Service(_) => "Application sections",
        }
    }

    /// The initial tab, reading query parameters with `query`: `?deployment=`
    /// opens deployments, `?release=` releases, and `?tab=` names a tab by
    /// its `slug`. Tabs the page lacks open the overview.
    #[must_use]
    pub fn initial_tab(&self, query: impl Fn(&str) -> Option<String>) -> &'static str {
        let requested = if query("deployment").is_some() {
            Some("deployments".to_owned())
        } else if query("release").is_some() {
            Some("releases".to_owned())
        } else {
            query("tab")
        };
        self.tabs()
            .iter()
            .find(|tab| requested.as_deref() == Some(slug(tab).as_str()))
            .unwrap_or(&"Overview")
    }
}

/// `text` as an address or element ID fragment: lowercase, with runs of
/// other characters as one hyphen, so "Command & arguments" is
/// `command-arguments`.
#[must_use]
pub fn slug(text: &str) -> String {
    let mut slug = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_end_matches('-').to_owned()
}

/// What the application editor derives from the saved application.
pub trait ApplicationViewExt {
    /// Name of one of the application's environments or previews; its ID
    /// once it is gone.
    fn environment_name(&self, id: &str) -> String;

    /// Names of the application's environments, without its previews.
    fn environment_names(&self) -> Vec<String>;

    /// Where an environment deploys from, naming a promotion source:
    /// "the saved manifest", "branch main", or "promoted from staging".
    fn describe_source(&self, source: &EnvironmentSource) -> String;

    /// Where a deployment came from: "Built", "Promoted from staging
    /// (deployment …)", "Release redeployed", "Restored from deployment …",
    /// "Rolled back from deployment … to deployment …", or "Local snapshot
    /// 0123456789ab on 89abcdef0123".
    fn describe_origin(&self, origin: &DeploymentOrigin) -> String;

    /// Whether `grants` hold `permission` on the application, or in
    /// `environment`, if that is one of its previews, the previews permission
    /// that also allows it there (see `AppPermission::on_previews`).
    fn allows(&self, grants: &Grants, permission: AppPermission, environment: Option<&str>)
    -> bool;

    /// Whether a repository-backed application at revision `generation`,
    /// last updated at `updated_at_ms`, holds a manifest fetched after this
    /// view's: same revision, updated later.
    fn fetched_since(&self, generation: u64, updated_at_ms: i64) -> bool;

    /// Whether `environment` deploys the saved manifest next: it follows the
    /// branch the repository connection names, and the application read a
    /// manifest from it, which it deploys until a push changes the manifest
    /// there. Previews always deploy their own branch's.
    fn follows_connection(&self, environment: &EnvironmentView) -> bool;

    /// IDs of the promoted environments a release can be deployed into:
    /// those not being deleted.
    fn release_targets(&self) -> Vec<String>;
}

impl ApplicationViewExt for ApplicationView {
    fn environment_name(&self, id: &str) -> String {
        self.deployable(id)
            .map_or_else(|| id.to_owned(), |environment| environment.name.to_string())
    }

    fn environment_names(&self) -> Vec<String> {
        self.environments
            .iter()
            .map(|environment| environment.name.to_string())
            .collect()
    }

    fn describe_source(&self, source: &EnvironmentSource) -> String {
        match source.promoted_from() {
            Some(from) => format!("promoted from {}", self.environment_name(from.as_str())),
            None => source.to_string(),
        }
    }

    fn describe_origin(&self, origin: &DeploymentOrigin) -> String {
        match origin {
            DeploymentOrigin::Build => "Built".into(),
            DeploymentOrigin::Promotion {
                environment,
                deployment,
            } => format!(
                "Promoted from {} (deployment {deployment})",
                self.environment_name(environment.as_str())
            ),
            DeploymentOrigin::Release => "Release redeployed".into(),
            DeploymentOrigin::Restore {
                deployment,
                rolled_back: None,
            } => format!("Restored from deployment {deployment}"),
            DeploymentOrigin::Restore {
                deployment,
                rolled_back: Some(failed),
            } => format!("Rolled back from deployment {failed} to deployment {deployment}"),
            DeploymentOrigin::Local(snapshot) => match &snapshot.base {
                Some(base) => format!(
                    "Local snapshot {} on {}",
                    short_commit(&snapshot.commit),
                    short_commit(base)
                ),
                None => format!(
                    "Local snapshot {}, uploaded complete",
                    short_commit(&snapshot.commit)
                ),
            },
        }
    }

    fn allows(
        &self,
        grants: &Grants,
        permission: AppPermission,
        environment: Option<&str>,
    ) -> bool {
        let holds = |permission| {
            grants
                .require_app(permission, self.application.id())
                .is_ok()
        };
        let preview = environment.is_some_and(|id| {
            self.previews
                .iter()
                .any(|preview| preview.id.as_str() == id)
        });
        holds(permission) || (preview && permission.on_previews().is_some_and(holds))
    }

    fn fetched_since(&self, generation: u64, updated_at_ms: i64) -> bool {
        self.application.spec().manifest.is_some()
            && generation == self.generation
            && updated_at_ms > self.updated_at_ms
    }

    fn follows_connection(&self, environment: &EnvironmentView) -> bool {
        self.application
            .spec()
            .manifest
            .as_ref()
            .is_some_and(|connection| {
                environment.preview().is_none()
                    && self.repository.is_some()
                    && environment.source.branch()
                        == TrackedBranch::of(&connection.repository).ok().as_ref()
            })
    }

    fn release_targets(&self) -> Vec<String> {
        self.environments
            .iter()
            .filter(|env| env.source.promoted_from().is_some() && !env.delete_intent)
            .map(|env| env.id.to_string())
            .collect()
    }
}

/// One editable export. Applications are typed as one comma-separated list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportDraft {
    /// The exported service.
    pub service: String,
    /// Applications allowed to reach it, separated by commas.
    pub applications: String,
    /// Whether previews may reach it too.
    pub previews: bool,
}

impl ExportDraft {
    /// The manifest's exports as editable rows.
    #[must_use]
    pub fn rows(manifest: &ApplicationManifest) -> Vec<Self> {
        manifest
            .spec
            .exports
            .iter()
            .map(|export| Self {
                service: export.service.clone(),
                applications: export.applications.join(", "),
                previews: export.previews,
            })
            .collect()
    }

    /// The export input, skipping blank application names; validation
    /// reports empty or invalid ones.
    #[must_use]
    pub fn export(self) -> Export {
        Export {
            service: self.service,
            applications: self
                .applications
                .split(',')
                .map(str::trim)
                .filter(|application| !application.is_empty())
                .map(str::to_owned)
                .collect(),
            previews: self.previews,
        }
    }
}

/// One editable import. An empty environment reaches the one named like the
/// importing environment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportDraft {
    /// The application exporting the service.
    pub application: String,
    /// The imported service.
    pub service: String,
    /// The exporting environment; empty for the same-named one.
    pub environment: String,
}

impl ImportDraft {
    /// The manifest's imports as editable rows.
    #[must_use]
    pub fn rows(manifest: &ApplicationManifest) -> Vec<Self> {
        manifest
            .spec
            .imports
            .iter()
            .map(|import| Self {
                application: import.application.clone(),
                service: import.service.clone(),
                environment: import
                    .environment
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
            })
            .collect()
    }

    /// The import input.
    #[must_use]
    pub fn import(self) -> Import {
        Import {
            application: self.application,
            service: self.service,
            environment: Some(self.environment)
                .filter(|environment| !environment.is_empty())
                .map(Into::into),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use piqueld_client::{
        ApplicationId, ApplicationSpec, EnvironmentId, EnvironmentKind, EnvironmentName,
        GitRepository, LocalSnapshot, Metadata, PromotedFrom, RepositoryManifest, RunState,
        access::{Permission, Scope},
    };
    use std::collections::BTreeMap;

    const APP: &str = "app-notes-01";

    fn environment(id: &str, name: &str, source: EnvironmentSource) -> EnvironmentView {
        EnvironmentView {
            id: EnvironmentId::parse(id).unwrap(),
            application_id: ApplicationId::parse(APP).unwrap(),
            name: EnvironmentName::parse(name).unwrap(),
            source,
            kind: EnvironmentKind::Environment,
            sync: false,
            synced: None,
            telemetry: false,
            run_state: RunState::Running,
            resolved_generation: None,
            delete_intent: false,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    fn preview(id: &str, branch: &str) -> EnvironmentView {
        let mut preview = environment(id, id, EnvironmentSource::Saved);
        preview.kind = EnvironmentKind::Preview(Box::new(piqueld_client::Preview {
            branch: branch.parse().unwrap(),
            slot: None,
            slug: id.parse().unwrap(),
        }));
        preview
    }

    fn promoted(id: &str, name: &str, from: &str) -> EnvironmentView {
        environment(
            id,
            name,
            EnvironmentSource::Promoted(PromotedFrom {
                environment: EnvironmentId::parse(from).unwrap(),
            }),
        )
    }

    fn branch(name: &str) -> EnvironmentSource {
        EnvironmentSource::Branch(TrackedBranch::new(name.into(), None).unwrap())
    }

    /// Application `notes` at revision 3, updated at 10, reading its
    /// manifest from branch `main` when `connected`.
    fn view(connected: bool, environments: Vec<EnvironmentView>) -> ApplicationView {
        let spec = ApplicationSpec {
            manifest: connected.then(|| RepositoryManifest {
                repository: GitRepository {
                    url: "https://example.com/notes.git".into(),
                    branch: "main".into(),
                    commit: None,
                },
                path: "piqueld.toml".into(),
                sync: piqueld_client::sync::RepositorySync::default(),
            }),
            ..ApplicationSpec::default()
        };
        let application = ApplicationManifest {
            api_version: "piqueld.dev/v1alpha1".into(),
            kind: "Application".into(),
            metadata: Metadata {
                name: "notes".into(),
            },
            spec,
        }
        .validate_template()
        .unwrap()
        .normalize(ApplicationId::parse(APP).unwrap());
        ApplicationView {
            spec_hash: application.spec_hash(),
            application,
            generation: 3,
            delete_intent: false,
            created_at_ms: 1,
            updated_at_ms: 10,
            environments,
            previews: Vec::new(),
            drift: Vec::new(),
            sync_check: None,
            repository: None,
        }
    }

    fn grants(permission: AppPermission, application: &str) -> Grants {
        let mut grants = Grants::default();
        grants
            .grant(
                Permission::App(permission),
                &Scope::one(ApplicationId::parse(application).unwrap()),
            )
            .unwrap();
        grants
    }

    #[test]
    fn query_parameters_open_only_tabs_the_page_has() {
        let query = |pairs: &'static [(&'static str, &'static str)]| {
            let pairs = pairs.iter().copied().collect::<BTreeMap<_, _>>();
            move |key: &str| pairs.get(key).map(|value| (*value).to_owned())
        };
        let environment = Page::Environment("env".into());
        // A deployment link wins over the tab it was opened from.
        assert_eq!(
            environment.initial_tab(query(&[("deployment", "d1"), ("tab", "logs")])),
            "Deployments"
        );
        assert_eq!(environment.initial_tab(query(&[("tab", "logs")])), "Logs");
        assert_eq!(slug("Command & arguments"), "command-arguments");
        // Environment pages have no releases; application pages no deployments or logs.
        assert_eq!(
            environment.initial_tab(query(&[("release", "r1")])),
            "Overview"
        );
        assert_eq!(
            Page::Application.initial_tab(query(&[("deployment", "d1")])),
            "Overview"
        );
        assert_eq!(
            Page::Application.initial_tab(query(&[("tab", "logs")])),
            "Overview"
        );
        assert_eq!(
            Page::Application.initial_tab(query(&[("release", "r1"), ("tab", "jobs")])),
            "Releases"
        );
    }

    #[test]
    fn sources_and_origins_name_environments_or_their_id_once_gone() {
        let saved = view(
            false,
            vec![
                environment("env-production-01", "production", EnvironmentSource::Saved),
                promoted("env-staging-01", "staging", "env-production-01"),
            ],
        );
        let promotion = |environment: &str| DeploymentOrigin::Promotion {
            environment: EnvironmentId::parse(environment).unwrap(),
            deployment: "dep-1".into(),
        };
        assert_eq!(
            saved.describe_origin(&promotion("env-production-01")),
            "Promoted from production (deployment dep-1)"
        );
        assert_eq!(
            saved.describe_origin(&promotion("env-deleted-01")),
            "Promoted from env-deleted-01 (deployment dep-1)"
        );
        assert_eq!(
            saved.describe_source(&saved.environments[1].source),
            "promoted from production"
        );
        assert_eq!(
            saved.describe_origin(&DeploymentOrigin::Restore {
                deployment: "dep-good".into(),
                rolled_back: Some("dep-failed".into()),
            }),
            "Rolled back from deployment dep-failed to deployment dep-good"
        );
        let snapshot = |base: Option<&str>| {
            DeploymentOrigin::Local(LocalSnapshot {
                commit: "0123456789abcdef0123456789abcdef01234567".into(),
                base: base.map(str::to_owned),
            })
        };
        assert_eq!(
            saved.describe_origin(&snapshot(Some("89abcdef0123456789abcdef0123456789abcdef"))),
            "Local snapshot 0123456789ab on 89abcdef0123"
        );
        assert_eq!(
            saved.describe_origin(&snapshot(None)),
            "Local snapshot 0123456789ab, uploaded complete"
        );
    }

    #[test]
    fn previews_permissions_allow_only_in_previews() {
        let mut saved = view(
            false,
            vec![environment(
                "env-production-01",
                "production",
                EnvironmentSource::Saved,
            )],
        );
        saved.previews.push(preview("feature-1a2b3c", "feature"));
        let previews = grants(AppPermission::PreviewsDeploy, APP);
        assert!(
            previews
                .require_app(AppPermission::LogsRead, saved.application.id())
                .is_err()
        );
        assert!(saved.allows(&previews, AppPermission::LogsRead, Some("feature-1a2b3c")));
        assert!(!saved.allows(
            &previews,
            AppPermission::LogsRead,
            Some("env-production-01")
        ));
        assert!(!saved.allows(&previews, AppPermission::LogsRead, None));
        // Previews permissions grant only what `on_previews` names.
        assert!(!saved.allows(&previews, AppPermission::Exec, Some("feature-1a2b3c")));
        // Grants on another application never apply.
        let elsewhere = grants(AppPermission::LogsRead, "app-other-01");
        assert!(!saved.allows(&elsewhere, AppPermission::LogsRead, None));
    }

    #[test]
    fn only_a_later_fetch_of_the_same_revision_counts_as_fetched() {
        let connected = view(true, Vec::new());
        assert!(connected.fetched_since(3, 11));
        assert!(!connected.fetched_since(3, 10), "not updated since");
        assert!(
            !connected.fetched_since(4, 11),
            "a newer revision conflicts"
        );
        assert!(
            !view(false, Vec::new()).fetched_since(3, 11),
            "only repositories are fetched"
        );
    }

    #[test]
    fn only_environments_on_the_connection_branch_deploy_the_saved_manifest() {
        let mut saved = view(true, Vec::new());
        let main = environment("env-production-01", "production", branch("main"));
        assert!(
            !saved.follows_connection(&main),
            "nothing was read from the repository yet"
        );
        saved.repository = Some(saved.application.clone());
        assert!(saved.follows_connection(&main));
        let other = environment("env-staging-01", "staging", branch("develop"));
        assert!(!saved.follows_connection(&other));
        let mut preview = preview("main-1a2b3c", "main");
        preview.source = branch("main");
        assert!(!saved.follows_connection(&preview));
        let mut unconnected = view(false, Vec::new());
        unconnected.repository = Some(unconnected.application.clone());
        assert!(!unconnected.follows_connection(&main));
    }

    #[test]
    fn releases_deploy_only_into_live_promoted_environments() {
        let mut deleting = promoted("env-qa-01", "qa", "env-production-01");
        deleting.delete_intent = true;
        let saved = view(
            false,
            vec![
                environment("env-production-01", "production", EnvironmentSource::Saved),
                promoted("env-staging-01", "staging", "env-production-01"),
                deleting,
            ],
        );
        assert_eq!(saved.release_targets(), ["env-staging-01"]);
    }

    #[test]
    fn export_rows_trim_names_and_drop_blank_ones() {
        let row = ExportDraft {
            service: "api".into(),
            applications: " notes ,, billing, ".into(),
            previews: true,
        };
        let export = row.export();
        assert_eq!(export.applications, ["notes", "billing"]);
        let mut manifest = view(false, Vec::new()).application.to_manifest();
        manifest.spec.exports.push(export);
        assert_eq!(
            ExportDraft::rows(&manifest),
            [ExportDraft {
                service: "api".into(),
                applications: "notes, billing".into(),
                previews: true,
            }]
        );
    }

    #[test]
    fn imports_without_an_environment_reach_the_same_named_one() {
        let row = |environment: &str| ImportDraft {
            application: "auth".into(),
            service: "api".into(),
            environment: environment.into(),
        };
        assert_eq!(row("").import().environment, None);
        let import = row("production").import();
        assert_eq!(
            import
                .environment
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("production")
        );
        let mut manifest = view(false, Vec::new()).application.to_manifest();
        manifest.spec.imports = vec![import, row("").import()];
        assert_eq!(ImportDraft::rows(&manifest), [row("production"), row("")]);
    }
}
