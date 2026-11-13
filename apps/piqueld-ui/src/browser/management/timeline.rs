//! Operation timelines: a deployment's steps as a waterfall on one time
//! axis, followed live over the daemon's timeline stream, with the shown
//! step's detail and output, and the operation's events as they happen.
//!
//! Each open timeline holds one `EventStream`, closed once the operation
//! ended, and one clock ticking every second while it runs. Rows are keyed
//! by step and read their own memo, so a new snapshot only redraws the rows
//! it changed; bars grow between ticks through CSS transitions.
use super::super::format::clock;
use super::super::observability::{headline, kind_badge};
use super::super::stream::EventStream;
use super::super::ui::{Icon, Tone, badge, empty, icon, loading, notice};
use super::super::{Alive, ClientErrorExt, document_hidden, format::now_ms, operation_events};
use super::editor;
use crate::log_output::LogLine;
use crate::timeline::{Progress, Row, RowId, Waterfall, percent};
use gloo_timers::callback::Interval;
use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use piqueld_client::observability::Elapsed;
use piqueld_client::timeline::{Step, StepKey, StepState, Timeline};
use piqueld_client::{
    API_PREFIX, BuildRecord, Client, Event, Operation, Phase, access::AppPermission,
};
use std::collections::VecDeque;
use std::time::Duration;

/// Events the live feed keeps, newest first.
const FEED: usize = 50;
/// Output lines a build or job step shows.
const TAIL: usize = 12;

/// One operation's timeline, followed over its stream until the operation
/// ended. Created by a component or effect; its stream and clock stop when
/// that owner is disposed.
#[derive(Clone, Copy)]
pub(super) struct LiveTimeline {
    timeline: RwSignal<Option<Timeline>>,
    /// The current time, ticking every second while the operation runs.
    now: RwSignal<i64>,
    /// Whether the operation may still change: true until a timeline shows
    /// it ended.
    live: Memo<bool>,
    waterfall: Memo<Waterfall>,
    error: RwSignal<Option<String>>,
    /// Whether the first timeline was drawn, so rows appearing later flash.
    settled: StoredValue<bool>,
    operation: StoredValue<String>,
    stream: StoredValue<Option<EventStream>, LocalStorage>,
    clock: StoredValue<Option<Interval>, LocalStorage>,
    alive: StoredValue<Alive>,
}

impl LiveTimeline {
    /// Follows `operation`'s timeline until it ended or the calling
    /// component unmounts.
    pub(super) fn follow(operation: String) -> Self {
        let timeline = RwSignal::new(None::<Timeline>);
        let now = RwSignal::new(now_ms());
        let live = Memo::new(move |_| timeline.with(|t| t.as_ref().is_none_or(|t| !t.ended())));
        let this = Self {
            timeline,
            now,
            live,
            waterfall: Memo::new(move |_| {
                // Only a running operation's waterfall follows the clock.
                let now = live.get().then(|| now.get());
                timeline.with(|t| t.as_ref().map_or_default(|t| Waterfall::new(&t.steps, now)))
            }),
            error: RwSignal::new(None),
            settled: StoredValue::new(false),
            operation: StoredValue::new(operation),
            stream: StoredValue::new_local(None),
            clock: StoredValue::new_local(None),
            alive: StoredValue::new(Alive::new()),
        };
        on_cleanup(move || this.stop());
        this.connect();
        this
    }

    /// How far the operation has come, once its timeline loaded.
    pub(super) fn progress(self) -> Memo<Option<Progress>> {
        Memo::new(move |_| {
            let now = self.now.get();
            self.timeline
                .with(|t| t.as_ref().map(|t| Progress::new(t, now)))
        })
    }

    /// The operation as of its newest snapshot, once one arrived.
    pub(super) fn operation(self) -> Option<Operation> {
        self.timeline
            .with(|t| t.as_ref().map(|t| t.operation.clone()))
    }

    /// Whether the operation may still change: true until a snapshot shows
    /// it ended.
    pub(super) fn live(self) -> bool {
        self.live.get()
    }

