//! Shared presentational primitives: icons, badges, notices, page chrome, dialogs, and inputs.
use super::format::{relative, timestamp};
use crate::state::ApplicationHealth;
use leptos::prelude::*;
use leptos::wasm_bindgen::JsCast;
use piqueld_client::{BuildState, OperationState, RouteName, TelemetryDestinationStatus};
use std::collections::BTreeSet;

/// Inline stroke icons, drawn on a 24-unit grid.
#[derive(Clone, Copy)]
pub(super) enum Icon {
    Home,
    Apps,
    Builds,
    Events,
    Errors,
    Daemon,
    Analytics,
    Notifications,
    Accounts,
    Settings,
    LogOut,
    Plus,
    ChevronRight,
    ArrowLeft,
    Close,
    Pencil,
    Refresh,
    Download,
    Eye,
    Rocket,
    Package,
    Key,
    Copy,
    GitBranch,
    Restore,
}

impl Icon {
    /// The SVG children drawing the icon.
    const fn markup(self) -> &'static str {
        match self {
            Self::Home => {
                r#"<path d="m3 9 9-7 9 7v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/><path d="M9 22V12h6v10"/>"#
            }
            Self::Apps => {
                r#"<rect width="7" height="7" x="3" y="3" rx="1"/><rect width="7" height="7" x="14" y="3" rx="1"/><rect width="7" height="7" x="14" y="14" rx="1"/><rect width="7" height="7" x="3" y="14" rx="1"/>"#
            }
            Self::Builds => {
                r#"<path d="m15 12-8.5 8.5a2.12 2.12 0 1 1-3-3L12 9"/><path d="M17.64 15 22 10.64"/><path d="m20.91 11.7-1.25-1.25c-.6-.6-.93-1.4-.93-2.25v-.86L16.01 4.6a5.56 5.56 0 0 0-3.94-1.64H9l.92.82A6.18 6.18 0 0 1 12 8.4v1.56l2 2h2.47l2.26 1.91"/>"#
            }
            Self::Events => r#"<path d="M22 12h-4l-3 9L9 3l-3 9H2"/>"#,
            Self::Errors => {
                r#"<path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3Z"/><path d="M12 9v4"/><path d="M12 17h.01"/>"#
            }
            Self::Daemon => {
                r#"<rect width="20" height="8" x="2" y="2" rx="2"/><rect width="20" height="8" x="2" y="14" rx="2"/><path d="M6 6h.01"/><path d="M6 18h.01"/>"#
            }
            Self::Analytics => {
                r#"<path d="M3 3v18h18"/><path d="M18 17V9"/><path d="M13 17V5"/><path d="M8 17v-3"/>"#
            }
            Self::Notifications => {
                r#"<path d="M6 8a6 6 0 0 1 12 0c0 7 3 9 3 9H3s3-2 3-9"/><path d="M10.3 21a1.94 1.94 0 0 0 3.4 0"/>"#
            }
            Self::Accounts => {
                r#"<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/>"#
            }
            Self::Settings => {
                r#"<path d="M21 4h-7"/><path d="M10 4H3"/><path d="M21 12h-9"/><path d="M8 12H3"/><path d="M21 20h-5"/><path d="M12 20H3"/><path d="M14 2v4"/><path d="M8 10v4"/><path d="M16 18v4"/>"#
            }
            Self::LogOut => {
                r#"<path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/><path d="m16 17 5-5-5-5"/><path d="M21 12H9"/>"#
            }
            Self::Plus => r#"<path d="M5 12h14"/><path d="M12 5v14"/>"#,
            Self::ChevronRight => r#"<path d="m9 18 6-6-6-6"/>"#,
            Self::ArrowLeft => r#"<path d="m12 19-7-7 7-7"/><path d="M19 12H5"/>"#,
            Self::Close => r#"<path d="M18 6 6 18"/><path d="m6 6 12 12"/>"#,
            Self::Pencil => {
                r#"<path d="M17 3a2.85 2.83 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5Z"/><path d="m15 5 4 4"/>"#
            }
            Self::Refresh => {
                r#"<path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"/><path d="M8 16H3v5"/>"#
            }
            Self::Download => {
                r#"<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="m7 10 5 5 5-5"/><path d="M12 15V3"/>"#
            }
            Self::Eye => {
                r#"<path d="M2 12s3-7 10-7 10 7 10 7-3 7-10 7-10-7-10-7Z"/><circle cx="12" cy="12" r="3"/>"#
            }
            Self::Rocket => {
                r#"<path d="M4.5 16.5c-1.5 1.26-2 5-2 5s3.74-.5 5-2c.71-.84.7-2.13-.09-2.91a2.18 2.18 0 0 0-2.91-.09z"/><path d="m12 15-3-3a22 22 0 0 1 2-3.95A12.88 12.88 0 0 1 22 2c0 2.72-.78 7.5-6 11a22.35 22.35 0 0 1-4 2z"/><path d="M9 12H4s.55-3.03 2-4c1.62-1.08 5 0 5 0"/><path d="M12 15v5s3.03-.55 4-2c1.08-1.62 0-5 0-5"/>"#
            }
            Self::Package => {
                r#"<path d="m7.5 4.27 9 5.15"/><path d="M21 8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16Z"/><path d="m3.3 7 8.7 5 8.7-5"/><path d="M12 22V12"/>"#
            }
            Self::Key => {
                r#"<circle cx="7.5" cy="15.5" r="5.5"/><path d="m21 2-9.6 9.6"/><path d="m15.5 7.5 3 3L22 7l-3-3"/>"#
            }
            Self::Copy => {
                r#"<rect width="14" height="14" x="8" y="8" rx="2" ry="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/>"#
            }
            Self::GitBranch => {
                r#"<path d="M6 3v12"/><circle cx="18" cy="6" r="3"/><circle cx="6" cy="18" r="3"/><path d="M18 9a9 9 0 0 1-9 9"/>"#
            }
            Self::Restore => {
                r#"<path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/>"#
            }
        }
    }
}

