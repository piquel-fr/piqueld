//! Pure, deterministic desired/observed planning.

use crate::resource::{
    Convergence, DesiredApplication, DesiredNetwork, DesiredSecret, DesiredService, DesiredVolume,
    InstanceId, ObservedApplication, OwnershipState, ResolutionRequirement,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fmt::Write as _,
};
use utoipa::ToSchema;

/// Request used to generate a runtime plan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanRequest {
    /// Reconcile the desired application with observed runtime state.
    Reconcile {
        /// Desired application state.
        desired: DesiredApplication,
    },
    /// Remove runtime resources for an application.
    Delete {
        /// Application identity.
        application_id: crate::ApplicationId,
        /// Owning control-plane instance.
        instance_id: InstanceId,
    },
    /// Will create a Plan with actions to resolve the resolution requirements
    /// and optionally reconcile with a desired application afterwards.
    Preview {
        /// Resolution requirements that have not yet been satisfied.
        unresolved: Vec<ResolutionRequirement>,
        /// Optional desired application to append after resolution.
        desired: Option<DesiredApplication>,
    },
}

/// Risk classification for a plan action.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActionRisk {
    /// No runtime or data impact.
    None,
    /// May temporarily affect service availability.
    Availability,
    /// Touches data-adjacent resources such as volumes.
    DataAdjacent,
    /// Removes or irreversibly changes runtime resources.
    Destructive,
}

/// Reason explaining why a plan action is required.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActionReason {
    /// The desired resource does not exist.
    Missing,
    /// The observed resource differs from desired state.
    Drift {
        /// Desired fields that differ from observation.
        fields: Vec<String>,
    },
    /// The observed resource is no longer desired.
    Obsolete,
    /// The action waits for a prior runtime change.
    ConvergencePending,
    /// A mutable source still needs resolution.
    ResolutionRequired,
    /// A secret generation changed.
    SecretGenerationChanged,
    /// The application is being deleted.
    ApplicationDeletion,
    /// The volume is intentionally retained.
    VolumeRetentionPolicy,
}

