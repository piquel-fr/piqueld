//! Interactive one-off commands in a running task of the shown environment.
use super::super::ui::{Tone, badge, notice};
use super::{EditorContext, client_error_message};
use crate::terminal::{Key, Terminal, parse_command};
use futures_channel::{mpsc, oneshot};
use futures_util::{
    StreamExt,
    future::{Either, select},
};
use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::wasm_bindgen::{JsCast, closure::Closure};
use piqueld_client::{
    Client, ClientError, ServiceName,
    access::AppPermission,
    exec::{ExecInput, ExecOutput, ExecRequest, TerminalSize},
};
use std::pin::pin;

/// Size used until the terminal element is measured.
const INITIAL_SIZE: TerminalSize = TerminalSize {
    width: 80,
    height: 24,
};

/// Where the latest session stands.
#[derive(Clone, Debug, PartialEq)]
enum Status {
    Idle,
    Connecting,
    Running,
    Exited(i64),
    Failed(String),
    /// The user disconnected; the command may still run.
    Detached,
}

impl Status {
    const fn active(&self) -> bool {
        matches!(self, Self::Connecting | Self::Running)
    }
}

/// What a session reports while it runs.
enum Update {
    /// The WebSocket opened and the command was requested.
    Running,
    /// Standard output or error, merged as a terminal shows them.
    Output(Vec<u8>),
    /// The final report; nothing follows it.
    Ended(Status),
}

/// A command session's input. Dropping it closes the WebSocket: `stop` ends
/// the session task, which drops the connection.
struct Session {
    input: mpsc::UnboundedSender<ExecInput>,
    /// The command has a terminal and reads input; otherwise nothing is sent.
    interactive: bool,
    _stop: oneshot::Sender<()>,
}

impl Session {
    /// Starts `request` in `environment`, reporting progress to `update`
    /// until the session ends or is dropped.
    fn start(environment: String, request: ExecRequest, update: impl Fn(Update) + 'static) -> Self {
        let (input, inputs) = mpsc::unbounded();
        let (stop, stopped) = oneshot::channel();
        let interactive = request.tty.is_some();
        spawn_local(async move {
            let run = Self::run(&environment, &request, inputs, &update);
            let outcome = match select(pin!(run), stopped).await {
                Either::Left((result, _)) => {
                    result.unwrap_or_else(|error| Status::Failed(client_error_message(&error)))
                }
                Either::Right(_) => Status::Detached,
            };
            update(Update::Ended(outcome));
        });
        Self {
            input,
            interactive,
            _stop: stop,
        }
    }

    /// Relays input and output until the command ends, and returns how.
    async fn run(
        environment: &str,
        request: &ExecRequest,
        mut inputs: mpsc::UnboundedReceiver<ExecInput>,
        update: &impl Fn(Update),
    ) -> Result<Status, ClientError> {
        let (mut reader, mut writer) = Client::browser().exec(environment, request).await?;
        update(Update::Running);
        let forward = async {
            while let Some(frame) = inputs.next().await {
                if writer.send(&frame).await.is_err() {
                    break;
                }
            }
            // Input ending does not end the session; output does.
            std::future::pending().await
        };
        let receive = async {
            loop {
                match reader.next().await? {
                    Some(ExecOutput::Stdout(data) | ExecOutput::Stderr(data)) => {
                        update(Update::Output(data));
                    }
                    Some(ExecOutput::Exit(code)) => return Ok(Status::Exited(code)),
                    // Start failures such as `service_not_running`, which a
                    // refused handshake could not report to a browser.
                    Some(ExecOutput::Failed { error, .. }) => {
                        return Ok(Status::Failed(error.message));
                    }
                    None => {
                        return Ok(Status::Failed(
                            "The connection closed before the command exited.".into(),
                        ));
                    }
                }
            }
        };
        match select(pin!(receive), pin!(forward)).await {
            Either::Left((result, _)) | Either::Right((result, _)) => result,
        }
    }