/// Inline SVG of `icon`, sized by CSS and hidden from assistive technology.
pub(super) fn icon(icon: Icon) -> AnyView {
    view! {
        <svg
            class="icon"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            stroke-linecap="round"
            stroke-linejoin="round"
            aria-hidden="true"
            inner_html={icon.markup()}
        ></svg>
    }
    .into_any()
}

/// A small button copying `text` to the clipboard, labelled `label` for
/// assistive technology, that says "Copied" once it did. Browsers expose no
/// clipboard on plain HTTP origins, so there it renders nothing and the text
/// is only selectable.
pub(super) fn copy_button(label: &'static str, text: String) -> AnyView {
    if window().navigator().clipboard().is_undefined() {
        return ().into_any();
    }
    let copied = RwSignal::new(false);
    let copy = move |_| {
        let text = text.clone();
        leptos::task::spawn_local(async move {
            let written = window().navigator().clipboard().write_text(&text);
            if wasm_bindgen_futures::JsFuture::from(written).await.is_ok() {
                copied.set(true);
            }
        });
    };
    view! {
        <button type="button" class="btn btn-sm" aria-label={label} title={label} on:click={copy}>
            {icon(Icon::Copy)}
            {move || if copied.get() { "Copied" } else { "Copy" }}
        </button>
    }
    .into_any()
}

/// A one-time secret to copy, kept outside live regions so assistive
/// technology announces only the notice about it, never the secret itself.
/// **Done** clears `secret`, removing it from the page.
pub(super) fn one_time_secret(
    label: &'static str,
    secret: RwSignal<Option<String>>,
) -> impl IntoView {
    move || {
        secret.get().map(|value| {
            view! {
                <div class="stack-sm">
                    <pre class="secret-box">{value.clone()}</pre>
                    <div class="btn-group">
                        {copy_button(label, value)}
                        <button
                            type="button"
                            class="btn btn-sm btn-ghost"
                            on:click={move |_| secret.set(None)}
                        >
                            "Done"
                        </button>
                    </div>
                </div>
            }
        })
    }
}

/// Semantic colour used by badges, notices, and status cards.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Tone {
    Ok,
    Warn,
    Bad,
    Pending,
    Info,
    Neutral,
}

impl Tone {
    /// The `data-tone` attribute value CSS colours by.
    pub(super) const fn attr(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Bad => "bad",
            Self::Pending => "pending",
            Self::Info => "info",
            Self::Neutral => "neutral",
        }
    }
}

