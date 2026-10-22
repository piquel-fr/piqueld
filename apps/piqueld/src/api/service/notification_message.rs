//! What a notification says: a Discord message, or the versioned JSON
//! envelope, built from its event and what it is about.
use piqueld_core::{
    Event,
    observability::{NotificationCategory, NotificationSubject},
};
use serde_json::{Value, json};

/// One notification, ready to render for a destination.
pub(super) struct Notification<'a> {
    /// What it reports; `None` for a test notification.
    pub(super) category: Option<NotificationCategory>,
    pub(super) event: &'a Event,
    /// Names of the application and environment it is about.
    pub(super) subject: &'a NotificationSubject,
    /// The destination's text for the category.
    pub(super) message: Option<&'a str>,
}

/// How a notification looks: an emoji, a title and a Discord embed colour.
struct Heading {
    emoji: &'static str,
    title: &'static str,
    color: u32,
}

const RED: u32 = 0x00ED_4245;
const AMBER: u32 = 0x00F0_B232;
const GREEN: u32 = 0x0057_F287;
const BLURPLE: u32 = 0x0058_65F2;
const BLUE: u32 = 0x0034_98DB;

impl Notification<'_> {
    /// The emoji, title and colour of its category, so what went wrong shows
    /// at a glance.
    const fn heading(&self) -> Heading {
        let (emoji, title, color) = match self.category {
            Some(NotificationCategory::BuildFailures) => ("❌", "Build failed", RED),
            Some(NotificationCategory::DeploymentFailures) => ("❌", "Deployment failed", RED),
            Some(NotificationCategory::ServiceDegradation) => ("⚠️", "Service degraded", AMBER),
            Some(NotificationCategory::DaemonFailures) => ("🛑", "Daemon failure", RED),
            Some(NotificationCategory::Recovery) => ("✅", "Recovered", GREEN),
            Some(NotificationCategory::DeploymentSucceeded) => ("🚀", "Deployed", GREEN),
            Some(NotificationCategory::Security) => ("🔐", "Security event", BLURPLE),
            Some(NotificationCategory::DaemonStarted) => ("🔄", "Daemon started", BLUE),
            None => ("🔔", "Test notification", BLURPLE),
        };
        Heading {
            emoji,
            title,
            color,
        }
    }

    /// What happened, in a sentence.
    fn summary(&self) -> &str {
        self.event
            .message
            .as_deref()
            .or_else(|| self.event.diagnostic.as_ref().map(|d| d.summary.as_str()))
            .unwrap_or(match self.category {
                Some(NotificationCategory::DeploymentSucceeded) => "The deployment succeeded.",
                Some(NotificationCategory::Recovery) => "Deployments succeed again.",
                _ => &self.event.kind,
            })
    }

    /// The versioned JSON envelope a `json` destination receives.
    pub(super) fn json(&self, delivery: &str, instance: &str) -> Value {
        json!({
            "version": 2,
            "instance_id": instance,
            "delivery_id": delivery,
            "category": self.category,
            "title": self.heading().title,
            "subject": self.subject,
            "message": self.message,
            "event": self.event,
        })
    }

    /// The Discord message a `discord` destination receives: the category's
    /// message as its text, and an embed with the coloured title, what it is
    /// about, the next step and the event's time, linking to the dashboard
    /// at `dashboard` (the daemon's public URL).
    pub(super) fn discord(&self, dashboard: &str) -> Value {
        let heading = self.heading();
        let mut title = format!("{} {}", heading.emoji, heading.title);
        if let Some(subject) = self.subject.describe() {
            title = format!("{title} · {subject}");
        }
        let mut fields = Vec::new();
        let mut field = |name: &str, value: String, inline: bool| {
            fields.push(json!({"name": name, "value": truncate(&value, 1024), "inline": inline}));
        };
        if let Some(application) = &self.subject.application {
            field("Application", application.to_string(), true);
        }
        if let Some(environment) = &self.subject.environment {
            let name = if self.subject.preview {
                "Preview"
            } else {
                "Environment"
            };
            field(name, environment.to_string(), true);
        }
        if let Some(branch) = &self.subject.branch {
            field("Branch", format!("`{branch}`"), true);
        }
        if let Some(diagnostic) = &self.event.diagnostic {
            if !diagnostic.causes.is_empty() {
                field("Cause", diagnostic.causes.join("\n"), false);
            }
            field("Next step", diagnostic.next_action.clone(), false);
        }
        let host = dashboard
            .split_once("://")
            .map_or(dashboard, |(_, host)| host);
        let mut embed = json!({
            "title": truncate(&title, 256),
            "description": truncate(self.summary(), 2000),
            "color": heading.color,
            "fields": fields,
            "footer": {"text": format!("piqueld · {host}")},
            "timestamp": rfc3339(self.event.created_at_ms),
        });
        if let Some(path) = self.link() {
            embed["url"] = json!(format!("{dashboard}{path}"));
        }
        // Only the content pings: it is the category message alone, the
        // operator's own text. Mentions in embeds never notify.
        json!({
            "content": self.message.unwrap_or_default(),
            "embeds": [embed],
            "allowed_mentions": {"parse": ["users", "roles", "everyone"]},
        })
    }

    /// The dashboard page showing more: the diagnostic, else the environment
    /// or preview, else the application.
    fn link(&self) -> Option<String> {
        let event = self.event;
        if let Some(diagnostic) = &event.diagnostic {
            return Some(format!("/dashboard/errors/{}", diagnostic.id));
        }
        let application = event.application_id.as_ref()?;
        Some(match &event.environment_id {
            Some(environment) if self.subject.preview => {
                format!("/dashboard/applications/{application}/previews/{environment}")
            }
            Some(environment) => {
                format!("/dashboard/applications/{application}/environments/{environment}")
            }
            None => format!("/dashboard/applications/{application}"),
        })
    }
}

