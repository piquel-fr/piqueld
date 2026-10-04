//! One-shot job editing. Jobs run in list order before each rollout.
use super::super::ui::{Icon, empty, icon, remove_button};
use super::{dirty_group, editor, save_actions};
use leptos::prelude::*;
use piqueld_client::{Job, JobRun, edit::ApplicationEdit};

/// One editable job. The command keeps one element per row, without shell parsing.
#[derive(Clone, PartialEq)]
struct JobDraft {
    name: String,
    service: String,
    command: Vec<String>,
    timeout: String,
}

impl Default for JobDraft {
    fn default() -> Self {
        Self {
            name: String::new(),
            service: String::new(),
            command: vec![String::new()],
            timeout: Job::DEFAULT_TIMEOUT_SECONDS.to_string(),
        }
    }
}

impl JobDraft {
    /// Saved jobs as editable rows.
    fn rows(jobs: Vec<Job>) -> Vec<Self> {
        jobs.into_iter().map(Self::from).collect()
    }

    /// The job input, or a message when the timeout does not parse.
    fn job(self) -> Result<Job, &'static str> {
        Ok(Job {
            name: self.name,
            service: self.service,
            command: self.command,
            run: JobRun::BeforeRollout,
            timeout_seconds: self
                .timeout
                .parse()
                .map_err(|_| "Job timeout must be a whole number of seconds")?,
        })
    }
}

impl From<Job> for JobDraft {
    fn from(job: Job) -> Self {
        Self {
            name: job.name,
            service: job.service,
            command: job.command,
            timeout: job.timeout_seconds.to_string(),
        }
    }
}

/// Reads one field of a job, empty when the job was removed.
fn field<T: Default>(
    draft: RwSignal<Vec<JobDraft>>,
    index: usize,
    read: impl Fn(&JobDraft) -> T,
) -> T {
    draft.with(|jobs| jobs.get(index).map(read).unwrap_or_default())
}

/// Updates one job in place.
fn edit(draft: RwSignal<Vec<JobDraft>>, index: usize, write: impl FnOnce(&mut JobDraft)) {
    draft.update(|jobs| {
        if let Some(job) = jobs.get_mut(index) {
            write(job);
        }
    });
}

/// Command elements of job `index`, one row each, plus a button adding one.
fn command_rows(draft: RwSignal<Vec<JobDraft>>, index: usize) -> AnyView {
    view! {
        <div class="form-list">
            <For
                each={move || (0..field(draft, index, |j| j.command.len())).collect::<Vec<_>>()}
                key={|element| *element}
                children={move |element| {
                    view! {
                        <div class="form-row">
                            <label class="field">
                                <span>"Command element"</span>
                                <input
                                    type="text"
                                    prop:value={move || {
                                        field(draft, index, |j| j.command.get(element).cloned().unwrap_or_default())
                                    }}
                                    on:input={move |event| {
                                        edit(
                                            draft,
                                            index,
                                            |j| {
                                                if let Some(value) = j.command.get_mut(element) {
                                                    *value = event_target_value(&event);
                                                }
                                            },
                                        );
                                    }}
                                />
                            </label>
                            {remove_button(move || {
                                edit(
                                    draft,
                                    index,
                                    |j| {
                                        j.command.remove(element);
                                    },
                                );
                            })}
                        </div>
                    }
                }}
            />
        </div>
        <div>
            <button
                type="button"
                class="btn btn-sm"
                on:click={move |_| edit(draft, index, |j| j.command.push(String::new()))}
            >
                {icon(Icon::Plus)}
                "Add command element"
            </button>
        </div>
    }
    .into_any()
}

