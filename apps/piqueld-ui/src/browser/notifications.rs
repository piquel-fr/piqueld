//! `/notifications` page: destinations, what each receives, and the delivery
//! history.
use super::observability::{Refresh, older_page};
use super::ui::{
    Icon, Modal, PageHeader, Tone, badge, checkbox, empty, icon, loaded, member_checkbox, notice,
    text_field, when,
};
use super::{ClientErrorExt, UserError, access::can, confirm, dashboard_context};
use leptos::prelude::*;
use leptos::task::spawn_local;
use piqueld_client::{
    ApplicationId, Client, ClientError,
    access::{GlobalPermission, Permission},
    observability::{
        ApplicationSelection, CategoryRule, DeliveryState, DestinationKind, DestinationRequest,
        DestinationSettings, NotificationCategory, NotificationDelivery, NotificationDestination,
        NotificationMessage,
    },
};
use std::collections::{BTreeMap, BTreeSet};

/// `/notifications` page: destinations, and paginated deliveries with a
/// retry button on failed ones.
#[component]
pub(super) fn NotificationsPage() -> impl IntoView {
    let refresh = Refresh::new();
    let cursor = RwSignal::new(None::<String>);
    view! {
        <PageHeader
            title="Notifications"
            description="Where notifications go, what each destination receives, and every delivery. A delivery may repeat after a lost acknowledgement."
        >
            <button
                type="button"
                class="btn"
                on:click={move |_| {
                    cursor.set(None);
                    refresh.request();
                }}
            >
                {icon(Icon::Refresh)}
                "Refresh"
            </button>
        </PageHeader>
        <div class="stack">
            <Destinations refresh={refresh} />
            <Deliveries refresh={refresh} cursor={cursor} />
        </div>
    }
}

/// Outcome of the last destination action: an error or a success notice.
#[derive(Clone, Copy)]
struct Outcome {
    busy: RwSignal<bool>,
    message: RwSignal<Option<(Tone, String)>>,
}

impl Outcome {
    /// Runs `request` unless another is running, then shows `success` or
    /// its error and runs `refresh`.
    fn run<T: 'static>(
        self,
        request: impl Future<Output = Result<T, ClientError>> + 'static,
        success: impl FnOnce(T) -> String + 'static,
        refresh: Refresh,
    ) {
        if self.busy.get_untracked() {
            return;
        }
        self.busy.set(true);
        self.message.set(None);
        spawn_local(async move {
            let message = match request.await {
                Ok(value) => (Tone::Ok, success(value)),
                Err(error) => (Tone::Bad, error.user_message()),
            };
            self.message.set(Some(message));
            self.busy.set(false);
            refresh.request();
        });
    }
}

