//! Browser login gate and account management. The small JavaScript bridge only
//! translates `WebAuthn` binary fields; all network/state handling stays in Rust.
use super::ui::{Icon, PageHeader, Tone, badge, empty, icon, notice, text_input, when};
use leptos::ev;
use leptos::prelude::*;
use leptos::task::spawn_local;
use piqueld_client::{
    Client,
    auth::{Ceremony, CeremonyFinish, DeviceRequest, Directory, Manage, RegistrationStart, User},
};
use wasm_bindgen::prelude::*;

#[wasm_bindgen(inline_js = r#"
const decode = value => Uint8Array.from(atob(value.replace(/-/g, '+').replace(/_/g, '/')), c => c.charCodeAt(0));
const encode = value => btoa(String.fromCharCode(...new Uint8Array(value))).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
export async function piqueldPasskey(optionsJSON, registration) {
    const options = JSON.parse(optionsJSON);
    const pk = options.publicKey;
    pk.challenge = decode(pk.challenge);
    if (registration) pk.user.id = decode(pk.user.id);
    for (const list of [pk.excludeCredentials, pk.allowCredentials]) {
        if (list) for (const credential of list) credential.id = decode(credential.id);
    }
    const credential = await navigator.credentials[registration ? 'create' : 'get'](options);
    if (!credential) throw new Error('Passkey operation was cancelled');
    const response = { clientDataJSON: encode(credential.response.clientDataJSON) };
    if (registration) {
        response.attestationObject = encode(credential.response.attestationObject);
        response.transports = credential.response.getTransports?.() ?? [];
    } else {
        response.authenticatorData = encode(credential.response.authenticatorData);
        response.signature = encode(credential.response.signature);
        response.userHandle = credential.response.userHandle ? encode(credential.response.userHandle) : null;
    }
    return JSON.stringify({ id: credential.id, rawId: encode(credential.rawId), type: credential.type,
        response, extensions: credential.getClientExtensionResults() });
}
"#)]
extern "C" {
    /// Runs a `WebAuthn` create (`registration`) or get ceremony from JSON options,
    /// resolving to the credential JSON with binary fields base64url-encoded.
    #[wasm_bindgen(catch, js_name = piqueldPasskey)]
    fn passkey(options: &str, registration: bool) -> Result<js_sys::Promise, JsValue>;
}
/// Runs the browser half of a server-issued passkey ceremony and packages the
/// credential for the matching `finish` endpoint.
async fn complete(ceremony: Ceremony, registration: bool) -> Result<CeremonyFinish, String> {
    let options = serde_json::to_string(&ceremony.options).map_err(|e| e.to_string())?;
    let promise = passkey(&options, registration).map_err(|e| js_error(&e))?;
    let response = wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .map_err(|e| js_error(&e))?;
    let response = response.as_string().ok_or("Invalid passkey response")?;
    Ok(CeremonyFinish {
        id: ceremony.id,
        credential: serde_json::from_str(&response).map_err(|e| e.to_string())?,
    })
}
/// Extracts a readable message from a thrown JavaScript value.
fn js_error(error: &JsValue) -> String {
    js_sys::Reflect::get(error, &JsValue::from_str("message"))
        .ok()
        .and_then(|v| v.as_string())
        .or_else(|| error.as_string())
        .unwrap_or_else(|| "Passkey operation failed or was cancelled".into())
}
/// Registers a new passkey: start, browser ceremony, finish.
async fn register(input: RegistrationStart) -> Result<(), String> {
    let client = Client::browser();
    let challenge = client
        .auth_register_start(&input)
        .await
        .map_err(|e| e.to_string())?;
    client
        .auth_register_finish(&complete(challenge, true).await?)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}
