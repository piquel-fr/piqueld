//! Pure, deterministic desired/observed planning for the supported Swarm model.

use crate::manifest::dependencies::StartupOrder;
use crate::resource::{
    Convergence, DesiredNetwork, DesiredService, DesiredVolume, ObservedApplication,
    ResolutionRequirement, ResolvedApplication,
};
use crate::{ApplicationId, InstanceId};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};
use utoipa::ToSchema;

use crate::codes;

/// Request used to generate a runtime plan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanRequest {
    /// Reconcile the desired application with observed runtime state.
    Reconcile {
        /// Desired runtime resources.
        desired: ResolvedApplication,
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
        desired: Option<ResolvedApplication>,
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
    /// Resolve and build a Git source before rollout.
    BuildGit {
        /// Logical service requesting a build.
        service: String,
    },
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
    /// Resource named by this action, without configuration or secret values.
    #[must_use]
    pub fn resource_name(&self) -> &str {
        match self {
            Self::EnsureNetwork { network } => network.name.as_str(),
            Self::EnsureVolume { volume } => volume.name.as_str(),
            Self::EnsureService { service } => service.name.as_str(),
            Self::RemoveService { name }
            | Self::RemoveNetwork { name }
            | Self::RetainVolume { name } => name,
            Self::WaitForService { service }
            | Self::WaitForServiceRemoval { service }
            | Self::ResolveImage { service, .. }
            | Self::BuildGit { service } => service,
        }
    }

    /// Pinned secret files the action's service mounts; empty for other actions.
    #[must_use]
    pub fn secrets(&self) -> &[crate::resource::SecretFile] {
        match self {
            Self::EnsureService { service } => &service.secrets,
            _ => &[],
        }
    }

    /// Classifies the effect of executing this action.
    #[must_use]
    pub const fn risk(&self) -> ActionRisk {
        match self {
            Self::EnsureVolume { .. } => ActionRisk::DataAdjacent,
            Self::EnsureService { .. } => ActionRisk::Availability,
            Self::RemoveService { .. } | Self::RemoveNetwork { .. } => ActionRisk::Destructive,
            Self::BuildGit { .. }
            | Self::ResolveImage { .. }
            | Self::EnsureNetwork { .. }
            | Self::WaitForService { .. }
            | Self::WaitForServiceRemoval { .. }
            | Self::RetainVolume { .. } => ActionRisk::None,
        }
    }

    /// Whether this action changes Docker resources.
    #[must_use]
    pub const fn mutates_runtime(&self) -> bool {
        matches!(
            self,
            Self::EnsureNetwork { .. }
                | Self::EnsureVolume { .. }
                | Self::EnsureService { .. }
                | Self::RemoveService { .. }
                | Self::RemoveNetwork { .. }
        )
    }

    /// Whether this action removes Docker resources.
    #[must_use]
    pub const fn destructive(&self) -> bool {
        matches!(self.risk(), ActionRisk::Destructive)
    }

    /// Returns the stable machine-readable action name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::BuildGit { .. } => "build_git",
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
            Self::BuildGit { service } => ("BUILD GIT", service.as_str()),
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

/// One action and its explanation in a runtime plan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanAction {
    /// Action details.
    pub kind: ActionKind,
    /// Why the action is present.
    pub reason: ActionReason,
}

impl PlanAction {
    /// Returns a stable, concise line suitable for an operation log.
    #[must_use]
    pub fn human_description(&self) -> String {
        self.kind.to_string()
    }

    /// Creates an action with its planning reason.
    #[must_use]
    pub fn new(kind: ActionKind, reason: ActionReason) -> Self {
        Self { kind, reason }
    }

    /// Creates a non-mutating service convergence wait.
    #[must_use]
    pub fn wait_for_service(service: &str) -> Self {
        Self::new(
            ActionKind::WaitForService {
                service: service.into(),
            },
            ActionReason::ConvergencePending,
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
        )
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
        self.actions
            .iter()
            .any(|action| action.kind.mutates_runtime())
    }

