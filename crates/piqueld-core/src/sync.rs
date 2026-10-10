//! Deploying repository-backed applications on push.
//!
//! The application's repository connection (`spec.manifest.sync`) says how
//! piqueld learns that branches moved: by polling them, or when GitHub sends a
//! push webhook. Either way piqueld lists the branch heads itself, with one
//! `git ls-remote` per repository, and deploys every environment and preview
//! whose branch head moved past the commit it last synced. A webhook only
//! hints that something moved; its payload is never trusted.

use crate::api::EnvironmentView;
use crate::manifest::{RepositoryManifest, ValidationError};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// How an application follows pushes to its manifest repository. Applies to
/// every preview, and to every environment following an unpinned branch that
/// opted in.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "mode", rename_all = "lowercase", deny_unknown_fields)]
pub enum RepositorySync {
    /// Deploy only when asked.
    #[default]
    Off,
    /// List the branch heads every `interval_seconds`.
    Poll {
        /// Seconds between listings, from 60 to 86400.
        #[serde(default = "RepositorySync::default_interval")]
        interval_seconds: u32,
    },
    /// List the branch heads when GitHub reports a push.
    Webhook,
}

impl RepositorySync {
    /// Default seconds between polls.
    pub const DEFAULT_INTERVAL: u32 = 300;
    /// Fewest seconds between polls.
    pub const MIN_INTERVAL: u32 = 60;
    /// Most seconds between polls: one day.
    pub const MAX_INTERVAL: u32 = 86_400;

    const fn default_interval() -> u32 {
        Self::DEFAULT_INTERVAL
    }

    /// Whether pushes deploy nothing.
    #[must_use]
    pub const fn is_off(&self) -> bool {
        matches!(self, Self::Off)
    }

    /// Records `sync_interval_invalid` at `path` for an interval out of range.
    pub(crate) fn validate(self, path: &str, errors: &mut Vec<ValidationError>) {
        if let Self::Poll { interval_seconds } = self
            && !(Self::MIN_INTERVAL..=Self::MAX_INTERVAL).contains(&interval_seconds)
        {
            errors.push(ValidationError {
                code: "sync_interval_invalid".into(),
                path: format!("{path}.interval_seconds"),
                message: format!(
                    "the poll interval must be between {} and {} seconds",
                    Self::MIN_INTERVAL,
                    Self::MAX_INTERVAL
                ),
            });
        }
    }
}

/// ```text
/// off    poll every 300s    webhook
/// ```
impl std::fmt::Display for RepositorySync {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => formatter.write_str("off"),
            Self::Poll { interval_seconds } => write!(formatter, "poll every {interval_seconds}s"),
            Self::Webhook => formatter.write_str("webhook"),
        }
    }
}

/// The branch head as of an environment's or preview's last deployment, by
/// sync or of its own branch. Sync deploys once the branch moves past it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct SyncedHead {
    /// Full commit hash.
    pub commit: String,
    /// When sync found it, in Unix milliseconds.
    pub at_ms: i64,
}

/// Whether pushes deploy an environment or preview, and why not.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// Pushes to its branch deploy it.
    Following,
    /// Its application does not sync, or it deploys the saved manifest.
    Off,
    /// It is pinned to a commit, so pushes never move it.
    Pinned,
    /// This environment has not opted into its application's sync.
    NotOptedIn,
    /// It follows its branch from its next deployment: it was never
    /// deployed, or its branch or repository changed since.
    AwaitingDeployment,
}

impl SyncState {
    /// Whether sync lists its branch: it follows pushes, or will once a
    /// deployment, possibly running, records the head it follows from.
    #[must_use]
    pub const fn listed(self) -> bool {
        matches!(self, Self::Following | Self::AwaitingDeployment)
    }
}

impl std::fmt::Display for SyncState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Following => "following",
            Self::Off => "off",
            Self::Pinned => "pinned",
            Self::NotOptedIn => "not opted in",
            Self::AwaitingDeployment => "waiting for a deployment",
        })
    }
}

impl EnvironmentView {
    /// Whether pushes deploy this environment or preview, given its
    /// application's repository `connection`. Environments opt in; previews
    /// follow their application's setting.
    #[must_use]
    pub fn sync_state(&self, connection: Option<&RepositoryManifest>) -> SyncState {
        match (connection, self.source.branch()) {
            (None, _) | (_, None) => SyncState::Off,
            (Some(connection), _) if connection.sync.is_off() => SyncState::Off,
            (_, Some(branch)) if branch.commit().is_some() => SyncState::Pinned,
            _ if self.preview().is_none() && !self.sync => SyncState::NotOptedIn,
            _ if self.synced.is_none() => SyncState::AwaitingDeployment,
            _ => SyncState::Following,
        }
    }
}

