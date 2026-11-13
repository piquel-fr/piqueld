//! Server-sent event streams over the browser's `EventSource`.
use leptos::wasm_bindgen::{JsCast, closure::Closure};
use std::rc::Rc;
use web_sys::{Event, EventSource, MessageEvent};

/// A listener for one kind of message.
type Listener = Closure<dyn FnMut(MessageEvent)>;

/// An open server-sent event stream, authenticated by the session cookie
/// like every dashboard request. The browser reconnects it after a dropped
/// connection on its own; dropping it closes the stream.
pub(super) struct EventStream {
    source: EventSource,
    listeners: Vec<(&'static str, Listener)>,
    _error: Closure<dyn FnMut(Event)>,
}

impl EventStream {
    /// Opens `url`, calling `message` with the kind and data of each message
    /// of one of `kinds`, and `failed` once the browser gave up on the
    /// stream: it was refused, or ended in a way the browser does not retry.
    /// `None` when the browser refused to open it.
    pub(super) fn open(
        url: &str,
        kinds: &[&'static str],
        message: impl Fn(&str, &str) + 'static,
        failed: impl Fn() + 'static,
    ) -> Option<Self> {
        let source = EventSource::new(url).ok()?;
        let message = Rc::new(message);
        let listeners = kinds
            .iter()
            .map(|&kind| {
                let message = Rc::clone(&message);
                let listener = Listener::new(move |event: MessageEvent| {
                    message(kind, &event.data().as_string().unwrap_or_default());
                });
                // Only fails for a listener that is not a function.
                let _ = source
                    .add_event_listener_with_callback(kind, listener.as_ref().unchecked_ref());
                (kind, listener)
            })
            .collect();
        let error = Closure::<dyn FnMut(Event)>::new({
            let source = source.clone();
            move |_| {
                // Errors also report each reconnection the browser tries.
                if source.ready_state() == EventSource::CLOSED {
                    failed();
                }
            }
        });
        source.set_onerror(Some(error.as_ref().unchecked_ref()));
        Some(Self {
            source,
            listeners,
            _error: error,
        })
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.source.set_onerror(None);
        for (kind, listener) in &self.listeners {
            let _ = self
                .source
                .remove_event_listener_with_callback(kind, listener.as_ref().unchecked_ref());
        }
        self.source.close();
    }
}
