//! Browser login gate and account management. The small JavaScript bridge only
//! translates `WebAuthn` binary fields; all network/state handling stays in Rust.
use leptos::*;
use piqueld_client::{Client, auth::*};
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
    #[wasm_bindgen(catch, js_name = piqueldPasskey)]
    fn passkey(options: &str, registration: bool) -> Result<js_sys::Promise, JsValue>;
}
async fn complete(ceremony: Ceremony, registration: bool) -> Result<CeremonyFinish, String> {
    let options = serde_json::to_string(&ceremony.options).map_err(|e| e.to_string())?;
    let promise = passkey(&options, registration).map_err(js_error)?;
    let response = wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .map_err(js_error)?;
    let response = response.as_string().ok_or("Invalid passkey response")?;
    Ok(CeremonyFinish {
        id: ceremony.id,
        credential: serde_json::from_str(&response).map_err(|e| e.to_string())?,
    })
}
fn js_error(error: JsValue) -> String {
    js_sys::Reflect::get(&error, &JsValue::from_str("message"))
        .ok()
        .and_then(|v| v.as_string())
        .or_else(|| error.as_string())
        .unwrap_or_else(|| "Passkey operation failed or was cancelled".into())
}
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
async fn sign_in() -> Result<User, String> {
    let client = Client::browser();
    let ceremony = client.auth_login_start().await.map_err(|e| e.to_string())?;
    client
        .auth_login_finish(&complete(ceremony, false).await?)
        .await
        .map_err(|e| e.to_string())
}
fn navigate(path: &str) {
    if let Some(window) = web_sys::window() {
        let _ = window.location().set_href(path);
    }
}
fn reload() {
    if let Some(window) = web_sys::window() {
        let _ = window.location().reload();
    }
}

#[derive(Clone, Copy)]
struct Feedback {
    busy: RwSignal<bool>,
    error: RwSignal<String>,
    message: RwSignal<String>,
    revision: RwSignal<u32>,
}
impl Feedback {
    fn new() -> Self {
        Self {
            busy: create_rw_signal(false),
            error: create_rw_signal(String::new()),
            message: create_rw_signal(String::new()),
            revision: create_rw_signal(0),
        }
    }
    fn run(self, task: impl std::future::Future<Output = Result<String, String>> + 'static) {
        if self.busy.get_untracked() {
            return;
        }
        self.busy.set(true);
        self.error.set(String::new());
        self.message.set(String::new());
        spawn_local(async move {
            match task.await {
                Ok(message) => {
                    self.message.try_set(message);
                    self.revision.try_update(|n| *n += 1);
                }
                Err(error) => {
                    self.error.try_set(error);
                }
            }
            self.busy.try_set(false);
        });
    }
    fn manage(self, command: Manage) {
        self.run(async move {
            let result = Client::browser()
                .auth_manage(&command)
                .await
                .map_err(|e| e.to_string())?;
            Ok(result
                .token
                .map(|s| format!("Copy this token now; it will not be shown again:\n{s}"))
                .or_else(|| {
                    result
                        .invitation_url
                        .map(|s| format!("Invitation link (valid for 24 hours):\n{s}"))
                })
                .unwrap_or_else(|| "Saved".into()))
        });
    }
    fn view(self) -> impl IntoView {
        view! { <p role="alert">{move || self.error.get()}</p><pre class="auth-secret" aria-live="polite">{move || self.message.get()}</pre> }
    }
}

#[derive(Clone, Copy)]
struct AuthState {
    loaded: RwSignal<bool>,
    initialized: RwSignal<bool>,
    current: RwSignal<Option<User>>,
    error: RwSignal<String>,
    expired: RwSignal<bool>,
}
impl AuthState {
    fn pending(self) -> View {
        if !self.loaded.get() {
            return view! { <main class="dashboard-main"><p>"Connecting…"</p></main> }.into_view();
        }
        if !self.error.get().is_empty() {
            return view! { <main class="dashboard-main"><p role="alert">{self.error.get()}</p><button on:click=move |_| reload()>"Retry"</button></main> }.into_view();
        }
        view! { <SignIn initialized=self.initialized.get() current=self.current.get()/> }
            .into_view()
    }
}

/// Keep the router and route definitions mounted for the lifetime of the page.
/// Only the route view changes when a browser session is established.
#[component]
pub(super) fn Gate() -> impl IntoView {
    let state = AuthState {
        loaded: create_rw_signal(false),
        initialized: create_rw_signal(false),
        current: create_rw_signal(None::<User>),
        error: create_rw_signal(String::new()),
        expired: create_rw_signal(false),
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
        <div inert=move || state.expired.get().then_some("")><super::App/></div>
        <Show when=move || state.expired.get()>
            <SessionExpired state=state/>
        </Show>
    }
}

