//! Reports about saved applications: listings, details, their services, routes and links, manifests and plans.

use super::super::HumanWriter;
use super::{EnvironmentRow, Names, Row};
use crate::support::join_or;
use piqueld_client::{
    ApplicationSummary, ApplicationView, Export, FileSource, Import, Page, PlanView, RouteUrl,
    SecretDeclaration, SecretGenerator, SecretMount, ServiceFile, Source, Template,
    sync::{WebhookSecret, WebhookView},
    system::RouteStatus,
};
use serde::Serialize;
use std::io;

/// One file or secret mount of `file list` and `service secret list`, with
/// the service mounting it.
#[derive(Serialize)]
pub(crate) struct ServiceRow<T> {
    pub(crate) service: String,
    #[serde(flatten)]
    pub(crate) item: T,
}

report!(Vec<ServiceRow<ServiceFile>> as [ServiceRow<ServiceFile>], self, out, {
    out.table(self, "No files.", ["SERVICE", "TARGET", "MODE", "SOURCE"], |row| {
        let source = match &row.item.source {
            FileSource::Path(path) => format!("repository {path}"),
            FileSource::Content(content) => format!("inline, {} bytes", content.len()),
        };
        [row.service.clone(), row.item.target.clone(), row.item.mode.to_string(), source]
    })
});

report!(Vec<ServiceRow<SecretMount>> as [ServiceRow<SecretMount>], self, out, {
    out.table(self, "No secret mounts.", ["SERVICE", "SECRET", "TARGET"], |row| {
        [row.service.clone(), row.item.name.to_string(), row.item.target.clone()]
    })
});

// `export list` result.
report!(Vec<Export> as [Export], self, out, {
    out.table(self, "No exports.", ["SERVICE", "APPLICATIONS", "PREVIEWS"], |export| {
        [
            export.service.clone(),
            export.applications.join(","),
            if export.previews { "yes" } else { "no" }.into(),
        ]
    })
});

// `generated-secret list` result.
report!(Vec<SecretDeclaration> as [SecretDeclaration], self, out, {
    out.table(self, "No generated secrets declared.", ["NAME", "GENERATES"], |secret| {
        let generates = match secret.generate {
            SecretGenerator::Random { bytes, encoding } => {
                format!("{bytes} random bytes, {encoding}")
            }
            SecretGenerator::Rsa { bits } => format!("{bits}-bit RSA key"),
        };
        [secret.name.clone(), generates]
    })
});

// `import list` result.
report!(Vec<Import> as [Import], self, out, {
    out.table(self, "No imports.", ["NAME", "ENVIRONMENT"], |import| {
        [
            format!("{}.{}", import.service, import.application),
            import
                .environment
                .as_ref()
                .map_or("same name", Template::as_str)
                .into(),
        ]
    })
});

/// One deployed route of `route list`, with its environment's name.
#[derive(Serialize)]
pub(crate) struct RouteRow {
    pub(crate) environment: String,
    #[serde(flatten)]
    pub(crate) route: RouteStatus,
}

report!(Vec<RouteRow> as [RouteRow], self, out, {
    out.table(
        self,
        "No deployed routes.",
        [
            "HOSTNAME",
            "NAME",
            "ENVIRONMENT",
            "VISIBILITY",
            "STATE",
            "DESTINATION",
            "DNS",
            "RECORDS",
        ],
        |row| {
            let route = &row.route;
            Row {
                cells: [
                    route.hostname.clone(),
                    route.name.as_ref().map_or("-", |name| name.as_str()).into(),
                    row.environment.clone(),
                    route.visibility.to_string(),
                    route.state.clone(),
                    route.target.to_string(),
                    route.dns_state.to_string(),
                    route.dns.to_string(),
                ],
                // What keeps an unready route from serving.
                note: (route.state != "ready").then(|| route.message.clone()),
            }
        },
    )
});

/// One `app list` row.
#[derive(Serialize)]
pub(crate) struct ApplicationRow {
    pub(crate) application: ApplicationSummary,
    pub(crate) environments: Vec<EnvironmentRow>,
}

report!(Page<ApplicationRow>, self, out, {
    out.table(
        &self.items,
        "No applications.",
        ["NAME", "ENVIRONMENTS", "GENERATION", "ID"],
        |row| {
            [
                row.application.name.clone(),
                join_or(
                    row.environments.iter().map(|environment| {
                        format!("{} {}", environment.environment.name, environment.state())
                    }),
                    "none",
                ),
                row.application.generation.to_string(),
                row.application.id.to_string(),
            ]
        },
    )
});

