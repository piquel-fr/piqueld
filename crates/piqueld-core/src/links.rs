//! Resolves private links between environments from what each one deploys.
//!
//! An import of environment `I` connects to the environment `E` of the
//! imported application named like `I` (or as the import says) once `E`
//! exports the service to `I`'s application, and to previews when `I` is one.
//! Both sides are read from their active targets, so a link appears or goes
//! away when either side deploys, and never from a manifest only saved.
//!
//! Manifests name applications, but deploying binds each name to the
//! application holding it ([`bind`]): links then follow that application,
//! renamed or not, and another one taking a freed name gains nothing.

use crate::manifest::{ValidatedExport, ValidatedImport};
use crate::{ApplicationId, ApplicationName, EnvironmentId, ServiceName};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use utoipa::ToSchema;

/// The application each name an environment's exports and imports use is
/// bound to.
pub type Bindings = BTreeMap<ApplicationName, ApplicationId>;

/// Binds each application `exports` and `imports` name: to the application
/// `previous` bound it to, while that one still exists, else to the one
/// holding the name in `applications`. A name no application holds stays
/// unbound, and matches whichever application holds it until the
/// environment deploys again.
#[must_use]
pub fn bind(
    exports: &[ValidatedExport],
    imports: &[ValidatedImport],
    previous: &Bindings,
    applications: &Bindings,
) -> Bindings {
    exports
        .iter()
        .flat_map(|export| &export.applications)
        .chain(imports.iter().map(|import| &import.application))
        .filter_map(|name| {
            let id = previous
                .get(name)
                .filter(|id| applications.values().any(|existing| existing == *id))
                .or_else(|| applications.get(name))?;
            Some((name.clone(), id.clone()))
        })
        .collect()
}

/// One environment's identity and the links its active target declares.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkParty {
    /// Environment identity.
    pub environment: EnvironmentId,
    /// Its application's identity.
    pub application_id: ApplicationId,
    /// Current name of its application.
    pub application: ApplicationName,
    /// Environment name, or a preview's slug.
    pub name: String,
    /// Whether the environment is a preview.
    pub preview: bool,
    /// Services it lets other applications reach.
    pub exports: Vec<ValidatedExport>,
    /// Services of other applications it reaches.
    pub imports: Vec<ValidatedImport>,
    /// The applications its exports and imports name, as deployed.
    pub bindings: Bindings,
}

/// A link between two environments: the `services` of `exporter` that every
/// service of `importer` reaches as `<service>.<exporter_application>`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Link {
    /// Environment whose services reach the exported ones.
    pub importer: EnvironmentId,
    /// Environment whose services are reached.
    pub exporter: EnvironmentId,
    /// Current name of the exporter's application, which aliases carry.
    pub exporter_application: ApplicationName,
    /// Exported services the importer imports.
    pub services: BTreeSet<ServiceName>,
}

/// Whether an import connects, and why not.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImportState {
    /// The service is reachable.
    Connected,
    /// No environment of that name of the application has deployed.
    EnvironmentMissing,
    /// That environment does not export the service to this application, or
    /// not to previews.
    NotExported,
}

/// One import of an environment and whether it connects.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportStatus {
    /// Application exporting the service.
    pub application: ApplicationName,
    /// Imported service.
    pub service: ServiceName,
    /// Environment of the application the import reaches.
    pub environment: String,
    /// Name the service answers to: `<service>.<application>`.
    pub alias: String,
    /// Whether it connects.
    pub state: ImportState,
}

/// An environment that reaches one of this environment's exported services.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Importer {
    /// Importing application.
    pub application: ApplicationName,
    /// Importing environment, or a preview's slug.
    pub environment: String,
    /// Exported service it reaches.
    pub service: ServiceName,
}

/// The links an environment takes part in, as deployed.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LinkReport {
    /// Each import of the environment and whether it connects.
    pub imports: Vec<ImportStatus>,
    /// Environments that reach its exported services.
    pub importers: Vec<Importer>,
}

impl LinkParty {
    /// The environment `import` reaches: the one it names, else the one named
    /// like this environment.
    fn target<'a>(&'a self, import: &'a ValidatedImport) -> &'a str {
        import
            .environment
            .as_ref()
            .map_or(self.name.as_str(), |environment| environment.as_str())
    }

    /// Whether `name`, as this environment's exports or imports use it, is
    /// `other`'s application: the one it bound the name to, or the one
    /// holding the name while it is unbound.
    fn names(&self, name: &ApplicationName, other: &Self) -> bool {
        self.bindings.get(name).map_or_else(
            || *name == other.application,
            |id| *id == other.application_id,
        )
    }