impl ApplicationHealth {
    /// Tone of the health's badge.
    pub(super) const fn tone(self) -> Tone {
        match self {
            Self::Converged => Tone::Ok,
            Self::Degraded => Tone::Warn,
            Self::Failed => Tone::Bad,
            Self::Pending => Tone::Pending,
            Self::NotDeployed | Self::Stopped => Tone::Neutral,
        }
    }
}

/// Small `tone`d pill with a status `label`.
pub(super) fn badge(tone: Tone, label: impl Into<String>) -> AnyView {
    view! {
        <span class="badge" data-tone={tone.attr()}>
            {label.into()}
        </span>
    }
    .into_any()
}

/// The tone of how export to a telemetry collector is going.
pub(super) const fn telemetry_tone(destination: &TelemetryDestinationStatus) -> Tone {
    match (
        destination.failing,
        destination.connected,
        destination.last_success_ms,
    ) {
        (true, _, _) => Tone::Bad,
        (false, true, _) => Tone::Ok,
        (false, false, Some(_)) => Tone::Warn,
        (false, false, None) => Tone::Pending,
    }
}

/// Badge of how export to a telemetry collector is going.
pub(super) fn telemetry_badge(destination: &TelemetryDestinationStatus) -> AnyView {
    badge(telemetry_tone(destination), destination.state())
}

/// Badge of an application's or environment's health.
pub(super) fn health_badge(health: ApplicationHealth) -> AnyView {
    badge(health.tone(), health.label())
}

/// Badge of an operation's state; running ones pulse.
pub(super) fn operation_badge(state: OperationState) -> AnyView {
    let tone = match state {
        OperationState::Requested => Tone::Pending,
        OperationState::Running => Tone::Info,
        OperationState::Succeeded => Tone::Ok,
        OperationState::Failed => Tone::Bad,
        OperationState::Cancelled | OperationState::Superseded => Tone::Neutral,
    };
    badge(tone, state.as_str())
}

/// Badge of a build's state; running ones pulse.
pub(super) fn build_badge(state: BuildState) -> AnyView {
    let (tone, label) = match state {
        BuildState::Running => (Tone::Info, "running"),
        BuildState::Succeeded => (Tone::Ok, "succeeded"),
        BuildState::Failed => (Tone::Bad, "failed"),
        BuildState::Interrupted => (Tone::Neutral, "interrupted"),
    };
    badge(tone, label)
}

/// A route's name, or a dash when it has none.
pub(super) fn route_name(name: Option<RouteName>) -> AnyView {
    code_or(name.map(|name| name.to_string()), "—")
}

/// `value` as code, or the muted `missing` text without one.
pub(super) fn code_or(value: Option<String>, missing: &'static str) -> AnyView {
    value.map_or_else(
        || view! { <span class="muted">{missing}</span> }.into_any(),
        |value| view! { <code>{value}</code> }.into_any(),
    )
}

/// Inline message box. Failures are announced assertively; everything else politely.
pub(super) fn notice(tone: Tone, content: impl IntoView) -> AnyView {
    let role = if tone == Tone::Bad { "alert" } else { "status" };
    view! {
        <div class="notice" data-tone={tone.attr()} role={role}>
            {content}
        </div>
    }
    .into_any()
}

/// Stat tile with a label, a large value, and an optional footnote.
pub(super) fn metric(
    label: &'static str,
    value: impl IntoView,
    detail: Option<impl IntoView>,
) -> AnyView {
    view! {
        <div class="metric">
            <span>{label}</span>
            <strong>{value}</strong>
            {detail.map(|detail| view! { <small>{detail}</small> })}
        </div>
    }
    .into_any()
}

/// Ghost "Remove" button for one row of an editable list.
pub(super) fn remove_button(remove: impl Fn() + 'static) -> AnyView {
    view! {
        <button type="button" class="btn btn-ghost" on:click={move |_| remove()}>
            "Remove"
        </button>
    }
    .into_any()
}

/// Labelled checkbox bound to `value`.
pub(super) fn checkbox(label: impl IntoView + 'static, value: RwSignal<bool>) -> AnyView {
    view! {
        <label class="checkbox">
            <input
                type="checkbox"
                prop:checked={move || value.get()}
                on:change={move |e| value.set(event_target_checked(&e))}
            />
            {label}
        </label>
    }
    .into_any()
}