/// Signs in with a passkey, returning the authenticated user.
async fn sign_in() -> Result<User, String> {
    let client = Client::browser();
    let ceremony = client.auth_login_start().await.map_err(|e| e.to_string())?;
    client
        .auth_login_finish(&complete(ceremony, false).await?)
        .await
        .map_err(|e| e.to_string())
}
/// Performs a full page navigation to `path`.
fn navigate(path: &str) {
    if let Some(window) = web_sys::window() {
        let _ = window.location().set_href(path);
    }
}
/// Reads an invitation secret once and removes it from the address bar and
/// session history, so it is not left behind if registration is abandoned.
fn take_invitation() -> Option<String> {
    let window = web_sys::window()?;
    let secret = window
        .location()
        .hash()
        .ok()?
        .strip_prefix("#invite=")?
        .to_owned();
    if let (Ok(history), Ok(path)) = (window.history(), window.location().pathname()) {
        let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&path));
    }
    Some(secret)
}
/// Reloads the current page.
fn reload() {
    if let Some(window) = web_sys::window() {
        let _ = window.location().reload();
    }
}
fn confirm(message: &str) -> bool {
    web_sys::window().is_some_and(|window| window.confirm_with_message(message).unwrap_or(false))
}

/// Outcome of the last asynchronous action: one busy flag, one error, one
/// success message, and an optional one-time secret to copy.
#[derive(Clone, Copy)]
struct Feedback {
    busy: RwSignal<bool>,
    error: RwSignal<String>,
    message: RwSignal<String>,
    secret: RwSignal<String>,
    revision: RwSignal<u32>,
}
impl Feedback {
    /// Creates idle feedback state.
    fn new() -> Self {
        Self {
            busy: RwSignal::new(false),
            error: RwSignal::new(String::new()),
            message: RwSignal::new(String::new()),
            secret: RwSignal::new(String::new()),
            revision: RwSignal::new(0),
        }
    }
    /// Runs `task` unless another action is in progress, showing its success
    /// message or error. `try_*` setters tolerate the owning view being unmounted.
    fn run(
        self,
        task: impl std::future::Future<Output = Result<(String, String), String>> + 'static,
    ) {
        if self.busy.get_untracked() {
            return;
        }
        self.busy.set(true);
        self.error.set(String::new());
        self.message.set(String::new());
        self.secret.set(String::new());
        spawn_local(async move {
            match task.await {
                Ok((message, secret)) => {
                    self.message.try_set(message);
                    self.secret.try_set(secret);
                    self.revision.try_update(|n| *n += 1);
                }
                Err(error) => {
                    self.error.try_set(error);
                }
            }
            self.busy.try_set(false);
        });
    }
    /// Sends an account management command and shows any one-time token or
    /// invitation link it returns.
    fn manage(self, command: Manage) {
        self.run(async move {
            let result = Client::browser()
                .auth_manage(&command)
                .await
                .map_err(|e| e.to_string())?;
            Ok(if let Some(token) = result.token {
                (
                    "Copy this token now; it will not be shown again.".into(),
                    token,
                )
            } else if let Some(url) = result.invitation_url {
                ("Invitation link, valid for 24 hours.".into(), url)
            } else {
                ("Saved.".into(), String::new())
            })
        });
    }
    /// Renders the current error and success message.
    fn view(self) -> impl IntoView {
        view! {
            {move || (!self.error.get().is_empty()).then(|| notice(Tone::Bad, self.error.get()))}
            {move || {
                (!self.message.get().is_empty())
                    .then(|| {
                        notice(
                            Tone::Ok,
                            view! {
                                {self.message.get()}
                                {(!self.secret.get().is_empty())
                                    .then(|| {
                                        view! { <pre class="secret-box">{self.secret.get()}</pre> }
                                    })}
                            },
                        )
                    })
            }}
        }
    }
}