    /// Whether this environment exports `service` to `importer`'s application.
    fn exports_to(&self, service: &ServiceName, importer: &Self) -> bool {
        self.exports.iter().any(|export| {
            export.service == *service
                && export
                    .applications
                    .iter()
                    .any(|name| self.names(name, importer))
                && (!importer.preview || export.previews)
        })
    }

    /// Resolves `import` among `parties`: the exporter it connects to, or why
    /// it does not. Previews are never exporters.
    fn resolve<'a>(
        &self,
        import: &ValidatedImport,
        parties: &'a [Self],
    ) -> Result<&'a Self, ImportState> {
        let exporter = parties
            .iter()
            .find(|party| {
                !party.preview
                    && self.names(&import.application, party)
                    && party.name == self.target(import)
                    && party.environment != self.environment
            })
            .ok_or(ImportState::EnvironmentMissing)?;
        if exporter.exports_to(&import.service, self) {
            Ok(exporter)
        } else {
            Err(ImportState::NotExported)
        }
    }
}

/// Every link between `parties`, one per importer and exporter pair.
#[must_use]
pub fn links(parties: &[LinkParty]) -> Vec<Link> {
    let mut links = BTreeMap::<(&EnvironmentId, &EnvironmentId), Link>::new();
    for importer in parties {
        for import in &importer.imports {
            if let Ok(exporter) = importer.resolve(import, parties) {
                links
                    .entry((&importer.environment, &exporter.environment))
                    .or_insert_with(|| Link {
                        importer: importer.environment.clone(),
                        exporter: exporter.environment.clone(),
                        exporter_application: exporter.application.clone(),
                        services: BTreeSet::new(),
                    })
                    .services
                    .insert(import.service.clone());
            }
        }
    }
    links.into_values().collect()
}