/// Concrete action that can appear in a runtime plan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActionKind {
    /// Resolve an image source.
    ResolveImage {
        /// Logical service name.
        service: String,
        /// Requested image reference.
        reference: String,
    },
    /// Resolve a Git source.
    ResolveGit {
        /// Logical service name.
        service: String,
        /// Repository URL.
        repository: String,
        /// Git reference.
        reference: String,
    },
    /// Build an image from a resolved source.
    BuildImage {
        /// Logical service name.
        service: String,
    },
    /// Push a built image to its registry.
    PushImage {
        /// Logical service name.
        service: String,
    },
    /// Ensure a desired network exists.
    EnsureNetwork {
        /// Desired network state.
        network: DesiredNetwork,
    },
    /// Ensure a desired volume exists.
    EnsureVolume {
        /// Desired volume state.
        volume: DesiredVolume,
    },
    /// Ensure a desired secret exists.
    EnsureSecret {
        /// Desired secret state.
        secret: DesiredSecret,
    },
    /// Ensure a desired service exists.
    EnsureService {
        /// Desired service state.
        service: Box<DesiredService>,
    },
    /// Wait for a service to converge.
    WaitForService {
        /// Logical service name.
        service: String,
    },
    /// Wait for a service to be removed.
    WaitForServiceRemoval {
        /// Logical service name.
        service: String,
    },
    /// Wait until a secret is no longer in use.
    WaitForSecretUnused {
        /// Docker secret name.
        name: String,
    },
    /// Remove a service.
    RemoveService {
        /// Docker service name.
        name: String,
    },
    /// Remove a private network.
    RemoveNetwork {
        /// Docker network name.
        name: String,
    },
    /// Remove a secret.
    RemoveSecret {
        /// Docker secret name.
        name: String,
    },
    /// Retain a volume by policy.
    RetainVolume {
        /// Docker volume name.
        name: String,
    },
    /// Wait for a newly generated secret to be adopted.
    AwaitSecretGeneration {
        /// Logical secret name.
        logical_name: String,
    },
}
impl ActionKind {
    /// Returns the stable machine-readable action name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::ResolveImage { .. } => "resolve_image",
            Self::ResolveGit { .. } => "resolve_git",
            Self::BuildImage { .. } => "build_image",
            Self::PushImage { .. } => "push_image",
            Self::EnsureNetwork { .. } => "ensure_network",
            Self::EnsureVolume { .. } => "ensure_volume",
            Self::EnsureSecret { .. } => "ensure_secret",
            Self::EnsureService { .. } => "ensure_service",
            Self::WaitForService { .. } => "wait_for_service",
            Self::WaitForServiceRemoval { .. } => "wait_for_service_removal",
            Self::WaitForSecretUnused { .. } => "wait_for_secret_unused",
            Self::RemoveService { .. } => "remove_service",
            Self::RemoveNetwork { .. } => "remove_network",
            Self::RemoveSecret { .. } => "remove_secret",
            Self::RetainVolume { .. } => "retain_volume",
            Self::AwaitSecretGeneration { .. } => "await_secret_generation",
        }
    }
}
impl fmt::Display for ActionKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (verb, resource) = match self {
            Self::ResolveImage { service, .. } => ("RESOLVE IMAGE", service.as_str()),
            Self::ResolveGit { service, .. } => ("RESOLVE GIT", service.as_str()),
            Self::BuildImage { service } => ("BUILD IMAGE", service.as_str()),
            Self::PushImage { service } => ("PUSH IMAGE", service.as_str()),
            Self::EnsureNetwork { network } => ("ENSURE NETWORK", network.name.as_str()),
            Self::EnsureVolume { volume } => ("ENSURE VOLUME", volume.name.as_str()),
            Self::EnsureSecret { secret } => ("ENSURE SECRET", secret.logical_name.as_str()),
            Self::EnsureService { service } => ("ENSURE SERVICE", service.logical_name.as_str()),
            Self::WaitForService { service } => ("WAIT SERVICE", service.as_str()),
            Self::WaitForServiceRemoval { service } => ("WAIT SERVICE REMOVAL", service.as_str()),
            Self::WaitForSecretUnused { name } => ("WAIT SECRET UNUSED", name.as_str()),
            Self::RemoveService { name } => ("REMOVE SERVICE", name.as_str()),
            Self::RemoveNetwork { name } => ("REMOVE NETWORK", name.as_str()),
            Self::RemoveSecret { name } => ("REMOVE SECRET", name.as_str()),
            Self::RetainVolume { name } => ("RETAIN VOLUME", name.as_str()),
            Self::AwaitSecretGeneration { logical_name } => ("AWAIT SECRET", logical_name.as_str()),
        };
        write!(formatter, "{verb} {resource}")
    }
}

/// One ordered action in a runtime plan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanAction {
    /// Stable action sequence number.
    pub sequence: u32,
    /// Action details.
    pub kind: ActionKind,
    /// Why the action is present.
    pub reason: ActionReason,
    /// Risk classification.
    pub risk: ActionRisk,
    /// Whether the action mutates runtime state.
    pub mutates_runtime: bool,
    /// Whether the action is destructive.
    pub destructive: bool,
}
impl PlanAction {
    /// Returns a stable, concise line suitable for a CLI or operation log.
    #[must_use]
    pub fn human_description(&self) -> String {
        self.kind.to_string()
    }

    /// Creates an unsequenced action with a derived destructive flag.
    #[must_use]
    pub fn new(
        kind: ActionKind,
        reason: ActionReason,
        risk: ActionRisk,
        mutates_runtime: bool,
    ) -> Self {
        Self {
            sequence: 0,
            destructive: risk == ActionRisk::Destructive,
            kind,
            reason,
            risk,
            mutates_runtime,
        }
    }

