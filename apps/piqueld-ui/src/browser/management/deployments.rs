//! Deployment and promotion actions, plan dialogs, history, and snapshot
//! inspection.
use super::super::format::timestamp;
use super::super::ui::{
    Icon, Modal, Tone, badge, empty, icon, key_values, load_older, loading, notice,
    operation_badge, text_field, when,
};
use super::super::{
    Alive, ClientErrorExt, confirm, document_hidden, live_row, operation_events, release_link,
};
use super::releases::ReleaseCard;
use super::timeline::{
    CompactProgress, DeploymentProgress, LiveTimeline, OperationTimeline, PlannedSteps,
    RunningDeployment, duration, status_pill,
};
use super::{EditorContext, EditorFeedback, back_link, editor};
use crate::application::DEPLOYMENT_TABS;
use crate::changes::{ChangeDisplay, Mark};
use crate::deployments::{Attempts, DeploymentList, DeploymentSummary, Promotion};
use crate::editor::Choice;
use crate::overview::build_arguments;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::NavigateOptions;
use leptos_router::components::A;
use leptos_router::hooks::use_navigate;
use piqueld_client::{
    AcceptedOperation, ApplyApplicationRequest, Build, Client, DeploymentView, EnvironmentId,
    EnvironmentView, HealthCheck, ManifestChange, ManifestRevision, Operation, Page, PlanView,
    RuntimeCoverage, SecretProblem, Service, Source, Template, ValidatedRollout,
    access::AppPermission,
};
use std::fmt::Display;

/// Deployments the history asks for per page.
const PAGE: u16 = 20;

/// "Preview" and "Deploy" buttons for the editor's target environment. Preview
/// asks the daemon to plan the manifest that environment deploys (its last
/// fetched one when it follows a branch; cleared whenever the saved view or
/// target changes); Deploy starts a deployment of the saved
/// generation, then shows the environment's deployments. A target following
/// a branch also offers Deploy revision (see `DeployRevision`). Without a single target
/// (an application with several environments or none), Deploy lists the
/// environments to choose one. A promoted target, which never builds,
/// offers Promote from its source instead. Either also offers Promote,
/// before its own action, into the environments promoted from it. On its own page, Deploy
/// doesn't name the environment. Preview is shown with `apps:write`, the
/// others with `apps:deploy`.
#[component]
#[expect(clippy::too_many_lines, reason = "flat view! markup")]
pub(super) fn DeploymentActions() -> impl IntoView {
    let context = editor();
    let preview = RwSignal::new(None::<PlanView>);
    let navigate = StoredValue::new(use_navigate());
    let promoted = Memo::new(move |_| {
        context
            .selected_environment()
            .is_some_and(|environment| environment.source.promoted_from().is_some())
    });
    // Environments promoted from this one, which its page promotes into.
    let dependents = Signal::derive(move || {
        let current = context.environment.get();
        context.saved.with(|saved| {
            saved
                .environments
                .iter()
                .filter(|environment| {
                    !environment.delete_intent
                        && environment
                            .source
                            .promoted_from()
                            .map(EnvironmentId::as_str)
                            == current.as_deref()
                })
                .cloned()
                .collect::<Vec<_>>()
        })
    });
    // Promotes into those environments, from any environment of a chain.
    let promote_into = move || {
        view! {
            <Show when={move || !dependents.with(Vec::is_empty)}>
                <ChooseEnvironment
                    label="Promote"
                    title="Promote into an environment"
                    environments={dependents}
                />
            </Show>
        }
    };
    let deploy = move |_| {
        let (environment, navigate) = (context.environment_id(), navigate.get_value());
        context.deploy(environment.clone(), None, move || {
            context.show_deployments(&environment, navigate);
        });
    };
    let inspect = move |_| {
        let Some(manifest) = context.next_manifest() else {
            return;
        };
        let app = context.saved.get_untracked();
        let request = ApplyApplicationRequest {
            manifest: manifest.to_manifest(),
            expected_generation: Some(app.generation),
            expected_application_id: Some(app.application.id().to_string()),
        };
        let environment = context.environment_id();
        context.busy.set(true);
        context.set_error(None);
        spawn_local(async move {
            match Client::browser()
                .plan_application(&request, Some(&environment))
                .await
            {
                Ok(plan) => preview.set(Some(plan)),
                Err(error) => context.set_error(Some(error.user_message())),
            }
            context.busy.set(false);
        });
    };
    Effect::new(move |_| {
        context.saved.track();
        context.environment.track();
        preview.set(None);
    });
    view! {
        <Show
            when={move || context.environment.get().is_some()}
            fallback={move || {
                let environments = Signal::derive(move || {
                    context.saved.with(|saved| saved.environments.clone())
                });
                view! {
                    // Without environments, Deploy leads to creating one.
                    <Show
                        when={move || !environments.with(Vec::is_empty)}
                        fallback={move || {
                            view! {
                                <button
                                    type="button"
                                    class="btn btn-primary"
                                    title="Create an environment to deploy"
                                    hidden={move || !context.can(AppPermission::Deploy)}
                                    on:click={move |_| context.tabs.selected.set("Environments")}
                                >
                                    {icon(Icon::Rocket)}
                                    "Deploy"
                                </button>
                            }
                        }}
                    >
                        <ChooseEnvironment
                            label="Deploy"
                            title="Deploy an environment"
                            environments={environments}
                        />
                    </Show>
                }
            }}
        >
            <StopAction />
            <Show
                when={move || !promoted.get()}
                fallback={move || {
                    view! {
                        {promote_into()}
                        <PromoteAction environment={context.environment_id()} />
                    }
                }}
            >
                <button
                    type="button"
                    class="btn"
                    hidden={move || !context.can(AppPermission::Write)}
                    disabled={move || {
                        context.environment_action_blocked() || context.next_manifest().is_none()
                    }}
                    on:click={inspect}
                    title={move || {
                        if context.next_manifest().is_some() {
                            "Show what deploying this environment's manifest would change"
                        } else {
                            "Deploy this environment to fetch its manifest from its branch first"
                        }
                    }}
                >
                    {icon(Icon::Eye)}
                    "Preview"
                </button>
                {promote_into()}
                // A one-time revision overrides the branch the manifest is fetched from.
                <Show when={move || {
                    context.can(AppPermission::Deploy)
                        && context
                            .selected_environment()
                            .is_some_and(|environment| environment.source.branch().is_some())
                }}>
                    <DeployRevision />
                </Show>
                <button
                    type="button"
                    class="btn btn-primary"
                    hidden={move || !context.can(AppPermission::Deploy)}
                    disabled={move || context.environment_action_blocked()}
                    on:click={deploy}
                >
                    {icon(Icon::Rocket)}
                    // Its own page names the environment already.
                    {move || match context.selected_environment() {
                        Some(environment) if !context.environment_page() => {
                            format!("Deploy to {}", environment.name)
                        }
                        _ => "Deploy".into(),
                    }}
                </button>
            </Show>
        </Show>
        <DeploymentPreview preview={preview} />
    }
}

