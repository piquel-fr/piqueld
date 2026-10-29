//! Interactive one-off commands in a running task of the shown environment.
use super::super::ClientErrorExt;
use super::super::ui::{Tone, badge, notice};
use super::EditorContext;
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
                    result.unwrap_or_else(|error| Status::Failed(error.user_message()))
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

/// The command form's fields.
#[derive(Clone, Copy)]
struct Form {
    service: RwSignal<String>,
    command: RwSignal<String>,
    /// Allocate a terminal and forward input.
    tty: RwSignal<bool>,
}

impl Form {
    /// The request the form describes, for a terminal of `size`.
    fn request(self, size: TerminalSize) -> Result<ExecRequest, String> {
        let service = ServiceName::parse(self.service.get_untracked())
            .map_err(|_| "Choose a service.".to_owned())?;
        let command =
            parse_command(&self.command.get_untracked()).map_err(|error| error.to_string())?;
        let tty = self.tty.get_untracked();
        Ok(ExecRequest {
            service,
            command,
            stdin: tty,
            tty: tty.then_some(size),
        })
    }
}

/// The terminal's state and elements, shared by its event handlers. Copy, so
/// every handler holds it whole.
#[derive(Clone, Copy)]
struct Console {
    status: RwSignal<Status>,
    /// The rendered screen, as `Terminal::html` returns it.
    screen: RwSignal<String>,
    terminal: StoredValue<Terminal, LocalStorage>,
    session: StoredValue<Option<Session>, LocalStorage>,
    /// Sessions ending after a newer one started, or after the component
    /// unmounted and disposed it, must not touch its state.
    generation: StoredValue<u64>,
    viewport: NodeRef<html::Div>,
    /// One character cell, measured to fit the screen to the viewport.
    probe: NodeRef<html::Span>,
    /// Hidden input receiving keystrokes, pastes and composed text.
    keyboard: NodeRef<html::Textarea>,
}

impl Console {
    /// An idle console; its session ends when the component unmounts.
    fn new() -> Self {
        let console = Self {
            status: RwSignal::new(Status::Idle),
            screen: RwSignal::new(String::new()),
            terminal: StoredValue::new_local(Terminal::new(INITIAL_SIZE, true)),
            session: StoredValue::new_local(None),
            generation: StoredValue::new(0),
            viewport: NodeRef::new(),
            probe: NodeRef::new(),
            keyboard: NodeRef::new(),
        };
        console.redraw();
        console.observe_resize();
        on_cleanup(move || {
            let _ = console.session.try_set_value(None);
        });
        console
    }

    fn redraw(self) {
        self.screen.set(self.terminal.with_value(Terminal::html));
    }

    /// Sends one input message to the running session, if any.
    fn send(self, frame: ExecInput) {
        self.session.with_value(|session| {
            if let Some(session) = session {
                session.send(frame);
            }
        });
    }

    /// Starts `form`'s command in `environment`, replacing, and so
    /// disconnecting, any previous session.
    fn start(self, environment: String, form: Form) {
        let request = match form.request(self.terminal.with_value(Terminal::size)) {
            Ok(request) => request,
            Err(error) => return self.status.set(Status::Failed(error)),
        };
        self.terminal
            .update_value(|terminal| *terminal = Terminal::new(terminal.size(), request.stdin));
        self.redraw();
        self.generation.update_value(|generation| *generation += 1);
        let current = self.generation.get_value();
        self.status.set(Status::Connecting);
        let started = Session::start(environment, request, move |update| {
            if self.generation.try_get_value() == Some(current) {
                self.update(update);
            }
        });
        self.session.set_value(Some(started));
        self.focus();
    }

    /// Applies one report of the current session.
    fn update(self, update: Update) {
        match update {
            Update::Running => self.status.set(Status::Running),
            Update::Output(data) => {
                let replies = self
                    .terminal
                    .try_update_value(|terminal| terminal.write(&data))
                    .unwrap_or_default();
                if !replies.is_empty() {
                    self.send(ExecInput::Stdin(replies));
                }
                self.redraw();
            }
            Update::Ended(outcome) => {
                self.session.set_value(None);
                self.status.set(outcome);
            }
        }
    }

    fn disconnect(self) {
        self.session.set_value(None);
    }

    fn running(self) -> bool {
        self.status.get_untracked() == Status::Running
    }

