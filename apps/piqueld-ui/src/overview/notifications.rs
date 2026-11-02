//! Notification destinations: what each receives, and the draft the
//! destination dialog edits.
use super::ApplicationRow;
use piqueld_client::{
    ApplicationId,
    observability::{
        ApplicationSelection, CategoryRule, DestinationKind, DestinationRequest,
        DestinationSettings, NotificationCategory, NotificationDestination, NotificationMessage,
    },
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// What a destination receives, in a few words: `5 categories · shop, notes
/// · previews`. Selected applications are named from `applications`, or by
/// ID when they are not listed.
pub struct Receives<'a> {
    /// The destination's settings.
    pub settings: &'a DestinationSettings,
    /// The listed applications.
    pub applications: &'a [ApplicationRow],
}

impl fmt::Display for Receives<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.settings.categories.len() {
            1 => f.write_str("1 category · ")?,
            count => write!(f, "{count} categories · ")?,
        }
        match &self.settings.applications {
            ApplicationSelection::All => f.write_str("every application")?,
            ApplicationSelection::Only(ids) if ids.is_empty() => f.write_str("no application")?,
            ApplicationSelection::Only(ids) => {
                for (index, id) in ids.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    match self
                        .applications
                        .iter()
                        .find(|row| &row.application.id == id)
                    {
                        Some(row) => f.write_str(&row.application.name)?,
                        None => write!(f, "{id}")?,
                    }
                }
            }
        }
        if self.settings.previews {
            f.write_str(" · previews")?;
        }
        Ok(())
    }
}

/// The destination the dialog edits. Messages are kept for unchecked
/// categories too, so unchecking one by mistake loses nothing.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is one of the dialog's checkboxes, edited independently"
)]
#[derive(Clone, Debug, PartialEq)]
pub struct DestinationDraft {
    /// Its name.
    pub name: String,
    /// Whether it edits an existing destination, whose name is fixed.
    pub existing: bool,
    /// Its payload format.
    pub kind: DestinationKind,
    /// A new URL; empty keeps an existing destination's.
    pub url: String,
    /// Whether it receives notifications.
    pub enabled: bool,
    /// The categories it receives.
    pub categories: BTreeSet<NotificationCategory>,
    /// The message typed for each category, checked or not.
    pub messages: BTreeMap<NotificationCategory, String>,
    /// Whether it receives every application, ignoring `applications`.
    pub all_applications: bool,
    /// The applications it receives otherwise.
    pub applications: BTreeSet<ApplicationId>,
    /// Whether it receives previews.
    pub previews: bool,
}

impl DestinationDraft {
    /// The draft of `destination`, or of a new Discord destination's defaults.
    #[must_use]
    pub fn new(destination: Option<&NotificationDestination>) -> Self {
        let settings = destination.map_or_else(
            || DestinationSettings::new(DestinationKind::Discord),
            |destination| destination.settings.clone(),
        );
        let (all_applications, applications) = match settings.applications {
            ApplicationSelection::All => (true, BTreeSet::new()),
            ApplicationSelection::Only(ids) => (false, ids),
        };
        Self {
            name: destination.map_or_else(String::new, |d| d.name.to_string()),
            existing: destination.is_some(),
            kind: settings.kind,
            url: String::new(),
            enabled: settings.enabled,
            categories: settings.categories.keys().copied().collect(),
            messages: settings
                .categories
                .iter()
                .filter_map(|(category, rule)| {
                    Some((*category, rule.message.as_ref()?.to_string()))
                })
                .collect(),
            all_applications,
            applications,
            previews: settings.previews,
        }
    }