#[component]
fn SessionExpired(state: AuthState) -> impl IntoView {
    let feedback = Feedback::new();
    view! {
        <div class="auth-overlay" role="dialog" aria-modal="true" aria-labelledby="session-expired-title">
            <section class="settings-card">
                <h2 id="session-expired-title">"Your session has expired or been revoked"</h2>
                <p>"Sign in to continue. Your unsaved edits are still here. Retry any action that failed after signing in."</p>
                {feedback.view()}
                <button class="primary" autofocus disabled=move || feedback.busy.get() on:click=move |_| feedback.run(async move {
                    let user = sign_in().await?;
                    state.current.set(Some(user));
                    state.expired.set(false);
                    Ok(String::new())
                })>"Sign in with a passkey"</button>
                <Logout/>
            </section>
        </div>
    }
}

#[component]
pub(super) fn ProtectedDashboardLayout() -> impl IntoView {
    let state = use_context::<AuthState>().expect("authentication gate");
    view! {
        <Show
            when=move || state.loaded.get() && state.error.get().is_empty() && state.current.get().is_some()
            fallback=move || state.pending()
        >
            <super::DashboardLayout/>
        </Show>
    }
}

#[component]
pub(super) fn AuthPage() -> impl IntoView {
    let state = use_context::<AuthState>().expect("authentication gate");
    move || state.pending()
}

#[component]
fn SignIn(initialized: bool, current: Option<User>) -> impl IntoView {
    let feedback = Feedback::new();
    let fragment = web_sys::window()
        .and_then(|w| w.location().hash().ok())
        .unwrap_or_default();
    let invitation = fragment.strip_prefix("#invite=").map(str::to_owned);
    let device = fragment == "#device";
    let username = create_rw_signal(String::new());
    let display_name = create_rw_signal(String::new());
    let name = create_rw_signal("My passkey".to_owned());
    let code = create_rw_signal(String::new());
    let is_registration = invitation.is_some();
    let signed_in = current.is_some();
    let who = current.map(|u| u.username).unwrap_or_default();
    view! {
        <main class="auth-page"><section class="settings-card">
            <h1>"piqueld"</h1>
            {feedback.view()}
            <fieldset disabled=move || feedback.busy.get()>
            {if is_registration {
                view! {
                    <h2>"Create your account"</h2>
                    <label class="field">"Username"<input autocomplete="username" prop:value=username on:input=move |e|username.set(event_target_value(&e))/></label>
                    <label class="field">"Display name (optional)"<input prop:value=display_name on:input=move |e|display_name.set(event_target_value(&e))/></label>
                    <label class="field">"Passkey name"<input prop:value=name on:input=move |e|name.set(event_target_value(&e))/></label>
                    <button class="primary" on:click=move |_| {
                        let input=RegistrationStart { invitation:invitation.clone(),user_id:None,username:username.get_untracked(),display_name:display_name.get_untracked(),passkey_name:name.get_untracked() };
                        feedback.run(async move {register(input).await?;navigate("/dashboard/");Ok(String::new())});
                    }>"Create account with a passkey"</button>
                }.into_view()
            } else if !initialized {
                view! { <h2>"Set up piqueld"</h2><p>"Open the setup link saved in the daemon’s data directory, in the setup-link file, to create the first account."</p><button on:click=move |_|reload()>"Check again"</button> }.into_view()
            } else if !signed_in {
                view! { <h2>"Sign in"</h2><button class="primary" on:click=move |_| feedback.run(async move {
                    sign_in().await?;
                    reload(); Ok(String::new())
                })>"Sign in with a passkey"</button> }.into_view()
            } else if device {
                view! { <h2>"Connect piquelctl"</h2><p>{format!("Signed in as {who}. Enter the code shown by the CLI you are connecting.")}</p>
                    <label class="field">"CLI code"<input autocomplete="off" prop:value=code on:input=move |e|code.set(event_target_value(&e))/></label>
                    <button class="primary" on:click=move |_|feedback.run(async move {
                        Client::browser().auth_device_approve(&code.get_untracked()).await.map_err(|e|e.to_string())?;
                        Ok("CLI approved. You can return to your terminal.".into())
                    })>"Approve CLI login"</button><Logout/>
                }.into_view()
            } else {view!{<a href="/dashboard/">"Open dashboard"</a><Logout/>}.into_view()}}
            </fieldset>
        </section></main>
    }
}

#[component]
pub(super) fn Logout() -> impl IntoView {
    let feedback = Feedback::new();
    view! { <button disabled=move ||feedback.busy.get() on:click=move |_|feedback.run(async move {
        match Client::browser().auth_logout().await {
            Ok(_) => {},
            Err(piqueld_client::ClientError::Api { status, .. }) if status.as_u16() == 401 => {},
            Err(error) => return Err(error.to_string()),
        }
        navigate("/dashboard/");Ok(String::new())
    })>"Sign out"</button><span role="alert">{move ||feedback.error.get()}</span> }
}