    /// Sends the bytes of a key press the terminal encodes.
    fn key(self, event: &web_sys::KeyboardEvent) {
        // Meta (Command) shortcuts belong to the browser.
        if event.is_composing() || event.meta_key() || !self.running() {
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
        if let Some(bytes) = self
            .terminal
            .try_update_value(|terminal| terminal.key(&key))
            .flatten()
        {
            event.prevent_default();
            self.send(ExecInput::Stdin(bytes));
            self.redraw();
        }
    }

    /// Sends pasted or composed text.
    fn paste(self, text: &str) {
        if text.is_empty() || !self.running() {
            return;
        }
        if let Some(bytes) = self
            .terminal
            .try_update_value(|terminal| terminal.paste(text))
        {
            self.send(ExecInput::Stdin(bytes));
            self.redraw();
        }
    }

    /// Sends the text composed (IME) or typed on an on-screen keyboard into
    /// the hidden input, which arrives as input events, and clears it.
    fn take_text(self) {
        let Some(keyboard) = self.keyboard.get_untracked() else {
            return;
        };
        let text = keyboard.value();
        keyboard.set_value("");
        self.paste(&text);
    }

    /// Scrolls through history by the wheel's vertical movement.
    fn wheel(self, event: &web_sys::WheelEvent) {
        // Horizontal scrolling is the page's.
        let (delta, Some(probe)) = (event.delta_y(), self.probe.get_untracked()) else {
            return;
        };
        if delta == 0.0 {
            return;
        }
        let height = probe.get_bounding_client_rect().height().max(1.0);
        // Wheel deltas scroll down; history is up. Every step moves a line or more.
        let lines = -delta / height;
        let lines = if lines.abs() < 1.0 {
            lines.signum()
        } else {
            lines.round()
        };
        #[expect(
            clippy::cast_possible_truncation,
            reason = "wheel steps are a few lines"
        )]
        let lines = lines as isize;
        if self
            .terminal
            .try_update_value(|terminal| terminal.scroll(lines))
            == Some(true)
        {
            event.prevent_default();
            self.redraw();
        }
    }

    /// Focuses input, unless the user is selecting text to copy.
    fn focus(self) {
        let selecting = window()
            .get_selection()
            .ok()
            .flatten()
            .is_some_and(|selection| !selection.is_collapsed());
        if let (false, Some(keyboard)) = (selecting, self.keyboard.get_untracked()) {
            let _ = keyboard.focus();
        }
    }

    /// Fits the screen to the viewport; hidden elements measure as empty.
    fn fit(self) {
        let (Some(viewport), Some(probe)) =
            (self.viewport.get_untracked(), self.probe.get_untracked())
        else {
            return;
        };
        let cell = probe.get_bounding_client_rect();
        let cells = |length: i32, cell: f64| {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "clamped to u16, and only whole cells fit"
            )]
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
        if self
            .terminal
            .try_update_value(|terminal| terminal.resize(size))
            == Some(true)
        {
            self.redraw();
            self.send(ExecInput::Resize(size));
        }
    }

    /// Fits the screen whenever the viewport changes size, including when it
    /// is first observed, until the component unmounts.
    fn observe_resize(self) {
        let callback = Closure::<dyn FnMut()>::new(move || self.fit());
        let Ok(observer) = web_sys::ResizeObserver::new(callback.as_ref().unchecked_ref()) else {
            return;
        };
        // Browser handles are not `Send`, so the owner keeps them until cleanup.
        let handles = StoredValue::new_local((observer, callback));
        Effect::new(move |_| {
            if let Some(element) = self.viewport.get() {
                handles.with_value(|(observer, _)| observer.observe(&element));
            }
        });
        on_cleanup(move || handles.with_value(|(observer, _)| observer.disconnect()));
    }
}

/// Terminal card running a command in one service of the shown environment
/// or preview, over the same exec WebSocket as `piquelctl app exec`. With a
/// terminal, keystrokes go to the command's standard input and the screen
/// size follows the element; without one, output is shown and input is closed.
/// Disconnecting, or leaving the page, closes the WebSocket, which stops
/// streaming but may leave the command running in the task. Its tab is hidden
/// without `apps:exec` (see `EditorContext::tab_hidden`).
#[component]
#[expect(clippy::too_many_lines, reason = "flat view! markup")]
pub(super) fn ExecTerminal() -> impl IntoView {
    let context = use_context::<EditorContext>().expect("application editor");
    let form = Form {
        service: RwSignal::new(String::new()),
        command: RwSignal::new("/bin/sh".to_owned()),
        tty: RwSignal::new(true),
    };
    let console = Console::new();
    let active = move || console.status.get().active();
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
            <form
                class="toolbar exec-form"
                on:submit={move |event| {
                    event.prevent_default();
                    console.start(context.environment_id(), form);
                }}
            >
                <label class="field">
                    <span>"Service"</span>
                    <select
                        prop:value={move || form.service.get()}
                        disabled={active}
                        on:change={move |e| form.service.set(event_target_value(&e))}
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
                        prop:value={move || form.command.get()}
                        disabled={active}
                        on:input={move |e| form.command.set(event_target_value(&e))}
                    />
                </label>
                <label class="checkbox">
                    <input
                        type="checkbox"
                        prop:checked={move || form.tty.get()}
                        disabled={active}
                        on:change={move |e| form.tty.set(event_target_checked(&e))}
                    />
                    "Interactive terminal"
                </label>
                {move || {
                    if active() {
                        view! {
                            <button type="button" class="btn" on:click={move |_| console.disconnect()}>
                                "Disconnect"
                            </button>
                        }
                            .into_any()
                    } else {
                        view! {
                            <button
                                type="submit"
                                class="btn btn-primary"
                                disabled={move || form.service.get().is_empty()}
                            >
                                "Run"
                            </button>
                        }
                            .into_any()
                    }
                }}
            </form>
            <div class="exec-status" role="status">
                {move || match console.status.get() {
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
            <div
                class="terminal exec-terminal"
                on:click={move |_| console.focus()}
                on:wheel={move |event| console.wheel(&event)}
            >
                <div class="exec-viewport" node_ref={console.viewport}>
                    <pre class="exec-screen" inner_html={move || console.screen.get()}></pre>
                    <span class="exec-probe" node_ref={console.probe} aria-hidden="true">
                        "W"
                    </span>
                    <textarea
                        class="exec-keyboard"
                        node_ref={console.keyboard}
                        aria-label="Terminal input"
                        autocapitalize="off"
                        autocomplete="off"
                        spellcheck="false"
                        on:keydown={move |event| console.key(&event)}
                        on:input={move |event| {
                            if !event
                                .dyn_ref::<web_sys::InputEvent>()
                                .is_some_and(web_sys::InputEvent::is_composing)
                            {
                                console.take_text();
                            }
                        }}
                        on:compositionend={move |_| console.take_text()}
                        on:paste={move |event: web_sys::ClipboardEvent| {
                            event.prevent_default();
                            let text = event
                                .clipboard_data()
                                .and_then(|data| data.get_data("text").ok())
                                .unwrap_or_default();
                            console.paste(&text);
                        }}
                    ></textarea>
                </div>
            </div>
        </section>
    }
}
