//! Shared form controls and accessible dialogs.
use leptos::{
    Callable, Callback, CollectView, IntoView, RwSignal, SignalGet, SignalGetUntracked, SignalSet,
    SignalUpdate, SignalWith, View, component, create_effect, event_target_value, view,
};

use leptos::wasm_bindgen::JsCast;

pub(super) fn text_input<T: Clone + 'static>(
    label: &'static str,
    state: RwSignal<T>,
    read: impl Fn(&T) -> String + Copy + 'static,
    write: impl Fn(&mut T, String) + Copy + 'static,
) -> View {
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
    .into_view()
}

/// Native dialogs provide focus containment, Escape handling, and focus restoration.
#[component]
pub(super) fn Modal(
    title: &'static str,
    opened: RwSignal<bool>,
    busy: RwSignal<bool>,
    on_close: Callback<()>,
    children: leptos::Children,
) -> impl IntoView {
    let dialog = leptos::create_node_ref::<leptos::html::Dialog>();
    create_effect(move |_| {
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
            on_close.call(());
        }
    };
    view! {
        <dialog
            class="modal"
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
                    class="icon-button"
                    aria-label="Close dialog"
                    disabled={move || busy.get()}
                    on:click={move |_| close()}
                >
                    "×"
                </button>
            </header>
            {children()}
        </dialog>
    }
}

#[component]
pub(super) fn Tabs(
    label: &'static str,
    options: &'static [&'static str],
    selected: RwSignal<&'static str>,
    #[prop(default = "tabs")] class: &'static str,
) -> impl IntoView {
    view! {
        <nav class={class} aria-label={label}>
            {options
                .iter()
                .copied()
                .map(|tab| {
                    view! {
                        <button
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