    /// Follows the timeline again once `operation`, as the page last polled
    /// it, ran again after its timeline ended: starting, stopping or
    /// reconciling an environment runs its latest operation again under its
    /// ID, often between two polls, so a newer attempt counts too.
    pub(super) fn resume(self, operation: &Operation) {
        let shown = self
            .timeline
            .with_untracked(|t| t.as_ref().map(|t| t.operation.attempt));
        let again =
            !operation.state.terminal() || shown.is_some_and(|attempt| operation.attempt > attempt);
        if again
            && operation.id == self.operation.get_value()
            && !self.live.get_untracked()
            && self.stream.with_value(Option::is_none)
        {
            self.connect();
        }
    }

    /// Whether the component following the timeline is still mounted.
    fn alive(self) -> bool {
        self.alive.try_with_value(Alive::get).unwrap_or(false)
    }

    /// Opens the stream, which sends the whole timeline at once, then again
    /// on every change.
    fn connect(self) {
        let url = format!(
            "{API_PREFIX}/operations/{}/timeline/stream",
            self.operation.get_value()
        );
        let stream = EventStream::open(
            &url,
            &["timeline", "access_ended"],
            move |kind, data| {
                if kind == "access_ended" {
                    self.error
                        .set(Some("You can no longer read this operation.".into()));
                    self.stop();
                    return;
                }
                match serde_json::from_str(data) {
                    Ok(timeline) => self.show(timeline),
                    Err(error) => self
                        .error
                        .set(Some(format!("The timeline could not be read: {error}"))),
                }
            },
            move || self.recover(),
        );
        self.stream.set_value(stream);
    }

    /// Shows `timeline`, then closes the stream once it ended, or keeps the
    /// clock ticking while it runs.
    fn show(self, timeline: Timeline) {
        let ended = timeline.ended();
        self.now.set(now_ms());
        self.timeline.set(Some(timeline));
        self.error.set(None);
        if ended {
            self.stop();
        } else if self.clock.with_value(Option::is_none) {
            let now = self.now;
            self.clock
                .set_value(Some(Interval::new(1_000, move || now.set(now_ms()))));
        }
        if !self.settled.get_value() {
            // After this timeline's rows are drawn.
            let settled = self.settled;
            request_animation_frame(move || {
                let _ = settled.try_set_value(true);
            });
        }
    }

    /// Closes the stream and stops the clock.
    fn stop(self) {
        let _ = self.stream.try_set_value(None);
        let _ = self.clock.try_set_value(None);
    }

    /// After the browser gave up on the stream: reads the timeline once, to
    /// show it or why it cannot be read, then follows it again while the
    /// operation runs and the failure may pass.
    fn recover(self) {
        self.stop();
        spawn_local(async move {
            let result = Client::browser()
                .operation_timeline(&self.operation.get_value())
                .await;
            if !self.alive() {
                return;
            }
            let retry = match result {
                Ok(timeline) => {
                    let ended = timeline.ended();
                    self.show(timeline);
                    !ended
                }
                Err(error) => {
                    self.error.set(Some(error.user_message()));
                    error.transport()
                }
            };
            if retry {
                TimeoutFuture::new(3_000).await;
                if self.alive() {
                    self.connect();
                }
            }
        });
    }
}

/// A duration as the timeline shows it.
pub(super) fn duration(milliseconds: u64) -> String {
    Elapsed(Duration::from_millis(milliseconds)).to_string()
}

/// The deployment an environment's page follows while it runs, as its
/// polled detail or deployment history first shows it: one timeline stream,
/// whose progress the Overview and Deployments tabs both show.
#[derive(Clone, Copy)]
pub(super) struct RunningDeployment {
    /// The newest operation seen running.
    operation: RwSignal<Option<String>>,
    live: RwSignal<Option<LiveTimeline>>,
}

impl RunningDeployment {
    /// Provides it to the environment page, following the operation its
    /// detail shows running.
    pub(super) fn provide() -> Self {
        let this = Self {
            operation: RwSignal::new(None),
            live: RwSignal::new(None),
        };
        // Followed under this effect, so the next operation's replaces it.
        Effect::new(move |_| {
            this.live
                .set(this.operation.get().map(LiveTimeline::follow));
        });
        let context = editor();
        let signals = context.signals();
        let environment = context.environment_id();
        Effect::new(move |_| {
            signals.detail.with(|detail| {
                if let Some(operation) = detail
                    .as_ref()
                    .filter(|detail| detail.environment.id.as_str() == environment)
                    .and_then(|detail| detail.latest_operation.as_ref())
                {
                    this.saw(operation);
                }
            });
        });
        provide_context(this);
        this
    }

