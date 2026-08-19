//! Pure, deterministic desired/observed planning for the supported Swarm model.

use crate::resource::{
    Convergence, DesiredApplication, DesiredNetwork, DesiredService, DesiredVolume,
    ObservedApplication, ObservedService, OwnershipState, ResolutionRequirement,
};
use crate::{ApplicationId, InstanceId};
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
        /// Desired runtime resources.
        desired: DesiredApplication,
    },
    /// Remove runtime resources for an application while retaining volumes.
    Delete {
        /// Application whose runtime resources are removed.
        application_id: ApplicationId,
        /// Control-plane instance that owns the resources.
        instance_id: InstanceId,
    },
    /// Preview image resolution and, when possible, the subsequent transition.
    Preview {
        /// Image resolutions still required before compilation.
        unresolved: Vec<ResolutionRequirement>,
        /// Compiled desired resources when all resolutions are reusable.
        desired: Option<DesiredApplication>,
    },
}

/// Risk classification for a plan action.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActionRisk {
    /// The action only records or waits for state.
    None,
    /// The action can temporarily affect availability.
    Availability,
    /// The action can affect persistent application data.
    DataAdjacent,
    /// The action removes or otherwise destroys runtime state.
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
        /// Desired fields that differ from the observation.
        fields: Vec<String>,
    },
    /// The observed resource is no longer desired.
    Obsolete,
    /// The action waits for a prior runtime change.
    ConvergencePending,
    /// An image still needs immutable digest resolution.
    ResolutionRequired,
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
        /// Service whose image must be resolved.
        service: String,
        /// Requested mutable image reference.
        reference: String,
    },
    /// Ensure a desired network exists.
    EnsureNetwork {
        /// Network state to create or verify.
        network: DesiredNetwork,
    },
    /// Ensure a desired volume exists.
    EnsureVolume {
        /// Volume state to create or verify.
        volume: DesiredVolume,
    },
    /// Ensure a desired service exists.
    EnsureService {
        /// Service state to create or verify.
        service: Box<DesiredService>,
    },
    /// Wait for a service to converge.
    WaitForService {
        /// Service whose convergence is awaited.
        service: String,
    },
    /// Wait for a service to be removed.
    WaitForServiceRemoval {
        /// Service whose removal is awaited.
        service: String,
    },
    /// Remove a service.
    RemoveService {
        /// Canonical service name to remove.
        name: String,
    },
    /// Remove a private network.
    RemoveNetwork {
        /// Canonical network name to remove.
        name: String,
    },
    /// Retain a volume by policy.
    RetainVolume {
        /// Canonical volume name intentionally left in place.
        name: String,
    },
}

impl ActionKind {
    /// Returns the stable machine-readable action name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::ResolveImage { .. } => "resolve_image",
            Self::EnsureNetwork { .. } => "ensure_network",
            Self::EnsureVolume { .. } => "ensure_volume",
            Self::EnsureService { .. } => "ensure_service",
            Self::WaitForService { .. } => "wait_for_service",
            Self::WaitForServiceRemoval { .. } => "wait_for_service_removal",
            Self::RemoveService { .. } => "remove_service",
            Self::RemoveNetwork { .. } => "remove_network",
            Self::RetainVolume { .. } => "retain_volume",
        }
    }
}

impl fmt::Display for ActionKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (verb, resource) = match self {
            Self::ResolveImage { service, .. } => ("RESOLVE IMAGE", service.as_str()),
            Self::EnsureNetwork { network } => ("ENSURE NETWORK", network.name.as_str()),
            Self::EnsureVolume { volume } => ("ENSURE VOLUME", volume.name.as_str()),
            Self::EnsureService { service } => ("ENSURE SERVICE", service.logical_name.as_str()),
            Self::WaitForService { service } => ("WAIT SERVICE", service.as_str()),
            Self::WaitForServiceRemoval { service } => ("WAIT SERVICE REMOVAL", service.as_str()),
            Self::RemoveService { name } => ("REMOVE SERVICE", name.as_str()),
            Self::RemoveNetwork { name } => ("REMOVE NETWORK", name.as_str()),
            Self::RetainVolume { name } => ("RETAIN VOLUME", name.as_str()),
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
    /// Returns a stable, concise line suitable for an operation log.
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