    /// Builds a plan for the requested desired/observed transition.
    ///
    /// `Preview` reconciles the compiled desired state when available and
    /// prepends one `BuildGit`/`ResolveImage` action per unresolved source.
    /// Diagnostics are sorted by resource then code for stable output.
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
                        ResolutionRequirement::BuildGit { service, .. } => PlanAction::new(
                            ActionKind::BuildGit {
                                service: service.to_string(),
                            },
                            ActionReason::ResolutionRequired,
                        ),
                        ResolutionRequirement::ResolveImage { service, reference } => {
                            PlanAction::new(
                                ActionKind::ResolveImage {
                                    service: service.to_string(),
                                    reference: reference.clone(),
                                },
                                ActionReason::ResolutionRequired,
                            )
                        }
                    })
                    .collect::<Vec<_>>();
                plan.actions.splice(0..0, prefix);
                plan
            }
        };
        plan.diagnostics.sort_by(|left, right| {
            left.resource
                .cmp(&right.resource)
                .then(left.code.cmp(&right.code))
        });
        plan
    }

    /// Records a blocking diagnostic for a same-name resource this application
    /// does not own. `seen` deduplicates reports per resource name.
    fn collision(&mut self, name: &str, seen: &mut BTreeSet<String>) {
        if seen.insert(name.into()) {
            self.diagnostics.push(PlanDiagnostic {
                code: codes::UNOWNED_NAME_COLLISION.into(),
                severity: DiagnosticSeverity::Error,
                resource: name.into(),
                message: "a same-name resource is not owned by this piqueld application and will not be changed".into(),
                blocking: true,
            });
        }
    }

    /// Records a blocking diagnostic for an owned network or volume whose
    /// settings drifted; Docker cannot update these in place.
    fn immutable_drift(&mut self, name: &str, resource: &str) {
        self.diagnostics.push(PlanDiagnostic {
            code: codes::IMMUTABLE_CONFIGURATION_DRIFT.into(),
            severity: DiagnosticSeverity::Error,
            resource: name.into(),
            message: format!(
                "the {resource} configuration differs from the desired immutable settings and cannot be repaired in place"
            ),
            blocking: true,
        });
    }

    /// Records a non-blocking note that an undesired resource is left untouched
    /// because this application does not own it.
    fn ignored(&mut self, name: &str) {
        self.diagnostics.push(PlanDiagnostic {
            code: codes::FOREIGN_RESOURCE_IGNORED.into(),
            severity: DiagnosticSeverity::Info,
            resource: name.into(),
            message: "foreign or unowned resource is outside this plan".into(),
            blocking: false,
        });
    }

    /// Records a non-blocking note that an obsolete owned resource is kept until
    /// the desired resources converge.
    fn cleanup_deferred(&mut self, name: &str, resource: &str) {
        self.diagnostics.push(PlanDiagnostic {
            code: codes::CLEANUP_DEFERRED.into(),
            severity: DiagnosticSeverity::Info,
            resource: name.into(),
            message: format!(
                "the {resource} remains until earlier desired changes converge; cleanup is deferred"
            ),
            blocking: false,
        });
    }

    /// Computes action counts from the current plan.
    #[must_use]
    pub fn summary(&self) -> PlanSummary {
        let mut by_action = BTreeMap::new();
        for action in &self.actions {
            *by_action.entry(action.kind.name().into()).or_insert(0) += 1;
        }
        PlanSummary {
            action_count: self.actions.len(),
            mutation_count: self
                .actions
                .iter()
                .filter(|action| action.kind.mutates_runtime())
                .count(),
            destructive_count: self
                .actions
                .iter()
                .filter(|action| action.kind.destructive())
                .count(),
            blocking_conflicts: self
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.blocking)
                .count(),
            by_action,
        }
    }

    /// Whether desired resources have converged and obsolete backends can be retired.
    /// Route cutover must succeed at this boundary before cleanup executes.
    ///
    /// The runtime mutation active-target repair should apply next, if any.
    /// Repair never waits, so it stops at the first convergence wait: services
    /// behind one start only after their dependencies converge.
    #[must_use]
    pub fn next_repair(&self) -> Option<&PlanAction> {
        self.actions
            .iter()
            .find(|action| {
                action.kind.mutates_runtime()
                    || matches!(action.kind, ActionKind::WaitForService { .. })
            })
            .filter(|action| action.kind.mutates_runtime())
    }

    /// True when the plan is unblocked and only cleanup actions (removals,
    /// removal waits, and volume retention) remain.
    #[must_use]
    pub fn desired_resources_ready(&self) -> bool {
        !self.is_blocked()
            && self.actions.iter().all(|action| {
                matches!(
                    action.kind,
                    ActionKind::RemoveService { .. }
                        | ActionKind::RemoveNetwork { .. }
                        | ActionKind::RetainVolume { .. }
                        | ActionKind::WaitForServiceRemoval { .. }
                )
            })
    }

    /// Plans the transition from `observed` to `desired`.
    ///
    /// Action order is significant for execution:
    /// 1. Ensure missing networks and volumes (dependencies of services).
    /// 2. Retain obsolete owned volumes; data is never deleted.
    /// 3. Ensure missing or drifted services, followed by service waits. An
    ///    ensure whose dependencies are still converging follows their waits.
    /// 4. Only when everything above is converged and unblocked, remove
    ///    obsolete services (then wait for removal) and obsolete networks.
    ///    Otherwise their cleanup is reported as deferred.
    ///
    /// `blocked_names` is shared so each colliding name is reported once.
    fn reconcile(desired: &ResolvedApplication, observed: &ObservedApplication) -> Self {
        let mut plan = Self::default();
        let mut blocked_names = BTreeSet::new();
        let networks_ready = plan.ensure_networks(desired, observed, &mut blocked_names);
        let volumes_ready = plan.ensure_volumes(desired, observed, &mut blocked_names);
        let infrastructure_ready = networks_ready && volumes_ready;
        plan.retain_obsolete_volumes(desired, observed);
        let services_ready = plan.ensure_services(desired, observed, &mut blocked_names);
        let cleanup_ready = infrastructure_ready && services_ready && !plan.is_blocked();
        plan.remove_obsolete_services(desired, observed, cleanup_ready);
        plan.remove_obsolete_networks(desired, observed, cleanup_ready);
        plan
    }

    /// Adds `EnsureNetwork` for missing networks and diagnostics for unowned or
    /// drifted ones. Returns whether every desired network already exists intact.
    ///
    /// Label drift ignores the spec hash, which changes on every spec edit.
    fn ensure_networks(
        &mut self,
        desired: &ResolvedApplication,
        observed: &ObservedApplication,
        blocked: &mut BTreeSet<String>,
    ) -> bool {
        let mut ready = true;
        for network in &desired.networks {
            match observed
                .networks
                .iter()
                .find(|found| found.name == network.name.as_str())
            {
                None => {
                    ready = false;
                    self.actions.push(PlanAction::new(
                        ActionKind::EnsureNetwork {
                            network: network.clone(),
                        },
                        ActionReason::Missing,
                    ));
                }
                Some(found) if !found.matches_ownership(network, desired) => {
                    ready = false;
                    self.collision(network.name.as_str(), blocked);
                }
                Some(found)
                    if !found.runtime_configuration_matches
                        || relevant_network_labels(&found.labels)
                            != relevant_network_labels(&network.labels) =>
                {
                    ready = false;
                    self.immutable_drift(network.name.as_str(), "network");
                }
                Some(_) => {}
            }
        }
        ready
    }

    /// Adds `EnsureVolume` for missing volumes and diagnostics for unowned or
    /// misconfigured ones. Returns whether every desired volume already exists
    /// intact. Volume labels are only checked for ownership, not drift.
    fn ensure_volumes(
        &mut self,
        desired: &ResolvedApplication,
        observed: &ObservedApplication,
        blocked: &mut BTreeSet<String>,
    ) -> bool {
        let mut ready = true;
        for volume in &desired.volumes {
            match observed
                .volumes
                .iter()
                .find(|found| found.name == volume.name.as_str())
            {
                None => {
                    ready = false;
                    self.actions.push(PlanAction::new(
                        ActionKind::EnsureVolume {
                            volume: volume.clone(),
                        },
                        ActionReason::Missing,
                    ));
                }
                Some(found) if !found.is_owned_by(&desired.instance_id, &desired.id) => {
                    ready = false;
                    self.collision(volume.name.as_str(), blocked);
                }
                Some(found) if !found.runtime_configuration_matches => {
                    ready = false;
                    self.immutable_drift(volume.name.as_str(), "volume");
                }
                Some(_) => {}
            }
        }
        ready
    }

    /// Adds `RetainVolume` for owned volumes no longer desired, and ignores
    /// foreign ones. Volumes are never removed automatically.
    fn retain_obsolete_volumes(
        &mut self,
        desired: &ResolvedApplication,
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
            if volume.is_owned_by(&desired.instance_id, &desired.id) {
                self.actions.push(PlanAction::new(
                    ActionKind::RetainVolume {
                        name: volume.name.clone(),
                    },
                    ActionReason::VolumeRetentionPolicy,
                ));
            } else {
                self.ignored(&volume.name);
            }
        }
    }

    /// Plans each desired service and returns whether all are converged.
    ///
    /// Missing or drifted services get `EnsureService` plus a wait; unconverged
    /// services only get a wait; failed updates and unowned names block the
    /// plan. Ensures come first so independent services roll out together.
    /// An ensure whose dependency is still converging is deferred behind that
    /// dependency's wait, and the remaining waits follow every ensure.
    fn ensure_services(
        &mut self,
        desired: &ResolvedApplication,
        observed: &ObservedApplication,
        blocked: &mut BTreeSet<String>,
    ) -> bool {
        let mut ready = true;
        // Pending convergence waits keyed by logical service name.
        let mut waits: Vec<(&str, PlanAction)> = Vec::new();
        // Ensures that must wait for a dependency to converge first.
        let mut deferred = Vec::new();
        // Startup order sees each dependency's wait before its dependents.
        let (ordered, cyclic) = desired.services.startup_order();
        for service in ordered.into_iter().chain(cyclic) {
            let reason = match observed
                .services
                .iter()
                .find(|found| found.name == service.name.as_str())
            {
                None => ActionReason::Missing,
                Some(found) if !found.matches_ownership(service, desired) => {
                    ready = false;
                    self.collision(service.name.as_str(), blocked);
                    continue;
                }
                Some(found) if !found.matches(service) => ActionReason::Drift {
                    fields: found.drift(service).into_iter().map(String::from).collect(),
                },
                Some(found) => {
                    match found.convergence {
                        Convergence::Converged => {}
                        Convergence::Updating | Convergence::Degraded => {
                            ready = false;
                            waits.push((
                                service.logical_name.as_str(),
                                PlanAction::wait_for_service(service.name.as_str()),
                            ));
                        }
                        Convergence::Failed => {
                            ready = false;
                            self.diagnostics.push(PlanDiagnostic {
                                code: codes::SERVICE_UPDATE_FAILED.into(),
                                severity: DiagnosticSeverity::Error,
                                resource: service.name.to_string(),
                                message: "service update failed; inspect the operation and Docker task state".into(),
                                blocking: true,
                            });
                        }
                    }
                    continue;
                }
            };
            ready = false;
            let ensure = PlanAction::new(
                ActionKind::EnsureService {
                    service: Box::new(service.clone()),
                },
                reason,
            );
            let waits_on_dependency = service
                .depends_on
                .iter()
                .any(|dependency| waits.iter().any(|(name, _)| *name == dependency.as_str()));
            if waits_on_dependency {
                deferred.push((service, ensure));
            } else {
                self.actions.push(ensure);
            }
            waits.push((
                service.logical_name.as_str(),
                PlanAction::wait_for_service(service.name.as_str()),
            ));
        }
        // The controller executes the first action and replans, so waiting
        // here rolls each deferred service out only once its dependencies converge.
        for (service, ensure) in deferred {
            for dependency in &service.depends_on {
                if let Some(index) = waits
                    .iter()
                    .position(|(name, _)| *name == dependency.as_str())
                {
                    self.actions.push(waits.remove(index).1);
                }
            }
            self.actions.push(ensure);
        }
        self.actions.extend(waits.into_iter().map(|(_, wait)| wait));
        ready
    }

    /// Removes owned services no longer desired once `cleanup_ready`, appending
    /// all removal waits after the removals. Otherwise defers their cleanup.
    fn remove_obsolete_services(
        &mut self,
        desired: &ResolvedApplication,
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
                    ));
                    waits.push(PlanAction::wait_for_service_removal(&service.name));
                } else {
                    self.cleanup_deferred(&service.name, "service");
                }
            } else {
                self.ignored(&service.name);
            }
        }
        self.actions.append(&mut waits);
    }

    /// Removes owned networks no longer desired once `cleanup_ready`; must run
    /// after service removal so no removed network is still attached.
    fn remove_obsolete_networks(
        &mut self,
        desired: &ResolvedApplication,
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
            if network.is_owned_by(&desired.instance_id, &desired.id) {
                if cleanup_ready {
                    self.actions.push(PlanAction::new(
                        ActionKind::RemoveNetwork {
                            name: network.name.clone(),
                        },
                        ActionReason::Obsolete,
                    ));
                } else {
                    self.cleanup_deferred(&network.name, "network");
                }
            } else {
                self.ignored(&network.name);
            }
        }
    }

    /// Plans application deletion: remove owned services and wait for them,
    /// then remove owned networks, and retain owned volumes. Any unowned
    /// observed resource is a blocking collision rather than being ignored.
    fn deletion(
        application_id: &ApplicationId,
        instance_id: &InstanceId,
        observed: &ObservedApplication,
    ) -> Self {
        let mut plan = Self::default();
        let mut waits = Vec::new();
        let mut collisions = BTreeSet::new();
        for service in sorted_by_name(&observed.services, |service| &service.name) {
            if service.is_owned_by(instance_id, application_id) {
                plan.actions.push(PlanAction::new(
                    ActionKind::RemoveService {
                        name: service.name.clone(),
                    },
                    ActionReason::ApplicationDeletion,
                ));
                waits.push(PlanAction::wait_for_service_removal(&service.name));
            } else {
                plan.collision(&service.name, &mut collisions);
            }
        }
        plan.actions.append(&mut waits);
        for network in sorted_by_name(&observed.networks, |network| &network.name) {
            if network.is_owned_by(instance_id, application_id) {
                plan.actions.push(PlanAction::new(
                    ActionKind::RemoveNetwork {
                        name: network.name.clone(),
                    },
                    ActionReason::ApplicationDeletion,
                ));
            } else {
                plan.collision(&network.name, &mut collisions);
            }
        }
        for volume in sorted_by_name(&observed.volumes, |volume| &volume.name) {
            if volume.is_owned_by(instance_id, application_id) {
                plan.actions.push(PlanAction::new(
                    ActionKind::RetainVolume {
                        name: volume.name.clone(),
                    },
                    ActionReason::VolumeRetentionPolicy,
                ));
            } else {
                plan.collision(&volume.name, &mut collisions);
            }
        }
        plan
    }
}

/// Borrows `values` sorted by name so observed Docker ordering never affects
/// the plan.
fn sorted_by_name<T, F>(values: &[T], name: F) -> Vec<&T>
where
    F: Fn(&T) -> &str,
{
    let mut values = values.iter().collect::<Vec<_>>();
    values.sort_by(|left, right| name(left).cmp(name(right)));
    values
}

/// Selects `io.piqueld.*` labels except the spec hash, which networks keep from
/// creation and which is expected to differ after later spec changes.
fn relevant_network_labels(labels: &BTreeMap<String, String>) -> BTreeMap<&str, &str> {
    labels
        .iter()
        .filter(|(key, _)| {
            key.starts_with("io.piqueld.") && key.as_str() != crate::resource::SPEC_HASH_LABEL
        })
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect()
}