/// Session state owned by `Gate` and shared through context.
#[derive(Clone, Copy)]
struct AuthState {
    loaded: RwSignal<bool>,
    initialized: RwSignal<bool>,
    current: RwSignal<Option<User>>,
    error: RwSignal<String>,
    expired: RwSignal<bool>,
    invitation: StoredValue<Option<String>>,
}
impl AuthState {
    /// View shown instead of the dashboard: connecting, a load error with retry,
    /// or the sign-in page.
    fn pending(self) -> AnyView {
        if !self.loaded.get() {
            return view! {
                <main class="auth-page">
                    <p class="hint" role="status">
                        "Connecting…"
                    </p>
                </main>
            }
            .into_any();
        }
        if !self.error.get().is_empty() {
            return view! {
                <main class="auth-page">
                    <section class="card auth-card">
                        {brand()} {notice(Tone::Bad, self.error.get())} <div class="form-actions">
                            <button type="button" class="btn" on:click={move |_| reload()}>
                                {icon(Icon::Refresh)}
                                "Retry"
                            </button>
                        </div>
                    </section>
                </main>
            }
            .into_any();
        }
        view! {
            <SignIn
                initialized={self.initialized.get()}
                current={self.current.get()}
                invitation={self.invitation.get_value()}
            />
        }
        .into_any()
    }
}

/// The signed-in account, for chrome that shows who is working.
pub(super) fn auth_user() -> RwSignal<Option<User>> {
    use_context::<AuthState>()
        .expect("authentication gate")
        .current
}

fn brand() -> AnyView {
    view! {
        <div class="auth-brand">
            <span class="brand-mark" aria-hidden="true">
                "p"
            </span>
            "piqueld"
        </div>
    }
    .into_any()
}

/// Keep the router and route definitions mounted for the lifetime of the page.
/// Only the route view changes when a browser session is established.
#[component]
pub(super) fn Gate() -> impl IntoView {
    let state = AuthState {
        loaded: RwSignal::new(false),
        initialized: RwSignal::new(false),
        current: RwSignal::new(None::<User>),
        error: RwSignal::new(String::new()),
        expired: RwSignal::new(false),
        invitation: StoredValue::new(take_invitation()),
    };
    provide_context(state);
    let listener = window_event_listener(
        ev::Custom::<web_sys::Event>::new(Client::AUTHENTICATION_REQUIRED_EVENT),
        move |_| {
            if state.current.get_untracked().is_some() {
                state.expired.set(true);
            }
        },
    );
    on_cleanup(move || listener.remove());
    spawn_local(async move {
        let client = Client::browser();
        match client.auth_status().await {
            Ok(status) => {
                state.initialized.set(status.initialized);
                match client.auth_me().await {
                    Ok(user) => state.current.set(Some(user)),
                    Err(piqueld_client::ClientError::Api { status, .. })
                        if status.as_u16() == 401 => {}
                    Err(e) => state.error.set(e.to_string()),
                }
            }
            Err(e) => state.error.set(e.to_string()),
        }
        state.loaded.set(true);
    });
    // Keep the editor mounted while reauthenticating so its unsaved drafts survive.
    view! {
        <div inert={move || state.expired.get().then_some("")}>
            <super::App />
        </div>
        <Show when={move || state.expired.get()}>
            <SessionExpired state={state} />
        </Show>
    }
}

/// Modal overlay shown when the session expires; signing in again clears
/// `expired` without remounting the dashboard.
#[component]
fn SessionExpired(state: AuthState) -> impl IntoView {
    let feedback = Feedback::new();
    view! {
        <div
            class="auth-overlay"
            role="dialog"
            aria-modal="true"
            aria-labelledby="session-expired-title"
        >
            <section class="card auth-card">
                {brand()} <h2 id="session-expired-title">"Your session has expired"</h2>
                <p class="hint">
                    "Sign in to continue. Unsaved edits are still here; retry any action that failed after signing in."
                </p> {feedback.view()} <div class="form-actions">
                    <button
                        type="button"
                        class="btn btn-primary"
                        autofocus
                        disabled={move || feedback.busy.get()}
                        on:click={move |_| {
                            feedback
                                .run(async move {
                                    let user = sign_in().await?;
                                    state.current.set(Some(user));
                                    state.expired.set(false);
                                    Ok((String::new(), String::new()))
                                });
                        }}
                    >
                        {icon(Icon::Key)}
                        "Sign in with a passkey"
                    </button>
                    <Logout />
                </div>
            </section>
        </div>
    }
}