    /// Returns a bounded operation-step identifier.
    #[must_use]
    pub fn operation_step(&self) -> String {
        let value = self.to_string();
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
    /// Informational planner result.
    Info,
    /// Non-blocking planner warning.
    Warning,
    /// Planner error that blocks execution.
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
        self.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.blocking)
    }

    /// Returns whether the plan contains runtime mutations.
    #[must_use]
    pub fn has_mutations(&self) -> bool {
        self.actions.iter().any(|action| action.mutates_runtime)
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
                let prefix = unresolved
                    .iter()
                    .map(|requirement| match requirement {
                        ResolutionRequirement::ResolveImage { service, reference } => {
                            PlanAction::new(
                                ActionKind::ResolveImage {
                                    service: service.clone(),
                                    reference: reference.clone(),
                                },
                                ActionReason::ResolutionRequired,
                                ActionRisk::None,
                                false,
                            )
                        }
                    })
                    .collect::<Vec<_>>();
                plan.actions.splice(0..0, prefix);
                plan
            }
        };
        for (index, action) in plan.actions.iter_mut().enumerate() {
            action.sequence = u32::try_from(index + 1).unwrap_or(u32::MAX);
        }
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
            self.diagnostics.push(PlanDiagnostic {
                code: "unowned_name_collision".into(),
                severity: DiagnosticSeverity::Error,
                resource: name.into(),
                message: "a same-name resource is not owned by this piqueld application and will not be changed".into(),
                blocking: true,
            });
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

    fn reconcile(desired: &DesiredApplication, observed: &ObservedApplication) -> Self {
        let mut plan = Self::default();
        let mut blocked_names = BTreeSet::new();
        let networks_ready = plan.ensure_networks(desired, observed, &mut blocked_names);
        let volumes_ready = plan.ensure_volumes(desired, observed, &mut blocked_names);
        let infrastructure_ready = networks_ready && volumes_ready;
        plan.retain_obsolete_volumes(desired, observed);
        let (services_ready, mut waits) =
            plan.ensure_services(desired, observed, &mut blocked_names);
        plan.actions.append(&mut waits);
        let cleanup_ready = infrastructure_ready && services_ready && !plan.is_blocked();
        plan.remove_obsolete_services(desired, observed, cleanup_ready);
        plan.remove_obsolete_networks(desired, observed, cleanup_ready);
        plan
    }

    fn ensure_networks(
        &mut self,
        desired: &DesiredApplication,
        observed: &ObservedApplication,
        blocked: &mut BTreeSet<String>,
    ) -> bool {
        let mut ready = true;
        for network in &desired.networks {
            match observed
                .networks
                .iter()
                .find(|found| found.name == network.name)
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
                    self.collision(&network.name, blocked);
                }
                Some(found)
                    if !found.runtime_configuration_matches
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
        blocked: &mut BTreeSet<String>,
    ) -> bool {
        let mut ready = true;
        for volume in &desired.volumes {
            match observed
                .volumes
                .iter()
                .find(|found| found.name == volume.name)
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
                    self.collision(&volume.name, blocked);
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
        let wanted = desired
            .volumes
            .iter()
            .map(|volume| volume.name.as_str())
            .collect::<BTreeSet<_>>();
        for volume in sorted_by_name(&observed.volumes, |volume| &volume.name)
            .into_iter()
            .filter(|volume| !wanted.contains(volume.name.as_str()))
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

    fn ensure_services(
        &mut self,
        desired: &DesiredApplication,
        observed: &ObservedApplication,
        blocked: &mut BTreeSet<String>,
    ) -> (bool, Vec<PlanAction>) {
        let mut ready = true;
        let mut waits = Vec::new();
        for service in &desired.services {
            match observed
                .services
                .iter()
                .find(|found| found.name == service.name)
            {
                None => {
                    ready = false;
                    self.actions.push(PlanAction::new(
                        ActionKind::EnsureService {
                            service: Box::new(service.clone()),
                        },
                        ActionReason::Missing,
                        ActionRisk::Availability,
                        true,
                    ));
                    waits.push(PlanAction::wait_for_service(&service.name));
                }
                Some(found) if !found.matches_ownership(service, desired) => {
                    ready = false;
                    self.collision(&service.name, blocked);
                }
                Some(found) if !found.matches(service) => {
                    ready = false;
                    self.actions.push(PlanAction::new(
                        ActionKind::EnsureService {
                            service: Box::new(service.clone()),
                        },
                        ActionReason::Drift {
                            fields: service_drift(found, service),
                        },
                        ActionRisk::Availability,
                        true,
                    ));
                    waits.push(PlanAction::wait_for_service(&service.name));
                }
                Some(found) => {
                    match found.convergence {
                        Convergence::Converged => {}
                        Convergence::Updating | Convergence::Degraded => {
                            ready = false;
                            waits.push(PlanAction::wait_for_service(&service.name));
                        }
                        Convergence::Failed => {
                            ready = false;
                            self.diagnostics.push(PlanDiagnostic { code: "service_update_failed".into(), severity: DiagnosticSeverity::Error, resource: service.name.clone(), message: "service update failed; inspect the operation and Docker task state".into(), blocking: true });
                        }
                    }
                }
            }
        }
        (ready, waits)
    }

    fn remove_obsolete_services(
        &mut self,
        desired: &DesiredApplication,
        observed: &ObservedApplication,
        cleanup_ready: bool,
    ) {
        let wanted = desired
            .services
            .iter()
            .map(|service| service.name.as_str())
            .collect::<BTreeSet<_>>();
        let mut waits = Vec::new();
        for service in sorted_by_name(&observed.services, |service| &service.name)
            .into_iter()
            .filter(|service| !wanted.contains(service.name.as_str()))
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
                    waits.push(PlanAction::wait_for_service_removal(&service.name));
                }
            } else {
                self.ignored(&service.name);
            }
        }
        self.actions.append(&mut waits);
    }

    fn remove_obsolete_networks(
        &mut self,
        desired: &DesiredApplication,
        observed: &ObservedApplication,
        cleanup_ready: bool,
    ) {
        let wanted = desired
            .networks
            .iter()
            .map(|network| network.name.as_str())
            .collect::<BTreeSet<_>>();
        for network in sorted_by_name(&observed.networks, |network| &network.name)
            .into_iter()
            .filter(|network| !wanted.contains(network.name.as_str()))
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

    fn deletion(
        application_id: &ApplicationId,
        instance_id: &InstanceId,
        observed: &ObservedApplication,
    ) -> Self {
        let mut plan = Self::default();
        let mut waits = Vec::new();
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
                waits.push(PlanAction::wait_for_service_removal(&service.name));
            } else {
                plan.ignored(&service.name);
            }
        }
        plan.actions.append(&mut waits);
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