/// `value` cut to at most `max` characters, ending with `…` when cut, as
/// Discord rejects longer embed texts.
fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    let mut cut: String = value.chars().take(max - 1).collect();
    cut.push('…');
    cut
}

/// `ms` since the Unix epoch as an RFC 3339 UTC timestamp, such as
/// `2026-10-22T12:33:42.269Z`, which Discord shows in each reader's time zone.
fn rfc3339(ms: i64) -> String {
    let (days, time) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    // Howard Hinnant's `civil_from_days`.
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        time / 3_600_000,
        time / 60_000 % 60,
        time / 1000 % 60,
        time % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use piqueld_core::{ApplicationId, EnvironmentId, observability::Diagnostic};

    #[test]
    fn timestamps_are_rfc3339_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(rfc3339(1_792_672_425_063), "2026-10-22T12:33:45.063Z");
    }

    /// A failure names its application and environment, with the next step,
    /// and links to its diagnostic; only the category message can ping.
    #[test]
    fn discord_messages_name_what_failed_without_identifiers() {
        let diagnostic = Diagnostic::new(
            "diagnostic-1".into(),
            piqueld_core::observability::DiagnosticCode::ServiceUpdateFailed,
            "Service web did not become healthy".into(),
        );
        let event = Event {
            id: 48_213,
            application_id: Some(ApplicationId::parse("app-00000001").unwrap()),
            environment_id: Some(EnvironmentId::parse("env-00000001").unwrap()),
            kind: "operation_failed".into(),
            diagnostic: Some(diagnostic),
            created_at_ms: 1_792_672_425_063,
            ..Event::default()
        };
        let subject = NotificationSubject {
            application: Some("shop".parse().unwrap()),
            environment: Some("feat-checkout".parse().unwrap()),
            preview: true,
            branch: Some("feat/checkout".parse().unwrap()),
        };
        let notification = Notification {
            category: Some(NotificationCategory::DeploymentFailures),
            event: &event,
            subject: &subject,
            message: Some("<@&1234>"),
        };
        let message = notification.discord("https://piqueld.example");
        let embed = &message["embeds"][0];
        assert_eq!(message["content"], "<@&1234>");
        assert_eq!(
            embed["title"],
            "❌ Deployment failed · shop / feat-checkout"
        );
        assert_eq!(embed["description"], "Service web did not become healthy");
        assert_eq!(
            embed["url"],
            "https://piqueld.example/dashboard/errors/diagnostic-1"
        );
        assert_eq!(embed["timestamp"], "2026-10-22T12:33:45.063Z");
        assert_eq!(embed["footer"]["text"], "piqueld · piqueld.example");
        let fields: Vec<_> = embed["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|field| field["name"].as_str().unwrap())
            .collect();
        assert_eq!(fields, ["Application", "Preview", "Branch", "Next step"]);
        let rendered = message.to_string();
        assert!(!rendered.contains("48213") && !rendered.contains("env-00000001"));
    }
}