    /// The destination's name and the request saving it, or why it can't be
    /// saved. Checked categories send their trimmed message, none when it is
    /// blank; an empty URL keeps the saved one.
    ///
    /// # Errors
    ///
    /// A checked category's message is not a valid notification message.
    pub fn request(&self) -> Result<(String, DestinationRequest), String> {
        let categories = self
            .categories
            .iter()
            .map(|&category| {
                let message = self
                    .messages
                    .get(&category)
                    .map(|message| message.trim())
                    .filter(|message| !message.is_empty())
                    .map(NotificationMessage::parse)
                    .transpose()
                    .map_err(|error| format!("{}: {error}", category.label()))?;
                Ok((category, CategoryRule { message }))
            })
            .collect::<Result<_, String>>()?;
        let url = self.url.trim();
        Ok((
            self.name.trim().to_owned(),
            DestinationRequest {
                settings: DestinationSettings {
                    kind: self.kind,
                    enabled: self.enabled,
                    categories,
                    applications: if self.all_applications {
                        ApplicationSelection::All
                    } else {
                        ApplicationSelection::Only(self.applications.clone())
                    },
                    previews: self.previews,
                },
                url: (!url.is_empty()).then(|| url.to_owned()),
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overview::fixtures::application;
    use serde_json::json;

    /// Destination `ops` receiving build failures with a mention, and
    /// deployment failures, about `shop` and its previews.
    fn destination() -> NotificationDestination {
        serde_json::from_value(json!({
            "name": "ops",
            "settings": {
                "kind": "json",
                "enabled": false,
                "categories": {
                    "build_failures": { "message": "<@&42>" },
                    "deployment_failures": {},
                },
                "applications": { "only": ["app-shop0001"] },
                "previews": true,
            },
            "created_at_ms": 0,
            "updated_at_ms": 0,
        }))
        .expect("destination")
    }

    #[test]
    fn an_unedited_destination_saves_its_settings_and_keeps_its_url() {
        let destination = destination();
        let draft = DestinationDraft::new(Some(&destination));
        assert!(draft.existing);
        let (name, request) = draft.request().expect("request");
        assert_eq!(name, "ops");
        assert_eq!(request.settings, destination.settings);
        assert_eq!(request.url, None);
    }

    #[test]
    fn only_checked_categories_send_their_trimmed_messages() {
        let mut draft = DestinationDraft::new(Some(&destination()));
        draft.name = "  ops ".into();
        draft.url = " https://hooks.example.com/a ".into();
        // An unchecked category's message is not sent.
        draft
            .categories
            .remove(&NotificationCategory::BuildFailures);
        draft
            .categories
            .insert(NotificationCategory::DaemonFailures);
        draft.messages.insert(
            NotificationCategory::DeploymentFailures,
            "  on call  ".into(),
        );
        draft
            .messages
            .insert(NotificationCategory::DaemonFailures, "   ".into());
        let (name, request) = draft.request().expect("request");
        assert_eq!(name, "ops");
        assert_eq!(request.url.as_deref(), Some("https://hooks.example.com/a"));
        let messages = request
            .settings
            .categories
            .iter()
            .map(|(category, rule)| (*category, rule.message.as_ref().map(ToString::to_string)))
            .collect::<Vec<_>>();
        assert_eq!(
            messages,
            [
                (
                    NotificationCategory::DeploymentFailures,
                    Some("on call".into())
                ),
                (NotificationCategory::DaemonFailures, None),
            ]
        );
    }

    #[test]
    fn an_invalid_message_names_its_category() {
        let mut draft = DestinationDraft::new(None);
        draft.messages.insert(
            NotificationCategory::BuildFailures,
            "x".repeat(NotificationMessage::MAX_CHARS + 1),
        );
        let error = draft.request().expect_err("too long");
        assert!(error.starts_with("Build failures: "), "{error}");
        // The same message on an unchecked category is not sent, so it saves.
        draft
            .categories
            .remove(&NotificationCategory::BuildFailures);
        assert!(draft.request().is_ok());
    }

    #[test]
    fn receives_names_listed_applications_and_falls_back_to_ids() {
        let mut shop = application("app-shop0001", Vec::new(), &[]);
        shop.application.name = "shop".into();
        let applications = [shop];
        let mut settings = destination().settings;
        let text = |settings: &DestinationSettings| {
            Receives {
                settings,
                applications: &applications,
            }
            .to_string()
        };
        settings.applications = ApplicationSelection::Only(
            ["app-shop0001", "app-gone0001"]
                .map(|id| ApplicationId::parse(id).expect("id"))
                .into(),
        );
        assert_eq!(
            text(&settings),
            "2 categories · app-gone0001, shop · previews"
        );
        settings.applications = ApplicationSelection::Only(BTreeSet::new());
        settings.previews = false;
        assert_eq!(text(&settings), "2 categories · no application");
        settings.applications = ApplicationSelection::All;
        assert_eq!(text(&settings), "2 categories · every application");
        settings.categories.pop_first();
        assert_eq!(text(&settings), "1 category · every application");
    }
}