/// Renders `DashboardLayout` once a user is signed in, otherwise the pending/sign-in view.
#[component]
pub(super) fn ProtectedDashboardLayout() -> impl IntoView {
    let state = use_context::<AuthState>().expect("authentication gate");
    view! {
        <Show
            when={move || {
                state.loaded.get() && state.error.get().is_empty() && state.current.get().is_some()
            }}
            fallback={move || state.pending()}
        >
            <super::DashboardLayout />
        </Show>
    }
}

/// Standalone `/dashboard/auth` page (sign-in, registration, device approval).
#[component]
pub(super) fn AuthPage() -> impl IntoView {
    let state = use_context::<AuthState>().expect("authentication gate");
    move || state.pending()
}

/// Authentication page body. Chooses between invitation registration, first-run
/// setup instructions, passkey sign-in, CLI device approval (`#device` fragment)
/// and a signed-in landing view.
#[component]
fn SignIn(initialized: bool, current: Option<User>, invitation: Option<String>) -> impl IntoView {
    let feedback = Feedback::new();
    let device = web_sys::window()
        .and_then(|w| w.location().hash().ok())
        .is_some_and(|fragment| fragment == "#device");
    let username = RwSignal::new(String::new());
    let display_name = RwSignal::new(String::new());
    let name = RwSignal::new("My passkey".to_owned());
    let is_registration = invitation.is_some();
    let signed_in = current.is_some();
    let who = current.map(|u| u.username).unwrap_or_default();
    let content = if is_registration {
        view! {
            <h2>"Create your account"</h2>
            <p class="hint">
                "You have been invited to this piqueld host. Choose a username and register a passkey to finish."
            </p>
            <div class="stack-sm">
                <label class="field">
                    <span>"Username"</span>
                    <input
                        type="text"
                        autocomplete="username"
                        prop:value={move || username.get()}
                        on:input={move |event| username.set(event_target_value(&event))}
                    />
                </label>
                {text_input("Display name (optional)", display_name, String::clone, |v, s| *v = s)}
                {text_input("Passkey name", name, String::clone, |v, s| *v = s)}
            </div>
            <div class="form-actions">
                <button
                    type="button"
                    class="btn btn-primary"
                    on:click={move |_| {
                        let input = RegistrationStart {
                            invitation: invitation.clone(),
                            user_id: None,
                            username: username.get_untracked(),
                            display_name: display_name.get_untracked(),
                            passkey_name: name.get_untracked(),
                        };
                        feedback
                            .run(async move {
                                register(input).await?;
                                navigate("/dashboard/");
                                Ok((String::new(), String::new()))
                            });
                    }}
                >
                    {icon(Icon::Key)}
                    "Create account with a passkey"
                </button>
            </div>
        }
        .into_any()
    } else if !initialized {
        view! {
            <h2>"Set up piqueld"</h2>
            <p class="hint">
                "No account exists yet. Run " <code>"piquelctl setup-link"</code>
                " on the daemon host and open the link it prints to create the first account. It is also saved in the "
                <code>"setup-link"</code> " file in the daemon’s data directory."
            </p>
            <div class="form-actions">
                <button type="button" class="btn" on:click={move |_| reload()}>
                    {icon(Icon::Refresh)}
                    "Check again"
                </button>
            </div>
        }
        .into_any()
    } else if !signed_in {
        view! {
            <h2>"Sign in"</h2>
            <p class="hint">"Use a passkey registered for this piqueld host."</p>
            <div class="form-actions">
                <button
                    type="button"
                    class="btn btn-primary"
                    on:click={move |_| {
                        feedback
                            .run(async move {
                                sign_in().await?;
                                reload();
                                Ok((String::new(), String::new()))
                            });
                    }}
                >
                    {icon(Icon::Key)}
                    "Sign in with a passkey"
                </button>
            </div>
        }
        .into_any()
    } else if device {
        view! { <DeviceApproval who={who} /> }.into_any()
    } else {
        view! {
            <h2>{format!("Signed in as {who}")}</h2>
            <div class="form-actions">
                <a class="btn btn-primary" href="/dashboard/">
                    "Open dashboard"
                </a>
                <Logout />
            </div>
        }
        .into_any()
    };
    view! {
        <main class="auth-page">
            <section class="card auth-card">
                {brand()} {feedback.view()}
                <fieldset disabled={move || feedback.busy.get()}>{content}</fieldset>
            </section>
        </main>
    }
}