    /// Follows `operation` while it runs, unless it is followed already;
    /// an operation that runs again under its ID is followed again.
    pub(super) fn saw(self, operation: &Operation) {
        if operation.state.terminal() {
            return;
        }
        if self
            .operation
            .with_untracked(|followed| followed.as_deref() != Some(operation.id.as_str()))
        {
            self.operation.set(Some(operation.id.clone()));
        } else if let Some(live) = self.live.get_untracked() {
            live.resume(operation);
        }
    }

    /// The followed timeline while it runs, if it is `operation`'s, or with
    /// `None`, whichever operation's it is.
    pub(super) fn running(self, operation: Option<&str>) -> Option<LiveTimeline> {
        self.live.get().filter(|live| {
            live.live.get()
                && operation.is_none_or(|operation| live.operation.with_value(|id| id == operation))
        })
    }
}

/// The environment's running deployment, in brief: its state, steps and
/// time left, linking to its page. Shown on the Overview tab while it runs.
#[component]
pub(super) fn RunningProgress() -> impl IntoView {
    let context = editor();
    let running = expect_context::<RunningDeployment>();
    move || {
        running.running(None).map(|live| {
            let href = live.operation.with_value(|operation| {
                context.deployment_href(&context.environment_id(), operation)
            });
            let progress = live.progress();
            view! {
                <section
                    class="card running-deployment list-row-link"
                    aria-label="Running deployment"
                >
                    {move || progress.with(|p| p.as_ref().map(status_pill))}
                    <span class="title">
                        <A href={href}>"Follow the deployment"</A>
                        <small>
                            {move || progress.with(|p| p.as_ref().map(|p| p.running.join(", ")))}
                        </small>
                    </span>
                    <CompactProgress live={live} />
                    <span class="chevron" aria-hidden="true">
                        {icon(Icon::ChevronRight)}
                    </span>
                </section>
            }
        })
    }
}

/// A running operation in brief: its steps done, time left, and a segment
/// per step.
#[component]
pub(super) fn CompactProgress(live: LiveTimeline) -> impl IntoView {
    let progress = live.progress();
    view! {
        <span class="progress-compact">
            <span>
                {move || {
                    progress
                        .with(|p| {
                            p.as_ref()
                                .map(|p| {
                                    let outlook = p
                                        .outlook()
                                        .map(|outlook| format!(" · {outlook}"));
                                    view! {
                                        <strong>{p.steps()}</strong>
                                        {outlook}
                                    }
                                })
                        })
                }}
            </span>
            <ProgressBar live={live} />
        </span>
    }
}

/// Where an operation stands, in a word, as a pill: "Deploying".
pub(super) fn status_pill(progress: &Progress) -> impl IntoView + use<> {
    view! {
        <span class="status-pill" data-state={progress.state.as_str()}>
            {progress.status()}
        </span>
    }
}

/// How far a deployment has come, for its page: a segment per step, the
/// steps done and running, the time it has run, about how long is left, and
/// how long it usually takes. The tab title follows it while the page is
/// open, titled by `subject`.
#[component]
pub(super) fn DeploymentProgress(live: LiveTimeline, subject: String) -> impl IntoView {
    let progress = live.progress();
    let title = Memo::new(move |_| progress.with(|p| p.as_ref().map(|p| p.title(&subject))));
    let original = document().title();
    Effect::new(move |_| {
        if let Some(title) = title.get() {
            document().set_title(&title);
        }
    });
    on_cleanup(move || document().set_title(&original));
    view! {
        <ProgressBar live={live} />
        <p class="deployment-progress-summary">
            <span>
                {move || {
                    progress
                        .with(|p| {
                            p.as_ref()
                                .map(|p| {
                                    let doing = (!p.running.is_empty())
                                        .then(|| format!(" · {}", p.running.join(", ")));
                                    view! {
                                        <strong>{p.steps()}</strong>
                                        " steps"
                                        {doing}
                                    }
                                })
                        })
                }}
            </span>
            <span>
                {move || {
                    progress
                        .with(|p| {
                            p.as_ref()
                                .map(|p| {
                                    let outlook = p
                                        .outlook()
                                        .map(|outlook| format!(" · {outlook}"));
                                    let usual = p
                                        .usual_ms
                                        .map(|usual| format!(" · usually {}", duration(usual)));
                                    view! {
                                        <strong>{duration(p.elapsed_ms)}</strong>
                                        " elapsed"
                                        {outlook}
                                        {usual}
                                    }
                                })
                        })
                }}
            </span>
        </p>
    }
}

