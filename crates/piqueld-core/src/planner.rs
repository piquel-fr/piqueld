//! Pure, deterministic desired/observed planning.
#![allow(missing_docs)]

use crate::resource::{
    Convergence, DesiredApplication, DesiredNetwork, DesiredSecret, DesiredService, DesiredVolume,
    InstanceId, ObservedApplication, OwnershipState, ResolutionRequirement, ownership_state,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanRequest {
    Reconcile {
        desired: DesiredApplication,
    },
    Delete {
        application_id: crate::ApplicationId,
        instance_id: InstanceId,
    },
    Preview {
        unresolved: Vec<ResolutionRequirement>,
        desired: Option<DesiredApplication>,
    },
}

#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ActionRisk {
    None,
    Availability,
    DataAdjacent,
    Destructive,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActionReason {
    Missing,
    Drift { fields: Vec<String> },
    Obsolete,
    ConvergencePending,
    ResolutionRequired,
    SecretGenerationChanged,
    ApplicationDeletion,
    VolumeRetentionPolicy,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActionKind {
    ResolveImage {
        service: String,
        reference: String,
    },
    ResolveGit {
        service: String,
        repository: String,
        reference: String,
    },
    BuildImage {
        service: String,
    },
    PushImage {
        service: String,
    },
    EnsureNetwork {
        network: DesiredNetwork,
    },
    EnsureVolume {
        volume: DesiredVolume,
    },
    EnsureSecret {
        secret: DesiredSecret,
    },
    EnsureService {
        service: Box<DesiredService>,
    },
    WaitForService {
        service: String,
    },
    RemoveService {
        name: String,
    },
    RemoveNetwork {
        name: String,
    },
    RemoveSecret {
        name: String,
    },
    RetainVolume {
        name: String,
    },
    AwaitSecretGeneration {
        logical_name: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanAction {
    pub sequence: u32,
    pub kind: ActionKind,
    pub reason: ActionReason,
    pub risk: ActionRisk,
    pub mutates_runtime: bool,
    pub destructive: bool,
}
impl PlanAction {
    /// Returns a stable, concise line suitable for a CLI or operation log.
    #[must_use]
    pub fn human_description(&self) -> String {
        let (verb, resource) = match &self.kind {
            ActionKind::ResolveImage { service, .. } => ("RESOLVE IMAGE", service.as_str()),
            ActionKind::ResolveGit { service, .. } => ("RESOLVE GIT", service.as_str()),
            ActionKind::BuildImage { service } => ("BUILD IMAGE", service.as_str()),
            ActionKind::PushImage { service } => ("PUSH IMAGE", service.as_str()),
            ActionKind::EnsureNetwork { network } => ("ENSURE NETWORK", network.name.as_str()),
            ActionKind::EnsureVolume { volume } => ("ENSURE VOLUME", volume.name.as_str()),
            ActionKind::EnsureSecret { secret } => ("ENSURE SECRET", secret.logical_name.as_str()),
            ActionKind::EnsureService { service } => {
                ("ENSURE SERVICE", service.logical_name.as_str())
            }
            ActionKind::WaitForService { service } => ("WAIT SERVICE", service.as_str()),
            ActionKind::RemoveService { name } => ("REMOVE SERVICE", name.as_str()),
            ActionKind::RemoveNetwork { name } => ("REMOVE NETWORK", name.as_str()),
            ActionKind::RemoveSecret { name } => ("REMOVE SECRET", name.as_str()),
            ActionKind::RetainVolume { name } => ("RETAIN VOLUME", name.as_str()),
            ActionKind::AwaitSecretGeneration { logical_name } => {
                ("AWAIT SECRET", logical_name.as_str())
            }
        };
        format!("{verb} {resource}")
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanDiagnostic {
    pub code: String,
    pub severity: DiagnosticSeverity,
    pub resource: String,
    pub message: String,
    pub blocking: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanSummary {
    pub action_count: usize,
    pub mutation_count: usize,
    pub destructive_count: usize,
    pub blocking_conflicts: usize,
    pub by_action: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub actions: Vec<PlanAction>,
    pub diagnostics: Vec<PlanDiagnostic>,
    pub summary: PlanSummary,
}
impl Plan {
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        self.diagnostics.iter().any(|d| d.blocking)
    }
    #[must_use]
    pub fn has_mutations(&self) -> bool {
        self.actions.iter().any(|a| a.mutates_runtime)
    }
    #[must_use]
    pub fn human_summary(&self) -> String {
        format!(
            "{} action(s), {} runtime mutation(s), {} destructive, {} blocking conflict(s)",
            self.summary.action_count,
            self.summary.mutation_count,
            self.summary.destructive_count,
            self.summary.blocking_conflicts
        )
    }
}

#[must_use]
pub fn plan(request: &PlanRequest, observed: &ObservedApplication) -> Plan {
    let mut plan = match request {
        PlanRequest::Reconcile { desired } => reconcile(desired, observed),
        PlanRequest::Delete {
            application_id,
            instance_id,
        } => deletion(application_id, instance_id, observed),
        PlanRequest::Preview {
            unresolved,
            desired,
        } => {
            let mut plan = desired
                .as_ref()
                .map_or_else(Plan::default, |d| reconcile(d, observed));
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
                        prefix.push(new_action(
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
                prefix.push(new_action(
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
        .for_each(|(i, a)| a.sequence = u32::try_from(i + 1).unwrap_or(u32::MAX));
    plan.diagnostics
        .sort_by(|a, b| a.resource.cmp(&b.resource).then(a.code.cmp(&b.code)));
    plan.summary = summarize(&plan);
    plan
}

#[allow(clippy::too_many_lines)]
fn reconcile(desired: &DesiredApplication, observed: &ObservedApplication) -> Plan {
    let mut plan = Plan::default();
    let mut blocked_names = BTreeSet::new();
    for network in &desired.networks {
        match observed.networks.iter().find(|v| v.name == network.name) {
            None => plan.actions.push(new_action(
                ActionKind::EnsureNetwork {
                    network: network.clone(),
                },
                ActionReason::Missing,
                ActionRisk::None,
                true,
            )),
            Some(found) if !network_owned(found, network, desired) => {
                collision(&mut plan, &network.name, &mut blocked_names);
            }
            Some(found)
                if found.ingress != network.ingress
                    || relevant_labels(&found.labels) != relevant_labels(&network.labels) =>
            {
                plan.actions.push(new_action(
                    ActionKind::EnsureNetwork {
                        network: network.clone(),
                    },
                    ActionReason::Drift {
                        fields: vec!["configuration".into()],
                    },
                    ActionRisk::Availability,
                    true,
                ));
            }
            Some(_) => {}
        }
    }
    for volume in &desired.volumes {
        match observed.volumes.iter().find(|v| v.name == volume.name) {
            None => plan.actions.push(new_action(
                ActionKind::EnsureVolume {
                    volume: volume.clone(),
                },
                ActionReason::Missing,
                ActionRisk::DataAdjacent,
                true,
            )),
            Some(found)
                if ownership_state(&found.labels, &desired.instance_id, &desired.id)
                    != OwnershipState::Owned =>
            {
                collision(&mut plan, &volume.name, &mut blocked_names);
            }
            Some(_) => {}
        }
    }
    let desired_volumes = desired
        .volumes
        .iter()
        .map(|volume| volume.name.as_str())
        .collect::<BTreeSet<_>>();
    for volume in observed
        .volumes
        .iter()
        .filter(|volume| !desired_volumes.contains(volume.name.as_str()))
    {
        if ownership_state(&volume.labels, &desired.instance_id, &desired.id)
            == OwnershipState::Owned
        {
            plan.actions.push(new_action(
                ActionKind::RetainVolume {
                    name: volume.name.clone(),
                },
                ActionReason::VolumeRetentionPolicy,
                ActionRisk::None,
                false,
            ));
        } else {
            ignored(&mut plan, &volume.name);
        }
    }
    for secret in &desired.secrets {
        match observed.secrets.iter().find(|v| v.name == secret.name) {
            None => plan.actions.push(new_action(
                ActionKind::EnsureSecret {
                    secret: secret.clone(),
                },
                ActionReason::Missing,
                ActionRisk::None,
                true,
            )),
            Some(found)
                if ownership_state(&found.labels, &desired.instance_id, &desired.id)
                    != OwnershipState::Owned =>
            {
                collision(&mut plan, &secret.name, &mut blocked_names);
            }
            Some(_) => {}
        }
    }
    let mut all_services_converged = true;
    for service in &desired.services {
        match observed.services.iter().find(|v| v.name == service.name) {
            None => {
                all_services_converged = false;
                plan.actions.push(new_action(
                    ActionKind::EnsureService {
                        service: Box::new(service.clone()),
                    },
                    ActionReason::Missing,
                    ActionRisk::Availability,
                    true,
                ));
                plan.actions.push(wait(&service.name));
            }
            Some(found) if !service_owned(found, service, desired) => {
                all_services_converged = false;
                collision(&mut plan, &service.name, &mut blocked_names);
            }
            Some(found) if !found.semantically_matches(service) => {
                all_services_converged = false;
                let reason = if found.secrets == service.secrets {
                    ActionReason::Drift {
                        fields: service_drift(found, service),
                    }
                } else {
                    ActionReason::SecretGenerationChanged
                };
                plan.actions.push(new_action(
                    ActionKind::EnsureService {
                        service: Box::new(service.clone()),
                    },
                    reason,
                    ActionRisk::Availability,
                    true,
                ));
                plan.actions.push(wait(&service.name));
            }
            Some(found) => match found.convergence {
                Convergence::Converged => {}
                Convergence::Updating | Convergence::Degraded => {
                    all_services_converged = false;
                    plan.actions.push(wait(&service.name));
                }
                Convergence::Failed => {
                    all_services_converged = false;
                    plan.diagnostics.push(PlanDiagnostic { code: "service_update_failed".into(), severity: DiagnosticSeverity::Error, resource: service.name.clone(), message: "service update failed; previous secret generations and healthy tasks are retained".into(), blocking: true });
                }
            },
        }
    }
    let desired_services = desired
        .services
        .iter()
        .map(|v| v.name.as_str())
        .collect::<BTreeSet<_>>();
    for service in observed
        .services
        .iter()
        .filter(|v| !desired_services.contains(v.name.as_str()))
    {
        if ownership_state(&service.labels, &desired.instance_id, &desired.id)
            == OwnershipState::Owned
        {
            plan.actions.push(new_action(
                ActionKind::RemoveService {
                    name: service.name.clone(),
                },
                ActionReason::Obsolete,
                ActionRisk::Availability,
                true,
            ));
        } else {
            ignored(&mut plan, &service.name);
        }
    }
    let desired_networks = desired
        .networks
        .iter()
        .map(|v| v.name.as_str())
        .collect::<BTreeSet<_>>();
    for network in observed
        .networks
        .iter()
        .filter(|v| !desired_networks.contains(v.name.as_str()))
    {
        if ownership_state(&network.labels, &desired.instance_id, &desired.id)
            == OwnershipState::Owned
        {
            plan.actions.push(new_action(
                ActionKind::RemoveNetwork {
                    name: network.name.clone(),
                },
                ActionReason::Obsolete,
                ActionRisk::Availability,
                true,
            ));
        } else {
            ignored(&mut plan, &network.name);
        }
    }
    if all_services_converged {
        let desired_secrets = desired
            .secrets
            .iter()
            .map(|v| v.name.as_str())
            .collect::<BTreeSet<_>>();
        for secret in observed
            .secrets
            .iter()
            .filter(|v| !desired_secrets.contains(v.name.as_str()))
        {
            if ownership_state(&secret.labels, &desired.instance_id, &desired.id)
                == OwnershipState::Owned
                && !secret.in_use
            {
                plan.actions.push(new_action(
                    ActionKind::RemoveSecret {
                        name: secret.name.clone(),
                    },
                    ActionReason::Obsolete,
                    ActionRisk::Destructive,
                    true,
                ));
            } else if ownership_state(&secret.labels, &desired.instance_id, &desired.id)
                != OwnershipState::Owned
            {
                ignored(&mut plan, &secret.name);
            }
        }
    }
    plan
}

fn deletion(
    application_id: &crate::ApplicationId,
    instance_id: &InstanceId,
    observed: &ObservedApplication,
) -> Plan {
    let mut p = Plan::default();
    for v in &observed.services {
        if ownership_state(&v.labels, instance_id, application_id) == OwnershipState::Owned {
            p.actions.push(new_action(
                ActionKind::RemoveService {
                    name: v.name.clone(),
                },
                ActionReason::ApplicationDeletion,
                ActionRisk::Availability,
                true,
            ));
        } else {
            ignored(&mut p, &v.name);
        }
    }
    for v in &observed.networks {
        if ownership_state(&v.labels, instance_id, application_id) == OwnershipState::Owned {
            p.actions.push(new_action(
                ActionKind::RemoveNetwork {
                    name: v.name.clone(),
                },
                ActionReason::ApplicationDeletion,
                ActionRisk::Availability,
                true,
            ));
        } else {
            ignored(&mut p, &v.name);
        }
    }
    for v in &observed.secrets {
        if ownership_state(&v.labels, instance_id, application_id) == OwnershipState::Owned
            && !v.in_use
        {
            p.actions.push(new_action(
                ActionKind::RemoveSecret {
                    name: v.name.clone(),
                },
                ActionReason::ApplicationDeletion,
                ActionRisk::Destructive,
                true,
            ));
        } else if ownership_state(&v.labels, instance_id, application_id) != OwnershipState::Owned {
            ignored(&mut p, &v.name);
        }
    }
    for v in &observed.volumes {
        if ownership_state(&v.labels, instance_id, application_id) == OwnershipState::Owned {
            p.actions.push(new_action(
                ActionKind::RetainVolume {
                    name: v.name.clone(),
                },
                ActionReason::VolumeRetentionPolicy,
                ActionRisk::None,
                false,
            ));
        } else {
            ignored(&mut p, &v.name);
        }
    }
    p
}

fn network_owned(
    found: &crate::resource::ObservedNetwork,
    desired_network: &DesiredNetwork,
    desired: &DesiredApplication,
) -> bool {
    if desired_network.ingress {
        found
            .labels
            .get(crate::resource::MANAGED_LABEL)
            .map(String::as_str)
            == Some("true")
            && found
                .labels
                .get(crate::resource::INSTANCE_LABEL)
                .map(String::as_str)
                == Some(desired.instance_id.as_str())
    } else {
        ownership_state(&found.labels, &desired.instance_id, &desired.id) == OwnershipState::Owned
    }
}
fn service_owned(
    found: &crate::resource::ObservedService,
    desired_service: &DesiredService,
    desired: &DesiredApplication,
) -> bool {
    ownership_state(&found.labels, &desired.instance_id, &desired.id) == OwnershipState::Owned
        && found
            .labels
            .get(crate::resource::SERVICE_LABEL)
            .map(String::as_str)
            == Some(desired_service.logical_name.as_str())
}
fn relevant_labels(labels: &BTreeMap<String, String>) -> BTreeMap<&str, &str> {
    labels
        .iter()
        .filter(|(k, _)| k.starts_with("io.piqueld.") || k.starts_with("traefik."))
        .map(|(k, v)| (k.as_str(), v.as_str()))
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
    if found.mounts != desired.mounts {
        fields.push("mounts".into());
    }
    if found.secrets != desired.secrets {
        fields.push("secrets".into());
    }
    if found.networks.iter().collect::<BTreeSet<_>>()
        != desired.networks.iter().collect::<BTreeSet<_>>()
    {
        fields.push("networks".into());
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
fn wait(service: &str) -> PlanAction {
    new_action(
        ActionKind::WaitForService {
            service: service.into(),
        },
        ActionReason::ConvergencePending,
        ActionRisk::None,
        false,
    )
}
fn collision(plan: &mut Plan, name: &str, seen: &mut BTreeSet<String>) {
    if seen.insert(name.into()) {
        plan.diagnostics.push(PlanDiagnostic { code: "unowned_name_collision".into(), severity: DiagnosticSeverity::Error, resource: name.into(), message: "a same-name resource is not owned by this piqueld application and will not be changed".into(), blocking: true });
    }
}
fn ignored(plan: &mut Plan, name: &str) {
    plan.diagnostics.push(PlanDiagnostic {
        code: "foreign_resource_ignored".into(),
        severity: DiagnosticSeverity::Info,
        resource: name.into(),
        message: "foreign or unowned resource is outside this plan".into(),
        blocking: false,
    });
}
fn new_action(
    kind: ActionKind,
    reason: ActionReason,
    risk: ActionRisk,
    mutates_runtime: bool,
) -> PlanAction {
    PlanAction {
        sequence: 0,
        destructive: risk == ActionRisk::Destructive,
        kind,
        reason,
        risk,
        mutates_runtime,
    }
}
fn summarize(plan: &Plan) -> PlanSummary {
    let mut by_action = BTreeMap::new();
    for action in &plan.actions {
        let name = action_name(&action.kind);
        *by_action.entry(name.into()).or_insert(0) += 1;
    }
    PlanSummary {
        action_count: plan.actions.len(),
        mutation_count: plan.actions.iter().filter(|a| a.mutates_runtime).count(),
        destructive_count: plan.actions.iter().filter(|a| a.destructive).count(),
        blocking_conflicts: plan.diagnostics.iter().filter(|d| d.blocking).count(),
        by_action,
    }
}
fn action_name(action: &ActionKind) -> &'static str {
    match action {
        ActionKind::ResolveImage { .. } => "resolve_image",
        ActionKind::ResolveGit { .. } => "resolve_git",
        ActionKind::BuildImage { .. } => "build_image",
        ActionKind::PushImage { .. } => "push_image",
        ActionKind::EnsureNetwork { .. } => "ensure_network",
        ActionKind::EnsureVolume { .. } => "ensure_volume",
        ActionKind::EnsureSecret { .. } => "ensure_secret",
        ActionKind::EnsureService { .. } => "ensure_service",
        ActionKind::WaitForService { .. } => "wait_for_service",
        ActionKind::RemoveService { .. } => "remove_service",
        ActionKind::RemoveNetwork { .. } => "remove_network",
        ActionKind::RemoveSecret { .. } => "remove_secret",
        ActionKind::RetainVolume { .. } => "retain_volume",
        ActionKind::AwaitSecretGeneration { .. } => "await_secret_generation",
    }
}
