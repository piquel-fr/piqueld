//! Shared log viewer, stream filter, and persisted display preferences.
use crate::log_output::LogLine;
use leptos::prelude::*;
use piqueld_client::LogStream;

/// Log display toggles shared by every viewer and persisted in `localStorage`.
/// Build and service-scoped viewers keep separate timestamp/service defaults.
#[derive(Clone, Copy)]
pub(super) struct LogPreferences {
    timestamps: RwSignal<bool>,
    build_timestamps: RwSignal<bool>,
    services: RwSignal<bool>,
    scoped_services: RwSignal<bool>,
    wrap: RwSignal<bool>,
}
impl LogPreferences {
    /// Loads the stored preferences and provides them as context for the whole app.
    pub(super) fn provide() {
        provide_context(Self {
            timestamps: Self::preference("timestamps", true),
            build_timestamps: Self::preference("build-timestamps", false),
            services: Self::preference("services", true),
            scoped_services: Self::preference("scoped-services", false),
            wrap: Self::preference("wrap", false),
        });
    }
    /// Signal initialised from `piqueld.logs.<name>` in `localStorage` (or
    /// `default`) that writes every change back.
    fn preference(name: &'static str, default: bool) -> RwSignal<bool> {
        let key = format!("piqueld.logs.{name}");
        let storage = window().local_storage().ok().flatten();
        let initial = storage
            .as_ref()
            .and_then(|storage| storage.get_item(&key).ok().flatten())
            .and_then(|value| value.parse().ok())
            .unwrap_or(default);
        let value = RwSignal::new(initial);
        Effect::new(move |_| {
            let current = value.get();
            if let Some(storage) = &storage {
                // Browser privacy settings may disable preference persistence.
                let _ = storage.set_item(&key, &current.to_string());
            }
        });
        value
    }
}

/// Select that filters logs to `stdout`, `stderr`, or both (`None`).
#[component]
pub(super) fn StreamFilter(stream: RwSignal<Option<LogStream>>) -> impl IntoView {
    view! {
        <label class="field">
            <span>"Stream"</span>
            <select
                prop:value={move || stream.get().map_or("", LogStream::as_str)}
                on:change={move |event| {
                    stream
                        .set(
                            match event_target_value(&event).as_str() {
                                "stdout" => Some(LogStream::Stdout),
                                "stderr" => Some(LogStream::Stderr),
                                _ => None,
                            },
                        );
                }}
            >
                <option value="">"Both"</option>
                <option value="stdout">"stdout"</option>
                <option value="stderr">"stderr"</option>
            </select>
        </label>
    }
}

impl LogLine {
    /// Returns the short local `HH:MM:SS` time and the full timestamp for its tooltip.
    /// Accepts Unix milliseconds or a date string; `"0"` or unparsable values show `—`.
    ///
    /// ```text
    /// "1700000000000"        -> ("22:13:20", "2023-11-14T22:13:20.000Z")  (UTC browser)
    /// "2024-01-01T00:00:00Z" -> ("00:00:00", "2024-01-01T00:00:00Z")      (UTC browser)
    /// ```
    fn display_time(&self) -> (String, String) {
        let date = self.timestamp.parse::<f64>().map_or_else(
            |_| js_sys::Date::new(&self.timestamp.clone().into()),
            |milliseconds| js_sys::Date::new(&milliseconds.into()),
        );
        if date.get_time().is_nan() || self.timestamp == "0" {
            return ("—".into(), "Timestamp unavailable".into());
        }
        let full = if self.timestamp.parse::<i64>().is_ok() {
            String::from(date.to_iso_string())
        } else {
            self.timestamp.clone()
        };
        (
            format!(
                "{:02}:{:02}:{:02}",
                date.get_hours(),
                date.get_minutes(),
                date.get_seconds()
            ),
            full,
        )
    }
}

/// Which preference set a `LogViewer` uses.
#[derive(Clone, Copy, Default)]
pub(super) enum LogKind {
    #[default]
    Application,
    Service,
    Build,
}