/// Checkbox input showing whether `set` holds `item`, adding or removing it
/// when toggled; callers label it.
pub(super) fn member_checkbox<T: Ord + Clone + Send + Sync + 'static>(
    set: RwSignal<BTreeSet<T>>,
    item: T,
) -> AnyView {
    let checked = item.clone();
    view! {
        <input
            type="checkbox"
            prop:checked={move || set.with(|set| set.contains(&checked))}
            on:change={move |event| {
                let item = item.clone();
                if event_target_checked(&event) {
                    set.update(|set| {
                        set.insert(item);
                    });
                } else {
                    set.update(|set| {
                        set.remove(&item);
                    });
                }
            }}
        />
    }
    .into_any()
}

/// Small `label`led button adding a row to an editable list.
pub(super) fn add_button(label: &'static str, add: impl Fn() + 'static) -> AnyView {
    limited_add_button(label, || false, add)
}

/// `add_button` for a list of bounded length, disabled once `full`.
pub(super) fn limited_add_button(
    label: &'static str,
    full: impl Fn() -> bool + Send + Sync + 'static,
    add: impl Fn() + 'static,
) -> AnyView {
    view! {
        <button type="button" class="btn btn-sm" disabled={full} on:click={move |_| add()}>
            {icon(Icon::Plus)}
            {label}
        </button>
    }
    .into_any()
}

/// "Load older …" button of a paginated list, shown while `cursor` names a
/// next page and disabled while `loading`.
pub(super) fn load_older(
    label: &'static str,
    cursor: RwSignal<Option<String>>,
    loading: RwSignal<bool>,
    load: impl Fn() + Clone + Send + Sync + 'static,
) -> impl IntoView {
    view! {
        <Show when={move || cursor.get().is_some()}>
            <div class="btn-group">
                <button
                    type="button"
                    class="btn"
                    disabled={move || loading.get()}
                    on:click={
                        let load = load.clone();
                        move |_| load()
                    }
                >
                    {label}
                </button>
            </div>
        </Show>
    }
}

/// The toggle of an expandable record (`.expander`): a chevron, then
/// `summary`, opening and closing `opened`.
pub(super) fn expander_summary(
    opened: RwSignal<bool>,
    summary: impl IntoView + 'static,
) -> AnyView {
    view! {
        <button
            type="button"
            class="expander-summary"
            aria-expanded={move || opened.get().to_string()}
            on:click={move |_| opened.update(|value| *value = !*value)}
        >
            <span class="chevron" aria-hidden="true">
                {icon(Icon::ChevronRight)}
            </span>
            {summary}
        </button>
    }
    .into_any()
}

/// Placeholder for a list or section without content.
pub(super) fn empty(message: impl Into<String>) -> AnyView {
    view! { <p class="empty">{message.into()}</p> }.into_any()
}

/// A loaded resource: `loading` until it loads, then its failure or `show`
/// of its value.
pub(super) fn loaded<T: Clone + Send + Sync + 'static>(
    data: LocalResource<Result<T, String>>,
    loading: &'static str,
    show: impl Fn(T) -> AnyView + Send + Sync + 'static,
) -> impl Fn() -> AnyView + Send + Sync + 'static {
    move || match data.get() {
        None => empty(loading),
        Some(Err(error)) => notice(Tone::Bad, error),
        Some(Ok(value)) => show(value),
    }
}

/// Definition list of `(term, description)` pairs.
pub(super) fn key_values<K: IntoView + 'static, V: IntoView + 'static>(
    pairs: impl IntoIterator<Item = (K, V)>,
) -> AnyView {
    view! {
        <dl class="kv">
            {pairs
                .into_iter()
                .map(|(key, value)| {
                    view! {
                        <dt>{key}</dt>
                        <dd>{value}</dd>
                    }
                })
                .collect_view()}
        </dl>
    }
    .into_any()
}

/// Relative age with the full local timestamp on hover.
pub(super) fn when(milliseconds: i64) -> AnyView {
    view! { <time title={timestamp(milliseconds)}>{relative(milliseconds)}</time> }.into_any()
}