/// Which revision of the manifest repository `DeployRevision` deploys, as
/// chosen in its form.
#[derive(Clone, Copy, PartialEq)]
enum RevisionKind {
    /// The head of a branch.
    Branch,
    /// A full commit hash.
    Commit,
}

impl Choice for RevisionKind {
    const ALL: &'static [Self] = &[Self::Branch, Self::Commit];

    fn value(self) -> &'static str {
        match self {
            Self::Branch => "branch",
            Self::Commit => "commit",
        }
    }
}

impl RevisionKind {
    /// The label of the input naming the revision.
    const fn label(self) -> &'static str {
        match self {
            Self::Branch => "Branch name",
            Self::Commit => "Commit hash",
        }
    }

    /// The revision `input` names, trimmed; none while blank.
    fn revision(self, input: &str) -> Option<ManifestRevision> {
        let input = input.trim().to_owned();
        (!input.is_empty()).then_some(match self {
            Self::Branch => ManifestRevision::Branch(input),
            Self::Commit => ManifestRevision::Commit(input),
        })
    }
}

/// "Deploy revision" button and dialog for the editor's target environment,
/// which follows a branch: deploys the manifest on another branch or commit
/// of the application's repository once, without saving either, then shows
/// the environment's deployments.
#[component]
fn DeployRevision() -> impl IntoView {
    let context = editor();
    let opened = RwSignal::new(false);
    let kind = RwSignal::new(RevisionKind::Branch);
    let input = RwSignal::new(String::new());
    let navigate = StoredValue::new(use_navigate());
    let revision = move || input.with(|input| kind.get().revision(input));
    // The branch the environment follows.
    let current = Memo::new(move |_| {
        context
            .selected_environment()
            .and_then(|environment| environment.source.branch().cloned())
    });
    // Each opening starts from a blank form, without earlier errors.
    let open = move |_| {
        kind.set(RevisionKind::Branch);
        input.set(String::new());
        context.set_error(None);
        opened.set(true);
    };
    let submit = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        let Some(revision) = untrack(revision) else {
            return;
        };
        // Validated here for a precise error; the daemon validates it again.
        if let Some(current) = current.get_untracked()
            && let Err(error) = current.at(&revision)
        {
            context.set_error(Some(error.to_string()));
            return;
        }
        let (environment, navigate) = (context.environment_id(), navigate.get_value());
        context.deploy(environment.clone(), Some(revision), move || {
            opened.set(false);
            context.show_deployments(&environment, navigate);
        });
    };
    view! {
        <button
            type="button"
            class="btn"
            title="Deploy another branch or commit once, without changing what this environment follows"
            disabled={move || context.environment_action_blocked()}
            on:click={open}
        >
            {icon(Icon::GitBranch)}
            "Deploy revision"
        </button>
        <Modal title="Deploy a branch or commit" opened={opened} busy={context.busy}>
            <form class="stack-sm" on:submit={submit}>
                <fieldset class="stack-sm" disabled={move || context.environment_action_blocked()}>
                    <p class="hint">
                        {move || {
                            format!(
                                "Deploys the manifest on another branch or commit of the application's repository once, without saving it. The environment still follows {}: its next deployment, or a push when it deploys on push, returns to it.",
                                current.get().map_or_default(|branch| branch.to_string()),
                            )
                        }}
                    </p>
                    <div class="form-grid">
                        <label class="field">
                            <span>"Deploy"</span>
                            <select
                                prop:value={move || kind.get().value()}
                                on:change={move |event| {
                                    if let Some(choice) = RevisionKind::parse(
                                        &event_target_value(&event),
                                    ) {
                                        kind.set(choice);
                                    }
                                }}
                            >
                                <option value="branch">"The head of a branch"</option>
                                <option value="commit">"A commit"</option>
                            </select>
                        </label>
                        {move || text_field(kind.get().label(), input)}
                    </div>
                </fieldset>
                {move || context.error.get().map(|error| notice(Tone::Bad, error))}
                <div class="form-actions">
                    <button
                        type="submit"
                        class="btn btn-primary"
                        disabled={move || {
                            context.environment_action_blocked() || revision().is_none()
                        }}
                    >
                        {icon(Icon::Rocket)}
                        "Deploy"
                    </button>
                </div>
            </form>
        </Modal>
    }
}

/// Stop, or Start once stopped, for the editor's target environment, once
/// it has been deployed; not for previews. Stopping, after confirmation,
/// withdraws its routes and scales its services to zero until it is started
/// or deployed again. Hidden without `apps:deploy`.
#[component]
fn StopAction() -> impl IntoView {
    let context = editor();
    let stopped = move || {
        context
            .selected_environment()
            .is_some_and(|environment| environment.stopped())
    };
    // Previews are deleted instead.
    let deployed = move || {
        context.can(AppPermission::Deploy)
            && context.selected_environment().is_some_and(|environment| {
                environment.preview().is_none() && environment.resolved_generation.is_some()
            })
    };
    let toggle = move |_| {
        let stopping = !stopped();
        if stopping
            && !confirm(&format!(
                "Stop environment {}? Its routes are withdrawn and its services stop, keeping their configuration and volumes, until it is started or deployed again.",
                context.selected_name()
            ))
        {
            return;
        }
        context.set_stopped(context.environment_id(), stopping);
    };
    view! {
        <Show when={deployed}>
            <button
                type="button"
                class="btn"
                disabled={move || context.environment_action_blocked()}
                title={move || {
                    if stopped() {
                        "Run this environment's services and serve its routes again"
                    } else {
                        "Stop this environment's services and withdraw its routes"
                    }
                }}
                on:click={toggle}
            >
                {move || if stopped() { "Start" } else { "Stop" }}
            </button>
        </Show>
    }
}

/// `environment`'s own action: Deploy for one that builds its source, or
/// Promote for a promoted one (see `PromoteAction`). Either shows its
/// deployments once accepted, then closes `dialog`. Hidden without
/// `apps:deploy`.
#[component]
pub(super) fn EnvironmentAction(
    environment: EnvironmentView,
    #[prop(optional)] compact: bool,
    #[prop(default = None)] dialog: Option<RwSignal<bool>>,
) -> impl IntoView {
    let context = editor();
    let EnvironmentView {
        id,
        name,
        source,
        delete_intent: deleting,
        ..
    } = environment;
    let id = id.to_string();
    if source.promoted_from().is_some() {
        return view! { <PromoteAction environment={id} compact={compact} dialog={dialog} /> }
            .into_any();
    }
    let navigate = StoredValue::new(use_navigate());
    let label = format!("Deploy to {name}");
    let id = StoredValue::new(id);
    view! {
        <button
            type="button"
            class={if compact { "btn btn-sm" } else { "btn btn-primary" }}
            aria-label={label.clone()}
            hidden={move || !context.can(AppPermission::Deploy)}
            disabled={move || context.action_blocked() || deleting}
            on:click={move |_| {
                let (id, navigate) = (id.get_value(), navigate.get_value());
                context
                    .deploy(
                        id.clone(),
                        None,
                        move || {
                            context.show_deployments(&id, navigate);
                            if let Some(dialog) = dialog {
                                dialog.set(false);
                            }
                        },
                    );
            }}
        >
            {icon(Icon::Rocket)}
            {if compact { "Deploy".to_owned() } else { label.clone() }}
        </button>
    }
    .into_any()
}