/// Builds a plan for a request and the current observed state.
#[must_use]
pub fn plan(request: &PlanRequest, observed: &ObservedApplication) -> Plan {
    Plan::from_request(request, observed)
}

fn sorted_by_name<T, F>(values: &[T], name: F) -> Vec<&T>
where
    F: Fn(&T) -> &str,
{
    let mut values = values.iter().collect::<Vec<_>>();
    values.sort_by(|left, right| name(left).cmp(name(right)));
    values
}

fn unordered_eq<T: Ord>(left: &[T], right: &[T]) -> bool {
    let mut left = left.iter().collect::<Vec<_>>();
    let mut right = right.iter().collect::<Vec<_>>();
    left.sort_unstable();
    right.sort_unstable();
    left == right
}

fn relevant_network_labels(labels: &BTreeMap<String, String>) -> BTreeMap<&str, &str> {
    labels
        .iter()
        .filter(|(key, _)| {
            key.starts_with("io.piqueld.") && key.as_str() != crate::resource::SPEC_HASH_LABEL
        })
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect()
}

fn service_drift(found: &ObservedService, desired: &DesiredService) -> Vec<String> {
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
    if found.networks.iter().collect::<BTreeSet<_>>()
        != desired.networks.iter().collect::<BTreeSet<_>>()
    {
        fields.push("networks".into());
    }
    if found.healthcheck != desired.healthcheck {
        fields.push("healthcheck".into());
    }
    if found.resources != desired.resources {
        fields.push("resources".into());
    }
    if !found.runtime_configuration_matches {
        fields.push("runtime_policy".into());
    }
    if found.labels != desired.labels {
        fields.push("labels".into());
    }
    fields
}