    /// Creates a non-mutating service convergence wait.
    #[must_use]
    pub fn wait_for_service(service: &str) -> Self {
        Self::new(
            ActionKind::WaitForService {
                service: service.into(),
            },
            ActionReason::ConvergencePending,
            ActionRisk::None,
            false,
        )
    }

    /// Creates a non-mutating service removal wait.
    #[must_use]
    pub fn wait_for_service_removal(service: &str) -> Self {
        Self::new(
            ActionKind::WaitForServiceRemoval {
                service: service.into(),
            },
            ActionReason::ConvergencePending,
            ActionRisk::None,
            false,
        )
    }

    /// Creates a non-mutating secret usage wait.
    #[must_use]
    pub fn wait_for_secret_unused(name: &str) -> Self {
        Self::new(
            ActionKind::WaitForSecretUnused { name: name.into() },
            ActionReason::ConvergencePending,
            ActionRisk::None,
            false,
        )
    }

    /// Returns a bounded operation-step identifier.
    #[must_use]
    pub fn operation_step(&self) -> String {
        let value = format!("{self}");
        if value.len() <= 64 {
            return value;
        }
        let digest = Sha256::digest(value.as_bytes());
        let mut suffix = String::with_capacity(16);
        for byte in digest.iter().take(8) {
            write!(&mut suffix, "{byte:02x}").expect("writing to a String cannot fail");
        }
        let prefix_limit = 64 - suffix.len() - 1;
        let mut prefix_end = value.len().min(prefix_limit);
        while !value.is_char_boundary(prefix_end) {
            prefix_end -= 1;
        }
        format!("{}~{suffix}", &value[..prefix_end])
    }
}
impl fmt::Display for PlanAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.kind.fmt(formatter)
    }
}

/// Severity of a planning diagnostic.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    /// Informational diagnostic.
    Info,
    /// Non-blocking warning.
    Warning,
    /// Blocking or otherwise serious diagnostic.
    Error,
}
/// A planner diagnostic attached to a resource.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanDiagnostic {
    /// Stable machine-readable code.
    pub code: String,
    /// Diagnostic severity.
    pub severity: DiagnosticSeverity,
    /// Resource associated with the diagnostic.
    pub resource: String,
    /// Safe human-readable explanation.
    pub message: String,
    /// Whether the diagnostic blocks execution.
    pub blocking: bool,
}

/// Aggregate counts for a generated plan.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanSummary {
    /// Total action count.
    pub action_count: usize,
    /// Count of runtime mutations.
    pub mutation_count: usize,
    /// Count of destructive actions.
    pub destructive_count: usize,
    /// Count of blocking ownership conflicts.
    pub blocking_conflicts: usize,
    /// Action counts grouped by stable action name.
    pub by_action: BTreeMap<String, usize>,
}