/// A `label`led button opening a dialog that lists `environments`, each
/// with its own action (see `EnvironmentAction`), so one can be chosen:
/// "Deploy" over an application's environments, "Promote" over the
/// environments promoted from the shown one. Closes once an action was
/// accepted. Hidden without `apps:deploy`.
#[component]
fn ChooseEnvironment(
    label: &'static str,
    title: &'static str,
    environments: Signal<Vec<EnvironmentView>>,
) -> impl IntoView {
    let context = editor();
    let opened = RwSignal::new(false);
    view! {
        <button
            type="button"
            class="btn btn-primary"
            hidden={move || !context.can(AppPermission::Deploy)}
            disabled={move || environments.with(Vec::is_empty)}
            on:click={move |_| opened.set(true)}
        >
            {icon(Icon::Rocket)}
            {label}
        </button>
        <Modal title={title} opened={opened} busy={context.busy}>
            <ul class="list">
                {move || {
                    environments
                        .get()
                        .into_iter()
                        .map(|environment| {
                            view! {
                                <li class="list-row">
                                    <span class="title">{environment.name.to_string()}</span>
                                    <EnvironmentAction
                                        environment={environment}
                                        compact=true
                                        dialog={Some(opened)}
                                    />
                                </li>
                            }
                        })
                        .collect_view()
                }}
            </ul>
        </Modal>
    }
}

/// "Promote" button for a promoted environment, or with `release` "Deploy"
/// of that earlier release into it. It plans the promotion and shows that
/// plan; confirming promotes exactly what was reviewed (the source deployment
/// it planned, which the daemon refuses if the source has moved on since, or
/// the release), then shows the environment's deployments. Confirmation
/// stays disabled while the release mounts secrets the environment lacks or
/// may not use. `compact` is the smaller list-row button; `dialog` closes once
/// the promotion was accepted. Hidden without `apps:deploy`.
#[component]
#[expect(clippy::too_many_lines, reason = "flat view! markup")]
pub(super) fn PromoteAction(
    environment: String,
    #[prop(optional)] release: Option<String>,
    #[prop(optional)] compact: bool,
    #[prop(default = None)] dialog: Option<RwSignal<bool>>,
) -> impl IntoView {
    let context = editor();
    let environment = StoredValue::new(environment);
    let promotion = StoredValue::new(Promotion::new(release));
    let plan = RwSignal::new(None::<PlanView>);
    let opened = RwSignal::new(false);
    let navigate = use_navigate();
    let verb = promotion.with_value(Promotion::verb);
    // A promotion names its source; deploying a release names its target.
    let label = move || {
        let target = environment.get_value();
        let source = context.saved.with(|saved| {
            saved
                .deployable(&target)
                .and_then(|environment| environment.source.promoted_from().cloned())
        });
        match source.filter(|_| verb == "Promote") {
            Some(source) => format!("Promote from {}", context.environment_name(source.as_str())),
            None => format!("{verb} to {}", context.environment_name(&target)),
        }
    };
    let deleting = move || {
        context.saved.with(|saved| {
            saved
                .deployable(&environment.get_value())
                .is_none_or(|environment| environment.delete_intent)
        })
    };
    let inspect = move |_| {
        context.busy.set(true);
        context.set_error(None);
        let (target, request) = (
            environment.get_value(),
            promotion.with_value(Promotion::request),
        );
        spawn_local(async move {
            match Client::browser().plan_promotion(&target, &request).await {
                Ok(view) => {
                    plan.set(Some(view));
                    opened.set(true);
                }
                Err(error) => context.set_error(Some(error.user_message())),
            }
            context.busy.set(false);
        });
    };
    // The request promoting exactly what was reviewed, if it can be promoted.
    let promotable = move || {
        plan.with(|plan| promotion.with_value(|promotion| promotion.confirmed(plan.as_ref()?)))
    };
    let confirm = move |_| {
        let Some(request) = promotable() else {
            return;
        };
        let (navigate, target) = (navigate.clone(), environment.get_value());
        let sent = target.clone();
        context.mutate(
            move |client| {
                let (request, target) = (request.clone(), sent.clone());
                async move { client.promote_environment(&target, &request, false).await }
            },
            move |_| {
                opened.set(false);
                plan.set(None);
                context
                    .notice
                    .set("Promotion accepted. Follow its progress below.".into());
                context.show_deployments(&target, navigate);
                context.refresh_dashboard();
                if let Some(dialog) = dialog {
                    dialog.set(false);
                }
            },
        );
    };
    view! {
        <button
            type="button"
            class={if compact { "btn btn-sm" } else { "btn btn-primary" }}
            aria-label={label}
            hidden={move || !context.can(AppPermission::Deploy)}
            disabled={move || context.action_blocked() || deleting()}
            on:click={inspect}
        >
            {icon(Icon::Rocket)}
            {label}
        </button>
        <Modal
            title="Promotion preview"
            opened={opened}
            wide=true
            busy={context.busy}
            on_close={Callback::new(move |()| plan.set(None))}
        >
            {move || plan.get().map(|plan| view! { <PromotionPlan plan={plan} /> })}
            {move || context.error.get().map(|error| notice(Tone::Bad, error))}
            <div class="form-actions">
                <button
                    type="button"
                    class="btn btn-primary"
                    // A failed promotion stays blocked until inspected again.
                    disabled={move || {
                        context.action_blocked() || context.error.with(Option::is_some)
                            || promotable().is_none()
                    }}
                    on:click={confirm}
                >
                    {icon(Icon::Rocket)}
                    {format!("{verb} release")}
                </button>
            </div>
        </Modal>
    }
}

/// A promotion plan: the release and where it comes from, the volumes it
/// adds, every secret the environment lacks for it, and what deploying it
/// changes.
#[component]
fn PromotionPlan(plan: PlanView) -> impl IntoView {
    let context = editor();
    let release = plan.release.clone().map(|release| {
        let volumes = if release.new_volumes.is_empty() {
            "None".to_owned()
        } else {
            release.new_volumes.join(", ")
        };
        let secrets = (!release.secrets.is_empty()).then(|| {
            view! {
                <div class="stack-sm">
                    {release
                        .secrets
                        .iter()
                        .map(|problem| {
                            notice(
                                Tone::Bad,
                                match problem {
                                    SecretProblem::Missing { secret } => {
                                        format!(
                                            "Secret {secret} is missing from the application's store.",
                                        )
                                    }
                                    SecretProblem::AccessDenied { secret } => {
                                        format!("Secret {secret} does not allow this environment.")
                                    }
                                    SecretProblem::Unavailable { secret } => {
                                        format!(
                                            "Secret {secret} lost its value in key recovery; store a new version.",
                                        )
                                    }
                                    SecretProblem::Deleting { secret } => {
                                        format!(
                                            "Secret {secret} is being deleted; finish or abandon the deletion.",
                                        )
                                    }
                                },
                            )
                        })
                        .collect_view()}
                    <p class="hint">
                        "Resolve each secret problem in the Secrets tab before promoting."
                    </p>
                </div>
            }
        });
        view! {
            <section class="stack-sm">
                <div class="section-header">
                    <h3>"Release"</h3>
                </div>
                <dl class="kv">
                    <dt>"Origin"</dt>
                    <dd>{context.describe_origin(&release.origin)}</dd>
                    <dt>"New volumes"</dt>
                    <dd>{volumes}</dd>
                </dl>
                {secrets}
                {view! { <ReleaseCard release={release.release} planned=true /> }.into_any()}
            </section>
        }
    });
    view! {
        {release}
        <PlanDetails plan={plan} />
    }
}