/// One segment per step: done, failed, left, or filling as a running step
/// nears its usual duration.
#[component]
fn ProgressBar(live: LiveTimeline) -> impl IntoView {
    let waterfall = live.waterfall;
    let ids = Memo::new(move |_| {
        waterfall.with(|w| w.rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>())
    });
    view! {
        <div class="progress-segments" aria-hidden="true">
            <For
                each={move || ids.get()}
                key={Clone::clone}
                children={move |id| {
                    let segment = Memo::new(move |_| {
                        waterfall.with(|w| w.row(&id).map(|row| (row.step.state, row.fill())))
                    });
                    view! {
                        <span
                            class="progress-segment"
                            data-state={move || segment.get().map(|(state, _)| state.as_str())}
                        >
                            <i style:width={move || {
                                segment.get().map(|(_, fill)| format!("{fill}%"))
                            }}></i>
                        </span>
                    }
                }}
            />
        </div>
    }
}

/// A followed operation's steps, the shown step's detail, and with
/// `events:read`, its events as they are recorded. The running step is
/// shown unless a step was picked; "Follow live" returns to it.
#[component]
pub(super) fn OperationTimeline(live: LiveTimeline) -> impl IntoView {
    let picked = RwSignal::new(None::<RowId>);
    let shown = Memo::new(move |_| {
        picked
            .get()
            .or_else(|| live.waterfall.with(|w| w.current().cloned()))
    });
    let feed = editor().can(AppPermission::EventsRead);
    view! {
        <div class="deployment-timeline">
            <section class="deployment-steps" aria-label="Steps and detail">
                {move || live.error.get().map(|error| notice(Tone::Bad, error))}
                <Show
                    when={move || live.timeline.with(Option::is_some)}
                    fallback={move || {
                        live.error.with(Option::is_none).then(|| loading("Loading the timeline…"))
                    }}
                >
                    <StepWaterfall
                        waterfall={live.waterfall.into()}
                        selection={Selection {
                            shown,
                            pick: Callback::new(move |id| picked.set(Some(id))),
                        }}
                        settled={live.settled}
                    />
                    <StepDetail live={live} shown={shown} picked={picked} />
                </Show>
            </section>
            {feed
                .then(|| {
                    view! { <EventFeed operation={live.operation.get_value()} live={live.live} /> }
                })}
        </div>
    }
}

/// The steps `steps` lists, as a waterfall without times: a plan.
#[component]
pub(super) fn PlannedSteps(steps: Vec<StepKey>) -> impl IntoView {
    let steps: Vec<_> = steps
        .into_iter()
        .map(|key| Step::pending(key, StepState::Planned, None))
        .collect();
    view! { <StepWaterfall waterfall={Signal::stored(Waterfall::new(&steps, None))} /> }
}

/// How a waterfall's rows are picked: the shown one, and what picking one
/// does.
#[derive(Clone, Copy)]
struct Selection {
    shown: Memo<Option<RowId>>,
    pick: Callback<RowId>,
}