/// Ordered runtime plan and its diagnostics.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// Ordered actions to execute.
    pub actions: Vec<PlanAction>,
    /// Diagnostics discovered during planning.
    pub diagnostics: Vec<PlanDiagnostic>,
    /// Aggregate action counts.
    pub summary: PlanSummary,
}
impl Plan {
    /// Returns whether any diagnostic blocks execution.
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        self.diagnostics.iter().any(|d| d.blocking)
    }

    /// Returns whether the plan contains runtime mutations.
    #[must_use]
    pub fn has_mutations(&self) -> bool {
        self.actions.iter().any(|a| a.mutates_runtime)
    }

    /// Builds a plan for the requested desired/observed transition.
    #[must_use]
    pub fn from_request(request: &PlanRequest, observed: &ObservedApplication) -> Self {
        let mut plan = match request {
            PlanRequest::Reconcile { desired } => Self::reconcile(desired, observed),
            PlanRequest::Delete {
                application_id,
                instance_id,
            } => Self::deletion(application_id, instance_id, observed),
            PlanRequest::Preview {
                unresolved,
                desired,
            } => {
                let mut plan = desired
                    .as_ref()
                    .map_or_else(Self::default, |desired| Self::reconcile(desired, observed));
                let mut prefix = Vec::new();
                for requirement in unresolved {
                    let kind = match requirement {
                        ResolutionRequirement::ResolveImage { service, reference } => {
                            ActionKind::ResolveImage {
                                service: service.clone(),
                                reference: reference.clone(),
                            }
                        }
                        ResolutionRequirement::ResolveGit {
                            service,
                            repository,
                            reference,
                        } => ActionKind::ResolveGit {
                            service: service.clone(),
                            repository: repository.clone(),
                            reference: reference.clone(),
                        },
                        ResolutionRequirement::BuildAndPush { service } => {
                            prefix.push(PlanAction::new(
                                ActionKind::BuildImage {
                                    service: service.clone(),
                                },
                                ActionReason::ResolutionRequired,
                                ActionRisk::None,
                                false,
                            ));
                            ActionKind::PushImage {
                                service: service.clone(),
                            }
                        }
                        ResolutionRequirement::ProvideSecretGeneration { logical_name } => {
                            ActionKind::AwaitSecretGeneration {
                                logical_name: logical_name.clone(),
                            }
                        }
                    };
                    prefix.push(PlanAction::new(
                        kind,
                        ActionReason::ResolutionRequired,
                        ActionRisk::None,
                        false,
                    ));
                }
                prefix.append(&mut plan.actions);
                plan.actions = prefix;
                plan
            }
        };
        plan.actions
            .iter_mut()
            .enumerate()
            .for_each(|(index, action)| {
                action.sequence = u32::try_from(index + 1).unwrap_or(u32::MAX);
            });
        plan.diagnostics.sort_by(|left, right| {
            left.resource
                .cmp(&right.resource)
                .then(left.code.cmp(&right.code))
        });
        plan.summary = plan.summarize();
        plan
    }

    fn collision(&mut self, name: &str, seen: &mut BTreeSet<String>) {
        if seen.insert(name.into()) {
            self.diagnostics.push(PlanDiagnostic { code: "unowned_name_collision".into(), severity: DiagnosticSeverity::Error, resource: name.into(), message: "a same-name resource is not owned by this piqueld application and will not be changed".into(), blocking: true });
        }
    }

    fn immutable_drift(&mut self, name: &str, resource: &str) {
        self.diagnostics.push(PlanDiagnostic {
            code: "immutable_configuration_drift".into(),
            severity: DiagnosticSeverity::Error,
            resource: name.into(),
            message: format!(
                "the {resource} configuration differs from the desired immutable settings and cannot be repaired in place"
            ),
            blocking: true,
        });
    }

    fn ignored(&mut self, name: &str) {
        self.diagnostics.push(PlanDiagnostic {
            code: "foreign_resource_ignored".into(),
            severity: DiagnosticSeverity::Info,
            resource: name.into(),
            message: "foreign or unowned resource is outside this plan".into(),
            blocking: false,
        });
    }

    fn summarize(&self) -> PlanSummary {
        let mut by_action = BTreeMap::new();
        for action in &self.actions {
            *by_action.entry(action.kind.name().into()).or_insert(0) += 1;
        }
        PlanSummary {
            action_count: self.actions.len(),
            mutation_count: self
                .actions
                .iter()
                .filter(|action| action.mutates_runtime)
                .count(),
            destructive_count: self
                .actions
                .iter()
                .filter(|action| action.destructive)
                .count(),
            blocking_conflicts: self
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.blocking)
                .count(),
            by_action,
        }
    }
}
impl fmt::Display for Plan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} action(s), {} runtime mutation(s), {} destructive, {} blocking conflict(s)",
            self.summary.action_count,
            self.summary.mutation_count,
            self.summary.destructive_count,
            self.summary.blocking_conflicts
        )
    }
}