/// Page title with an optional description and right-aligned actions.
#[component]
pub(super) fn PageHeader(
    #[prop(into)] title: String,
    #[prop(optional, into)] description: Option<String>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    view! {
        <header class="page-header">
            <div>
                <h1>{title}</h1>
                {description.map(|text| view! { <p>{text}</p> })}
            </div>
            {children.map(|children| view! { <div class="page-actions">{children()}</div> })}
        </header>
    }
}

/// Labelled text input bound to the part of `state` that `read` and `write`
/// select.
pub(super) fn text_input<T: Send + Sync + 'static>(
    label: &'static str,
    state: RwSignal<T>,
    read: impl Fn(&T) -> String + Copy + Send + Sync + 'static,
    write: impl Fn(&mut T, String) + Copy + 'static,
) -> AnyView {
    view! {
        <label class="field">
            <span>{label}</span>
            <input
                type="text"
                prop:value={move || state.with(read)}
                on:input={move |event| state.update(|v| write(v, event_target_value(&event)))}
            />
        </label>
    }
    .into_any()
}

/// Labelled text input bound to a whole string signal.
pub(super) fn text_field(label: &'static str, value: RwSignal<String>) -> AnyView {
    text_input(label, value, String::clone, |value, input| *value = input)
}

/// Native dialogs provide focus containment, Escape handling, and focus restoration.
#[component]
pub(super) fn Modal(
    title: &'static str,
    opened: RwSignal<bool>,
    #[prop(default = Signal::derive(|| false), into)] busy: Signal<bool>,
    #[prop(optional, into)] on_close: Option<Callback<()>>,
    #[prop(optional)] wide: bool,
    children: Children,
) -> impl IntoView {
    let dialog = NodeRef::<leptos::html::Dialog>::new();
    Effect::new(move |_| {
        if let Some(dialog) = dialog.get() {
            if opened.get() {
                if let Err(error) = dialog.show_modal() {
                    leptos::logging::error!("Could not open dialog: {error:?}");
                }
                if let Ok(Some(input)) = dialog.query_selector("input")
                    && let Some(input) = input.dyn_ref::<web_sys::HtmlElement>()
                {
                    let _ = input.focus();
                }
            } else {
                dialog.close();
            }
        }
    });
    let close = move || {
        if !busy.get_untracked() {
            opened.set(false);
            if let Some(on_close) = on_close {
                on_close.run(());
            }
        }
    };
    view! {
        <dialog
            class="modal"
            class:modal-wide={wide}
            node_ref={dialog}
            aria-label={title}
            on:cancel={move |event: web_sys::Event| {
                event.prevent_default();
                close();
            }}
        >
            <header>
                <h2>{title}</h2>
                <button
                    type="button"
                    class="btn btn-ghost btn-icon"
                    aria-label="Close dialog"
                    disabled={move || busy.get()}
                    on:click={move |_| close()}
                >
                    {icon(Icon::Close)}
                </button>
            </header>
            <div class="modal-body">{children()}</div>
        </dialog>
    }
}

/// Tab buttons choosing one of `options` into `selected`; the chosen tab is
/// marked current. Tabs `hidden` holds for are not offered, and the first
/// tab replaces one selected anyway, e.g. by a link.
#[component]
pub(super) fn Tabs(
    label: &'static str,
    options: &'static [&'static str],
    selected: RwSignal<&'static str>,
    #[prop(optional, into)] hidden: Option<Callback<&'static str, bool>>,
) -> impl IntoView {
    let is_hidden = move |tab| hidden.is_some_and(|hidden| hidden.run(tab));
    let reveal = move || {
        if is_hidden(selected.get()) {
            selected.set(options[0]);
        }
    };
    // At once, so panels rendered after the tabs never mount a hidden one,
    // then whenever the selected tab becomes hidden.
    untrack(reveal);
    Effect::new(move |_| reveal());
    view! {
        <nav class="tabs" aria-label={label}>
            {options
                .iter()
                .copied()
                .map(|tab| {
                    view! {
                        <button
                            type="button"
                            hidden={move || is_hidden(tab)}
                            class:active={move || selected.get() == tab}
                            aria-current={move || {
                                if selected.get() == tab { "page" } else { "false" }
                            }}
                            on:click={move |_| selected.set(tab)}
                        >
                            {tab}
                        </button>
                    }
                })
                .collect_view()}
        </nav>
    }
}
