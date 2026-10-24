//! `notification`: destinations, what each receives, test notifications, and
//! the delivery history. Destination URLs come from a file or stdin, never
//! shell arguments or output.
use crate::{
    cli::one_of,
    commands::{fold_applications, resolve_application},
    error::{CliError, ErrorKind, Result},
    output::{Console, reports::report},
    support::read_input,
};
use clap::{
    Args, Subcommand,
    builder::{PossibleValue, PossibleValuesParser, TypedValueParser},
};
use piqueld_client::{
    ApplicationId, Client, Page,
    observability::{
        ApplicationSelection, CategoryRule, DestinationKind, DestinationRequest,
        DestinationSettings, DestinationTest, NotificationCategory, NotificationDelivery,
        NotificationDestination, NotificationMessage,
    },
};
use std::{
    collections::BTreeMap,
    io::{self, Read},
    path::{Path, PathBuf},
};

// `notification` subcommands; `///` on variants and fields is user-facing help.
#[derive(Debug, Subcommand)]
pub(crate) enum NotificationCommand {
    /// List notification destinations and what each receives.
    ///
    /// Their URLs are never shown. Needs `system:read` or
    /// `notifications:manage`.
    List,
    /// Create a notification destination, or change one.
    ///
    /// Options you omit keep the current settings. A new destination needs
    /// `--url-file` or `--url-stdin`. It receives every category but
    /// `deployment_succeeded`, about every application, without previews.
    /// Changing its URL or kind cancels its pending deliveries. Needs
    /// `notifications:manage`.
    Set(SetArgs),
    /// Send a test notification to a destination now.
    ///
    /// It fails unless the receiver acknowledges it. The test skips the
    /// delivery queue and history. Needs `notifications:manage`.
    Test {
        /// Destination name.
        name: String,
    },
    /// Delete a notification destination and cancel its pending deliveries.
    ///
    /// The delivery history keeps its name. Needs `notifications:manage`.
    Delete {
        /// Destination name.
        name: String,
        /// Skip the confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },
    /// List notification deliveries, newest first.
    ///
    /// Needs `system:read`.
    Deliveries {
        /// Read the page after this cursor, which the previous page printed.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Try a failed delivery again, with the destination's current settings.
    ///
    /// Needs `system:operate`.
    Retry {
        /// Delivery ID, from `notification deliveries`.
        id: String,
    },
}

// Destination changes; `///` on fields is user-facing help.
#[derive(Debug, Args)]
pub(crate) struct SetArgs {
    /// Destination name.
    name: String,
    /// Payload format.
    ///
    /// A new destination defaults to `json`.
    #[arg(long, value_parser = one_of!(DestinationKind::ALL, DestinationKind::parse, SetArgs::describe_kind))]
    kind: Option<DestinationKind>,
    /// Read the webhook URL from this file.
    #[arg(long, value_name = "PATH", conflicts_with = "url_stdin")]
    url_file: Option<PathBuf>,
    /// Read the webhook URL from standard input.
    #[arg(long)]
    url_stdin: bool,
    /// Whether to send notifications.
    ///
    /// `false` pauses the destination and cancels its pending deliveries.
    /// `true` resumes it, without sending what happened meanwhile.
    #[arg(long, value_name = "BOOL")]
    enabled: Option<bool>,
    /// Receive only these categories: comma-separated, or `''` for none.
    ///
    /// A category that stays keeps its message.
    #[arg(long, value_delimiter = ',', value_parser = SetArgs::categories())]
    categories: Option<Vec<Option<NotificationCategory>>>,
    /// Send TEXT with each notification of CATEGORY, and receive that category.
    ///
    /// TEXT has 1 to 1000 characters, such as a Discord role mention
    /// `<@&ROLE_ID>`. CATEGORY is one of the values `--categories` accepts.
    /// Repeat it for several categories.
    #[arg(long = "message", value_name = "CATEGORY=TEXT", value_parser = parse_message)]
    messages: Vec<(NotificationCategory, NotificationMessage)>,
    /// Stop sending text with this category's notifications.
    ///
    /// Repeat it for several categories.
    #[arg(
        long = "no-message",
        value_name = "CATEGORY",
        value_parser = one_of!(NotificationCategory::ALL, NotificationCategory::parse)
    )]
    no_messages: Vec<NotificationCategory>,
    /// Receive notifications only about these applications: comma-separated
    /// names or IDs, or `''` for none.
    ///
    /// Daemon-wide notifications, such as daemon failures and security events,
    /// arrive either way.
    #[arg(long, value_delimiter = ',', conflicts_with = "all_apps")]
    apps: Option<Vec<String>>,
    /// Receive notifications about every application, including ones created later.
    #[arg(long)]
    all_apps: bool,
    /// Whether to receive notifications about previews.
    #[arg(long, value_name = "BOOL")]
    previews: Option<bool>,
}