/// Builds a plan for a request and the current observed state.
#[must_use]
pub fn plan(request: &PlanRequest, observed: &ObservedApplication) -> Plan {
    Plan::from_request(request, observed)
}

impl Plan {
    fn reconcile(desired: &DesiredApplication, observed: &ObservedApplication) -> Plan {
        let mut plan = Plan::default();
        let mut blocked_names = BTreeSet::new();
        let networks_ready = plan.ensure_networks(desired, observed, &mut blocked_names);
        let volumes_ready = plan.ensure_volumes(desired, observed, &mut blocked_names);
        let secrets_ready = plan.ensure_secrets(desired, observed, &mut blocked_names);
        let infrastructure_ready = networks_ready && volumes_ready && secrets_ready;
        plan.retain_obsolete_volumes(desired, observed);
        let (all_services_converged, mut convergence_actions) =
            plan.ensure_services(desired, observed, &mut blocked_names);
        plan.actions.append(&mut convergence_actions);
        let cleanup_ready = infrastructure_ready && all_services_converged && !plan.is_blocked();
        plan.remove_obsolete_services(desired, observed, cleanup_ready);
        plan.remove_obsolete_networks(desired, observed, cleanup_ready);
        plan.remove_obsolete_secrets(desired, observed, cleanup_ready);
        plan
    }

    fn ensure_networks(
        &mut self,
        desired: &DesiredApplication,
        observed: &ObservedApplication,
        blocked_names: &mut BTreeSet<String>,
    ) -> bool {
        let mut ready = true;
        for network in &desired.networks {
            match observed
                .networks
                .iter()
                .find(|observed_network| observed_network.name == network.name)
            {
                None => {
                    ready = false;
                    self.actions.push(PlanAction::new(
                        ActionKind::EnsureNetwork {
                            network: network.clone(),
                        },
                        ActionReason::Missing,
                        ActionRisk::None,
                        true,
                    ));
                }
                Some(found) if !found.matches_ownership(network, desired) => {
                    ready = false;
                    self.collision(&network.name, blocked_names);
                }
                Some(found)
                    if found.ingress != network.ingress
                        || !found.runtime_configuration_matches
                        || relevant_network_labels(&found.labels)
                            != relevant_network_labels(&network.labels) =>
                {
                    ready = false;
                    self.immutable_drift(&network.name, "network");
                }
                Some(_) => {}
            }
        }
        ready
    }

    fn ensure_volumes(
        &mut self,
        desired: &DesiredApplication,
        observed: &ObservedApplication,
        blocked_names: &mut BTreeSet<String>,
    ) -> bool {
        let mut ready = true;
        for volume in &desired.volumes {
            match observed
                .volumes
                .iter()
                .find(|observed_volume| observed_volume.name == volume.name)
            {
                None => {
                    ready = false;
                    self.actions.push(PlanAction::new(
                        ActionKind::EnsureVolume {
                            volume: volume.clone(),
                        },
                        ActionReason::Missing,
                        ActionRisk::DataAdjacent,
                        true,
                    ));
                }
                Some(found)
                    if OwnershipState::from_labels(
                        &found.labels,
                        &desired.instance_id,
                        &desired.id,
                    ) != OwnershipState::Owned =>
                {
                    ready = false;
                    self.collision(&volume.name, blocked_names);
                }
                Some(found) if !found.runtime_configuration_matches => {
                    ready = false;
                    self.immutable_drift(&volume.name, "volume");
                }
                Some(_) => {}
            }
        }
        ready
    }