/// The automated actors piqueld records on what they caused, next to
/// accounts and the host operator.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub enum SystemActor {
    /// Sync found a moved branch while polling.
    #[serde(rename = "sync:poll")]
    SyncPoll,
    /// Sync found a moved branch after a GitHub push webhook.
    #[serde(rename = "sync:webhook")]
    SyncWebhook,
}

impl SystemActor {
    /// Stored and displayed form, e.g. `sync:poll`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SyncPoll => "sync:poll",
            Self::SyncWebhook => "sync:webhook",
        }
    }

    /// Parses the stored form.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        [Self::SyncPoll, Self::SyncWebhook]
            .into_iter()
            .find(|actor| actor.as_str() == value)
    }
}

impl std::fmt::Display for SystemActor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The last time sync listed an application's branches, and why it failed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct SyncCheck {
    /// When the listing finished, in Unix milliseconds.
    pub checked_at_ms: i64,
    /// Why the repository could not be listed; sync retries with backoff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Where GitHub sends an application's push webhooks, and its secret.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct WebhookView {
    /// Payload URL to configure in GitHub; absent until the daemon sets
    /// `ingress.webhook_hostname`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// When the current secret was generated; absent until one is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_created_at_ms: Option<i64>,
}

/// A newly generated webhook secret. It is shown only in this response.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct WebhookSecret {
    /// Payload URL to configure in GitHub, when exposed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Secret to configure in GitHub, which signs each delivery with it.
    pub secret: String,
    /// When it was generated, in Unix milliseconds.
    pub created_at_ms: i64,
}

/// Opts one environment into or out of its application's sync.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentSyncRequest {
    /// Whether pushes deploy it while its application syncs.
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::GitRepository;
    use crate::{ApplicationId, TrackedBranch};
    use crate::{EnvironmentId, EnvironmentKind, EnvironmentName, EnvironmentSource};

    fn view(commit: Option<&str>, sync: bool, synced: bool) -> EnvironmentView {
        EnvironmentView {
            id: EnvironmentId::parse("env-0000000001").unwrap(),
            application_id: ApplicationId::parse("app-0000000001").unwrap(),
            name: EnvironmentName::default_name(),
            source: EnvironmentSource::Branch(
                TrackedBranch::new("main".into(), commit.map(Into::into)).unwrap(),
            ),
            kind: EnvironmentKind::Environment,
            sync,
            synced: synced.then(|| SyncedHead {
                commit: "0123456789abcdef0123456789abcdef01234567".into(),
                at_ms: 1,
            }),
            resolved_generation: None,
            delete_intent: false,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn only_deployed_opted_in_unpinned_branches_of_a_syncing_application_follow_pushes() {
        let mut connection = RepositoryManifest {
            repository: GitRepository {
                url: "https://example.com/notes.git".into(),
                branch: "main".into(),
                commit: None,
            },
            path: "app.toml".into(),
            sync: RepositorySync::Webhook,
        };
        let pinned = "0123456789abcdef0123456789abcdef01234567";
        let state = |commit, sync, synced, connection: Option<&RepositoryManifest>| {
            view(commit, sync, synced).sync_state(connection)
        };
        let syncing = Some(&connection);
        assert_eq!(state(None, true, true, syncing), SyncState::Following);
        assert_eq!(
            state(None, true, false, syncing),
            SyncState::AwaitingDeployment
        );
        assert_eq!(state(Some(pinned), true, true, syncing), SyncState::Pinned);
        assert_eq!(state(None, false, true, syncing), SyncState::NotOptedIn);
        assert_eq!(state(None, true, true, None), SyncState::Off);
        connection.sync = RepositorySync::Off;
        assert_eq!(state(None, true, true, Some(&connection)), SyncState::Off);
    }

    #[test]
    fn sync_settings_decode_with_a_default_interval_and_validate_its_range() {
        let decode = |toml: &str| toml::from_str::<RepositorySync>(toml).unwrap();
        assert_eq!(
            decode("mode = 'poll'"),
            RepositorySync::Poll {
                interval_seconds: RepositorySync::DEFAULT_INTERVAL
            }
        );
        assert_eq!(decode("mode = 'webhook'"), RepositorySync::Webhook);
        let mut errors = Vec::new();
        RepositorySync::Poll {
            interval_seconds: 10,
        }
        .validate("spec.manifest.sync", &mut errors);
        assert_eq!(errors[0].path, "spec.manifest.sync.interval_seconds");
    }
}