/// Two-step CLI approval: the approver first sees where the request came from.
#[component]
fn DeviceApproval(who: String) -> impl IntoView {
    let feedback = Feedback::new();
    let code = RwSignal::new(String::new());
    let request = RwSignal::new(None::<DeviceRequest>);
    let review = move |_| {
        feedback.run(async move {
            let found = Client::browser()
                .auth_device_inspect(&code.get_untracked())
                .await
                .map_err(|e| e.to_string())?;
            request.set(Some(found));
            Ok((String::new(), String::new()))
        });
    };
    let approve = move |_| {
        let Some(pending) = request.get_untracked() else {
            return;
        };
        feedback.run(async move {
            Client::browser()
                .auth_device_approve(&pending.user_code)
                .await
                .map_err(|e| e.to_string())?;
            request.set(None);
            code.set(String::new());
            Ok((
                "CLI approved. You can return to your terminal.".into(),
                String::new(),
            ))
        });
    };
    view! {
        <h2>"Connect piquelctl"</h2>
        <p class="hint">
            "Signed in as " <strong>{who}</strong>
            ". Enter the code shown by the CLI you are connecting."
        </p>
        {notice(
            Tone::Warn,
            view! {
                <strong>
                    "Only approve a code shown in a terminal you started yourself in the last ten minutes."
                </strong>
                "Approval gives that terminal full access as your account. Never enter a code someone else sent you."
            },
        )}
        {feedback.view()}
        <fieldset disabled={move || {
            feedback.busy.get()
        }}>
            {move || match request.get() {
                None => {
                    view! {
                        <div class="stack-sm">
                            <label class="field">
                                <span>"CLI code"</span>
                                <input
                                    autocomplete="off"
                                    placeholder="ABCD-EFGH"
                                    prop:value={code}
                                    on:input={move |e| code.set(event_target_value(&e))}
                                />
                            </label>
                        </div>
                        <div class="form-actions">
                            <button type="button" class="btn btn-primary" on:click={review}>
                                "Review request"
                            </button>
                        </div>
                    }
                        .into_any()
                }
                Some(pending) => {
                    view! {
                        <dl class="kv device-request">
                            <dt>"Code"</dt>
                            <dd class="device-code">{pending.user_code.clone()}</dd>
                            <dt>"Requested from"</dt>
                            <dd>{requester_label(pending.requester.as_deref())}</dd>
                            <dt>"Started"</dt>
                            <dd>{age_label(pending.age)}</dd>
                        </dl>
                        <p class="hint" style="margin-top:12px">
                            "piquelctl printed the address it connected from. Approve only if it matches this request."
                        </p>
                        <div class="form-actions">
                            <button type="button" class="btn btn-primary" on:click={approve}>
                                "Approve CLI login"
                            </button>
                            <button type="button" class="btn" on:click={move |_| request.set(None)}>
                                "Cancel"
                            </button>
                        </div>
                    }
                        .into_any()
                }
            }}
        </fieldset>
        <div class="form-actions">
            <Logout />
        </div>
    }
}
/// Describes where a device request originated; `None` means the local socket.
fn requester_label(requester: Option<&str>) -> String {
    requester.map_or_else(
        || "the daemon's local Unix socket".into(),
        |address| format!("network address {address}"),
    )
}
/// Coarse relative age of a device request.
///
/// ```text
/// 30 -> "less than a minute ago"
/// 150 -> "2 minutes ago"
/// ```
fn age_label(seconds: u32) -> String {
    match seconds / 60 {
        0 => "less than a minute ago".into(),
        1 => "1 minute ago".into(),
        minutes => format!("{minutes} minutes ago"),
    }
}