    fn retain_obsolete_volumes(
        &mut self,
        desired: &DesiredApplication,
        observed: &ObservedApplication,
    ) {
        let desired_volumes = desired
            .volumes
            .iter()
            .map(|volume| volume.name.as_str())
            .collect::<BTreeSet<_>>();
        for volume in sorted_by_name(&observed.volumes, |volume| &volume.name)
            .into_iter()
            .filter(|volume| !desired_volumes.contains(volume.name.as_str()))
        {
            if OwnershipState::from_labels(&volume.labels, &desired.instance_id, &desired.id)
                == OwnershipState::Owned
            {
                self.actions.push(PlanAction::new(
                    ActionKind::RetainVolume {
                        name: volume.name.clone(),
                    },
                    ActionReason::VolumeRetentionPolicy,
                    ActionRisk::None,
                    false,
                ));
            } else {
                self.ignored(&volume.name);
            }
        }
    }

    fn ensure_secrets(
        &mut self,
        desired: &DesiredApplication,
        observed: &ObservedApplication,
        blocked_names: &mut BTreeSet<String>,
    ) -> bool {
        let mut ready = true;
        for secret in &desired.secrets {
            match observed
                .secrets
                .iter()
                .find(|observed_secret| observed_secret.name == secret.name)
            {
                None => {
                    ready = false;
                    self.actions.push(PlanAction::new(
                        ActionKind::EnsureSecret {
                            secret: secret.clone(),
                        },
                        ActionReason::Missing,
                        ActionRisk::None,
                        true,
                    ));
                }
                Some(found)
                    if OwnershipState::from_labels(
                        &found.labels,
                        &desired.instance_id,
                        &desired.id,
                    ) != OwnershipState::Owned =>
                {
                    ready = false;
                    self.collision(&secret.name, blocked_names);
                }
                Some(_) => {}
            }
        }
        ready
    }

    fn ensure_services(
        &mut self,
        desired: &DesiredApplication,
        observed: &ObservedApplication,
        blocked_names: &mut BTreeSet<String>,
    ) -> (bool, Vec<PlanAction>) {
        let mut all_services_converged = true;
        let mut convergence_actions = Vec::new();
        for service in &desired.services {
            match observed
                .services
                .iter()
                .find(|observed_service| observed_service.name == service.name)
            {
                None => {
                    all_services_converged = false;
                    self.actions.push(PlanAction::new(
                        ActionKind::EnsureService {
                            service: Box::new(service.clone()),
                        },
                        ActionReason::Missing,
                        ActionRisk::Availability,
                        true,
                    ));
                    convergence_actions.push(PlanAction::wait_for_service(&service.name));
                }
                Some(found) if !found.matches_ownership(service, desired) => {
                    all_services_converged = false;
                    self.collision(&service.name, blocked_names);
                }
                Some(found) if !found.matches(service) => {
                    all_services_converged = false;
                    let reason = if unordered_eq(&found.secrets, &service.secrets) {
                        ActionReason::Drift {
                            fields: service_drift(found, service),
                        }
                    } else {
                        ActionReason::SecretGenerationChanged
                    };
                    self.actions.push(PlanAction::new(
                        ActionKind::EnsureService {
                            service: Box::new(service.clone()),
                        },
                        reason,
                        ActionRisk::Availability,
                        true,
                    ));
                    convergence_actions.push(PlanAction::wait_for_service(&service.name));
                }
                Some(found) => match found.convergence {
                    Convergence::Converged => {}
                    Convergence::Updating | Convergence::Degraded => {
                        all_services_converged = false;
                        convergence_actions.push(PlanAction::wait_for_service(&service.name));
                    }
                    Convergence::Failed => {
                        all_services_converged = false;
                        self.diagnostics.push(PlanDiagnostic {
                            code: "service_update_failed".into(),
                            severity: DiagnosticSeverity::Error,
                            resource: service.name.clone(),
                            message: "service update failed; previous secret generations and healthy tasks are retained".into(),
                            blocking: true,
                        });
                    }
                },
            }
        }
        (all_services_converged, convergence_actions)
    }