/// Job editor. Rows are drafted as [`JobDraft`] text and re-synced from saved
/// configuration only while there are no local edits. Saving validates the
/// timeouts and replaces all jobs; their order is their execution order.
#[component]
pub(super) fn JobSettings() -> impl IntoView {
    let context = editor();
    let draft = RwSignal::new(JobDraft::rows(context.manifest().spec.jobs));
    let baseline = RwSignal::new(draft.get_untracked());
    dirty_group("jobs".into(), draft, baseline);
    Effect::new(move |_| {
        let saved_jobs = context
            .saved
            .with(|saved| JobDraft::rows(saved.application.to_manifest().spec.jobs));
        if draft.get_untracked() == baseline.get_untracked() {
            draft.set(saved_jobs.clone());
            baseline.set(saved_jobs);
        }
    });
    let save = move || {
        let jobs = match draft
            .get_untracked()
            .into_iter()
            .map(JobDraft::job)
            .collect()
        {
            Ok(jobs) => jobs,
            Err(message) => {
                context.error.set(Some(message.into()));
                return;
            }
        };
        context.save(
            ApplicationEdit::Jobs(jobs),
            Callback::new(move |saved: piqueld_client::ApplicationView| {
                draft.set(JobDraft::rows(saved.application.to_manifest().spec.jobs));
                baseline.set(draft.get_untracked());
            }),
        );
    };
    let swap = move |first: usize| {
        draft.update(|jobs| {
            if first + 1 < jobs.len() {
                jobs.swap(first, first + 1);
            }
        });
    };
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"Jobs"</h3>
                    <p>
                        "Each deployment runs these commands to completion, in order, after preparing images and before changing any service — for example database migrations. A job reuses its service's image, environment, secrets, and volume mounts. A failed job stops the deployment and the current version keeps running. Each command element is one row; no shell parsing is applied."
                    </p>
                </div>
            </header>
            <fieldset class="stack-sm" disabled={move || context.blocked()}>
                <Show when={move || draft.with(Vec::is_empty)}>
                    {empty("No jobs. Deployments roll out services directly.")}
                </Show>
                <For
                    each={move || (0..draft.with(Vec::len)).collect::<Vec<_>>()}
                    key={|index| *index}
                    children={move |index| {
                        view! {
                            <div class="stack-sm job-draft" data-job={index}>
                                <div class="form-row">
                                    <label class="field">
                                        <span>"Name"</span>
                                        <input
                                            type="text"
                                            placeholder="migrate"
                                            prop:value={move || field(draft, index, |j| j.name.clone())}
                                            on:input={move |event| {
                                                edit(draft, index, |j| j.name = event_target_value(&event));
                                            }}
                                        />
                                    </label>
                                    <label class="field">
                                        <span>"Service"</span>
                                        <select
                                            prop:value={move || field(draft, index, |j| j.service.clone())}
                                            on:change={move |event| {
                                                edit(draft, index, |j| j.service = event_target_value(&event));
                                            }}
                                        >
                                            <option value="">"Select a service"</option>
                                            {move || {
                                                context
                                                    .manifest()
                                                    .spec
                                                    .services
                                                    .into_iter()
                                                    .map(|service| {
                                                        view! {
                                                            <option value={service
                                                                .name
                                                                .clone()}>{service.name.clone()}</option>
                                                        }
                                                    })
                                                    .collect_view()
                                            }}
                                        </select>
                                    </label>
                                    <label class="field" style="max-width:160px">
                                        <span>"Timeout (seconds)"</span>
                                        <input
                                            type="number"
                                            min="1"
                                            max={Job::MAX_TIMEOUT_SECONDS}
                                            prop:value={move || field(draft, index, |j| j.timeout.clone())}
                                            on:input={move |event| {
                                                edit(draft, index, |j| j.timeout = event_target_value(&event));
                                            }}
                                        />
                                    </label>
                                    <div class="btn-group">
                                        <button
                                            type="button"
                                            class="btn btn-ghost"
                                            disabled={move || index == 0}
                                            on:click={move |_| swap(index - 1)}
                                        >
                                            "Move up"
                                        </button>
                                        <button
                                            type="button"
                                            class="btn btn-ghost"
                                            disabled={move || index + 1 >= draft.with(Vec::len)}
                                            on:click={move |_| swap(index)}
                                        >
                                            "Move down"
                                        </button>
                                        {remove_button(move || {
                                            draft
                                                .update(|jobs| {
                                                    jobs.remove(index);
                                                });
                                        })}
                                    </div>
                                </div>
                                {command_rows(draft, index)}
                            </div>
                        }
                    }}
                />
                <div>
                    <button
                        type="button"
                        class="btn btn-sm"
                        disabled={move || draft.with(Vec::len) >= Job::MAX_PER_APPLICATION}
                        on:click={move |_| draft.update(|jobs| jobs.push(JobDraft::default()))}
                    >
                        {icon(Icon::Plus)}
                        "Add job"
                    </button>
                </div>
                {save_actions(draft, baseline, save, || false)}
            </fieldset>
        </section>
    }
}