/// Steps on one time axis, grouped by stage: each step's bar, its usual
/// duration dashed behind it, and the line of now while it runs. Without an
/// axis, as for a plan, just the steps. With `settled`, rows appearing once
/// it is set flash.
#[component]
fn StepWaterfall(
    waterfall: Signal<Waterfall>,
    #[prop(optional)] selection: Option<Selection>,
    #[prop(optional)] settled: Option<StoredValue<bool>>,
) -> impl IntoView {
    let ids = Memo::new(move |_| {
        waterfall.with(|w| w.rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>())
    });
    let axis = Memo::new(move |_| waterfall.with(|w| w.axis));
    let now = Memo::new(move |_| waterfall.with(|w| w.now));
    let shown = selection.map(|selection| Selector::new(move || selection.shown.get()));
    view! {
        <div class="waterfall" class:timed={move || axis.get().is_some()}>
            <div class="waterfall-axis" aria-hidden="true" hidden={move || axis.get().is_none()}>
                <div class="waterfall-ticks">
                    {move || {
                        axis.get()
                            .map(|axis| {
                                axis.ticks()
                                    .map(|(at, label)| {
                                        view! { <span style:left={percent(at)}>{label}</span> }
                                    })
                                    .collect_view()
                            })
                    }}
                </div>
            </div>
            <div class="waterfall-body">
                <div
                    class="waterfall-guides"
                    aria-hidden="true"
                    hidden={move || axis.get().is_none()}
                >
                    {move || {
                        axis.get()
                            .map(|axis| {
                                axis.ticks()
                                    .map(|(at, _)| view! { <span style:left={percent(at)}></span> })
                                    .collect_view()
                            })
                    }}
                    <i
                        class="waterfall-now"
                        hidden={move || now.get().is_none()}
                        style:left={move || now.get().map(percent)}
                    ></i>
                </div>
                <ol class="waterfall-rows" aria-label="Steps">
                    <For
                        each={move || ids.get()}
                        key={Clone::clone}
                        children={move |id| {
                            let row = Memo::new({
                                let id = id.clone();
                                move |_| waterfall.with(|w| w.row(&id).cloned())
                            });
                            let flash = settled.is_some_and(|settled| settled.get_value());
                            let selected = Signal::derive({
                                let (shown, id) = (shown.clone(), Some(id.clone()));
                                move || shown.as_ref().is_some_and(|shown| shown.selected(&id))
                            });
                            let pick = selection
                                .map(|selection| Callback::new(move |()| {
                                    selection.pick.run(id.clone());
                                }));
                            view! {
                                <StepRow row={row} flash={flash} selected={selected} pick={pick} />
                            }
                        }}
                    />
                </ol>
            </div>
        </div>
    }
}

/// One step of a waterfall, after its stage's heading when it opens one.
/// Picking it, by click or keyboard, shows its detail.
#[component]
fn StepRow(
    row: Memo<Option<Row>>,
    flash: bool,
    selected: Signal<bool>,
    pick: Option<Callback<()>>,
) -> impl IntoView {
    let field = move |read: fn(&Row) -> String| {
        move || row.with(|row| row.as_ref().map(read)).unwrap_or_default()
    };
    let stage = Memo::new(move |_| {
        row.with(|row| {
            row.as_ref().and_then(|row| {
                row.stage
                    .map(|stage| (stage, row.step.state == StepState::Forecast))
            })
        })
    });
    let state = move || row.with(|row| row.as_ref().map(|row| row.step.state.as_str()));
    let slow = move || row.with(|row| row.as_ref().is_some_and(|row| row.slow));
    let span = move |read: fn(&Row) -> Option<crate::timeline::Span>| {
        move || row.with(|row| row.as_ref().and_then(read))
    };
    let (bar, usual) = (span(|row| row.bar), span(|row| row.usual));
    let interactive = pick.is_some();
    view! {
        {move || {
            stage
                .get()
                .map(|(stage, forecast)| {
                    view! {
                        <li class="waterfall-stage" aria-hidden="true">
                            {stage.label()}
                        </li>
                        {forecast
                            .then(|| {
                                view! {
                                    <li class="waterfall-note">
                                        "Forecast from the previous deployment: these steps are planned once the steps before them are done."
                                    </li>
                                }
                            })}
                    }
                })
        }}
        <li
            class="waterfall-row"
            class:flash={flash}
            class:selected={selected}
            data-state={state}
            role={interactive.then_some("button")}
            tabindex={interactive.then_some("0")}
            aria-pressed={move || interactive.then(|| selected.get().to_string())}
            on:click={move |_| {
                if let Some(pick) = pick {
                    pick.run(());
                }
            }}
            on:keydown={move |event| {
                if let Some(pick) = pick && matches!(event.key().as_str(), "Enter" | " ") {
                    event.prevent_default();
                    pick.run(());
                }
            }}
        >
            <span class="waterfall-label" title={field(|row| row.step.key.to_string())}>
                <span class="step-icon" aria-hidden="true"></span>
                <span class="step-name">
                    {field(|row| row.step.key.phase.label().to_owned())} " "
                    <span class="step-resource">
                        {field(|row| row.step.key.resource.clone().unwrap_or_default())}
                    </span>
                </span>
                <span class="step-summary">{field(Row::summary)}</span>
            </span>
            <span class="waterfall-track">
                <span
                    class="step-usual"
                    hidden={move || usual().is_none()}
                    style:left={move || usual().map(|span| percent(span.left))}
                    style:width={move || usual().map(|span| percent(span.width))}
                ></span>
                <span
                    class="step-bar"
                    class:slow={slow}
                    hidden={move || bar().is_none()}
                    style:left={move || bar().map(|span| percent(span.left))}
                    style:width={move || bar().map(|span| percent(span.width))}
                ></span>
            </span>
            <span class="waterfall-duration" class:slow={slow}>
                {field(|row| match row.duration_ms {
                    Some(ms) if row.step.state.started() => duration(ms),
                    Some(ms) => format!("~{}", duration(ms)),
                    None => String::new(),
                })}
            </span>
        </li>
    }
}