/// The destinations, with buttons to test, edit and delete them for
/// accounts with `notifications:manage`.
#[component]
fn Destinations(refresh: Refresh) -> impl IntoView {
    let manage = can(Permission::Global(GlobalPermission::NotificationsManage));
    let outcome = Outcome {
        busy: RwSignal::new(false),
        message: RwSignal::new(None),
    };
    let form = DestinationForm::new();
    let opened = RwSignal::new(false);
    let data = LocalResource::new(move || {
        refresh.track();
        async move {
            Client::browser()
                .notification_destinations()
                .await
                .user_error()
        }
    });
    let edit = move |destination: Option<NotificationDestination>| {
        form.load(destination.as_ref());
        opened.set(true);
    };
    let save = move || match form.request() {
        Ok((name, request)) => {
            opened.set(false);
            outcome.run(
                async move {
                    Client::browser()
                        .put_notification_destination(&name, &request)
                        .await
                },
                |destination| format!("Saved {}.", destination.name),
                refresh,
            );
        }
        Err(message) => outcome.message.set(Some((Tone::Bad, message))),
    };
    let row = move |destination: NotificationDestination| {
        let name = destination.name.to_string();
        let (test, remove) = (name.clone(), name.clone());
        let settings = destination.settings.clone();
        view! {
            <tr>
                <td>
                    <strong>{name.clone()}</strong>
                </td>
                <td>{settings.kind.to_string()}</td>
                <td>{receives(&settings)}</td>
                <td>
                    {if settings.enabled {
                        badge(Tone::Ok, "enabled")
                    } else {
                        badge(Tone::Neutral, "disabled")
                    }}
                </td>
                <td class="actions">
                    <Show when={move || manage.get()}>
                        <span class="btn-group" style="justify-content:flex-end">
                            <button
                                type="button"
                                class="btn btn-sm"
                                disabled={move || outcome.busy.get()}
                                on:click={
                                    let name = test.clone();
                                    move |_| {
                                        let name = name.clone();
                                        outcome.run(
                                            async move {
                                                Client::browser()
                                                    .test_notification_destination(&name)
                                                    .await
                                            },
                                            |test| match test.error {
                                                None => "The test notification was acknowledged.".into(),
                                                Some(error) => format!("The test notification failed: {error}"),
                                            },
                                            refresh,
                                        );
                                    }
                                }
                            >
                                "Test"
                            </button>
                            <button
                                type="button"
                                class="btn btn-sm"
                                disabled={move || outcome.busy.get()}
                                on:click={
                                    let destination = destination.clone();
                                    move |_| edit(Some(destination.clone()))
                                }
                            >
                                "Edit"
                            </button>
                            <button
                                type="button"
                                class="btn btn-ghost btn-sm"
                                disabled={move || outcome.busy.get()}
                                on:click={
                                    let name = remove.clone();
                                    move |_| {
                                        if !confirm(
                                            &format!(
                                                "Delete {name}? Its pending deliveries are cancelled.",
                                            ),
                                        ) {
                                            return;
                                        }
                                        let name = name.clone();
                                        outcome.run(
                                            async move {
                                                Client::browser()
                                                    .delete_notification_destination(&name)
                                                    .await
                                            },
                                            |()| "Destination deleted.".into(),
                                            refresh,
                                        );
                                    }
                                }
                            >
                                "Delete"
                            </button>
                        </span>
                    </Show>
                </td>
            </tr>
        }
    };
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"Destinations"</h3>
                    <p>
                        "Webhooks receiving notifications. Each chooses its categories, applications and previews; daemon notifications reach it whatever its applications."
                    </p>
                </div>
                <Show when={move || manage.get()}>
                    <button
                        type="button"
                        class="btn btn-sm btn-primary"
                        on:click={move |_| edit(None)}
                    >
                        {icon(Icon::Plus)}
                        "Add destination"
                    </button>
                </Show>
            </header>
            <div class="stack-sm">
                {move || outcome.message.get().map(|(tone, message)| notice(tone, message))}
                {loaded(
                    data,
                    "Loading destinations…",
                    move |destinations| {
                        if destinations.is_empty() {
                            return empty("No destinations. Nothing is notified.");
                        }
                        view! {
                            <div class="table-wrap">
                                <table class="table">
                                    <thead>
                                        <tr>
                                            <th>"Name"</th>
                                            <th>"Kind"</th>
                                            <th>"Receives"</th>
                                            <th>"Status"</th>
                                            <th></th>
                                        </tr>
                                    </thead>
                                    <tbody>
                                        {destinations.into_iter().map(row).collect_view()}
                                    </tbody>
                                </table>
                            </div>
                        }
                            .into_any()
                    },
                )}
            </div>
            <Modal title="Notification destination" opened={opened} wide=true>
                {form.view()}
                <div class="form-actions">
                    <button
                        type="button"
                        class="btn btn-primary"
                        disabled={move || outcome.busy.get()}
                        on:click={move |_| save()}
                    >
                        "Save destination"
                    </button>
                </div>
            </Modal>
        </section>
    }
}

/// What a destination receives, in a few words: `5 categories · shop, notes · previews`.
fn receives(settings: &DestinationSettings) -> String {
    let applications = match &settings.applications {
        ApplicationSelection::All => "every application".to_owned(),
        ApplicationSelection::Only(ids) => {
            let names = dashboard_context()
                .signals
                .applications
                .with_untracked(|rows| {
                    ids.iter()
                        .map(|id| {
                            rows.iter()
                                .find(|row| &row.application.id == id)
                                .map_or_else(|| id.to_string(), |row| row.application.name.clone())
                        })
                        .collect::<Vec<_>>()
                });
            if names.is_empty() {
                "no application".to_owned()
            } else {
                names.join(", ")
            }
        }
    };
    let previews = if settings.previews {
        " · previews"
    } else {
        ""
    };
    format!(
        "{} categories · {applications}{previews}",
        settings.categories.len()
    )
}

/// The destination the dialog edits. Messages are kept for unchecked
/// categories too, so unchecking one by mistake loses nothing.
#[derive(Clone, Copy)]
struct DestinationForm {
    name: RwSignal<String>,
    /// Whether it edits an existing destination, whose name is fixed.
    existing: RwSignal<bool>,
    kind: RwSignal<DestinationKind>,
    /// A new URL; empty keeps an existing destination's.
    url: RwSignal<String>,
    enabled: RwSignal<bool>,
    categories: RwSignal<BTreeSet<NotificationCategory>>,
    messages: RwSignal<BTreeMap<NotificationCategory, String>>,
    all_applications: RwSignal<bool>,
    applications: RwSignal<BTreeSet<ApplicationId>>,
    previews: RwSignal<bool>,
}