fn parse_category(value: &str) -> std::result::Result<NotificationCategory, String> {
    NotificationCategory::parse(value).ok_or_else(|| {
        let names: Vec<_> = NotificationCategory::ALL
            .iter()
            .map(|c| c.as_str())
            .collect();
        format!("expected one of {}", names.join(", "))
    })
}

fn parse_message(
    value: &str,
) -> std::result::Result<(NotificationCategory, NotificationMessage), String> {
    let (category, text) = value
        .split_once('=')
        .ok_or_else(|| "expected CATEGORY=TEXT".to_owned())?;
    let message = NotificationMessage::parse(text).map_err(|error| error.to_string())?;
    Ok((parse_category(category)?, message))
}

impl SetArgs {
    /// Parses `--categories` values: a category, or `None` for the empty
    /// value `--categories ''` gives, which clap doesn't list.
    fn categories() -> impl TypedValueParser<Value = Option<NotificationCategory>> {
        let listed =
            NotificationCategory::ALL.map(|category| PossibleValue::new(category.as_str()));
        PossibleValuesParser::new(
            listed
                .into_iter()
                .chain([PossibleValue::new("").hide(true)]),
        )
        .map(|value| NotificationCategory::parse(&value))
    }

    /// What a payload format is, for `--help`.
    fn describe_kind(kind: DestinationKind) -> &'static str {
        match kind {
            DestinationKind::Json => "Versioned JSON describing the event",
            DestinationKind::Discord => "A Discord webhook message",
        }
    }

    /// `current` settings, or a new destination's, with the options applied.
    /// Application names resolve to IDs, so renames keep their selection.
    async fn apply(
        &self,
        client: &Client,
        current: Option<&DestinationSettings>,
    ) -> Result<DestinationSettings> {
        let mut settings = current
            .cloned()
            .unwrap_or_else(|| DestinationSettings::new(self.kind.unwrap_or_default()));
        if let Some(kind) = self.kind {
            settings.kind = kind;
        }
        if let Some(enabled) = self.enabled {
            settings.enabled = enabled;
        }
        if let Some(categories) = &self.categories {
            let previous = std::mem::take(&mut settings.categories);
            settings.categories = categories
                .iter()
                .flatten()
                .map(|category| {
                    let rule = previous.get(category).cloned().unwrap_or_default();
                    (*category, rule)
                })
                .collect();
        }
        for (category, message) in &self.messages {
            settings.categories.insert(
                *category,
                CategoryRule {
                    message: Some(message.clone()),
                },
            );
        }
        for category in &self.no_messages {
            if let Some(rule) = settings.categories.get_mut(category) {
                rule.message = None;
            }
        }
        if let Some(applications) = &self.apps {
            let mut selected = std::collections::BTreeSet::new();
            for application in applications.iter().filter(|a| !a.is_empty()) {
                let view = resolve_application(client, application).await?;
                selected.insert(view.application.id().clone());
            }
            settings.applications = ApplicationSelection::Only(selected);
        }
        if self.all_apps {
            settings.applications = ApplicationSelection::All;
        }
        if let Some(previews) = self.previews {
            settings.previews = previews;
        }
        Ok(settings)
    }

    /// Reads the URL from `--url-file` or stdin, if either was given.
    async fn url(&self) -> Result<Option<String>> {
        if self.url_file.is_none() && !self.url_stdin {
            return Ok(None);
        }
        let file = self.url_file.clone();
        // Stdin can block indefinitely; read off the runtime so Ctrl-C stays responsive.
        let url = read_input("destination URL", move || Self::read_url(file.as_deref())).await?;
        Ok(Some(url.trim().to_owned()))
    }

    /// Reads at most 8 KiB, more than any webhook URL needs.
    fn read_url(file: Option<&Path>) -> io::Result<String> {
        let input: Box<dyn Read> = if let Some(path) = file {
            Box::new(std::fs::File::open(path)?)
        } else {
            Box::new(io::stdin())
        };
        let mut url = String::new();
        input.take(8192).read_to_string(&mut url)?;
        Ok(url)
    }
}