/// The shown step: how long it took against how long it usually takes, its
/// attempt and retries, why it failed, and for builds and jobs, the end of
/// their output.
#[component]
fn StepDetail(
    live: LiveTimeline,
    shown: Memo<Option<RowId>>,
    picked: RwSignal<Option<RowId>>,
) -> impl IntoView {
    let row = Memo::new(move |_| {
        shown
            .get()
            .and_then(|id| live.waterfall.with(|w| w.row(&id).cloned()))
    });
    // A build or job that started, which has output; remounted per step.
    let output = Memo::new(move |_| {
        row.with(|row| {
            row.as_ref()
                .filter(|row| {
                    row.step.state.started()
                        && matches!(row.step.key.phase, Phase::BuildGit | Phase::RunJob)
                })
                .map(|row| row.id.clone())
        })
    });
    let running = Signal::derive(move || {
        row.with(|row| row.as_ref().is_some_and(|row| row.step.state.running()))
    });
    let logs = editor().can(AppPermission::LogsRead);
    // Redrawn when another step is shown or it changes state, unlike the
    // rest, which follows the clock while it runs.
    let head = Memo::new(move |_| {
        row.with(|row| {
            row.as_ref()
                .map(|row| (row.step.key.to_string(), row.step.state))
        })
    });
    view! {
        <section class="step-detail" aria-label="Step detail">
            <div class="step-detail-head">
                {move || {
                    head.get()
                        .map_or_else(
                            || empty("No step has started yet."),
                            |(label, state)| {
                                let tone = match state {
                                    StepState::Succeeded => Tone::Ok,
                                    StepState::Failed | StepState::Interrupted => Tone::Bad,
                                    StepState::Retrying => Tone::Warn,
                                    StepState::Running => Tone::Info,
                                    StepState::Planned | StepState::Forecast => Tone::Pending,
                                };
                                view! {
                                    <h3>{label}</h3>
                                    {badge(tone, state.as_str())}
                                }
                                    .into_any()
                            },
                        )
                }} <Show when={move || picked.get().is_some()}>
                    <button type="button" class="btn btn-sm" on:click={move |_| picked.set(None)}>
                        "Follow live"
                    </button>
                </Show>
            </div>
            {move || {
                let row = row.get()?;
                let step = row.step;
                let took = row
                    .duration_ms
                    .filter(|_| step.state.started())
                    .map(|ms| {
                        let label = if step.state.running() { "Running for" } else { "Took" };
                        view! {
                            <dt>{label}</dt>
                            <dd class:slow={row.slow}>{duration(ms)}</dd>
                        }
                    });
                let diagnostic = step
                    .diagnostic
                    .map(|diagnostic| {
                        let tone = if step.state == StepState::Failed {
                            Tone::Bad
                        } else {
                            Tone::Warn
                        };
                        notice(
                            tone,
                            view! {
                                <strong>{diagnostic.code}</strong>
                                <span>{diagnostic.summary}</span>
                                <span>{diagnostic.next_action}</span>
                            },
                        )
                    });
                Some(
                    view! {
                        <dl class="kv">
                            <dt>"Usually"</dt>
                            <dd>
                                {step.expected_ms.map_or_else(|| "Not known yet".into(), duration)}
                            </dd>
                            {took}
                            {step
                                .attempt
                                .map(|attempt| {
                                    view! {
                                        <dt>"Attempt"</dt>
                                        <dd>{attempt}</dd>
                                    }
                                })}
                            <dt>"Retries"</dt>
                            <dd>{step.retries}</dd>
                            {step
                                .action_id
                                .map(|action| {
                                    view! {
                                        <dt>"Action"</dt>
                                        <dd>
                                            <code>{action}</code>
                                        </dd>
                                    }
                                })}
                        </dl>
                        {diagnostic}
                    },
                )
            }}
            {move || {
                output
                    .get()
                    .filter(|_| logs)
                    .map(|id| {
                        let operation = live.operation.get_value();
                        let environment = live
                            .timeline
                            .with_untracked(|t| {
                                t.as_ref().map(|t| t.operation.environment_id.to_string())
                            })
                            .unwrap_or_default();
                        view! {
                            <StepOutput
                                environment={environment}
                                operation={operation}
                                id={id}
                                running={running}
                            />
                        }
                    })
            }}
        // Swarm task lanes and downtime of `ensure_service` and
        // `wait_for_service` steps go here once the timeline records them.
        </section>
    }
}