/// Sign-out button; an already-invalid session (401) still counts as signed out.
#[component]
pub(super) fn Logout(#[prop(optional)] compact: bool) -> impl IntoView {
    let feedback = Feedback::new();
    let sign_out = move |_| {
        feedback.run(async move {
            match Client::browser().auth_logout().await {
                Ok(_) => {}
                Err(piqueld_client::ClientError::Api { status, .. }) if status.as_u16() == 401 => {}
                Err(error) => return Err(error.to_string()),
            }
            navigate("/dashboard/");
            Ok((String::new(), String::new()))
        });
    };
    let error =
        move || (!feedback.error.get().is_empty()).then(|| notice(Tone::Bad, feedback.error.get()));
    if compact {
        view! {
            <button
                type="button"
                class="btn btn-ghost btn-icon"
                title="Sign out"
                aria-label="Sign out"
                disabled={move || feedback.busy.get()}
                on:click={sign_out}
            >
                {icon(Icon::LogOut)}
            </button>
            {error}
        }
        .into_any()
    } else {
        view! {
            <button
                type="button"
                class="btn"
                disabled={move || feedback.busy.get()}
                on:click={sign_out}
            >
                {icon(Icon::LogOut)}
                "Sign out"
            </button>
            {error}
        }
        .into_any()
    }
}

/// Account administration page. Loads the user directory (reloading after each
/// successful action) and renders every account plus pending invitations.
#[component]
pub(super) fn AccountsPage() -> impl IntoView {
    let feedback = Feedback::new();
    let directory = RwSignal::new(None::<Directory>);
    Effect::new(move |_| {
        feedback.revision.get();
        spawn_local(async move {
            match Client::browser().auth_directory().await {
                Ok(value) => directory.set(Some(value)),
                Err(error) => feedback.error.set(error.to_string()),
            }
        });
    });
    view! {
        <PageHeader
            title="Accounts"
            description="Every account has the same capabilities. Invite people with single-use links and manage passkeys, sessions, and API tokens."
        >
            <button
                type="button"
                class="btn btn-primary"
                disabled={move || feedback.busy.get()}
                on:click={move |_| feedback.manage(Manage::CreateInvitation)}
            >
                {icon(Icon::Plus)}
                "Create invitation"
            </button>
        </PageHeader>
        <div class="stack">
            {feedback.view()} <fieldset class="stack" disabled={move || feedback.busy.get()}>
                {move || {
                    directory
                        .get()
                        .map_or_else(
                            || {
                                feedback
                                    .error
                                    .with(String::is_empty)
                                    .then(|| empty("Loading accounts…"))
                                    .into_any()
                            },
                            |data| {
                                view! {
                                    {data
                                        .users
                                        .iter()
                                        .cloned()
                                        .map(|user| {
                                            view! {
                                                <Account
                                                    user={user}
                                                    directory={data.clone()}
                                                    feedback={feedback}
                                                />
                                            }
                                        })
                                        .collect_view()}
                                    <Invitations directory={data} feedback={feedback} />
                                }
                                    .into_any()
                            },
                        )
                }}
            </fieldset>
        </div>
    }
}