impl NotificationCommand {
    /// Runs a notification command. `set` reads the current destination first,
    /// so unset options keep its settings.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        match self {
            Self::List => {
                let destinations = client.notification_destinations().await?;
                let applications = application_names(client).await?;
                console.emit(&DestinationsReport {
                    destinations: &destinations,
                    applications: &applications,
                })
            }
            Self::Set(args) => {
                let destinations = client.notification_destinations().await?;
                let current = destinations
                    .iter()
                    .find(|destination| destination.name.as_str() == args.name);
                let settings = args.apply(client, current.map(|d| &d.settings)).await?;
                let url = args.url().await?;
                if current.is_none() && url.is_none() {
                    return Err(CliError::new(
                        ErrorKind::Input,
                        "a new destination needs --url-file or --url-stdin",
                    ));
                }
                let destination = client
                    .put_notification_destination(&args.name, &DestinationRequest { settings, url })
                    .await?;
                let applications = application_names(client).await?;
                console.emit(&DestinationsReport {
                    destinations: std::slice::from_ref(&destination),
                    applications: &applications,
                })
            }
            Self::Test { name } => {
                let test = client.test_notification_destination(name).await?;
                if let Some(error) = &test.error {
                    return Err(CliError::new(
                        ErrorKind::General,
                        format!("{name} did not acknowledge the test notification: {error}"),
                    ));
                }
                console.emit(&TestReport { name, test })
            }
            Self::Delete { name, yes } => {
                console
                    .confirm(
                        *yes,
                        &format!("Delete notification destination {name:?}, cancelling its pending deliveries?"),
                    )
                    .await?;
                client.delete_notification_destination(name).await?;
                console.emit(&crate::output::reports::NameDeletionReport { deleted: name })
            }
            Self::Deliveries { cursor } => {
                console.emit(&client.notification_deliveries(cursor.as_deref()).await?)
            }
            Self::Retry { id } => {
                client.retry_notification(id).await?;
                console.info(format_args!("Delivery {id} will be retried."))
            }
        }
    }
}

/// Every application's name by ID, to show selections by name.
async fn application_names(client: &Client) -> Result<BTreeMap<ApplicationId, String>> {
    fold_applications(client, BTreeMap::new(), |names, application| {
        names.insert(application.id, application.name);
    })
    .await
}

/// Destinations with their settings; JSON is the destinations.
struct DestinationsReport<'a> {
    destinations: &'a [NotificationDestination],
    /// Application names by ID; applications not listed show their ID.
    applications: &'a BTreeMap<ApplicationId, String>,
}
report!(DestinationsReport<'_> as [NotificationDestination] = destinations, self, out, {
    if self.destinations.is_empty() {
        return out.line("No notification destinations.");
    }
    for destination in self.destinations {
        let settings = &destination.settings;
        out.label(
            destination.name.as_str(),
            format!(
                "{} {}",
                settings.kind,
                if settings.enabled { "enabled" } else { "disabled" }
            ),
        )?;
        let applications = match &settings.applications {
            ApplicationSelection::All => "all".to_owned(),
            ApplicationSelection::Only(ids) if ids.is_empty() => "none".to_owned(),
            ApplicationSelection::Only(ids) => ids
                .iter()
                .map(|id| self.applications.get(id).cloned().unwrap_or_else(|| id.to_string()))
                .collect::<Vec<_>>()
                .join(", "),
        };
        out.label("  Applications", applications)?;
        out.label("  Previews", if settings.previews { "yes" } else { "no" })?;
        if settings.categories.is_empty() {
            out.label("  Categories", "none")?;
        }
        for (category, rule) in &settings.categories {
            match &rule.message {
                Some(message) => out.label(&format!("  {category}"), message.as_str())?,
                None => out.line(format_args!("  {category}"))?,
            }
        }
    }
    Ok(())
});

/// An acknowledged test notification; JSON is the outcome.
struct TestReport<'a> {
    name: &'a str,
    test: DestinationTest,
}
report!(TestReport<'_> as DestinationTest = test, self, out, {
    out.line(format_args!("{} acknowledged the test notification.", self.name))
});

report!(Page<NotificationDelivery>, self, out, {
    if self.items.is_empty() {
        out.line("No notification deliveries.")?;
    }
    for delivery in &self.items {
        out.line(format_args!(
            "{}  {}  {}  {}  {}  {} attempts{}",
            delivery.id,
            delivery.destination,
            delivery.category,
            delivery
                .subject
                .describe()
                .unwrap_or_else(|| "daemon".into()),
            delivery.state,
            delivery.attempts,
            delivery
                .last_error
                .as_ref()
                .map_or_else(String::new, |error| format!("  {error}"))
        ))?;
    }
    out.next_cursor(self.next_cursor.as_deref())
});

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    /// `--categories ''` selects no category, though clap doesn't list the
    /// empty value among the categories.
    #[test]
    fn empty_categories_select_none() {
        let categories = |value: &str| {
            let arguments = [
                "piquelctl",
                "notification",
                "set",
                "ops",
                "--categories",
                value,
            ];
            let cli = crate::cli::Cli::try_parse_from(arguments).unwrap();
            let crate::cli::Command::Notification {
                command: NotificationCommand::Set(set),
            } = cli.command
            else {
                panic!("notification set command");
            };
            set.categories
        };
        assert_eq!(categories(""), Some(vec![None]));
        assert_eq!(
            categories("recovery"),
            Some(vec![Some(NotificationCategory::Recovery)])
        );
    }
}