#[component]
pub(super) fn AccountsPage() -> impl IntoView {
    let feedback = Feedback::new();
    let directory = create_rw_signal(None::<Directory>);
    create_effect(move |_| {
        feedback.revision.get();
        spawn_local(async move {
            match Client::browser().auth_directory().await {
                Ok(value) => directory.set(Some(value)),
                Err(error) => feedback.error.set(error.to_string()),
            }
        });
    });
    view! {
        <header class="page-heading"><h1>"Accounts"</h1><button disabled=move ||feedback.busy.get() on:click=move |_|feedback.manage(Manage::CreateInvitation)>"Create invitation"</button></header>
        {feedback.view()}
        <fieldset disabled=move ||feedback.busy.get()>
        {move ||directory.get().map(|data| {
            let invitations=data.invitations.clone();
            view! {
                {data.users.iter().cloned().map(|user|view!{<Account user=user directory=data.clone() feedback=feedback/>}).collect_view()}
                <section class="settings-card"><h2>"Pending invitations"</h2>
                {invitations.into_iter().map(|invite| {
                    let issuer=data.users.iter().find(|u|u.id==invite.issuer_id).map_or_else(||invite.issuer_id.clone(),|u|u.username.clone());
                    view!{<p>{format!("Issued by {issuer} · expires {}", timestamp(invite.expires_at))}<button on:click=move |_|feedback.manage(Manage::RevokeInvitation{id:invite.id.clone()})>"Revoke"</button></p>}
                }).collect_view()}
                </section>
            }
        })}
        </fieldset>
    }
}
fn timestamp(seconds: i64) -> String {
    js_sys::Date::new(&JsValue::from_f64(seconds as f64 * 1000.0))
        .to_locale_string("default", &JsValue::UNDEFINED)
        .into()
}
#[component]
fn Account(user: User, directory: Directory, feedback: Feedback) -> impl IntoView {
    let id = store_value(user.id.clone());
    let username = create_rw_signal(user.username);
    let display_name = create_rw_signal(user.display_name);
    let passkey_name = create_rw_signal("New passkey".to_owned());
    let token_name = create_rw_signal(String::new());
    let days = create_rw_signal("90".to_owned());
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
    view! {
        <section class="settings-card auth-account">
            <h2>{move ||username.get()}</h2>
            <label class="field">"Username"<input prop:value=username on:input=move |e|username.set(event_target_value(&e))/></label>
            <label class="field">"Display name"<input prop:value=display_name on:input=move |e|display_name.set(event_target_value(&e))/></label>
            <button on:click=move |_|feedback.manage(Manage::UpdateUser{user_id:id.get_value(),username:username.get_untracked(),display_name:display_name.get_untracked()})>"Save profile"</button>
            <h3>"Passkeys"</h3>
            {passkeys.into_iter().map(|key|{
                let key_id=store_value(key.id);
                let label=create_rw_signal(key.name);
                view!{<div class="form-actions"><label class="field"><span>"Passkey name"</span><input prop:value=label on:input=move |e|label.set(event_target_value(&e))/></label>
                    <button on:click=move |_|feedback.manage(Manage::RenamePasskey{id:key_id.get_value(),name:label.get_untracked()})>"Rename"</button>
                    <button on:click=move |_|feedback.manage(Manage::RemovePasskey{id:key_id.get_value()})>"Remove passkey"</button></div>}
            }).collect_view()}
            <label class="field">"New passkey name"<input prop:value=passkey_name on:input=move |e|passkey_name.set(event_target_value(&e))/></label>
            <button on:click=move |_|feedback.run(async move {
                register(RegistrationStart{invitation:None,user_id:Some(id.get_value()),username:String::new(),display_name:String::new(),passkey_name:passkey_name.get_untracked()}).await?;
                Ok("Passkey added".into())
            })>"Add passkey"</button>
            <h3>"Sessions and API tokens"</h3>
            {credentials.into_iter().map(|credential|view!{
                <p>{format!("{} ({}) · last used {} · {}",credential.name,credential.kind,timestamp(credential.last_used_at),credential.expires_at.map_or_else(||"never expires".into(),|t|format!("expires {}",timestamp(t))))}
                    <button on:click=move |_|feedback.manage(Manage::RevokeCredential{id:credential.id.clone()})>"Revoke"</button></p>
            }).collect_view()}
            <button on:click=move |_|feedback.manage(Manage::RevokeAll{user_id:id.get_value()})>"Revoke all sessions and tokens"</button>
            <h3>"Create API token"</h3>
            <label class="field">"Token name"<input prop:value=token_name on:input=move |e|token_name.set(event_target_value(&e))/></label>
            <label class="field">"Expires in days (blank: never)"<input type="number" min="1" prop:value=days on:input=move |e|days.set(event_target_value(&e))/></label>
            <button on:click=move |_|{
                let days=days.get_untracked();
                let parsed=if days.is_empty(){Ok(None)}else{days.parse::<u32>().map(Some)};
                match parsed {Ok(days)=>feedback.manage(Manage::CreateToken{user_id:id.get_value(),name:token_name.get_untracked(),days}),Err(_)=>feedback.error.set("Enter a positive number of days, or leave blank".into())}
            }>"Create token"</button>
            <hr/>
            <button class="danger" on:click=move |_| {
                let message = format!("Delete account '{}' and all its passkeys, sessions, and tokens? This cannot be undone.", username.get_untracked());
                if web_sys::window().is_some_and(|window| window.confirm_with_message(&message).unwrap_or(false)) {
                    feedback.manage(Manage::DeleteUser{user_id:id.get_value()});
                }
            }>"Delete account"</button>
        </section>
    }
}