#[component]
fn Invitations(directory: Directory, feedback: Feedback) -> impl IntoView {
    let users = directory.users;
    let issuer = move |id: &str| {
        users
            .iter()
            .find(|u| u.id == id)
            .map_or_else(|| id.to_owned(), |u| u.username.clone())
    };
    let pending = directory.invitations.is_empty();
    let rows = directory
        .invitations
        .into_iter()
        .map(|invite| {
            let id = invite.id;
            view! {
                <tr>
                    <td>{issuer(&invite.issuer_id)}</td>
                    <td class="muted">{when(invite.expires_at * 1000)}</td>
                    <td class="actions">
                        <button
                            type="button"
                            class="btn btn-ghost btn-sm"
                            on:click={move |_| {
                                feedback
                                    .manage(Manage::RevokeInvitation {
                                        id: id.clone(),
                                    });
                            }}
                        >
                            "Revoke"
                        </button>
                    </td>
                </tr>
            }
        })
        .collect_view();
    view! {
        <section class="card card-flush">
            <header>
                <div>
                    <h2>"Pending invitations"</h2>
                    <p>
                        "Links expire after 24 hours. The first person to complete registration chooses their own account details."
                    </p>
                </div>
            </header>
            {if pending {
                empty("No pending invitations.")
            } else {
                view! {
                    <table class="table">
                        <thead>
                            <tr>
                                <th>"Issued by"</th>
                                <th>"Expires"</th>
                                <th></th>
                            </tr>
                        </thead>
                        <tbody>{rows}</tbody>
                    </table>
                }
                    .into_any()
            }}
        </section>
    }
}