    fn remove_obsolete_services(
        &mut self,
        desired: &DesiredApplication,
        observed: &ObservedApplication,
        cleanup_ready: bool,
    ) {
        let desired_services = desired
            .services
            .iter()
            .map(|service| service.name.as_str())
            .collect::<BTreeSet<_>>();
        let mut removal_waits = Vec::new();
        for service in sorted_by_name(&observed.services, |service| &service.name)
            .into_iter()
            .filter(|service| !desired_services.contains(service.name.as_str()))
        {
            if service.is_owned_by(&desired.instance_id, &desired.id) {
                if cleanup_ready {
                    self.actions.push(PlanAction::new(
                        ActionKind::RemoveService {
                            name: service.name.clone(),
                        },
                        ActionReason::Obsolete,
                        ActionRisk::Availability,
                        true,
                    ));
                    removal_waits.push(PlanAction::wait_for_service_removal(&service.name));
                }
            } else {
                self.ignored(&service.name);
            }
        }
        self.actions.append(&mut removal_waits);
    }

    fn remove_obsolete_networks(
        &mut self,
        desired: &DesiredApplication,
        observed: &ObservedApplication,
        cleanup_ready: bool,
    ) {
        let desired_networks = desired
            .networks
            .iter()
            .map(|network| network.name.as_str())
            .collect::<BTreeSet<_>>();
        for network in sorted_by_name(&observed.networks, |network| &network.name)
            .into_iter()
            .filter(|network| !desired_networks.contains(network.name.as_str()))
        {
            if OwnershipState::from_labels(&network.labels, &desired.instance_id, &desired.id)
                == OwnershipState::Owned
            {
                if cleanup_ready {
                    self.actions.push(PlanAction::new(
                        ActionKind::RemoveNetwork {
                            name: network.name.clone(),
                        },
                        ActionReason::Obsolete,
                        ActionRisk::Availability,
                        true,
                    ));
                }
            } else {
                self.ignored(&network.name);
            }
        }
    }

    fn remove_obsolete_secrets(
        &mut self,
        desired: &DesiredApplication,
        observed: &ObservedApplication,
        cleanup_ready: bool,
    ) {
        let desired_secrets = desired
            .secrets
            .iter()
            .map(|secret| secret.name.as_str())
            .collect::<BTreeSet<_>>();
        for secret in sorted_by_name(&observed.secrets, |secret| &secret.name)
            .into_iter()
            .filter(|secret| !desired_secrets.contains(secret.name.as_str()))
        {
            if OwnershipState::from_labels(&secret.labels, &desired.instance_id, &desired.id)
                == OwnershipState::Owned
            {
                if cleanup_ready && !secret.in_use {
                    self.actions.push(PlanAction::new(
                        ActionKind::RemoveSecret {
                            name: secret.name.clone(),
                        },
                        ActionReason::Obsolete,
                        ActionRisk::Destructive,
                        true,
                    ));
                }
            } else {
                self.ignored(&secret.name);
            }
        }
    }