    /// Queues one input message for an interactive command.
    fn send(&self, frame: ExecInput) {
        if self.interactive {
            let _ = self.input.unbounded_send(frame);
        }
    }
}

/// Terminal card running a command in one service of the shown environment
/// or preview, over the same exec WebSocket as `piquelctl app exec`. With a
/// terminal, keystrokes go to the command's standard input and the screen
/// size follows the element; without one, output is shown and input is closed.
/// Disconnecting, or leaving the page, closes the WebSocket, which stops
/// streaming but may leave the command running in the task.
#[component]
pub(super) fn ExecTerminal() -> impl IntoView {
    let context = use_context::<EditorContext>().expect("application editor");
    let allowed = super::super::access::can_app(
        AppPermission::Exec,
        context
            .saved
            .with_untracked(|saved| saved.application.id().clone()),
    );
    let service = RwSignal::new(String::new());
    let command = RwSignal::new("/bin/sh".to_owned());
    let tty = RwSignal::new(true);
    let status = RwSignal::new(Status::Idle);
    let screen = RwSignal::new(String::new());
    let terminal = StoredValue::new_local(Terminal::new(INITIAL_SIZE, true));
    let session = StoredValue::new_local(None::<Session>);
    // Sessions ending after a newer one started, or after the component
    // unmounted and disposed it, must not touch its state.
    let generation = StoredValue::new(0_u64);
    let viewport = NodeRef::<html::Div>::new();
    let probe = NodeRef::<html::Span>::new();
    let keyboard = NodeRef::<html::Textarea>::new();

    let redraw = move || screen.set(terminal.with_value(Terminal::html));
    let send = move |frame: ExecInput| {
        session.with_value(|session| {
            if let Some(session) = session {
                session.send(frame);
            }
        });
    };
    // Fits the screen to the element; hidden elements measure as empty.
    let fit = move || {
        let (Some(viewport), Some(probe)) = (viewport.get_untracked(), probe.get_untracked())
        else {
            return;
        };
        let cell = probe.get_bounding_client_rect();
        let cells = |length: i32, cell: f64| {
            // Truncation is intended: only whole cells fit.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let count = (f64::from(length) / cell)
                .floor()
                .clamp(0.0, f64::from(u16::MAX)) as u16;
            count
        };
        let size = TerminalSize {
            width: cells(viewport.client_width(), cell.width()),
            height: cells(viewport.client_height(), cell.height()),
        };
        if size.width == 0 || size.height == 0 {
            return;
        }
        if terminal.try_update_value(|terminal| terminal.resize(size)) == Some(true) {
            redraw();
            send(ExecInput::Resize(size));
        }
    };
    observe_resize(viewport, fit);
    redraw();

    let start = move || {
        let request = (|| {
            let service = ServiceName::parse(service.get_untracked())
                .map_err(|_| "Choose a service.".to_owned())?;
            let command = parse_command(&command.get_untracked()).map_err(|e| e.to_string())?;
            let tty = tty.get_untracked();
            let size = terminal.with_value(Terminal::size);
            Ok(ExecRequest {
                service,
                command,
                stdin: tty,
                tty: tty.then_some(size),
            })
        })();
        let request: ExecRequest = match request {
            Ok(request) => request,
            Err(error) => return status.set(Status::Failed(error)),
        };
        terminal.update_value(|terminal| *terminal = Terminal::new(terminal.size(), request.stdin));
        redraw();
        generation.update_value(|generation| *generation += 1);
        let current = generation.get_value();
        status.set(Status::Connecting);
        let started = Session::start(context.environment_id(), request, move |update| {
            if generation.try_get_value() != Some(current) {
                return;
            }
            match update {
                Update::Running => status.set(Status::Running),
                Update::Output(data) => {
                    terminal.update_value(|terminal| terminal.write(&data));
                    redraw();
                }
                Update::Ended(outcome) => {
                    session.set_value(None);
                    status.set(outcome);
                }
            }
        });
        // Replacing a session drops, and so disconnects, the previous one.
        session.set_value(Some(started));
        if let Some(keyboard) = keyboard.get_untracked() {
            let _ = keyboard.focus();
        }
    };
    let disconnect = move || session.set_value(None);
    on_cleanup(move || {
        let _ = session.try_set_value(None);
    });

    let on_key = move |event: web_sys::KeyboardEvent| {
        if event.is_composing() || status.get_untracked() != Status::Running {
            return;
        }
        // Meta (Command) shortcuts belong to the browser.
        if event.meta_key() {
            return;
        }
        // AltGr reports Control and Alt on some systems; its character is plain text.
        let alt_graph = event.get_modifier_state("AltGraph");
        let key = event.key();
        let key = Key {
            key: &key,
            ctrl: event.ctrl_key() && !alt_graph,
            alt: event.alt_key() && !alt_graph,
            shift: event.shift_key(),
        };
        if let Some(bytes) = terminal
            .try_update_value(|terminal| terminal.key(&key))
            .flatten()
        {
            event.prevent_default();
            send(ExecInput::Stdin(bytes));
            redraw();
        }
    };
    let paste = move |text: &str| {
        if text.is_empty() || status.get_untracked() != Status::Running {
            return;
        }
        if let Some(bytes) = terminal.try_update_value(|terminal| terminal.paste(text)) {
            send(ExecInput::Stdin(bytes));
            redraw();
        }
    };
    // Composed (IME) and on-screen keyboard text arrives as input events.
    let on_text = move || {
        let Some(keyboard) = keyboard.get_untracked() else {
            return;
        };
        let text = keyboard.value();
        keyboard.set_value("");
        paste(&text);
    };
    let on_paste = move |event: web_sys::ClipboardEvent| {
        let text = event
            .clipboard_data()
            .and_then(|data| data.get_data("text").ok())
            .unwrap_or_default();
        event.prevent_default();
        paste(&text);
    };
    let on_wheel = move |event: web_sys::WheelEvent| {
        let Some(probe) = probe.get_untracked() else {
            return;
        };
        let height = probe.get_bounding_client_rect().height().max(1.0);
        // Wheel deltas scroll down; history is up. Every step moves a line or more.
        let lines = -event.delta_y() / height;
        let lines = if lines.abs() < 1.0 {
            lines.signum()
        } else {
            lines.round()
        };
        // Wheel steps are small whole numbers of lines.
        #[allow(clippy::cast_possible_truncation)]
        let lines = lines as isize;
        if terminal.try_update_value(|terminal| terminal.scroll(lines)) == Some(true) {
            event.prevent_default();
            redraw();
        }
    };
    // Clicks focus input unless they select text to copy.
    let on_click = move |_| {
        let selecting = window()
            .get_selection()
            .ok()
            .flatten()
            .is_some_and(|selection| !selection.is_collapsed());
        if let (false, Some(keyboard)) = (selecting, keyboard.get_untracked()) {
            let _ = keyboard.focus();
        }
    };

    let services = move || {
        context
            .environment_services()
            .into_iter()
            .map(|name| view! { <option value={name.clone()}>{name.clone()}</option> })
            .collect_view()
    };
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"Terminal"</h3>
                    <p>
                        "Run a command in a running task of a service, like "
                        <code>"piquelctl app exec"</code>
                        ". It shares the task's image, environment, secrets, mounts and networks. Disconnecting stops streaming, but the command may keep running in the task."
                    </p>
                </div>
            </header>
            <Show
                when={move || allowed.get()}
                fallback={|| notice(
                    Tone::Neutral,
                    "Running commands needs the apps:exec permission on this application.",
                )}
            >
                <form
                    class="toolbar exec-form"
                    on:submit={move |event| {
                        event.prevent_default();
                        start();
                    }}
                >
                    <label class="field">
                        <span>"Service"</span>
                        <select
                            prop:value={move || service.get()}
                            disabled={move || status.get().active()}
                            on:change={move |e| service.set(event_target_value(&e))}
                        >
                            <option value="" disabled>"Choose a service"</option>
                            {services}
                        </select>
                    </label>
                    <label class="field exec-command">
                        <span>"Command"</span>
                        <input
                            type="text"
                            spellcheck="false"
                            autocomplete="off"
                            prop:value={move || command.get()}
                            disabled={move || status.get().active()}
                            on:input={move |e| command.set(event_target_value(&e))}
                        />
                    </label>
                    <label class="checkbox">
                        <input
                            type="checkbox"
                            prop:checked={move || tty.get()}
                            disabled={move || status.get().active()}
                            on:change={move |e| tty.set(event_target_checked(&e))}
                        />
                        "Interactive terminal"
                    </label>
                    {move || {
                        if status.get().active() {
                            view! {
                                <button type="button" class="btn" on:click={move |_| disconnect()}>
                                    "Disconnect"
                                </button>
                            }
                                .into_any()
                        } else {
                            view! {
                                <button
                                    type="submit"
                                    class="btn btn-primary"
                                    disabled={move || service.get().is_empty()}
                                >
                                    "Run"
                                </button>
                            }
                                .into_any()
                        }
                    }}
                </form>
                <div class="exec-status" role="status">
                    {move || match status.get() {
                        Status::Idle => None,
                        Status::Connecting => Some(badge(Tone::Neutral, "Connecting…")),
                        Status::Running => Some(badge(Tone::Info, "Running")),
                        Status::Exited(0) => Some(badge(Tone::Ok, "Exited with code 0")),
                        Status::Exited(code) => {
                            Some(badge(Tone::Bad, format!("Exited with code {code}")))
                        }
                        Status::Failed(error) => Some(notice(Tone::Bad, error)),
                        Status::Detached => Some(notice(
                            Tone::Warn,
                            "Disconnected. The command may still be running in the task.",
                        )),
                    }}
                </div>
            </Show>
            <div class="terminal exec-terminal" on:click={on_click} on:wheel={on_wheel}>
                <div class="exec-viewport" node_ref={viewport}>
                    <pre class="exec-screen" inner_html={move || screen.get()}></pre>
                    <span class="exec-probe" node_ref={probe} aria-hidden="true">
                        "W"
                    </span>
                    <textarea
                        class="exec-keyboard"
                        node_ref={keyboard}
                        aria-label="Terminal input"
                        autocapitalize="off"
                        autocomplete="off"
                        spellcheck="false"
                        on:keydown={on_key}
                        on:input={move |event| {
                            if !event
                                .dyn_ref::<web_sys::InputEvent>()
                                .is_some_and(web_sys::InputEvent::is_composing)
                            {
                                on_text();
                            }
                        }}
                        on:compositionend={move |_| on_text()}
                        on:paste={on_paste}
                    ></textarea>
                </div>
            </div>
        </section>
    }
}

/// Calls `fit` whenever `element` changes size, including when it is first
/// observed, until the component unmounts.
fn observe_resize(element: NodeRef<html::Div>, fit: impl Fn() + 'static) {
    let callback = Closure::<dyn FnMut()>::new(fit);
    let Ok(observer) = web_sys::ResizeObserver::new(callback.as_ref().unchecked_ref()) else {
        return;
    };
    // Browser handles are not `Send`, so the owner keeps them until cleanup.
    let handles = StoredValue::new_local((observer, callback));
    Effect::new(move |_| {
        if let Some(element) = element.get() {
            handles.with_value(|(observer, _)| observer.observe(&element));
        }
    });
    on_cleanup(move || handles.with_value(|(observer, _)| observer.disconnect()));
}