/// `app show` result: configuration, environments with their state, and
/// per-service sources.
#[derive(Serialize)]
pub(crate) struct ShowReport<'a> {
    pub(crate) application: &'a ApplicationView,
    pub(crate) environments: &'a [EnvironmentRow],
}
report!(ShowReport<'_>, self, out, {
    let app = self.application;
    let spec = app.application.spec();
    out.line(format_args!(
        "{} ({})",
        app.application.metadata().name,
        app.application.id()
    ))?;
    out.blank()?;
    out.label("Configuration revision", app.generation)?;
    if let Some(connection) = &spec.manifest {
        out.label("Sync", connection.sync)?;
        if let Some(check) = &app.sync_check {
            match &check.error {
                Some(error) => out.label("Last sync check", format_args!("failed: {error}"))?,
                None => out.label("Last sync check", "succeeded")?,
            }
        }
    }
    for row in self.environments {
        out.label(
            "Environment",
            format_args!(
                "{} ({}): {}, deploys from {}",
                row.environment.name,
                row.environment.id,
                row.state(),
                Names(&app.environments).source(&row.environment.source)
            ),
        )?;
    }
    if self.environments.is_empty() {
        out.label("Environments", "none")?;
    }
    if !app.drift.is_empty() {
        out.blank()?;
        out.heading("Differs from the manifest:")?;
        out.environment_changes(&app.drift)?;
    }
    if app.edited() {
        out.blank()?;
        out.heading("Edited since read from the repository (reset with `app repository reset`):")?;
        out.manifest_changes(&app.edits())?;
    }
    for service in &spec.services {
        out.blank()?;
        out.label("Service", &service.name)?;
        out.label("  Replicas", &service.replicas)?;
        match &service.source {
            Source::Image { image } => out.label("  Source", format_args!("image {image}"))?,
            Source::Git { repository, .. } => {
                out.label("  Source", format_args!("git {repository}"))?;
            }
        }
    }
    if !spec.volumes.is_empty() {
        out.label(
            "Named volumes",
            join_or(spec.volumes.iter().map(|volume| &volume.name), ""),
        )?;
        out.line("Named volumes are retained on deletion.")?;
    }
    if !spec.variables.is_empty()
        || !spec.environments.is_empty()
        || !spec.previews.variables.is_empty()
    {
        out.blank()?;
        for (name, value) in &spec.variables {
            out.label("Variable", format_args!("{name} = {value}"))?;
        }
        for (environment, config) in &spec.environments {
            for (name, value) in &config.variables {
                out.label(
                    "Variable",
                    format_args!("{name} = {value} (in {environment})"),
                )?;
            }
        }
        for (name, value) in &spec.previews.variables {
            out.label("Variable", format_args!("{name} = {value} (in previews)"))?;
        }
    }
    Ok(())
});

// `url --route`: only the URL, so it can be used as is; `url` warns while
// it is pending.
report!(RouteUrl, self, out, { out.line(&self.url) });

// `url` result.
report!(Vec<RouteUrl> as [RouteUrl], self, out, {
    if self.is_empty() {
        return out.line("No routes.");
    }
    out.urls(self)
});

// `app repository webhook show` result.
report!(WebhookView, self, out, {
    match &self.url {
        Some(url) => out.label("Payload URL", url)?,
        None => out.label(
            "Payload URL",
            "not served; set ingress.webhook_hostname on the daemon",
        )?,
    }
    out.label("Content type", "application/json")?;
    out.label("Events", "push")?;
    match self.secret_created_at_ms {
        Some(_) => out.label("Secret", "generated; rotate it to see a new one")?,
        None => out.label(
            "Secret",
            "none; generate one with `app repository webhook rotate`",
        )?,
    }
    Ok(())
});

// `app repository webhook rotate` result: the only time the secret is shown.
report!(WebhookSecret, self, out, {
    if let Some(url) = &self.url {
        out.label("Payload URL", url)?;
    }
    out.label("Secret", &self.secret)?;
    out.line("Configure this secret in GitHub now; piqueld never shows it again.")
});

/// Local `app validate` success; JSON carries the manifest's application name.
#[derive(Serialize)]
pub(crate) struct ValidManifestReport {
    pub(crate) application: String,
}
report!(ValidManifestReport, self, out, {
    out.line(format_args!(
        "Manifest is valid for application {:?}.",
        self.application
    ))
});

/// Saved TOML in human mode; a JSON string in machine mode.
pub(crate) struct ManifestReport(pub(crate) String);
report!(ManifestReport as str = 0, self, out, {
    out.line(self.0.trim_end())
});

report!(PlanView, self, out, { Names(&[]).plan(out, self) });

named!(PlanView, plan, names, out, { names.plan(out, plan) });