/// The end of a build or job step's output, scrolled to its newest line,
/// read again every 2 seconds while the step runs (and the page is
/// visible), then once more after it ended.
#[component]
fn StepOutput(
    environment: String,
    operation: String,
    id: RowId,
    running: Signal<bool>,
) -> impl IntoView {
    let lines = RwSignal::new(None::<Vec<LogLine>>);
    let error = RwSignal::new(None::<String>);
    let alive = Alive::new();
    let label = id.key.to_string();
    // Scrolled to the newest line whenever lines arrive.
    let tail = NodeRef::<leptos::html::Pre>::new();
    Effect::new(move |_| {
        lines.track();
        request_animation_frame(move || {
            if let Some(tail) = tail.get_untracked() {
                tail.set_scroll_top(tail.scroll_height());
            }
        });
    });
    spawn_local(async move {
        let client = Client::browser();
        let mut build = None;
        loop {
            let ran = !running.get_untracked();
            if !document_hidden() {
                if build.is_none() {
                    match find_build(&client, &environment, &operation, &id.key).await {
                        Ok(found) => build = found,
                        Err(e) => error.set(Some(e.user_message())),
                    }
                }
                if let Some(build) = &build {
                    match client.build_logs(build.id, None, None).await {
                        Ok(page) => {
                            let mut all = LogLine::build(&page.items, &build.service);
                            all.drain(..all.len().saturating_sub(TAIL));
                            lines.set(Some(all));
                        }
                        Err(e) => error.set(Some(e.user_message())),
                    }
                }
                // Read after it ended: final.
                if ran {
                    break;
                }
            }
            TimeoutFuture::new(2_000).await;
            if !alive.get() {
                break;
            }
        }
    });
    view! {
        <div class="step-output" role="region" aria-label={format!("Output of {label}")}>
            {move || error.get().map(|e| notice(Tone::Bad, e))}
            {move || match lines.get() {
                None => view! { <p class="hint">"Waiting for output…"</p> }.into_any(),
                Some(lines) if lines.is_empty() => {
                    view! { <p class="hint">"No output yet."</p> }.into_any()
                }
                Some(lines) => {
                    view! {
                        <pre class="step-output-lines" node_ref={tail}>
                            {lines
                                .into_iter()
                                .map(|line| {
                                    let severity = line.severity();
                                    view! {
                                        <span class="log-line" data-severity={severity}>
                                            {line.message}
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

/// The build record of `operation` running the build or job step `key`,
/// looked up in `environment`'s newest builds; `None` until it is recorded.
async fn find_build(
    client: &Client,
    environment: &str,
    operation: &str,
    key: &StepKey,
) -> Result<Option<BuildRecord>, piqueld_client::ClientError> {
    let resource = key.resource.as_deref();
    let runs = |build: &BuildRecord| {
        build.operation_id == operation
            && match key.phase {
                Phase::RunJob => build.job.as_deref() == resource,
                _ => build.job.is_none() && Some(build.service.as_str()) == resource,
            }
    };
    let mut cursor = None;
    // An older deployment's builds are a few pages back at most.
    for _ in 0..5 {
        let page = client
            .builds(None, Some(environment), cursor.as_deref())
            .await?;
        if let Some(build) = page.items.into_iter().find(runs) {
            return Ok(Some(build));
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    Ok(None)
}

/// The operation's events as they are recorded, newest first, keeping the
/// newest `FEED`, with a link to all of them. Its stream replays the
/// operation's history, then follows it; it closes a moment after the
/// operation ended, once its last events arrived. Events arriving together
/// are drawn in one frame.
#[component]
fn EventFeed(operation: String, live: Memo<bool>) -> impl IntoView {
    let events = RwSignal::new(VecDeque::<Event>::new());
    let pending = StoredValue::new_local(Vec::<Event>::new());
    let settled = StoredValue::new(false);
    let stream = StoredValue::new_local(None::<EventStream>);
    let flush = move || {
        let arrived = pending.try_update_value(std::mem::take).unwrap_or_default();
        events.update(|events| {
            for event in arrived {
                events.push_front(event);
            }
            events.truncate(FEED);
        });
        request_animation_frame(move || {
            let _ = settled.try_set_value(true);
        });
    };
    let url = format!("{API_PREFIX}/events/stream?operation_id={operation}");
    let history = operation_events(&operation);
    // The newest event received, which a reopened stream resumes after.
    let last = StoredValue::new(0_i64);
    let open = move || {
        let url = match last.get_value() {
            0 => url.clone(),
            id => format!("{url}&cursor=v1:{id}"),
        };
        stream.set_value(EventStream::open(
            &url,
            &["event"],
            move |_, data| {
                let Ok(event) = serde_json::from_str::<Event>(data) else {
                    return;
                };
                last.set_value(event.id);
                let first = pending
                    .try_update_value(|pending| {
                        pending.push(event);
                        pending.len() == 1
                    })
                    .unwrap_or(false);
                if first {
                    request_animation_frame(flush);
                }
            },
            // The history the feed shows stays as it is.
            || {},
        ));
    };
    open();
    let alive = Alive::new();
    Effect::new(move |_| {
        if live.get() {
            // The operation runs again: follow its new events.
            if stream.with_value(Option::is_none) {
                open();
            }
        } else {
            let alive = alive.clone();
            spawn_local(async move {
                TimeoutFuture::new(2_000).await;
                if alive.get() && !live.get_untracked() {
                    stream.set_value(None);
                }
            });
        }
    });
    on_cleanup(move || {
        let _ = stream.try_set_value(None);
    });
    view! {
        <aside class="event-feed" aria-label="Live events">
            <div class="event-feed-head">
                <h3>"Live events"</h3>
                <Show when={move || live.get()}>
                    <span class="event-feed-live">"streaming"</span>
                </Show>
                <A href={history} attr:class="event-feed-all">
                    "All events"
                </A>
            </div>
            <Show when={move || events.with(VecDeque::is_empty)}>
                <p class="hint">"No events yet."</p>
            </Show>
            <ol>
                <For
                    each={move || events.get()}
                    key={|event| event.id}
                    children={move |event| {
                        view! {
                            <li class:fresh={settled.get_value()}>
                                <time>{clock(event.created_at_ms)}</time>
                                <span class="event-feed-kind">{kind_badge(&event)}</span>
                                <span class="event-feed-message">{headline(&event)}</span>
                            </li>
                        }
                    }}
                />
            </ol>
        </aside>
    }
}