/// Panel showing a deployment plan: configuration changes, planned runtime
/// actions and plan diagnostics.
#[component]
fn DeploymentPreview(preview: RwSignal<Option<PlanView>>) -> impl IntoView {
    let opened = RwSignal::new(false);
    Effect::new(move |_| opened.set(preview.get().is_some()));
    view! {
        <Modal
            title="Deployment preview"
            opened={opened}
            wide=true
            on_close={Callback::new(move |()| preview.set(None))}
        >
            {move || {
                preview
                    .get()
                    .map(|plan| {
                        view! {
                            {plan
                                .identical
                                .then(|| {
                                    notice(
                                        Tone::Info,
                                        "Saved configuration matches the latest deployment snapshot. Deploying refreshes image resolution only.",
                                    )
                                })}
                            <p class="hint">
                                "Image tags and runtime state may change before the deployment runs."
                            </p>
                            <PlanDetails plan={plan} />
                        }
                    })
            }}
        </Modal>
    }
}

/// One manifest field change: its path, then its value before and after,
/// or line by line when the value is structured (see `ChangeDisplay`).
pub(super) fn change_row(change: ManifestChange) -> impl IntoView {
    let display = ChangeDisplay::of(&change);
    let block = matches!(display, ChangeDisplay::Lines(_));
    view! {
        <div class="diff-row" class:diff-block={block}>
            <code>{change.field}</code>
            {match display {
                ChangeDisplay::Scalar { before, after } => {
                    view! {
                        <span class="before">{before.unwrap_or_else(|| "absent".into())}</span>
                        <span class="arrow" aria-hidden="true">
                            "→"
                        </span>
                        <span class="after">{after.unwrap_or_else(|| "absent".into())}</span>
                    }
                        .into_any()
                }
                ChangeDisplay::Lines(lines) => {
                    view! {
                        <pre class="diff-lines">
                            {lines
                                .into_iter()
                                .map(|line| {
                                    let class = match line.mark {
                                        Mark::Same => "diff-line",
                                        Mark::Removed => "diff-line removed",
                                        Mark::Added => "diff-line added",
                                    };
                                    view! {
                                        <span class={class}>
                                            {format!("{} {}\n", line.mark.sign(), line.text)}
                                        </span>
                                    }
                                })
                                .collect_view()}
                        </pre>
                    }
                        .into_any()
                }
            }}
        </div>
    }
}

/// What deploying a plan changes: configuration changes, variable values,
/// planned runtime actions, rollouts, and plan diagnostics.
#[component]
fn PlanDetails(plan: PlanView) -> impl IntoView {
    view! {
        <section>
            <div class="section-header">
                <h3>"Configuration changes"</h3>
            </div>
            {if plan.changes.is_empty() {
                view! { <p class="hint">"No configuration changes since the last deployment."</p> }
                    .into_any()
            } else {
                plan.changes.into_iter().map(change_row).collect_view().into_any()
            }}
        </section>
        {(!plan.variables.is_empty())
            .then(|| {
                view! {
                    <section>
                        <div class="section-header">
                            <h3>"Variables"</h3>
                        </div>
                        {variable_values(&plan.variables)}
                    </section>
                }
            })}
        <section>
            <div class="section-header">
                <h3>"Planned actions"</h3>
            </div>
            {plan.runtime_coverage.note().map(|note| view! { <p class="hint">{note}</p> })}
            {
                let steps = plan.plan.steps();
                if steps.is_empty() {
                    (plan.runtime_coverage == RuntimeCoverage::Complete)
                        .then(|| view! { <p class="hint">"No runtime actions are required."</p> })
                        .into_any()
                } else {
                    view! { <PlannedSteps steps={steps} /> }.into_any()
                }
            }
        </section>
        <section>
            <div class="section-header">
                <h3>"Rollout"</h3>
            </div>
            <ul class="stack-sm">
                {plan
                    .rollouts
                    .into_iter()
                    .map(|rollout| {
                        view! {
                            <li class="btn-group">
                                <code>{rollout.service}</code>
                                {badge(Tone::Neutral, rollout.order.as_str())}
                                <span class="hint">
                                    {format!(
                                        "{} · monitor {}s",
                                        rollout.order_source.as_str(),
                                        rollout.monitor_seconds,
                                    )}
                                </span>
                            </li>
                        }
                    })
                    .collect_view()}
            </ul>
        </section>
        {(!plan.plan.diagnostics.is_empty())
            .then(|| {
                view! {
                    <div class="stack-sm">
                        {plan
                            .plan
                            .diagnostics
                            .into_iter()
                            .map(|d| notice(Tone::Warn, format!("{}: {}", d.resource, d.message)))
                            .collect_view()}
                    </div>
                }
            })}
    }
}

/// The deployments shown for one environment: its first page, polled, and
/// older pages appended on request.
#[derive(Clone, Copy)]
struct History {
    items: RwSignal<Vec<DeploymentView>>,
    /// Cursor of the next older page.
    cursor: RwSignal<Option<String>>,
    /// Whether an older page was appended, so polls merge instead of replacing.
    paginated: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    loading: RwSignal<bool>,
    /// Whether the first page was read, so the list is known to be empty
    /// rather than still loading.
    fetched: RwSignal<bool>,
}

impl History {
    fn new() -> Self {
        Self {
            items: RwSignal::new(Vec::new()),
            cursor: RwSignal::new(None),
            paginated: RwSignal::new(false),
            error: RwSignal::new(None),
            loading: RwSignal::new(false),
            fetched: RwSignal::new(false),
        }
    }

    /// Applies a freshly polled first page. Before any older page is loaded,
    /// the page simply replaces the list and cursor; afterwards it is merged
    /// into the older pages (see `DeploymentList::merge_newest`).
    fn merge(self, page: Page<DeploymentView>) {
        self.items.update(|items| {
            if self.paginated.get_untracked() {
                items.merge_newest(page.items);
            } else {
                *items = page.items;
                self.cursor.set(page.next_cursor);
            }
        });
    }