/// Reports the links of `environment` among `parties`: each of its imports,
/// and the environments importing its services.
#[must_use]
pub fn report(environment: &EnvironmentId, parties: &[LinkParty]) -> LinkReport {
    let Some(party) = parties
        .iter()
        .find(|party| &party.environment == environment)
    else {
        return LinkReport::default();
    };
    let imports = party
        .imports
        .iter()
        .map(|import| ImportStatus {
            application: import.application.clone(),
            service: import.service.clone(),
            environment: party.target(import).to_owned(),
            alias: import.alias(),
            state: party
                .resolve(import, parties)
                .map_or_else(|state| state, |_| ImportState::Connected),
        })
        .collect();
    let mut importers = parties
        .iter()
        .flat_map(|importer| {
            importer.imports.iter().filter_map(move |import| {
                let exporter = importer.resolve(import, parties).ok()?;
                (&exporter.environment == environment).then(|| Importer {
                    application: importer.application.clone(),
                    environment: importer.name.clone(),
                    service: import.service.clone(),
                })
            })
        })
        .collect::<Vec<_>>();
    importers.sort();
    LinkReport { imports, importers }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EnvironmentName;

    fn party(id: &str, application: &str, name: &str) -> LinkParty {
        LinkParty {
            environment: EnvironmentId::parse(format!("{id}-0000000")).unwrap(),
            application_id: ApplicationId::parse(format!("app-{application}")).unwrap(),
            application: ApplicationName::parse(application).unwrap(),
            name: name.into(),
            preview: false,
            exports: Vec::new(),
            imports: Vec::new(),
            bindings: Bindings::new(),
        }
    }

    /// Binds every party's names as deploying each now would.
    fn deployed(mut parties: Vec<LinkParty>) -> Vec<LinkParty> {
        let applications = parties
            .iter()
            .map(|party| (party.application.clone(), party.application_id.clone()))
            .collect::<Bindings>();
        for party in &mut parties {
            party.bindings = bind(
                &party.exports,
                &party.imports,
                &party.bindings,
                &applications,
            );
        }
        parties
    }

    fn export(service: &str, applications: &[&str], previews: bool) -> ValidatedExport {
        ValidatedExport {
            service: ServiceName::parse(service).unwrap(),
            applications: applications
                .iter()
                .map(|application| ApplicationName::parse(*application).unwrap())
                .collect(),
            previews,
        }
    }

    fn import(application: &str, service: &str, environment: Option<&str>) -> ValidatedImport {
        ValidatedImport {
            application: ApplicationName::parse(application).unwrap(),
            service: ServiceName::parse(service).unwrap(),
            environment: environment.map(|name| EnvironmentName::parse(name).unwrap()),
        }
    }

    #[test]
    fn imports_connect_to_the_same_named_environment_that_exports_to_them() {
        let mut auth_staging = party("a", "auth", "staging");
        auth_staging.exports = vec![export("api", &["notes"], false)];
        let mut auth_production = party("b", "auth", "production");
        auth_production.exports = vec![export("api", &["notes"], false)];
        let mut notes = party("c", "notes", "staging");
        notes.imports = vec![import("auth", "api", None)];
        let mut other = party("d", "other", "staging");
        other.imports = vec![import("auth", "api", None)];
        let parties = deployed(vec![auth_staging, auth_production, notes, other]);

        let links = links(&parties);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].importer, parties[2].environment);
        assert_eq!(links[0].exporter, parties[0].environment);
        assert_eq!(links[0].exporter_application.as_str(), "auth");

        let report = report(&parties[3].environment, &parties);
        assert_eq!(report.imports[0].state, ImportState::NotExported);
        assert_eq!(report.imports[0].alias, "api.auth");
        let report = super::report(&parties[0].environment, &parties);
        assert_eq!(report.importers.len(), 1);
        assert_eq!(report.importers[0].application.as_str(), "notes");
    }

    #[test]
    fn imports_name_another_environment_and_previews_need_the_exporters_consent() {
        let mut auth = party("a", "auth", "staging");
        auth.exports = vec![export("api", &["notes"], false)];
        let mut preview = party("b", "notes", "notes-feat-1a2b3c");
        preview.preview = true;
        preview.imports = vec![import("auth", "api", Some("staging"))];
        let mut missing = party("c", "notes", "production");
        missing.imports = vec![import("auth", "api", None)];
        let mut parties = deployed(vec![auth, preview, missing]);

        assert_eq!(links(&parties), Vec::new());
        assert_eq!(
            report(&parties[1].environment, &parties).imports[0].state,
            ImportState::NotExported
        );
        assert_eq!(
            report(&parties[2].environment, &parties).imports[0].state,
            ImportState::EnvironmentMissing
        );

        parties[0].exports[0].previews = true;
        let links = links(&parties);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].importer, parties[1].environment);
    }

    #[test]
    fn links_group_the_services_one_environment_imports_from_another() {
        let mut auth = party("a", "auth", "staging");
        auth.exports = vec![
            export("api", &["notes"], false),
            export("events", &["notes"], false),
            export("admin", &["other"], false),
        ];
        let mut notes = party("b", "notes", "staging");
        notes.imports = vec![
            import("auth", "api", None),
            import("auth", "events", None),
            import("auth", "admin", None),
        ];
        let links = links(&deployed(vec![auth, notes]));
        assert_eq!(links.len(), 1);
        assert_eq!(
            links[0]
                .services
                .iter()
                .map(ServiceName::as_str)
                .collect::<Vec<_>>(),
            ["api", "events"]
        );
    }

    #[test]
    fn grants_follow_the_application_bound_when_deployed_not_its_name() {
        let mut auth = party("a", "auth", "staging");
        auth.exports = vec![export("api", &["notes"], false)];
        let mut notes = party("b", "notes", "staging");
        notes.imports = vec![import("auth", "api", None)];
        let mut parties = deployed(vec![auth, notes]);
        assert_eq!(links(&parties).len(), 1);

        // notes is renamed, and another application takes its name: notes
        // keeps its link, the newcomer gets none, even after auth redeploys.
        parties[1].application = ApplicationName::parse("journal").unwrap();
        let mut impostor = party("c", "notes", "staging");
        impostor.application_id = ApplicationId::parse("app-impostor").unwrap();
        impostor.imports = vec![import("auth", "api", None)];
        parties.push(impostor);
        let parties = deployed(parties);
        let links = links(&parties);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].importer, parties[1].environment);
        assert_eq!(
            report(&parties[2].environment, &parties).imports[0].state,
            ImportState::NotExported
        );
    }

    #[test]
    fn unbound_names_match_the_application_holding_them() {
        let mut auth = party("a", "auth", "staging");
        auth.exports = vec![export("api", &["notes"], false)];
        // auth deploys before notes exists, so its grant stays unbound.
        let auth = deployed(vec![auth]).remove(0);
        assert_eq!(auth.bindings, Bindings::new());
        let mut notes = party("b", "notes", "staging");
        notes.imports = vec![import("auth", "api", None)];
        let notes = deployed(vec![auth.clone(), notes]).remove(1);
        assert_eq!(links(&[auth, notes]).len(), 1);
    }
}