/// Management card for one user: profile, passkeys, sessions and API tokens,
/// token creation and account deletion.
#[component]
fn Account(user: User, directory: Directory, feedback: Feedback) -> impl IntoView {
    let id = StoredValue::new(user.id.clone());
    let username = RwSignal::new(user.username);
    let display_name = RwSignal::new(user.display_name);
    let passkey_name = RwSignal::new("New passkey".to_owned());
    let token_name = RwSignal::new(String::new());
    let days = RwSignal::new("90".to_owned());
    let passkeys = directory
        .passkeys
        .into_iter()
        .filter(|p| p.user_id == user.id)
        .collect::<Vec<_>>();
    let credentials = directory
        .credentials
        .into_iter()
        .filter(|c| c.user_id == user.id)
        .collect::<Vec<_>>();
    let delete = move |_| {
        let message = format!(
            "Delete account '{}' and all its passkeys, sessions, and tokens? This cannot be undone.",
            username.get_untracked()
        );
        if confirm(&message) {
            feedback.manage(Manage::DeleteUser {
                user_id: id.get_value(),
            });
        }
    };
    let create_token = move |_| {
        let days = days.get_untracked();
        let parsed = if days.is_empty() {
            Ok(None)
        } else {
            days.parse::<u32>().map(Some)
        };
        match parsed {
            Ok(days) => feedback.manage(Manage::CreateToken {
                user_id: id.get_value(),
                name: token_name.get_untracked(),
                days,
            }),
            Err(_) => feedback
                .error
                .set("Enter a positive number of days, or leave blank".into()),
        }
    };
    view! {
        <section class="card auth-account">
            <header>
                <div>
                    <h2>{move || username.get()}</h2>
                    <p>
                        {move || {
                            let name = display_name.get();
                            if name.is_empty() { "No display name".to_owned() } else { name }
                        }}
                    </p>
                </div>
                <button type="button" class="btn btn-danger btn-sm" on:click={delete}>
                    "Delete account"
                </button>
            </header>

            <div class="section-header">
                <h3>"Profile"</h3>
            </div>
            <div class="form-grid">
                {text_input("Username", username, String::clone, |v, s| *v = s)}
                {text_input("Display name", display_name, String::clone, |v, s| *v = s)}
            </div>
            <div class="form-actions">
                <button
                    type="button"
                    class="btn"
                    on:click={move |_| {
                        feedback
                            .manage(Manage::UpdateUser {
                                user_id: id.get_value(),
                                username: username.get_untracked(),
                                display_name: display_name.get_untracked(),
                            });
                    }}
                >
                    "Save profile"
                </button>
            </div>

            <div class="section-header">
                <h3>"Passkeys"</h3>
            </div>
            <div class="form-list">
                {passkeys
                    .into_iter()
                    .map(|key| {
                        let key_id = StoredValue::new(key.id);
                        let label = RwSignal::new(key.name);
                        view! {
                            <div class="form-row">
                                {text_input("Passkey name", label, String::clone, |v, s| *v = s)}
                                <button
                                    type="button"
                                    class="btn"
                                    on:click={move |_| {
                                        feedback
                                            .manage(Manage::RenamePasskey {
                                                id: key_id.get_value(),
                                                name: label.get_untracked(),
                                            });
                                    }}
                                >
                                    "Rename"
                                </button>
                                <button
                                    type="button"
                                    class="btn btn-ghost"
                                    on:click={move |_| {
                                        feedback
                                            .manage(Manage::RemovePasskey {
                                                id: key_id.get_value(),
                                            });
                                    }}
                                >
                                    "Remove"
                                </button>
                            </div>
                        }
                    })
                    .collect_view()}
                <div class="form-row">
                    {text_input("New passkey name", passkey_name, String::clone, |v, s| *v = s)}
                    <button
                        type="button"
                        class="btn"
                        on:click={move |_| {
                            feedback
                                .run(async move {
                                    register(RegistrationStart {
                                            invitation: None,
                                            user_id: Some(id.get_value()),
                                            username: String::new(),
                                            display_name: String::new(),
                                            passkey_name: passkey_name.get_untracked(),
                                        })
                                        .await?;
                                    Ok(("Passkey added.".into(), String::new()))
                                });
                        }}
                    >
                        {icon(Icon::Key)}
                        "Add passkey"
                    </button>
                </div>
            </div>

            <div class="section-header">
                <h3>"Sessions and API tokens"</h3>
                <button
                    type="button"
                    class="btn btn-danger btn-sm"
                    on:click={move |_| {
                        feedback
                            .manage(Manage::RevokeAll {
                                user_id: id.get_value(),
                            });
                    }}
                >
                    "Revoke all"
                </button>
            </div>
            <div class="table-wrap">
                {if credentials.is_empty() {
                    empty("No active sessions or tokens.")
                } else {
                    view! {
                        <table class="table">
                            <thead>
                                <tr>
                                    <th>"Name"</th>
                                    <th>"Kind"</th>
                                    <th>"Last used"</th>
                                    <th>"Expires"</th>
                                    <th></th>
                                </tr>
                            </thead>
                            <tbody>
                                {credentials
                                    .into_iter()
                                    .map(|credential| {
                                        let credential_id = credential.id.clone();
                                        view! {
                                            <tr>
                                                <td>{credential.name}</td>
                                                <td>{badge(Tone::Neutral, credential.kind)}</td>
                                                <td class="muted">
                                                    {when(credential.last_used_at * 1000)}
                                                </td>
                                                <td class="muted">
                                                    {credential
                                                        .expires_at
                                                        .map_or_else(|| "Never".into_any(), |t| when(t * 1000))}
                                                </td>
                                                <td class="actions">
                                                    <button
                                                        type="button"
                                                        class="btn btn-ghost btn-sm"
                                                        on:click={move |_| {
                                                            feedback
                                                                .manage(Manage::RevokeCredential {
                                                                    id: credential_id.clone(),
                                                                });
                                                        }}
                                                    >
                                                        "Revoke"
                                                    </button>
                                                </td>
                                            </tr>
                                        }
                                    })
                                    .collect_view()}
                            </tbody>
                        </table>
                    }
                        .into_any()
                }}
            </div>
            <div class="form-row" style="margin-top:14px">
                {text_input("New token name", token_name, String::clone, |v, s| *v = s)}
                <label class="field">
                    <span>"Expires in days (blank: never)"</span>
                    <input
                        type="number"
                        min="1"
                        prop:value={days}
                        on:input={move |e| days.set(event_target_value(&e))}
                    />
                </label>
                <button type="button" class="btn" on:click={create_token}>
                    "Create token"
                </button>
            </div>
        </section>
    }
}