    /// Polls the first page of `environment` every 2 seconds while the
    /// editor's Deployments tab is selected and the page is visible, and
    /// right after the tab opens, following its newest deployment while it
    /// runs (see `RunningDeployment`). Stops when the owning view unmounts.
    fn poll(self, context: EditorContext, environment: String) {
        let active = Alive::new();
        let running = use_context::<RunningDeployment>();
        spawn_local(async move {
            while active.get() {
                let shown = context.tabs.selected.get_untracked() == "Deployments";
                if shown && !self.loading.get_untracked() && !document_hidden() {
                    let result = Client::browser()
                        .deployments(&environment, None, Some(PAGE))
                        .await;
                    if !active.get() {
                        break;
                    }
                    match result {
                        Ok(page) => {
                            if let (Some(newest), Some(running)) = (page.items.first(), running) {
                                running.saw(&newest.operation);
                            }
                            self.merge(page);
                            self.error.set(None);
                            self.fetched.set(true);
                        }
                        Err(e) => self.error.set(Some(e.user_message())),
                    }
                }
                // Checked often while the tab is closed, so opening it
                // never waits a whole poll interval.
                let delay = if shown { 2000 } else { 250 };
                gloo_timers::future::TimeoutFuture::new(delay).await;
            }
        });
    }

    /// Appends the next older page of `environment`, skipping deployments
    /// already shown.
    fn load_older(self, environment: String) {
        let next = self.cursor.get_untracked();
        self.loading.set(true);
        spawn_local(async move {
            match Client::browser()
                .deployments(&environment, next.as_deref(), Some(PAGE))
                .await
            {
                Ok(page) => {
                    self.error.set(None);
                    self.paginated.set(true);
                    self.cursor.set(page.next_cursor);
                    self.items.update(|items| items.append_older(page.items));
                }
                Err(e) => self.error.set(Some(e.user_message())),
            }
            self.loading.set(false);
        });
    }
}

/// Deployment list for the editor's environment, a row per deployment
/// opening its page. The first page is polled while the deployments tab is
/// visible; older pages are appended on request without duplicating
/// deployments already shown.
#[component]
pub(super) fn DeploymentHistory() -> impl IntoView {
    let context = editor();
    let history = History::new();
    let id = context.environment_id();
    history.poll(context, id.clone());
    view! {
        <section class="stack-sm" aria-label="Deployment history">
            {move || history.error.get().map(|e| notice(Tone::Bad, e))}
            {move || {
                history
                    .items
                    .with(Vec::is_empty)
                    .then(|| {
                        if history.fetched.get() {
                            empty(
                                "No deployments yet. Deploy the saved configuration to create one.",
                            )
                        } else if history.error.get().is_none() {
                            loading("Loading deployments…")
                        } else {
                            ().into_any()
                        }
                    })
            }}
            <ul
                class="list"
                aria-label="Deployments"
                hidden={move || history.items.with(Vec::is_empty)}
            >
                <For
                    each={move || history.items.get()}
                    key={|d| d.operation.id.clone()}
                    children={move |initial| {
                        let deployment = live_row(history.items, initial, |d| &d.operation.id);
                        view! { <DeploymentRow deployment={deployment} /> }
                    }}
                />
            </ul>
            {load_older(
                "Load older deployments",
                history.cursor,
                history.loading,
                move || history.load_older(id.clone()),
            )}
        </section>
    }
}

/// One deployment in its environment's history, opening its page: its
/// state, what brought it (see `ApplicationViewExt::deployment_title`), the
/// configuration revision and commit it deploys, and how long it ran, or
/// while the page follows it running, its progress.
#[component]
fn DeploymentRow(deployment: Signal<DeploymentView>) -> impl IntoView {
    let context = editor();
    let running = expect_context::<RunningDeployment>();
    let id = deployment.with_untracked(|deployment| deployment.operation.id.clone());
    let href = context.deployment_href(&context.environment_id(), &id);
    let live = Signal::derive(move || running.running(Some(&id)));
    let tag = move |shown: fn(&DeploymentView) -> bool, label: &'static str| {
        move || {
            deployment
                .with(shown)
                .then(|| view! { <span class="tag">{label}</span> })
        }
    };
    view! {
        <li class="list-row list-row-link deployment-row">
            {move || operation_badge(deployment.get().operation.state)} <span class="title">
                <A href={href}>{move || context.deployment_title(&deployment.get().origin)}</A>
                <small>{move || deployment.with(DeploymentSummary::commit)}</small>
            </span>
            <span class="tag">
                {move || format!("Revision {}", deployment.get().operation.generation)}
            </span> {tag(|deployment| deployment.current_target, "Current target")}
            {tag(|deployment| deployment.last_successful, "Last successful")}
            <span class="meta">
                {move || match live.get() {
                    Some(live) => view! { <CompactProgress live={live} /> }.into_any(),
                    None => {
                        deployment
                            .with(|deployment| {
                                let took = deployment
                                    .duration_ms()
                                    .map(|ms| format!("{} · ", duration(ms)));
                                view! {
                                    {took}
                                    {when(deployment.operation.created_at_ms)}
                                }
                                    .into_any()
                            })
                    }
                }}
            </span> <span class="chevron" aria-hidden="true">
                {icon(Icon::ChevronRight)}
            </span>
        </li>
    }
}

/// The deployment a deployment page shows, read from its environment's
/// history, as the page opens and again once it ended.
#[derive(Clone, Copy)]
struct ShownDeployment {
    deployment: RwSignal<Option<DeploymentView>>,
    /// Whether a newer deployment is listed: one that failed before
    /// replacing the current target leaves Restore offered on it.
    newer: RwSignal<bool>,
    error: RwSignal<Option<String>>,
}

impl ShownDeployment {
    fn new() -> Self {
        Self {
            deployment: RwSignal::new(None),
            newer: RwSignal::new(false),
            error: RwSignal::new(None),
        }
    }

    /// Reads deployment `operation` of `environment`, paging back through
    /// the environment's history until it is found. Abandoned when the
    /// calling owner is cleaned up.
    fn load(self, environment: String, operation: String) {
        let alive = Alive::new();
        spawn_local(async move {
            let client = Client::browser();
            let mut cursor = None;
            let mut newest = true;
            loop {
                let result = client
                    .deployments(&environment, cursor.as_deref(), Some(PAGE))
                    .await;
                if !alive.get() {
                    return;
                }
                let page = match result {
                    Ok(page) => page,
                    Err(error) => return self.error.set(Some(error.user_message())),
                };
                if let Some(index) = page
                    .items
                    .iter()
                    .position(|deployment| deployment.operation.id == operation)
                {
                    self.newer.set(!newest || index > 0);
                    self.deployment.set(page.items.into_iter().nth(index));
                    return self.error.set(None);
                }
                newest = false;
                cursor = page.next_cursor;
                if cursor.is_none() {
                    return self.error.set(Some(
                        "This deployment is not in the environment's history.".into(),
                    ));
                }
            }
        });
    }
}

