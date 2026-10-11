//! Exec connections over the browser's `WebSocket`.
//!
//! The browser sends the session cookie and the page's `Origin` with the
//! handshake. It hides the response to a refused handshake, so refusals only
//! surface as a connection closed before it opened.

use super::exchange;
use crate::{Client, ClientError};
use futures_channel::mpsc::{UnboundedReceiver, unbounded};
use futures_util::StreamExt;
use std::{
    future::{Ready, ready},
    rc::Rc,
};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use web_sys::{BinaryType, CloseEvent, Event, MessageEvent, WebSocket};

/// What the socket's event handlers report, in order.
enum SocketEvent {
    Open,
    Message(Vec<u8>),
    Closed(CloseEvent),
}

/// The socket and the handlers feeding its events to the [`Reader`]. Dropping
/// it, once both halves are gone, detaches the handlers and closes the socket.
struct Connection {
    socket: WebSocket,
    _opened: Closure<dyn FnMut(Event)>,
    _message: Closure<dyn FnMut(MessageEvent)>,
    _closed: Closure<dyn FnMut(CloseEvent)>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.socket.set_onopen(None);
        self.socket.set_onmessage(None);
        self.socket.set_onclose(None);
        let _ = self.socket.close();
    }
}

/// Receiving half: binary message payloads.
pub(super) struct Reader {
    _connection: Rc<Connection>,
    events: UnboundedReceiver<SocketEvent>,
}

/// Sending half: binary messages.
pub(super) struct Writer(Rc<Connection>);

/// Opens a WebSocket to `url`, waits for the handshake, and sends `request`
/// as the first message. Unlike native requests, the handshake has no client
/// timeout; the browser bounds it.
pub(super) async fn open(
    _client: &Client,
    url: String,
    request: String,
) -> Result<(Reader, Writer), ClientError> {
    // The browser client's base URL is the page's HTTP(S) origin.
    let url = url
        .strip_prefix("http")
        .map(|rest| format!("ws{rest}"))
        .ok_or_else(|| ClientError::invalid_request("dashboard origin is not HTTP"))?;
    let socket = WebSocket::new(&url).map_err(|error| ClientError::invalid_request(js_error(&error)))?;
    socket.set_binary_type(BinaryType::Arraybuffer);
    let (sender, mut events) = unbounded();
    let opened = Closure::<dyn FnMut(Event)>::new({
        let sender = sender.clone();
        move |_| {
            let _ = sender.unbounded_send(SocketEvent::Open);
        }
    });
    let message = Closure::<dyn FnMut(MessageEvent)>::new({
        let sender = sender.clone();
        // Text messages are not part of the daemon's side of the protocol.
        move |event: MessageEvent| {
            if let Ok(data) = event.data().dyn_into::<js_sys::ArrayBuffer>() {
                let data = js_sys::Uint8Array::new(&data).to_vec();
                let _ = sender.unbounded_send(SocketEvent::Message(data));
            }
        }
    });
    let closed = Closure::<dyn FnMut(CloseEvent)>::new(move |event| {
        let _ = sender.unbounded_send(SocketEvent::Closed(event));
    });
    socket.set_onopen(Some(opened.as_ref().unchecked_ref()));
    socket.set_onmessage(Some(message.as_ref().unchecked_ref()));
    socket.set_onclose(Some(closed.as_ref().unchecked_ref()));
    let connection = Rc::new(Connection {
        socket,
        _opened: opened,
        _message: message,
        _closed: closed,
    });
    match events.next().await {
        Some(SocketEvent::Open) => {}
        Some(SocketEvent::Closed(event)) => {
            return Err(exchange(&format!(
                "the daemon refused the connection (close code {}); \
                 the session may have expired",
                event.code()
            )));
        }
        Some(SocketEvent::Message(_)) | None => {
            return Err(exchange(&"the connection ended before it opened"));
        }
    }
    connection
        .socket
        .send_with_str(&request)
        .map_err(|error| exchange(&js_error(&error)))?;
    Ok((
        Reader {
            _connection: Rc::clone(&connection),
            events,
        },
        Writer(connection),
    ))
}

impl Reader {
    /// Returns the next binary payload, or `None` once the connection closed
    /// cleanly. An abnormal closure is an error.
    pub(super) async fn next(&mut self) -> Result<Option<Vec<u8>>, ClientError> {
        loop {
            match self.events.next().await {
                Some(SocketEvent::Message(data)) => return Ok(Some(data)),
                Some(SocketEvent::Open) => {}
                Some(SocketEvent::Closed(event)) if !event.was_clean() => {
                    return Err(exchange(&format!(
                        "connection lost (close code {})",
                        event.code()
                    )));
                }
                Some(SocketEvent::Closed(_)) | None => return Ok(None),
            }
        }
    }
}

impl Writer {
    /// Ready at once: the browser buffers outgoing messages itself.
    pub(super) fn send(&mut self, data: &[u8]) -> Ready<Result<(), ClientError>> {
        // Browsers silently discard messages sent on a closing socket.
        ready(if self.0.socket.ready_state() == WebSocket::OPEN {
            self.0
                .socket
                .send_with_u8_array(data)
                .map_err(|error| exchange(&js_error(&error)))
        } else {
            Err(exchange(&"the connection is closed"))
        })
    }
}

/// Describes a JavaScript exception.
fn js_error(error: &JsValue) -> String {
    error.dyn_ref::<js_sys::Error>().map_or_else(
        || format!("{error:?}"),
        |error| String::from(error.message()),
    )
}
