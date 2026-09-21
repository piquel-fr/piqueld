//! Typed diagnostic policy; wire strings are decoded only at storage/API boundaries.
use super::{Diagnostic, EventScope};

macro_rules! diagnostic_codes {
    ($($(#[$meta:meta])* $variant:ident => $wire:literal),+ $(,)?) => {
        /// Failure classifications produced by this daemon.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum DiagnosticCode {
            $($(#[$meta])* $variant),+
        }
        impl DiagnosticCode {
            /// Stable storage and API representation.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $wire),+ }
            }
            /// Decodes a classification read from storage or an API response.
            #[must_use]
            pub fn parse(code: &str) -> Option<Self> {
                match code { $($wire => Some(Self::$variant)),+, _ => None }
            }
        }
    };
}

diagnostic_codes! {
    /// Docker Engine cannot be reached.
    DockerUnavailable => "docker_unavailable",
    /// Docker is not an active Swarm manager.
    SwarmManagerUnavailable => "swarm_manager_unavailable",
    /// The Swarm topology is unsupported.
    SwarmTopologyUnsupported => "swarm_topology_unsupported",
    /// Action intent or completion could not be persisted.
    JournalUnavailable => "journal_unavailable",
    /// Control-plane storage cannot be accessed.
    StorageUnavailable => "storage_unavailable",
    /// Persisted state could not be decoded or validated.
    StoredStateCorrupt => "stored_state_corrupt",
    /// The database schema is incompatible.
    SchemaMismatch => "schema_mismatch",
    /// An unexpected daemon failure occurred.
    InternalError => "internal_error",
    /// Validated application inputs could not be compiled.
    ApplicationCompilationFailed => "application_compilation_failed",
    /// Image resolution failed transiently.
    ImageResolutionFailed => "image_resolution_failed",
    /// The registry rejected the image or credentials.
    ImageResolutionRejected => "image_resolution_rejected",
    /// A Docker request failed.
    DockerRequestFailed => "docker_request_failed",
    /// The runtime did not converge before its deadline.
    ConvergenceTimeout => "convergence_timeout",
    /// Repository checkout or image build failed.
    GitBuildFailed => "git_build_failed",
    /// Docker paused a failed service update.
    ServiceUpdateFailed => "service_update_failed",
    /// A runtime resource is not safely owned.
    OwnershipConflict => "ownership_conflict",
    /// An immutable runtime configuration conflicts.
    DockerConfigurationConflict => "docker_configuration_conflict",
    /// The repository manifest is absent.
    ManifestNotFound => "manifest_not_found",
    /// The repository manifest is invalid.
    ManifestInvalid => "manifest_invalid",
    /// The manifest repository could not be fetched.
    ManifestFetchFailed => "manifest_fetch_failed",
    /// Saved configuration could not be rendered.
    ManifestSerializationFailed => "manifest_serialization_failed",
    /// The secret master key is missing, unreadable, or does not match stored values.
    SecretStorageUnavailable => "secret_storage_unavailable",
    /// Pinned secret values were discarded and need replacement.
    SecretUnavailable => "secret_unavailable",
    /// A Docker request failed local validation.
    ValidationFailed => "validation_failed",
    /// The runtime plan cannot execute safely.
    PlanBlocked => "plan_blocked",
    /// The operation was cancelled.
    Cancelled => "cancelled",
    /// A newer operation superseded this one.
    Superseded => "superseded",
    /// Observed application health is degraded.
    ServiceDegraded => "service_degraded",
    /// A route hostname is reserved by another application.
    HostnameConflict => "hostname_conflict",
    /// The managed gateway could not apply a routing transition.
    IngressUnavailable => "ingress_unavailable",
}

const INSPECT_DIAGNOSTIC: &str =
    "Inspect the diagnostic and related events; resolve the cause before retrying.";
const AUTOMATIC_RETRY: &str =
    "Reconciliation will retry. Inspect the affected resource if the failure persists.";

impl DiagnosticCode {
    // Every classification must explicitly select ownership, retryability and guidance.
    // Adding a variant cannot silently inherit application retention or retry policy.
    const fn policy(self) -> (EventScope, bool, &'static str) {
        use EventScope::{Application, Daemon};
        match self {
            Self::DockerUnavailable => (
                Daemon,
                true,
                "Check Docker Engine availability. Reconciliation retries after connectivity recovers.",
            ),
            Self::JournalUnavailable | Self::StorageUnavailable => (
                Daemon,
                true,
                "Restore writable control-plane storage before infrastructure changes can resume.",
            ),
            Self::SwarmManagerUnavailable
            | Self::SwarmTopologyUnsupported
            | Self::StoredStateCorrupt
            | Self::SchemaMismatch
            | Self::InternalError
            | Self::ApplicationCompilationFailed => (Daemon, false, INSPECT_DIAGNOSTIC),
            Self::ImageResolutionFailed | Self::DockerRequestFailed => {
                (Application, true, AUTOMATIC_RETRY)
            }
            Self::ImageResolutionRejected => (
                Application,
                false,
                "Check the image reference and registry credentials, then retry the deployment.",
            ),
            Self::GitBuildFailed => (
                Application,
                false,
                "Open the build output, fix the source or build configuration, then deploy again.",
            ),
            Self::ServiceUpdateFailed => (
                Application,
                false,
                "Inspect service health and logs. The previous healthy task may still be running.",
            ),
            Self::ConvergenceTimeout => (
                Application,
                true,
                "Inspect service health and resource capacity. Reconciliation will retry.",
            ),
            Self::IngressUnavailable => (
                Application,
                true,
                "Check ingress health and daemon logs. Reconciliation reapplies routes from durable intent.",
            ),
            Self::HostnameConflict => (
                Application,
                false,
                "Choose a hostname that no other application reserves, then deploy again.",
            ),
            Self::OwnershipConflict | Self::DockerConfigurationConflict => (
                Application,
                false,
                "Inspect the conflicting resource and resolve its ownership or immutable configuration.",
            ),
            Self::SecretStorageUnavailable => (
                Daemon,
                false,
                "Restore the original secrets.key, owned by the daemon user with private permissions. If it is lost, `piquelctl secrets recover-key` recovers by discarding stored values. Secret metadata remains readable.",
            ),
            Self::SecretUnavailable => (
                Application,
                false,
                "Supply replacement values for the listed secrets, then start a new deployment.",
            ),
            Self::ManifestNotFound
            | Self::ManifestInvalid
            | Self::ManifestFetchFailed
            | Self::ManifestSerializationFailed
            | Self::ValidationFailed
            | Self::PlanBlocked
            | Self::Cancelled
            | Self::Superseded
            | Self::ServiceDegraded => (Application, false, INSPECT_DIAGNOSTIC),
        }
    }
}

impl Diagnostic {
    /// Creates a diagnostic with the policy of a compile-time classification.
    #[must_use]
    pub fn new(id: String, code: DiagnosticCode, summary: String) -> Self {
        Self::from_recorded_code(id, code.as_str(), summary)
    }

    /// Decodes a code already stored in history or returned by an API boundary.
    /// Unknown historical codes keep their identity and the legacy application
    /// ownership/non-retryable policy. New producers should use `Self::new`.
    #[must_use]
    pub fn from_recorded_code(id: String, code: &str, summary: String) -> Self {
        let (scope, retryable, next_action) = DiagnosticCode::parse(code).map_or(
            (EventScope::Application, false, INSPECT_DIAGNOSTIC),
            DiagnosticCode::policy,
        );
        Self {
            id,
            code: code.to_owned(),
            summary,
            causes: Vec::new(),
            retryable,
            next_action: next_action.to_owned(),
            scope,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_policy_preserves_ownership_retryability_and_wire_codes() {
        for code in [
            DiagnosticCode::DockerUnavailable,
            DiagnosticCode::SwarmManagerUnavailable,
            DiagnosticCode::SwarmTopologyUnsupported,
            DiagnosticCode::JournalUnavailable,
            DiagnosticCode::StorageUnavailable,
            DiagnosticCode::StoredStateCorrupt,
            DiagnosticCode::SchemaMismatch,
            DiagnosticCode::InternalError,
            DiagnosticCode::ApplicationCompilationFailed,
        ] {
            let diagnostic = Diagnostic::new("occurrence".into(), code, "failure".into());
            assert_eq!(diagnostic.scope, EventScope::Daemon, "{code:?}");
            assert_eq!(DiagnosticCode::parse(code.as_str()), Some(code));
            assert_eq!(
                serde_json::to_value(&diagnostic).unwrap()["code"],
                code.as_str()
            );
        }
        for (code, retryable) in [
            (DiagnosticCode::DockerUnavailable, true),
            (DiagnosticCode::JournalUnavailable, true),
            (DiagnosticCode::StorageUnavailable, true),
            (DiagnosticCode::ImageResolutionFailed, true),
            (DiagnosticCode::DockerRequestFailed, true),
            (DiagnosticCode::ConvergenceTimeout, true),
            (DiagnosticCode::IngressUnavailable, true),
            (DiagnosticCode::HostnameConflict, false),
            (DiagnosticCode::ImageResolutionRejected, false),
            (DiagnosticCode::OwnershipConflict, false),
            (DiagnosticCode::ServiceDegraded, false),
        ] {
            let diagnostic = Diagnostic::new("occurrence".into(), code, "failure".into());
            assert_eq!(diagnostic.retryable, retryable, "{code:?}");
            assert!(!diagnostic.next_action.is_empty());
        }
        let unknown = Diagnostic::from_recorded_code(
            "legacy".into(),
            "legacy_failure",
            "Retained historical failure".into(),
        );
        assert_eq!(unknown.code, "legacy_failure");
        assert_eq!(unknown.scope, EventScope::Application);
        assert!(!unknown.retryable);
    }
}