/// One deployment's page: its progress, followed live over its timeline
/// stream while it runs, then its steps and events, details, configuration
/// snapshot, and attempts. A deployment that may be restored offers
/// Restore, which opens the restoring deployment's page.
#[component]
pub(super) fn DeploymentPage(operation: String) -> impl IntoView {
    let context = editor();
    let tabs = context.tabs;
    let environment = context.environment_id();
    let application_href = format!("/dashboard/applications/{}", context.id());
    if context.selected_environment().is_none() {
        return view! {
            <div class="stack-sm">
                {notice(Tone::Bad, "This environment no longer exists.")}
                {back_link(application_href, "Back to the application")}
            </div>
        }
        .into_any();
    }
    let live = LiveTimeline::follow(operation.clone());
    let progress = live.progress();
    // Starting or reconciling the environment runs this operation again.
    let signals = context.signals();
    Effect::new(move |_| {
        signals.detail.with(|detail| {
            if let Some(latest) = detail
                .as_ref()
                .and_then(|detail| detail.latest_operation.as_ref())
            {
                live.resume(latest);
            }
        });
    });
    let shown = ShownDeployment::new();
    Effect::new({
        let environment = environment.clone();
        move |_| {
            // Read again once it ended, for its outcome and flags.
            live.live();
            shown.load(environment.clone(), operation.clone());
        }
    });
    // The operation as its newest snapshot has it, which runs ahead of the
    // history's.
    let deployment = Signal::derive(move || {
        let mut deployment = shown.deployment.get()?;
        if let Some(operation) = live.operation() {
            deployment.operation = operation;
        }
        Some(deployment)
    });
    // What brought it, as its page is titled.
    let title = Memo::new(move |_| {
        shown.deployment.with(|deployment| {
            deployment.as_ref().map_or_else(
                || "Deployment".to_owned(),
                |deployment| context.deployment_title(&deployment.origin),
            )
        })
    });
    let loaded = Memo::new(move |_| shown.deployment.with(Option::is_some));
    let subject = format!("{}/{}", context.name(), context.selected_name());
    view! {
        <nav class="breadcrumb" aria-label="Breadcrumb">
            <A href="/dashboard/applications">"Applications"</A>
            {icon(Icon::ChevronRight)}
            <A href={application_href}>{move || context.name()}</A>
            {icon(Icon::ChevronRight)}
            <A href={context.deployable_href(&environment)}>{move || context.selected_name()}</A>
            {icon(Icon::ChevronRight)}
            <span>{title}</span>
        </nav>
        <header class="detail-head">
            <div class="detail-title">
                <h1>{title}</h1>
                {move || progress.with(|p| p.as_ref().map(status_pill))}
            </div>
            <div class="page-actions">
                <RestoreDeployment deployment={deployment} newer={shown.newer} />
            </div>
        </header>
        <EditorFeedback />
        <section class="card deployment-head" aria-label="Deployment progress">
            {move || deployment.get().map(|deployment| context.deployment_meta(&deployment))}
            <DeploymentProgress live={live} subject={subject} />
        </section>
        {move || shown.error.get().map(|error| notice(Tone::Bad, error))}
        {tabs.bar(&DEPLOYMENT_TABS, None)}
        {tabs.panel("Timeline", view! { <OperationTimeline live={live} /> })}
        {move || {
            if !loaded.get() {
                return None;
            }
            let initial = shown.deployment.get_untracked()?;
            let deployment = Signal::derive(move || {
                deployment.get().unwrap_or_else(|| initial.clone())
            });
            Some(
                // Read, not just tracked: a memo only follows its source once read.
                view! { <DeploymentSections deployment={deployment} /> },
            )
        }}
    }
    .into_any()
}

/// A deployment page's tabs past its timeline: its operation's fields, the
/// configuration it captured, and its attempts, loaded once their tab opens.
#[component]
fn DeploymentSections(deployment: Signal<DeploymentView>) -> impl IntoView {
    let context = editor();
    let tabs = context.tabs;
    view! {
        {tabs
            .panel(
                "Details",
                move || deployment.with(|deployment| context.deployment_details(deployment)),
            )}
        {tabs.panel("Snapshot", view! { <DeploymentSnapshot deployment={deployment} /> })}
        {tabs
            .panel(
                "Attempts",
                view! {
                    <Show when={move || tabs.selected.get() == "Attempts"}>
                        <DeploymentAttempts deployment={deployment} />
                    </Show>
                },
            )}
    }
}

/// Restore, offered on a deployment the caller may restore that succeeded
/// and is not the current target, unless a `newer` deployment failed to
/// replace it. Opens the restoring deployment's page once accepted.
#[component]
fn RestoreDeployment(
    deployment: Signal<Option<DeploymentView>>,
    newer: RwSignal<bool>,
) -> impl IntoView {
    let context = editor();
    let navigate = use_navigate();
    move || {
        let id = deployment.with(|deployment| {
            deployment
                .as_ref()
                .filter(|deployment| {
                    deployment.succeeded_at_ms.is_some()
                        && (!deployment.current_target || newer.get())
                })
                .map(|deployment| deployment.operation.id.clone())
        })?;
        let navigate = navigate.clone();
        context.can_restore().then(|| {
            view! {
                <button
                    type="button"
                    class="btn"
                    title="Deploy this deployment again exactly as it ran: its manifest, images and secret versions."
                    disabled={move || context.action_blocked()}
                    on:click={move |_| {
                        let navigate = navigate.clone();
                        context
                            .restore(
                                id.clone(),
                                move |accepted| {
                                    navigate(
                                        &context
                                            .deployment_href(
                                                &accepted.environment_id,
                                                &accepted.operation_id,
                                            ),
                                        NavigateOptions::default(),
                                    );
                                },
                            );
                    }}
                >
                    {icon(Icon::Restore)}
                    "Restore"
                </button>
            }
        })
    }
}

impl EditorContext {
    /// Whether the caller may restore deployments of the shown environment:
    /// `apps:deploy`, or `previews:deploy` in a preview.
    fn can_restore(self) -> bool {
        let preview = self
            .selected_environment()
            .is_some_and(|environment| environment.preview().is_some());
        self.can(if preview {
            AppPermission::PreviewsDeploy
        } else {
            AppPermission::Deploy
        })
    }

    /// Confirms, then restores `deployment` of the shown environment or
    /// preview exactly as it ran, and runs `restored` once accepted.
    fn restore(self, deployment: String, restored: impl FnOnce(AcceptedOperation) + 'static) {
        if !confirm(&format!(
            "Restore deployment {deployment}? Its jobs don't run again, and volume contents and migrations stay as they are."
        )) {
            return;
        }
        let Some(environment) = self.selected_environment() else {
            return;
        };
        self.mutate(
            move |client| {
                let (environment, deployment) = (environment.clone(), deployment.clone());
                async move { client.restore_deployment(&environment, &deployment).await }
            },
            move |accepted| {
                self.refresh_dashboard();
                restored(accepted);
            },
        );
    }