fn toggle(label: &'static str, value: RwSignal<bool>) -> AnyView {
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

/// A stable scroll container; refreshing rows never remounts the viewer.
#[component]
pub(super) fn LogViewer(
    #[prop(into)] lines: Signal<Vec<LogLine>>,
    #[prop(into)] label: String,
    #[prop(into)] empty: String,
    #[prop(default = LogKind::Application)] kind: LogKind,
    #[prop(default = Signal::derive(|| 0), into)] prepend_revision: Signal<u64>,
) -> impl IntoView {
    let preferences = use_context::<LogPreferences>().expect("log preferences");
    let timestamps = match kind {
        LogKind::Build => preferences.build_timestamps,
        LogKind::Application | LogKind::Service => preferences.timestamps,
    };
    let service = match kind {
        LogKind::Application => Some(preferences.services),
        LogKind::Service => Some(preferences.scoped_services),
        LogKind::Build => None,
    };
    let show_service = Signal::derive(move || service.is_some_and(|value| value.get()));
    let container = NodeRef::<leptos::html::Div>::new();
    let follow = RwSignal::new(true);
    let position = RwSignal::new(0);
    let previous_prepend = StoredValue::new(0);
    let height = StoredValue::new(0);
    let updating = StoredValue::new(false);
    Effect::new(move |_| {
        lines.with(|_| ());
        // A prepended chunk can complete an existing partial line, so rendered
        // line equality cannot reliably identify an older page insertion.
        let revision = prepend_revision.get();
        let prepend = revision != previous_prepend.get_value();
        previous_prepend.set_value(revision);
        let _ = (preferences.wrap.get(), timestamps.get(), show_service.get());
        let following = follow.get_untracked();
        let old_position = position.get_untracked();
        let old_height = height.get_value();
        updating.set_value(true);
        request_animation_frame(move || {
            if let Some(node) = container.get() {
                let target = if prepend {
                    old_position + node.scroll_height() - old_height
                } else if following {
                    node.scroll_height()
                } else {
                    old_position
                };
                node.set_scroll_top(target);
                height.set_value(node.scroll_height());
                position.set(node.scroll_top());
                follow.set(node.scroll_height() - node.client_height() - node.scroll_top() <= 24);
            }
            updating.set_value(false);
        });
    });
    view! {
        <div class="log-options" role="group" aria-label="Log display">
            {toggle("Timestamps", timestamps)}
            {service.map(|service| toggle("Service names", service))}
            {toggle("Wrap lines", preferences.wrap)}
        </div>
        <div
            class="terminal"
            class:log-wrap={move || preferences.wrap.get()}
            node_ref={container}
            tabindex="0"
            role="region"
            aria-label={label}
            on:scroll={move |_| {
                if let Some(node) = container.get() {
                    if updating.get_value() {
                        return;
                    }
                    position.set(node.scroll_top());
                    follow
                        .set(node.scroll_height() - node.client_height() - node.scroll_top() <= 24);
                }
            }}
        >
            <div class="log-rows">
                {move || {
                    if lines.with(Vec::is_empty) {
                        view! { <p class="log-empty">{empty.clone()}</p> }.into_any()
                    } else {
                        lines
                            .get()
                            .into_iter()
                            .map(|line| {
                                let severity = line.severity();
                                let (time, full) = line.display_time();
                                view! {
                                    <div class="log-line" data-severity={severity}>
                                        <span
                                            class="log-metadata"
                                            hidden={move || !timestamps.get() && !show_service.get()}
                                        >
                                            <time
                                                class="log-time"
                                                title={full}
                                                hidden={move || !timestamps.get()}
                                            >
                                                {time}
                                            </time>
                                            {service
                                                .map(|_| {
                                                    view! {
                                                        <span
                                                            class="log-service"
                                                            hidden={move || !show_service.get()}
                                                        >
                                                            {line.service}
                                                        </span>
                                                    }
                                                })}
                                        </span>
                                        <span class="log-message">{line.message}</span>
                                    </div>
                                }
                            })
                            .collect_view()
                            .into_any()
                    }
                }}
            </div>
        </div>
    }
}
