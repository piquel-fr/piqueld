//! One-shot job editing. Jobs run in list order before each rollout.
use super::super::ui::{add_button, empty, limited_add_button, remove_button};
use super::{Draft, Rows, editor};
use leptos::prelude::*;
use piqueld_client::{ApplicationManifest, Job, JobRun, JobVolumeAccess, edit::ApplicationEdit};

/// One editable job. The command keeps one element per row, without shell parsing.
#[derive(Clone, PartialEq)]
struct JobDraft {
    name: String,
    service: String,
    command: Vec<String>,
    timeout: String,
    volume_access: JobVolumeAccess,
}

impl Default for JobDraft {
    fn default() -> Self {
        Self {
            name: String::new(),
            service: String::new(),
            command: vec![String::new()],
            timeout: Job::DEFAULT_TIMEOUT_SECONDS.to_string(),
            volume_access: JobVolumeAccess::default(),
        }
    }
}

impl JobDraft {
    /// The manifest's jobs as editable rows.
    fn rows(manifest: &ApplicationManifest) -> Vec<Self> {
        manifest.spec.jobs.iter().cloned().map(Self::from).collect()
    }

    /// The job input, or a message when the timeout does not parse.
    fn job(self) -> Result<Job, &'static str> {
        Ok(Job {
            name: self.name,
            service: self.service,
            command: self
                .command
                .iter()
                .map(|value| value.as_str().into())
                .collect(),
            run: JobRun::BeforeRollout,
            timeout_seconds: self
                .timeout
                .parse()
                .map_err(|_| "Job timeout must be a whole number of seconds")?,
            volume_access: self.volume_access,
        })
    }
}

impl From<Job> for JobDraft {
    fn from(job: Job) -> Self {
        Self {
            name: job.name,
            service: job.service,
            command: job.command.iter().map(ToString::to_string).collect(),
            timeout: job.timeout_seconds.to_string(),
            volume_access: job.volume_access,
        }
    }
}

/// Command elements of job `index`, one row each, plus a button adding one.
fn command_rows(jobs: RwSignal<Vec<JobDraft>>, index: usize) -> AnyView {
    view! {
        <div class="form-list">
            <For
                each={move || (0..jobs.field(index, |j| j.command.len())).collect::<Vec<_>>()}
                key={|element| *element}
                children={move |element| {
                    view! {
                        <div class="form-row">
                            <label class="field">
                                <span>"Command element"</span>
                                <input
                                    type="text"
                                    prop:value={move || {
                                        jobs.field(
                                            index,
                                            |j| j.command.get(element).cloned().unwrap_or_default(),
                                        )
                                    }}
                                    on:input={move |event| {
                                        jobs.edit(
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
                                jobs.edit(
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
            {add_button(
                "Add command element",
                move || jobs.edit(index, |j| j.command.push(String::new())),
            )}
        </div>
    }
    .into_any()
}

/// Job editor. Rows are drafted as [`JobDraft`] text and re-synced from saved
/// configuration only while there are no local edits. Saving validates the
/// timeouts and replaces all jobs; their order is their execution order.
#[component]
#[expect(clippy::too_many_lines, reason = "flat view! markup")]
pub(super) fn JobSettings() -> impl IntoView {
    let context = editor();
    let draft = Draft::synced("jobs", JobDraft::rows);
    let jobs = draft.value;
    let save = move || match jobs
        .get_untracked()
        .into_iter()
        .map(JobDraft::job)
        .collect()
    {
        Ok(saved) => draft.save(ApplicationEdit::Jobs(saved), JobDraft::rows),
        Err(message) => context.error.set(Some(message.into())),
    };
    let swap = move |first: usize| {
        jobs.update(|jobs| {
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
                        "Each deployment runs these commands to completion, in order, after preparing images. A job reuses its service's image, environment, secrets, and volume mounts, and waits for that service's startup dependencies to be healthy. Dependencies start or update first; other services roll out only after all jobs succeed. A failed job stops the deployment and dependencies return to the running deployment. Volumes are read-only unless the job has exclusive access, which stops every service using its writable volumes until the rollout. Each command element is one row; no shell parsing is applied."
                    </p>
                </div>
            </header>
            <fieldset class="stack-sm" disabled={move || context.blocked()}>
                <Show when={move || {
                    jobs.with(Vec::is_empty)
                }}>{empty("No jobs. Deployments roll out services directly.")}</Show>
                <For
                    each={move || jobs.indices()}
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
                                            prop:value={move || jobs.field(index, |j| j.name.clone())}
                                            on:input={move |event| {
                                                jobs.edit(index, |j| j.name = event_target_value(&event));
                                            }}
                                        />
                                    </label>
                                    <label class="field">
                                        <span>"Service"</span>
                                        <select
                                            prop:value={move || {
                                                jobs.field(index, |j| j.service.clone())
                                            }}
                                            on:change={move |event| {
                                                jobs.edit(
                                                    index,
                                                    |j| j.service = event_target_value(&event),
                                                );
                                            }}
                                        >
                                            {context.service_options()}
                                        </select>
                                    </label>
                                    <label class="field" style="max-width:160px">
                                        <span>"Timeout (seconds)"</span>
                                        <input
                                            type="number"
                                            min="1"
                                            max={Job::MAX_TIMEOUT_SECONDS}
                                            prop:value={move || {
                                                jobs.field(index, |j| j.timeout.clone())
                                            }}
                                            on:input={move |event| {
                                                jobs.edit(
                                                    index,
                                                    |j| j.timeout = event_target_value(&event),
                                                );
                                            }}
                                        />
                                    </label>
                                    <label class="field" style="max-width:200px">
                                        <span>"Volume access"</span>
                                        <select
                                            prop:value={move || {
                                                jobs.field(index, |j| j.volume_access.as_str())
                                            }}
                                            on:change={move |event| {
                                                if let Ok(access) = event_target_value(&event).parse() {
                                                    jobs.edit(index, |j| j.volume_access = access);
                                                }
                                            }}
                                        >
                                            <option value={JobVolumeAccess::ReadOnly
                                                .as_str()}>"Read-only"</option>
                                            <option value={JobVolumeAccess::Exclusive
                                                .as_str()}>"Exclusive"</option>
                                        </select>
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
                                            disabled={move || index + 1 >= jobs.with(Vec::len)}
                                            on:click={move |_| swap(index)}
                                        >
                                            "Move down"
                                        </button>
                                        {remove_button(move || jobs.remove(index))}
                                    </div>
                                </div>
                                {command_rows(jobs, index)}
                            </div>
                        }
                    }}
                />
                <div>
                    {limited_add_button(
                        "Add job",
                        move || jobs.with(Vec::len) >= Job::MAX_PER_APPLICATION,
                        move || jobs.update(|jobs| jobs.push(JobDraft::default())),
                    )}
                </div>
                {draft.actions(save, || false)}
            </fieldset>
        </section>
    }
}