    /// What a deployment page's header says of `deployment`: where it came
    /// from, the commit, revision and release it deploys, when it started,
    /// and its operation's ID.
    fn deployment_meta(self, deployment: &DeploymentView) -> impl IntoView + use<> {
        let operation = &deployment.operation;
        let release = deployment.release.as_ref().map(|release| {
            view! { <span>"Release " {release_link(&self.id(), &release.to_string())}</span> }
        });
        view! {
            <p class="deployment-meta">
                <span>{self.describe_origin(&deployment.origin)}</span>
                {deployment.commit().map(|commit| view! { <span>{commit}</span> })}
                <span>{format!("Revision {}", operation.generation)}</span>
                {release}
                <span>"Created " {when(operation.created_at_ms)}</span>
                <span>"Attempt " {operation.attempt}</span>
                <code>{operation.id.clone()}</code>
            </p>
        }
    }

    /// A deployment's Details tab: its operation's fields, and why it failed.
    fn deployment_details(self, deployment: &DeploymentView) -> AnyView {
        let op = deployment.operation.clone();
        let release = deployment.release.as_ref().map_or_else(
            || "Not recorded".into_any(),
            |release| release_link(&self.id(), &release.to_string()),
        );
        view! {
            <div class="stack-sm">
                <dl class="kv">
                    <dt>"Operation ID"</dt>
                    <dd>
                        <code>{op.id}</code>
                    </dd>
                    <dt>"Release"</dt>
                    <dd>{release}</dd>
                    <dt>"Origin"</dt>
                    <dd>{self.describe_origin(&deployment.origin)}</dd>
                    <dt>"Kind"</dt>
                    <dd>{op.kind.as_str()}</dd>
                    <dt>"Configuration revision"</dt>
                    <dd>{op.generation}</dd>
                    <dt>"Attempts"</dt>
                    <dd>{op.attempt}</dd>
                    <dt>"Phase"</dt>
                    <dd>{op.phase.map_or("Waiting", piqueld_client::Phase::as_str)}</dd>
                    {op
                        .resource
                        .map(|resource| {
                            view! {
                                <dt>"Resource"</dt>
                                <dd>
                                    <code>{resource}</code>
                                </dd>
                            }
                        })}
                    <dt>"Created"</dt>
                    <dd>{timestamp(op.created_at_ms)}</dd>
                    <dt>"Updated"</dt>
                    <dd>{timestamp(op.updated_at_ms)}</dd>
                    {op
                        .finished_at_ms
                        .map(|ms| {
                            view! {
                                <dt>"Finished"</dt>
                                <dd>{timestamp(ms)}</dd>
                            }
                        })}
                </dl>
                {op
                    .error_message
                    .map(|error| {
                        notice(
                            Tone::Bad,
                            view! {
                                {op.error_code.map(|code| view! { <strong>{code}</strong> })}
                                <span>{error}</span>
                            },
                        )
                    })}
            </div>
        }
        .into_any()
    }
}

/// The branch, warnings, variable values, services, volumes, and jobs captured
/// in a deployment's configuration snapshot: the rendered configuration, or
/// the captured manifest until a repository-backed manifest is fetched.
#[component]
fn DeploymentSnapshot(deployment: Signal<DeploymentView>) -> impl IntoView {
    view! {
        {move || {
            let deployment = deployment.get();
            let (hint, spec) = match &deployment.application {
                Some(application) => {
                    (
                        "Configuration captured when this deployment was accepted, rendered for its environment.",
                        application.to_manifest().spec,
                    )
                }
                None => {
                    (
                        "The repository manifest has not been fetched yet; showing the captured configuration with references unresolved.",
                        deployment.template.to_manifest().spec,
                    )
                }
            };
            let fetched = deployment
                .template
                .spec()
                .manifest
                .as_ref()
                .map(|manifest| {
                    let revision = manifest
                        .repository
                        .commit
                        .as_ref()
                        .unwrap_or(&manifest.repository.branch);
                    format!("Manifest {} from {revision}.", manifest.path)
                });
            view! {
                <p class="hint">{hint}</p>
                {fetched.map(|fetched| view! { <p class="hint">{fetched}</p> })}
                {deployment
                    .warnings
                    .iter()
                    .map(|warning| notice(
                        Tone::Warn,
                        format!("{}: {}", warning.code, warning.message),
                    ))
                    .collect_view()}
                {(!deployment.variables.is_empty())
                    .then(|| {
                        view! {
                            <div class="snapshot-service">
                                <h4>"Variables"</h4>
                                {variable_values(&deployment.variables)}
                            </div>
                        }
                    })}
                {spec
                    .services
                    .into_iter()
                    .map(|service| view! { <SnapshotService service={service} /> })
                    .collect_view()}
                <div class="snapshot-service">
                    <h4>"Volumes"</h4>
                    <p class="hint">
                        {if spec.volumes.is_empty() {
                            "None".to_owned()
                        } else {
                            spec.volumes.into_iter().map(|v| v.name).collect::<Vec<_>>().join(", ")
                        }}
                    </p>
                </div>
                <div class="snapshot-service">
                    <h4>"Jobs"</h4>
                    {if spec.jobs.is_empty() {
                        view! { <p class="hint">"None"</p> }.into_any()
                    } else {
                        view! {
                            <ol class="hint">
                                {spec
                                    .jobs
                                    .into_iter()
                                    .map(|job| {
                                        view! {
                                            <li>
                                                <strong>{job.name}</strong>
                                                {format!(
                                                    " on {}, up to {}s: ",
                                                    job.service,
                                                    job.timeout_seconds,
                                                )}
                                                <code>{join(&job.command, " ")}</code>
                                            </li>
                                        }
                                    })
                                    .collect_view()}
                            </ol>
                        }
                            .into_any()
                    }}
                </div>
            }
        }}
    }
}

