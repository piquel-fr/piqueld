//! Unsaved edit guards for browser navigation.
use leptos::wasm_bindgen::JsCast;
use leptos::wasm_bindgen::closure::Closure;
use leptos::{
    RwSignal, SignalGetUntracked, SignalSet, create_rw_signal, document, ev, on_cleanup,
    provide_context, use_context, window, window_event_listener,
};
use std::collections::BTreeSet;

/// The editor location to restore if the user cancels a back/forward navigation.
#[derive(Clone)]
struct GuardedLocation {
    /// The editor's dirty form groups; navigation is only guarded while non-empty.
    dirty: RwSignal<BTreeSet<String>>,
    url: String,
    state: leptos::wasm_bindgen::JsValue,
}

/// App-wide `popstate` guard; holds the active editor's location while one is mounted.
#[derive(Clone, Copy)]
pub(in crate::browser) struct HistoryGuard(RwSignal<Option<GuardedLocation>>);
impl HistoryGuard {
    /// Installs the guard as context; must run before the router mounts because
    /// window-targeted history events must be intercepted before the router's listener.
    /// A cancelled `popstate` re-pushes the saved editor URL and history state.
    pub(in crate::browser) fn install() {
        let guard = Self(create_rw_signal(None::<GuardedLocation>));
        provide_context(guard);
        let listener = window_event_listener(ev::popstate, move |event| {
            let Some(location) = guard.0.get_untracked() else {
                return;
            };
            if !location.dirty.get_untracked().is_empty()
                && !window()
                    .confirm_with_message("Leave this application and discard unsaved form edits?")
                    .unwrap_or(false)
            {
                event.stop_immediate_propagation();
                if let Ok(history) = window().history() {
                    let _ = history.push_state_with_url(&location.state, "", Some(&location.url));
                }
            }
        });
        on_cleanup(move || listener.remove());
    }
}

/// Asks for confirmation before leaving an editor with unsaved edits, covering
/// page unload (`beforeunload`), back/forward (via `HistoryGuard`), and link clicks
/// (a capturing document `click` listener; `download` links are exempt).
/// All listeners are removed when the editor unmounts.
pub(super) fn guard_navigation(dirty: RwSignal<BTreeSet<String>>) {
    let listener = window_event_listener(ev::beforeunload, move |event| {
        if !dirty.get_untracked().is_empty() {
            event.prevent_default();
            event.set_return_value("");
        }
    });
    on_cleanup(move || listener.remove());
    let guard = use_context::<HistoryGuard>().expect("history guard installed before router");
    guard.0.set(Some(GuardedLocation {
        dirty,
        url: window().location().href().unwrap_or_default(),
        state: window()
            .history()
            .and_then(|h| h.state())
            .unwrap_or_default(),
    }));
    on_cleanup(move || guard.0.set(None));
    let callback = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
        if dirty.get_untracked().is_empty() {
            return;
        }
        let anchor = event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
            .and_then(|element| element.closest("a[href]").ok().flatten())
            .filter(|anchor| !anchor.has_attribute("download"));
        if anchor.is_some()&&!window().confirm_with_message("Leave this application and discard unsaved form edits? Saved configuration is already stored.").unwrap_or(false){event.prevent_default();event.stop_propagation();}
    });
    let document = document();
    if document
        .add_event_listener_with_callback_and_bool("click", callback.as_ref().unchecked_ref(), true)
        .is_ok()
    {
        on_cleanup(move || {
            let _ = document.remove_event_listener_with_callback_and_bool(
                "click",
                callback.as_ref().unchecked_ref(),
                true,
            );
        });
    }
}