    fn deletion(
        application_id: &crate::ApplicationId,
        instance_id: &InstanceId,
        observed: &ObservedApplication,
    ) -> Plan {
        let mut plan = Plan::default();
        let mut removal_waits = Vec::new();
        for service in sorted_by_name(&observed.services, |service| &service.name) {
            if service.is_owned_by(instance_id, application_id) {
                plan.actions.push(PlanAction::new(
                    ActionKind::RemoveService {
                        name: service.name.clone(),
                    },
                    ActionReason::ApplicationDeletion,
                    ActionRisk::Availability,
                    true,
                ));
                removal_waits.push(PlanAction::wait_for_service_removal(&service.name));
            } else {
                plan.ignored(&service.name);
            }
        }
        plan.actions.append(&mut removal_waits);
        for network in sorted_by_name(&observed.networks, |network| &network.name) {
            if OwnershipState::from_labels(&network.labels, instance_id, application_id)
                == OwnershipState::Owned
            {
                plan.actions.push(PlanAction::new(
                    ActionKind::RemoveNetwork {
                        name: network.name.clone(),
                    },
                    ActionReason::ApplicationDeletion,
                    ActionRisk::Availability,
                    true,
                ));
            } else {
                plan.ignored(&network.name);
            }
        }
        for secret in sorted_by_name(&observed.secrets, |secret| &secret.name) {
            if OwnershipState::from_labels(&secret.labels, instance_id, application_id)
                == OwnershipState::Owned
            {
                if secret.in_use {
                    plan.actions
                        .push(PlanAction::wait_for_secret_unused(&secret.name));
                }
                plan.actions.push(PlanAction::new(
                    ActionKind::RemoveSecret {
                        name: secret.name.clone(),
                    },
                    ActionReason::ApplicationDeletion,
                    ActionRisk::Destructive,
                    true,
                ));
            } else {
                plan.ignored(&secret.name);
            }
        }
        for volume in sorted_by_name(&observed.volumes, |volume| &volume.name) {
            if OwnershipState::from_labels(&volume.labels, instance_id, application_id)
                == OwnershipState::Owned
            {
                plan.actions.push(PlanAction::new(
                    ActionKind::RetainVolume {
                        name: volume.name.clone(),
                    },
                    ActionReason::VolumeRetentionPolicy,
                    ActionRisk::None,
                    false,
                ));
            } else {
                plan.ignored(&volume.name);
            }
        }
        plan
    }
}
fn sorted_by_name<T, F>(values: &[T], name: F) -> Vec<&T>
where
    F: Fn(&T) -> &str,
{
    let mut values = values.iter().collect::<Vec<_>>();
    values.sort_by(|left, right| name(left).cmp(name(right)));
    values
}
fn unordered_eq<T: Ord>(observed: &[T], desired: &[T]) -> bool {
    let mut observed = observed.iter().collect::<Vec<_>>();
    let mut desired = desired.iter().collect::<Vec<_>>();
    observed.sort_unstable();
    desired.sort_unstable();
    observed == desired
}
fn relevant_labels(labels: &BTreeMap<String, String>) -> BTreeMap<&str, &str> {
    labels
        .iter()
        .filter(|(k, _)| k.starts_with("io.piqueld.") || k.starts_with("traefik."))
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect()
}
fn relevant_network_labels(labels: &BTreeMap<String, String>) -> BTreeMap<&str, &str> {
    relevant_labels(labels)
        .into_iter()
        .filter(|(key, _)| *key != crate::resource::SPEC_HASH_LABEL)
        .collect()
}
fn service_drift(
    found: &crate::resource::ObservedService,
    desired: &DesiredService,
) -> Vec<String> {
    let mut fields = Vec::new();
    if found.image != desired.image {
        fields.push("image".into());
    }
    if found.replicas != desired.replicas {
        fields.push("replicas".into());
    }
    if found.environment != desired.environment {
        fields.push("environment".into());
    }
    if found.command != desired.command || found.arguments != desired.arguments {
        fields.push("process".into());
    }
    if !unordered_eq(&found.ports, &desired.ports) {
        fields.push("ports".into());
    }
    if !unordered_eq(&found.mounts, &desired.mounts) {
        fields.push("mounts".into());
    }
    if !unordered_eq(&found.secrets, &desired.secrets) {
        fields.push("secrets".into());
    }
    if found.networks.iter().collect::<BTreeSet<_>>()
        != desired.networks.iter().collect::<BTreeSet<_>>()
    {
        fields.push("networks".into());
    }
    if !found.runtime_configuration_matches {
        fields.push("runtime_policy".into());
    }
    if relevant_labels(&found.labels) != relevant_labels(&desired.labels) {
        fields.push("labels".into());
    }
    if found.healthcheck != desired.healthcheck {
        fields.push("healthcheck".into());
    }
    if found.resources != desired.resources {
        fields.push("resources".into());
    }
    fields
}