/// Paginated attempt outcomes of one deployment, plus the live attempt while it runs.
#[component]
fn DeploymentAttempts(deployment: Signal<DeploymentView>) -> impl IntoView {
    let op = deployment.get_untracked().operation;
    let attempts = RwSignal::new(Attempts::default());
    let failure = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);
    let active = Alive::new();
    let load = Callback::new(move |next: Option<String>| {
        if loading.get_untracked() {
            return;
        }
        let environment = op.environment_id.to_string();
        let deployment_id = op.id.clone();
        let active = active.clone();
        loading.set(true);
        spawn_local(async move {
            let result = Client::browser()
                .deployment_attempts(&environment, &deployment_id, next.as_deref())
                .await;
            if !active.get() {
                return;
            }
            match result {
                Ok(page) => {
                    failure.set(None);
                    attempts.update(|attempts| attempts.show(page, next.is_some()));
                }
                Err(error) => failure.set(Some(error.user_message())),
            }
            loading.set(false);
        });
    });
    load.run(None);
    let visible_attempts = Signal::derive(move || {
        let live = deployment.get().operation;
        attempts.with(|attempts| attempts.visible(&live))
    });

    view! {
        <div class="stack-sm">
            {move || failure.get().map(|error| notice(Tone::Bad, error))}
            {move || loading.get().then(|| super::super::ui::loading("Loading attempts…"))}
            {move || {
                if visible_attempts.with(Vec::is_empty) {
                    (!loading.get() && failure.get().is_none())
                        .then(|| empty("No attempts yet."))
                        .into_any()
                } else {
                    view! {
                        <div class="table-wrap">
                            <table class="table">
                                <thead>
                                    <tr>
                                        <th class="num">"Attempt"</th>
                                        <th>"State"</th>
                                        <th>"Outcome"</th>
                                        <th>"Updated"</th>
                                        <th></th>
                                    </tr>
                                </thead>
                                <tbody>
                                    {visible_attempts
                                        .get()
                                        .into_iter()
                                        .map(|attempt| view! { <AttemptRow attempt={attempt} /> })
                                        .collect_view()}
                                </tbody>
                            </table>
                        </div>
                    }
                        .into_any()
                }
            }} <div class="btn-group">
                <button
                    type="button"
                    class="btn btn-sm"
                    disabled={move || loading.get()}
                    on:click={move |_| load.run(None)}
                >
                    {icon(Icon::Refresh)}
                    "Refresh attempts"
                </button>
                <Show when={move || attempts.with(|attempts| attempts.cursor.is_some())}>
                    <button
                        type="button"
                        class="btn btn-sm"
                        disabled={move || loading.get()}
                        on:click={move |_| {
                            load.run(attempts.with_untracked(|attempts| attempts.cursor.clone()));
                        }}
                    >
                        "Older attempts"
                    </button>
                </Show>
            </div>
        </div>
    }
}

/// One attempt outcome with a link to its events and diagnostics, shown with
/// `events:read`.
#[component]
fn AttemptRow(attempt: Operation) -> impl IntoView {
    let context = editor();
    let history = operation_events(&attempt.id);
    let outcome = match (attempt.error_code, attempt.error_message) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (Some(code), None) => code,
        (None, Some(message)) => message,
        (None, None) => String::new(),
    };
    view! {
        <tr>
            <td class="num">{attempt.attempt}</td>
            <td>{operation_badge(attempt.state)}</td>
            <td class="muted">{outcome}</td>
            <td class="muted">{when(attempt.updated_at_ms)}</td>
            <td class="actions">
                <Show when={move || context.can(AppPermission::EventsRead)}>
                    <A href={history.clone()}>"Events"</A>
                </Show>
            </td>
        </tr>
    }
}

/// Definition list describing one snapshotted service's configuration.
#[component]
fn SnapshotService(service: Service) -> impl IntoView {
    let source = match service.source.clone() {
        Source::Image { image } => image.to_string(),
        Source::Git {
            repository,
            build:
                Build::Docker {
                    dockerfile,
                    context,
                    args,
                    target,
                },
        } => [
            Some(format!("Git: {repository}")),
            Some(format!("Dockerfile: {dockerfile}")),
            Some(format!("context: {context}")),
            target.map(|target| format!("target: {target}")),
            (!args.is_empty()).then(|| format!("args: {}", build_arguments(&args))),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · "),
    };
    let list = |items: Vec<String>| {
        if items.is_empty() {
            "None".to_owned()
        } else {
            items.join("\n")
        }
    };
    view! {
        <div class="snapshot-service">
            <h4>{service.name.clone()}</h4>
            <dl class="kv">
                <dt>"Source"</dt>
                <dd>
                    <code>{source}</code>
                </dd>
                <dt>"Replicas"</dt>
                <dd>{service.replicas.to_string()}</dd>
                <dt>"Environment"</dt>
                <dd>
                    {list(service.environment.iter().map(|(k, v)| format!("{k}={v}")).collect())}
                </dd>
                <dt>"Command"</dt>
                <dd>{list(service.command.iter().map(ToString::to_string).collect())}</dd>
                <dt>"Arguments"</dt>
                <dd>{list(service.arguments.iter().map(ToString::to_string).collect())}</dd>
                <dt>"Mounts"</dt>
                <dd>
                    {list(
                        service
                            .mounts
                            .iter()
                            .map(|m| {
                                format!(
                                    "{} → {}{}",
                                    m.volume,
                                    m.target,
                                    if m.read_only { " (read only)" } else { "" },
                                )
                            })
                            .collect(),
                    )}
                </dd>
                <dt>"Secret files"</dt>
                <dd>
                    {list(
                        service
                            .secrets
                            .iter()
                            .map(|s| format!("{} → {}", s.name, s.target))
                            .collect(),
                    )}
                </dd>
                <dt>"Startup dependencies"</dt>
                <dd>{list(service.depends_on.clone())}</dd>
                <dt>"Rollout"</dt>
                <dd>
                    {format!(
                        "{} · monitor {}s",
                        service
                            .rollout
                            .order
                            .as_ref()
                            .map_or_else(|| "derived from mounts".into(), ToString::to_string),
                        service
                            .rollout
                            .monitor_seconds
                            .as_ref()
                            .map_or_else(
                                || ValidatedRollout::DEFAULT_MONITOR_SECONDS.to_string(),
                                ToString::to_string,
                            ),
                    )}
                </dd>
                <SnapshotRuntime service={service} />
            </dl>
        </div>
    }
}

/// Health check and resource limit rows for `SnapshotService`.
#[component]
fn SnapshotRuntime(service: Service) -> impl IntoView {
    view! {
        <dt>"Health check"</dt>
        <dd>
            {service
                .healthcheck
                .map_or_else(
                    || "None".into(),
                    |check| {
                        let probe = match &check {
                            HealthCheck::Http { port, path, .. } => format!("HTTP :{port}{path}"),
                            HealthCheck::Command { command, .. } => join(command, " "),
                        };
                        let timing = check.timing();
                        format!(
                            "{probe} · every {}s · timeout {}s · start period {}s · {} retries",
                            timing.interval_seconds,
                            timing.timeout_seconds,
                            timing.start_period_seconds,
                            timing.retries,
                        )
                    },
                )}
        </dd>
        <dt>"Resource limits"</dt>
        <dd>
            {service
                .resources
                .map_or_else(
                    || "Runtime defaults".into(),
                    |r| {
                        format!(
                            "CPU: {} · Memory: {}",
                            r
                                .cpu_millis
                                .map_or_else(|| "default".into(), |v| format!("{v} millicores")),
                            r
                                .memory_bytes
                                .map_or_else(|| "default".into(), |v| format!("{v} bytes")),
                        )
                    },
                )}
        </dd>
    }
}

/// Definition list of the values of `${{ vars.<name> }}` references.
fn variable_values<'a, V: Display + 'a>(
    variables: impl IntoIterator<Item = (&'a String, &'a V)>,
) -> AnyView {
    key_values(variables.into_iter().map(|(reference, value)| {
        (
            view! { <code>{reference.clone()}</code> },
            value.to_string(),
        )
    }))
}

/// Joins template elements as written.
fn join(values: &[Template], separator: &str) -> String {
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(separator)
}
