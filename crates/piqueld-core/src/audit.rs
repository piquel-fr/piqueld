//! Audit trail contracts: who did what through the API, with what outcome.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Whether an audited request was allowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    /// The request was authorized and succeeded.
    Allowed,
    /// The request was refused: missing credentials or permission, or a
    /// target hidden from the caller.
    Denied,
    /// The request was authorized but failed.
    Failed,
}

impl AuditOutcome {
    /// Stable storage and wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Denied => "denied",
            Self::Failed => "failed",
        }
    }

    /// Parses a stored name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        [Self::Allowed, Self::Denied, Self::Failed]
            .into_iter()
            .find(|outcome| outcome.as_str() == value)
    }
}

/// One audited API request. Accounts and credentials are copied when
/// recorded, so records outlive them. Request and response bodies are never
/// recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AuditEvent {
    /// Ordering cursor.
    pub id: i64,
    /// Unix milliseconds.
    pub created_at_ms: i64,
    /// Method and route template, e.g. `POST /api/v1/applications/{id}/deploy`.
    pub action: String,
    /// Whether the request was allowed.
    pub outcome: AuditOutcome,
    /// HTTP response status.
    pub status: u16,
    /// Account that made the request, or signed in by it.
    pub user_id: Option<String>,
    /// That account's name when recorded.
    pub username: Option<String>,
    /// Credential that authenticated the request.
    pub credential_id: Option<String>,
    /// `browser`, `cli`, or `token`.
    pub credential_kind: Option<String>,
    /// Whether that credential was limited to its own grants.
    pub scoped: Option<bool>,
    /// Network address the daemon saw; absent over the Unix socket.
    pub peer: Option<String>,
    /// Request ID, matching `x-request-id` and daemon logs.
    pub request_id: Option<String>,
    /// Application the request addressed: by ID, or as the owner of the
    /// environment it addressed when that environment existed.
    pub application_id: Option<String>,
    /// Environment the request addressed by ID.
    pub environment_id: Option<String>,
    /// Permission a refused request lacked.
    pub permission: Option<String>,
}

impl AuditEvent {
    /// What the request addressed, e.g. `application app-blog` or
    /// `environment env-staging of app-blog`.
    #[must_use]
    pub fn target(&self) -> Option<String> {
        match (&self.environment_id, &self.application_id) {
            (Some(environment), Some(application)) if environment != application => {
                Some(format!("environment {environment} of {application}"))
            }
            (Some(environment), _) => Some(format!("environment {environment}")),
            (None, Some(application)) => Some(format!("application {application}")),
            (None, None) => None,
        }
    }
}

/// Audit trail selection, newest first.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AuditFilter {
    /// Only this account's requests. Callers without `audit:read` always see
    /// only their own.
    pub user_id: Option<String>,
    /// Only requests by accounts that had this username when making them.
    pub username: Option<String>,
    /// Only requests made with this credential.
    pub credential_id: Option<String>,
    /// Only requests with this outcome.
    pub outcome: Option<AuditOutcome>,
}

/// Result of checking the audit trail's hash chain.
///
/// Each record stores a SHA-256 link over its predecessor's link and its own
/// fields, so editing, inserting, or removing a record breaks every later
/// link. Removing the newest records leaves a valid but shorter chain:
/// compare `head` with a value kept elsewhere to detect that.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AuditVerification {
    /// Records whose links were checked.
    pub checked: u64,
    /// Records written before the chain existed, which carry no link.
    pub unlinked: u64,
    /// Link of the newest record, if any record is linked.
    pub head: Option<String>,
    /// First record whose link does not match, if the chain is broken.
    pub broken_at: Option<i64>,
}