impl DestinationForm {
    fn new() -> Self {
        Self {
            name: RwSignal::new(String::new()),
            existing: RwSignal::new(false),
            kind: RwSignal::new(DestinationKind::Discord),
            url: RwSignal::new(String::new()),
            enabled: RwSignal::new(true),
            categories: RwSignal::new(BTreeSet::new()),
            messages: RwSignal::new(BTreeMap::new()),
            all_applications: RwSignal::new(true),
            applications: RwSignal::new(BTreeSet::new()),
            previews: RwSignal::new(false),
        }
    }

    /// Shows `destination`, or a new Discord destination's defaults.
    fn load(self, destination: Option<&NotificationDestination>) {
        let settings = destination.map_or_else(
            || DestinationSettings::new(DestinationKind::Discord),
            |destination| destination.settings.clone(),
        );
        self.name
            .set(destination.map_or_else(String::new, |d| d.name.to_string()));
        self.existing.set(destination.is_some());
        self.kind.set(settings.kind);
        self.url.set(String::new());
        self.enabled.set(settings.enabled);
        self.categories
            .set(settings.categories.keys().copied().collect());
        self.messages.set(
            settings
                .categories
                .iter()
                .filter_map(|(category, rule)| {
                    Some((*category, rule.message.as_ref()?.to_string()))
                })
                .collect(),
        );
        match settings.applications {
            ApplicationSelection::All => {
                self.all_applications.set(true);
                self.applications.set(BTreeSet::new());
            }
            ApplicationSelection::Only(ids) => {
                self.all_applications.set(false);
                self.applications.set(ids);
            }
        }
        self.previews.set(settings.previews);
    }

    /// The destination's name and the request saving it, or why it can't be saved.
    fn request(self) -> Result<(String, DestinationRequest), String> {
        let messages = self.messages.get_untracked();
        let categories = self
            .categories
            .get_untracked()
            .into_iter()
            .map(|category| {
                let message = messages
                    .get(&category)
                    .map(|message| message.trim())
                    .filter(|message| !message.is_empty())
                    .map(NotificationMessage::parse)
                    .transpose()
                    .map_err(|error| format!("{}: {error}", category.label()))?;
                Ok((category, CategoryRule { message }))
            })
            .collect::<Result<_, String>>()?;
        let url = self.url.get_untracked().trim().to_owned();
        Ok((
            self.name.get_untracked().trim().to_owned(),
            DestinationRequest {
                settings: DestinationSettings {
                    kind: self.kind.get_untracked(),
                    enabled: self.enabled.get_untracked(),
                    categories,
                    applications: if self.all_applications.get_untracked() {
                        ApplicationSelection::All
                    } else {
                        ApplicationSelection::Only(self.applications.get_untracked())
                    },
                    previews: self.previews.get_untracked(),
                },
                url: (!url.is_empty()).then_some(url),
            },
        ))
    }

    fn view(self) -> impl IntoView {
        let applications = dashboard_context().signals.applications;
        let category = move |category: NotificationCategory| {
            let checked = move || self.categories.with(|set| set.contains(&category));
            view! {
                <div class="category-rule">
                    <label class="checkbox">
                        {member_checkbox(self.categories, category)}
                        {category.label()}
                    </label>
                    <div class="field">
                        <input
                            type="text"
                            aria-label={format!("{} message", category.label())}
                            placeholder="Optional message, such as <@&ROLE_ID>"
                            disabled={move || !checked()}
                            prop:value={move || {
                                self.messages
                                    .with(|messages| messages.get(&category).cloned())
                                    .unwrap_or_default()
                            }}
                            on:input={move |event| {
                                let message = event_target_value(&event);
                                self.messages
                                    .update(|messages| {
                                        messages.insert(category, message);
                                    });
                            }}
                        />
                    </div>
                </div>
            }
        };
        view! {
            <div class="stack-sm">
                <fieldset class="stack-sm" disabled={move || self.existing.get()}>
                    {text_field("Name", self.name)}
                </fieldset>
                <label class="field">
                    <span>"Kind"</span>
                    <select
                        prop:value={move || self.kind.get().to_string()}
                        on:change={move |event| {
                            if let Some(kind) = DestinationKind::parse(&event_target_value(&event)) {
                                self.kind.set(kind);
                            }
                        }}
                    >
                        <option value="discord">"Discord webhook"</option>
                        <option value="json">"JSON webhook"</option>
                    </select>
                </label>
                <label class="field">
                    <span>"Webhook URL"</span>
                    <input
                        type="password"
                        autocomplete="off"
                        placeholder={move || {
                            if self.existing.get() { "Unchanged" } else { "https://…" }
                        }}
                        prop:value={move || self.url.get()}
                        on:input={move |event| self.url.set(event_target_value(&event))}
                    />
                </label>
                <p class="hint">
                    "Stored encrypted and never shown again. A new URL or kind makes it a new receiver: pending deliveries are cancelled."
                </p>
                {checkbox("Enabled", self.enabled)}
                <h4>"Categories"</h4>
                <p class="hint">
                    "A category's message is sent with each of its notifications. On Discord it is the message text, so mentions in it notify."
                </p>
                {NotificationCategory::ALL.into_iter().map(category).collect_view()}
                <h4>"Applications"</h4>
                {checkbox("Every application, including ones created later", self.all_applications)}
                <div hidden={move || self.all_applications.get()} class="stack-sm">
                    <For
                        each={move || applications.get()}
                        key={|row| row.application.id.clone()}
                        children={move |row| {
                            view! {
                                <label class="checkbox">
                                    {member_checkbox(self.applications, row.application.id)}
                                    {row.application.name}
                                </label>
                            }
                        }}
                    />
                </div>
                {checkbox("Previews", self.previews)}
            </div>
        }
    }
}

