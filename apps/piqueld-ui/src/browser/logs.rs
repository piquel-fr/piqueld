use crate::log_output::LogLine;
use leptos::*;
use piqueld_client::LogStream;

#[derive(Clone, Copy)]
pub(super) struct LogPreferences {
    timestamps: RwSignal<bool>,
    services: RwSignal<bool>,
    scoped_services: RwSignal<bool>,
    wrap: RwSignal<bool>,
}
impl LogPreferences {
    pub(super) fn provide() {
        provide_context(Self {
            timestamps: Self::preference("timestamps", true),
            services: Self::preference("services", true),
            scoped_services: Self::preference("scoped-services", false),
            wrap: Self::preference("wrap", false),
        });
    }
    fn preference(name: &'static str, default: bool) -> RwSignal<bool> {
        let key = format!("piqueld.logs.{name}");
        let storage = window().local_storage().ok().flatten();
        let initial = storage
            .as_ref()
            .and_then(|storage| storage.get_item(&key).ok().flatten())
            .and_then(|value| value.parse().ok())
            .unwrap_or(default);
        let value = create_rw_signal(initial);
        create_effect(move |_| {
            let current = value.get();
            if let Some(storage) = &storage {
                // Browser privacy settings may disable preference persistence.
                let _ = storage.set_item(&key, &current.to_string());
            }
        });
        value
    }
}

#[component]
pub(super) fn StreamFilter(stream: RwSignal<Option<LogStream>>) -> impl IntoView {
    view! {
        <label class="log-filter">"Stream"
            <select prop:value=move ||stream.get().map_or("", LogStream::as_str)
                on:change=move |event|stream.set(match event_target_value(&event).as_str() {
                    "stdout" => Some(LogStream::Stdout), "stderr" => Some(LogStream::Stderr), _ => None,
                })>
                <option value="">"Both"</option><option value="stdout">"stdout"</option><option value="stderr">"stderr"</option>
            </select>
        </label>
    }
}

impl LogLine {
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

/// A stable scroll container; refreshing rows never remounts the viewer.
#[component]
pub(super) fn LogViewer(
    #[prop(into)] lines: Signal<Vec<LogLine>>,
    #[prop(into)] label: String,
    #[prop(into)] empty: String,
    #[prop(default = false)] scoped: bool,
) -> impl IntoView {
    let preferences = use_context::<LogPreferences>().expect("log preferences");
    let service = if scoped {
        preferences.scoped_services
    } else {
        preferences.services
    };
    let container = create_node_ref::<html::Div>();
    let follow = create_rw_signal(true);
    let position = create_rw_signal(0);
    let previous = store_value(Vec::<LogLine>::new());
    let height = store_value(0);
    let updating = store_value(false);
    create_effect(move |_| {
        let current = lines.get();
        let prepend = previous.with_value(|old| {
            !old.is_empty() && current.len() > old.len() && current.ends_with(old)
        });
        previous.set_value(current);
        let _ = (
            preferences.wrap.get(),
            preferences.timestamps.get(),
            service.get(),
        );
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
        <div class="log-display-controls" role="group" aria-label="Log display">
            <label><input type="checkbox" prop:checked=move ||preferences.timestamps.get() on:change=move |e|preferences.timestamps.set(event_target_checked(&e))/ >"Timestamp"</label>
            <label><input type="checkbox" prop:checked=move ||service.get() on:change=move |e|service.set(event_target_checked(&e))/ >"Service"</label>
            <label><input type="checkbox" prop:checked=move ||preferences.wrap.get() on:change=move |e|preferences.wrap.set(event_target_checked(&e))/ >"Wrap lines"</label>
        </div>
        <div class="application-logs" class:log-wrap=move ||preferences.wrap.get()
            node_ref=container tabindex="0" role="region" aria-label=label
            on:scroll=move |_|if let Some(node)=container.get() {
                if updating.get_value() { return; }
                position.set(node.scroll_top());
                follow.set(node.scroll_height()-node.client_height()-node.scroll_top() <= 24);
            }>
            <div class="log-rows">
                {move || if lines.with(Vec::is_empty) {view!{<p class="log-empty">{empty.clone()}</p>}.into_view()} else {
                    lines.get().into_iter().map(|line| {
                        let severity=line.severity();
                        let (time, full)=line.display_time();
                        view!{<div class="log-line" data-severity=severity>
                            <span class="log-metadata" hidden=move ||!preferences.timestamps.get() && !service.get()>
                                <time class="log-time" title=full hidden=move ||!preferences.timestamps.get()>{time}</time>
                                <span class="log-service" hidden=move ||!service.get()>{line.service}</span>
                            </span>
                            <span class="log-message">{line.message}</span>
                        </div>}
                    }).collect_view()
                }}
            </div>
        </div>
    }
}