/// Retrying failed notification deliveries: `allowed` with `system:operate`,
/// and one at a time, `busy` while one is sent.
#[derive(Clone, Copy)]
struct Retry {
    allowed: Signal<bool>,
    busy: RwSignal<bool>,
    run: Callback<String>,
}

/// A page of deliveries, newest first, from `cursor`.
#[component]
fn Deliveries(refresh: Refresh, cursor: RwSignal<Option<String>>) -> impl IntoView {
    let error = RwSignal::new(None::<String>);
    let data = LocalResource::new(move || {
        refresh.track();
        let cursor = cursor.get();
        async move {
            Client::browser()
                .notification_deliveries(cursor.as_deref())
                .await
                .user_error()
        }
    });
    let retrying = RwSignal::new(false);
    let retry = Retry {
        allowed: can(Permission::Global(GlobalPermission::SystemOperate)),
        busy: retrying,
        run: Callback::new(move |id: String| {
            if retrying.get_untracked() {
                return;
            }
            retrying.set(true);
            spawn_local(async move {
                match Client::browser().retry_notification(&id).await {
                    Ok(()) => {
                        error.set(None);
                        refresh.request();
                    }
                    Err(e) => error.set(Some(e.user_message())),
                }
                retrying.set(false);
            });
        }),
    };
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"Deliveries"</h3>
                </div>
            </header>
            <div class="stack-sm">
                {move || error.get().map(|e| notice(Tone::Bad, e))}
                {loaded(
                    data,
                    "Loading deliveries…",
                    move |page| {
                        view! {
                            <div class="table-wrap">
                                {if page.items.is_empty() {
                                    empty("No notification deliveries.")
                                } else {
                                    view! {
                                        <table class="table">
                                            <thead>
                                                <tr>
                                                    <th>"Destination"</th>
                                                    <th>"Category"</th>
                                                    <th>"About"</th>
                                                    <th>"State"</th>
                                                    <th class="num">"Attempts"</th>
                                                    <th>"Updated"</th>
                                                    <th>"Last error"</th>
                                                    <th></th>
                                                </tr>
                                            </thead>
                                            <tbody>
                                                {page
                                                    .items
                                                    .into_iter()
                                                    .map(|delivery| delivery_row(delivery, retry))
                                                    .collect_view()}
                                            </tbody>
                                        </table>
                                    }
                                        .into_any()
                                }}
                            </div>
                            {older_page(cursor, page.next_cursor, "Older deliveries", None)}
                        }
                            .into_any()
                    },
                )}
            </div>
        </section>
    }
}

/// Table row for one delivery, with a button that runs `retry` on a failed
/// one when allowed.
fn delivery_row(delivery: NotificationDelivery, retry: Retry) -> AnyView {
    let id = delivery.id.clone();
    let tone = match delivery.state {
        DeliveryState::Delivered => Tone::Ok,
        DeliveryState::Failed => Tone::Bad,
        DeliveryState::Pending => Tone::Pending,
        DeliveryState::Cancelled => Tone::Neutral,
    };
    view! {
        <tr>
            <td title={delivery.id.clone()}>{delivery.destination}</td>
            <td>{delivery.category.label()}</td>
            <td>{delivery.subject.describe().unwrap_or_else(|| "daemon".into())}</td>
            <td>{badge(tone, delivery.state.to_string())}</td>
            <td class="num">{delivery.attempts}</td>
            <td class="muted">{when(delivery.updated_at_ms)}</td>
            <td class="muted">{delivery.last_error.unwrap_or_default()}</td>
            <td class="actions">
                {(delivery.state == DeliveryState::Failed && retry.allowed.get())
                    .then(|| {
                        view! {
                            <button
                                type="button"
                                class="btn btn-sm"
                                disabled={move || retry.busy.get()}
                                on:click={move |_| retry.run.run(id.clone())}
                            >
                                "Retry"
                            </button>
                        }
                    })}
            </td>
        </tr>
    }
    .into_any()
}
