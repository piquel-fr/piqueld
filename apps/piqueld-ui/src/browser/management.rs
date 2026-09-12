//! Application management forms. Polling never replaces local edits.
use super::{client_error_message, dashboard_context, detail_view};
use crate::editor::{Section, ServiceForm};
use leptos::wasm_bindgen::{JsCast, closure::Closure};
use leptos::{
    Callable, Callback, CollectView, For, IntoView, RwSignal, Show, Signal, SignalGet,
    SignalGetUntracked, SignalSet, SignalUpdate, SignalWith, SignalWithUntracked, StoredValue,
    View, component, create_effect, create_rw_signal, document, ev, event_target_checked,
    event_target_value, on_cleanup, provide_context, spawn_local, store_value, use_context, view,
    window, window_event_listener,
};
use leptos_router::{A, NavigateOptions, use_navigate};
use piqueld_client::{
    ApplicationManifest, ApplicationSpec, ApplicationView, ApplyApplicationRequest, Client,
    ClientError, DeploymentView, GitRepository, Metadata, Mount, Page, RepositoryManifest, Service,
    Source, Volume,
};
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

#[derive(Clone, Copy)]
struct EditorContext {
    dashboard: StoredValue<super::DashboardContext>,
    latest_deployment: RwSignal<Option<DeploymentView>>,
    saved: RwSignal<ApplicationView>,
    dirty: RwSignal<BTreeSet<String>>,
    busy: RwSignal<bool>,
    uncertain: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    notice: RwSignal<String>,
    tab: RwSignal<&'static str>,
}
impl EditorContext {
    fn manifest(self) -> ApplicationManifest {
        let saved = self.saved.get_untracked();
        ApplicationManifest {
            api_version: "piqueld.dev/v1alpha1".into(),
            kind: "Application".into(),
            metadata: saved.application.metadata,
            spec: saved.application.spec,
        }
    }
    fn blocked(self) -> bool {
        self.busy.get() || self.uncertain.get()
    }
    fn action_blocked(self) -> bool {
        self.blocked() || !self.dirty.get().is_empty() || self.saved.get().delete_intent
    }
    fn failure(self, error: &ClientError) {
        self.error.set(Some(client_error_message(error)));
        if matches!(error, ClientError::Transport { .. }) {
            self.uncertain.set(true);
            self.error.set(Some("The request outcome is unknown. Reload saved configuration and deployment history before another action.".into()));
        }
    }
    fn save(self, manifest: ApplicationManifest, on_saved: Callback<ApplicationView>) {
        if self.blocked() {
            return;
        }
        let validated = match manifest.clone().validate() {
            Ok(v) => v,
            Err(error) => {
                self.error.set(Some(error.to_string()));
                return;
            }
        };
        let saved = self.saved.get_untracked();
        let request = ApplyApplicationRequest {
            manifest,
            expected_generation: Some(saved.generation),
            expected_application_id: Some(saved.application.id.to_string()),
        };
        let client = match mutation_client() {
            Ok(client) => client,
            Err(error) => {
                self.error.set(Some(error));
                return;
            }
        };
        self.busy.set(true);
        self.error.set(None);
        spawn_local(async move {
            let mut result = client.apply_application(&request).await;
            if result.as_ref().is_err_and(transport_failure) {
                result = client.apply_application(&request).await;
            }
            match result {
                Ok(receipt) => {
                    let application = validated.normalize(saved.application.id);
                    let updated = ApplicationView {
                        spec_hash: application.spec_hash(),
                        application,
                        generation: receipt.generation,
                        ..saved
                    };
                    self.saved.set(updated.clone());
                    on_saved.call(updated);
                    self.notice
                        .set("Changes saved. Deploy when you are ready to apply them.".into());
                    self.dashboard.with_value(|d| (d.refresh)());
                }
                Err(error) => self.failure(&error),
            }
            self.busy.set(false);
        });
    }
}
fn editor() -> EditorContext {
    use_context().expect("application editor context")
}
fn transport_failure(error: &ClientError) -> bool {
    matches!(error, ClientError::Transport { .. })
}
fn mutation_client() -> Result<Client, String> {
    let id = window()
        .crypto()
        .map_err(|_| "Browser could not create a request identity.")?
        .random_uuid();
    Ok(Client::browser().with_request_id(id))
}
fn dirty_group<T: Clone + PartialEq + 'static>(
    key: String,
    draft: RwSignal<T>,
    baseline: RwSignal<T>,
) {
    let context = editor();
    let cleanup = key.clone();
    create_effect(move |_| {
        let dirty = draft.get() != baseline.get();
        context.dirty.update(|groups| {
            if dirty {
                groups.insert(key.clone());
            } else {
                groups.remove(&key);
            }
        });
    });
    on_cleanup(move || {
        context.dirty.update(|groups| {
            groups.remove(&cleanup);
        });
    });
}
fn text_input<T: Clone + 'static>(
    label: &'static str,
    state: RwSignal<T>,
    read: impl Fn(&T) -> String + Copy + 'static,
    write: impl Fn(&mut T, String) + Copy + 'static,
) -> View {
    view!{<label class="field"><span>{label}</span><input type="text" prop:value=move ||state.with(read) on:input=move |event|state.update(|v|write(v,event_target_value(&event)))/></label>}.into_view()
}

#[component]
pub(super) fn CreateApplication() -> impl IntoView {
    let opened = create_rw_signal(false);
    let name = create_rw_signal(String::new());
    let busy = create_rw_signal(false);
    let error = create_rw_signal(None::<String>);
    let navigate = use_navigate();
    let refresh = dashboard_context().refresh;
    let create = move |_| {
        let manifest = ApplicationManifest {
            api_version: "piqueld.dev/v1alpha1".into(),
            kind: "Application".into(),
            metadata: Metadata {
                name: name.get_untracked(),
            },
            spec: ApplicationSpec::default(),
        };
        if let Err(problem) = manifest.clone().validate() {
            error.set(Some(problem.to_string()));
            return;
        }
        let client = match mutation_client() {
            Ok(client) => client,
            Err(problem) => {
                error.set(Some(problem));
                return;
            }
        };
        let navigate = navigate.clone();
        let refresh = refresh.clone();
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let request = ApplyApplicationRequest {
                manifest,
                expected_generation: Some(0),
                expected_application_id: None,
            };
            let mut result = client.apply_application(&request).await;
            if result.as_ref().is_err_and(transport_failure) {
                result = client.apply_application(&request).await;
            }
            match result {
                Ok(saved) => {
                    refresh();
                    navigate(
                        &format!("/dashboard/applications/{}", saved.application_id),
                        NavigateOptions::default(),
                    );
                }
                Err(problem) => error.set(Some(client_error_message(&problem))),
            }
            busy.set(false);
        });
    };
    view! {<div class="create-app"><button class="primary" on:click=move |_|opened.update(|v|*v = !*v)>"+ Create application"</button>
        <Show when=move ||opened.get()><section class="settings-card"><h3>"Create application"</h3><p class="help">"Start with an empty application. Add services and deploy when ready."</p><fieldset disabled=move ||busy.get()>{text_input("Application name",name,String::clone,|v,s|*v=s)}<button class="primary" on:click=create.clone()>"Create application"</button></fieldset>{move ||error.get().map(|e|view!{<p class="form-error" role="alert">{e}</p>})}</section></Show>
    </div>}
}

#[component]
pub(super) fn ApplicationPage(id: String) -> impl IntoView {
    let initial = create_rw_signal(None::<ApplicationView>);
    let error = create_rw_signal(None::<String>);
    spawn_local(async move {
        match Client::browser().application(&id).await {
            Ok(app) => initial.set(Some(app)),
            Err(e) => error.set(Some(client_error_message(&e))),
        }
    });
    view! {<A href="/dashboard/applications" class="back-link">"← Applications"</A>{move ||error.get().map(|e|view!{<p class="form-error" role="alert">{e}</p>})}{move ||initial.get().map(|initial|view!{<ApplicationEditor initial=initial/>})}}
}

#[component]
fn ApplicationEditor(initial: ApplicationView) -> impl IntoView {
    let context = EditorContext {
        dashboard: store_value(dashboard_context()),
        latest_deployment: create_rw_signal(None),
        saved: create_rw_signal(initial),
        dirty: create_rw_signal(BTreeSet::new()),
        busy: create_rw_signal(false),
        uncertain: create_rw_signal(false),
        error: create_rw_signal(None),
        notice: create_rw_signal(String::new()),
        tab: create_rw_signal("Configuration"),
    };
    provide_context(context);
    guard_navigation(context.dirty);
    let signals = dashboard_context().signals;
    let client = dashboard_context().client;
    let retry = move |_| context.dashboard.with_value(|d| (d.refresh)());
    view! {
        <header class="application-heading"><div><p class="eyebrow">"APPLICATION"</p><h2>{move ||context.saved.with(|a|a.application.metadata.name.clone())}</h2><p class="help">{move ||format!("Configuration revision {}",context.saved.get().generation)}</p></div><DeploymentActions/></header>
        <p class="help">{move ||context.latest_deployment.get().map_or_else(||"Not deployed yet".to_string(),|deployment|if deployment.application.spec==context.saved.get().application.spec{"Saved configuration matches the latest deployment".into()}else{"Saved changes awaiting deployment".into()})}</p>
        <p class="save-status" role="status">{move ||if context.busy.get(){"Saving or submitting request…".into()}else if context.dirty.get().is_empty(){context.notice.get()}else{format!("{} settings group(s) have unsaved changes.",context.dirty.get().len())}}</p>
        {move ||context.error.get().map(|e|view!{<div class="form-error" role="alert"><p>{e}</p><p>"Your form edits have been kept. Reload to review the latest saved configuration."</p><button on:click=move |_|{let _=window().location().reload();}>"Reload saved configuration"</button></div>})}
        {move ||signals.detail.get().filter(|d|d.application.generation>context.saved.get().generation).map(|_|view!{<p class="conflict-notice">"Configuration changed elsewhere. Your edits are preserved; reload to review the latest version."</p>})}
        <nav class="tabs" aria-label="Application sections">{["Configuration","Deployments","Runtime"].into_iter().map(|tab|view!{<button class:active=move ||context.tab.get()==tab aria-current=move ||if context.tab.get()==tab{"page"}else{"false"} on:click=move |_|context.tab.set(tab)>{tab}</button>}).collect_view()}</nav>
        <div hidden=move ||context.tab.get()!="Configuration"><RepositorySettings/>{move ||context.saved.get().application.spec.manifest.is_some().then(||view!{<p class="help">"Application settings are managed by the repository. Edit its manifest, or disconnect the repository to edit here."</p>})}<fieldset disabled=move ||context.saved.get().application.spec.manifest.is_some()><MetadataSettings/><VolumeSettings/><ServiceSettings/><NewService/></fieldset><DeleteApplication/></div>
        <div hidden=move ||context.tab.get()!="Deployments"><DeploymentHistory/></div>
        <div hidden=move ||context.tab.get()!="Runtime"><Show when=move ||signals.detail.get().is_none()><p role="status">{move ||signals.detail_error.get().map_or_else(||"Loading runtime detail…".into(),|error|format!("Detail unavailable: {error}"))}</p><button disabled=move ||signals.detail_loading.get() on:click=retry>"Retry runtime detail"</button></Show>{move ||signals.detail.get().map(|detail|detail_view(&detail,signals,client.clone()))}</div>
    }
}

#[derive(Clone)]
struct GuardedLocation {
    dirty: RwSignal<BTreeSet<String>>,
    url: String,
    state: leptos::wasm_bindgen::JsValue,
}

#[derive(Clone, Copy)]
pub(super) struct HistoryGuard(RwSignal<Option<GuardedLocation>>);
impl HistoryGuard {
    /// Window-targeted history events must be intercepted before the router's listener.
    pub(super) fn install() {
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

fn guard_navigation(dirty: RwSignal<BTreeSet<String>>) {
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
            .and_then(|element| element.closest("a[href]").ok().flatten());
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

#[component]
fn RepositorySettings() -> impl IntoView {
    let context = editor();
    let backing = context.saved.get_untracked().application.spec.manifest;
    let draft = create_rw_signal((
        backing.is_some(),
        backing.map(|b| *b).unwrap_or(RepositoryManifest {
            repository: GitRepository {
                url: String::new(),
                branch: "main".into(),
                commit: None,
            },
            path: "infra/piqueld/app.toml".into(),
        }),
    ));
    let baseline = create_rw_signal(draft.get_untracked());
    dirty_group("repository".into(), draft, baseline);
    let other_edits = move || {
        context
            .dirty
            .with(|groups| groups.iter().any(|group| group != "repository"))
    };
    let save = move |_| {
        if other_edits() {
            return;
        }
        let value = draft.get_untracked();
        let mut manifest = context.manifest();
        manifest.spec.manifest = value.0.then(|| Box::new(value.1));
        context.save(
            manifest,
            Callback::new(move |_| baseline.set(draft.get_untracked())),
        );
    };
    view! {<section class="settings-card"><h3>"Repository manifest"</h3><Show when=other_edits><p class="help">"Save or discard edits in other settings before changing the repository connection."</p></Show><fieldset disabled=move ||context.blocked()><label><input type="checkbox" prop:checked=move ||draft.get().0 on:change=move |ev|draft.update(|v|v.0=event_target_checked(&ev))/>"Load configuration from Git on Deploy"</label><div hidden=move ||!draft.get().0 class="form-grid">{text_input("Repository",draft,|v|v.1.repository.url.clone(),|v,s|v.1.repository.url=s)}{text_input("Branch",draft,|v|v.1.repository.branch.clone(),|v,s|v.1.repository.branch=s)}{text_input("Commit (optional)",draft,|v|v.1.repository.commit.clone().unwrap_or_default(),|v,s|v.1.repository.commit=(!s.is_empty()).then_some(s))}{text_input("Manifest path",draft,|v|v.1.path.clone(),|v,s|v.1.path=s)}</div><div class="form-actions"><button class="primary" disabled=move ||draft.get()==baseline.get() || other_edits() on:click=save>"Save Changes"</button><button on:click=move |_|draft.set(baseline.get_untracked())>"Discard edits"</button></div></fieldset></section>}
}

#[component]
fn MetadataSettings() -> impl IntoView {
    let context = editor();
    let name = create_rw_signal(
        context
            .saved
            .with_untracked(|a| a.application.metadata.name.clone()),
    );
    let baseline = create_rw_signal(name.get_untracked());
    dirty_group("name".into(), name, baseline);
    let save = move |_| {
        let Ok(client) = mutation_client() else {
            context
                .error
                .set(Some("Unable to create request identity.".into()));
            return;
        };
        let app = context.saved.get_untracked();
        let request = piqueld_client::RenameApplicationRequest {
            name: name.get_untracked(),
            expected_generation: Some(app.generation),
        };
        context.busy.set(true);
        context.error.set(None);
        spawn_local(async move {
            let result = client
                .rename_application(app.application.id.as_str(), &request)
                .await;
            match result {
                Ok(renamed) => {
                    context.saved.update(|app| {
                        app.application.metadata.name.clone_from(&renamed.name);
                        app.generation = renamed.generation;
                    });
                    baseline.set(renamed.name);
                    context.notice.set("Application name saved.".into());
                    context.dashboard.with_value(|d| (d.refresh)());
                }
                Err(error) => context.failure(&error),
            }
            context.busy.set(false);
        });
    };
    view! {<section class="settings-card"><h3>"Application name"</h3><fieldset disabled=move ||context.blocked()>{text_input("Name",name,String::clone,|v,s|*v=s)}<div class="form-actions"><button class="primary" disabled=move ||name.get()==baseline.get() on:click=save>"Save Changes"</button><button on:click=move |_|name.set(baseline.get_untracked())>"Discard edits"</button></div></fieldset><p class="help">"Renaming preserves resources and requires any running deployment to finish."</p></section>}
}

#[component]
fn VolumeSettings() -> impl IntoView {
    let context = editor();
    let volumes = create_rw_signal(context.saved.with_untracked(|a| {
        a.application
            .spec
            .volumes
            .iter()
            .map(|v| v.name.clone())
            .collect::<Vec<_>>()
    }));
    let baseline = create_rw_signal(volumes.get_untracked());
    dirty_group("volumes".into(), volumes, baseline);
    let save = move |_| {
        let mut manifest = context.manifest();
        manifest.spec.volumes = volumes
            .get_untracked()
            .into_iter()
            .map(|name| Volume { name })
            .collect();
        context.save(
            manifest,
            Callback::new(move |app: ApplicationView| {
                let names = app
                    .application
                    .spec
                    .volumes
                    .into_iter()
                    .map(|v| v.name)
                    .collect();
                volumes.set(names);
                baseline.set(volumes.get_untracked());
            }),
        );
    };
    view! {<section class="settings-card"><h3>"Named volumes"</h3><p class="help">"Declare storage here, then mount it in a service. Removing a declaration retains its Docker data."</p><fieldset disabled=move ||context.blocked()><For each={move ||(0..volumes.with(Vec::len)).collect::<Vec<_>>()} key=|i|*i children=move |i|view!{<div class="field-row">{text_input("Volume name",volumes,move |v|v.get(i).cloned().unwrap_or_default(),move |v,s|{if let Some(value)=v.get_mut(i){*value=s;}})}<button on:click=move |_|volumes.update(|v|{v.remove(i);})>"Remove"</button></div>}/><button on:click=move |_|volumes.update(|v|v.push(String::new()))>"+ Add volume"</button><div class="form-actions"><button class="primary" disabled=move ||volumes.get()==baseline.get() on:click=save>"Save Changes"</button><button on:click=move |_|volumes.set(baseline.get_untracked())>"Discard edits"</button></div></fieldset></section>}
}

#[component]
fn ServiceSettings() -> impl IntoView {
    let context = editor();
    view! {<For each={move ||context.saved.with(|a|a.application.spec.services.iter().map(|s|s.name.clone()).collect::<Vec<_>>())} key=|name|name.clone() children=move |name|view!{<ServicePanel name=name/>}/>}
}
#[component]
fn ServicePanel(name: String) -> impl IntoView {
    let context = editor();
    let remove_name = name.clone();
    let title = name.clone();
    let remove = move |_| {
        if !window().confirm_with_message("Remove this service from saved configuration? Its running containers remain until Deploy.").unwrap_or(false){return;}
        let mut manifest = context.manifest();
        manifest.spec.services.retain(|s| s.name != remove_name);
        context.save(manifest, Callback::new(|_| {}));
    };
    let groups = Section::ALL
        .into_iter()
        .map(move |section| view! {<ServiceGroup name=name.clone() section=section/>})
        .collect_view();
    view! {<section class="service-panel"><header><div><p class="eyebrow">"SERVICE"</p><h3>{title}</h3></div><button class="danger" disabled=move ||context.action_blocked() on:click=remove>"Remove service"</button></header>{groups}</section>}
}
#[component]
fn ServiceGroup(name: String, section: Section) -> impl IntoView {
    let context = editor();
    let service = context
        .saved
        .with_untracked(|a| {
            a.application
                .spec
                .services
                .iter()
                .find(|s| s.name == name)
                .cloned()
        })
        .expect("listed service");
    let draft = create_rw_signal(ServiceForm::from(&service));
    let baseline = create_rw_signal(draft.get_untracked());
    dirty_group(format!("{name}:{}", section.title()), draft, baseline);
    let save = move |_| {
        let mut manifest = context.manifest();
        let Some(service) = manifest.spec.services.iter_mut().find(|s| s.name == name) else {
            context
                .error
                .set(Some("Service was removed. Reload configuration.".into()));
            return;
        };
        if let Err(error) = draft.get_untracked().patch(section, service) {
            context.error.set(Some(error));
            return;
        }
        let sent = draft.get_untracked();
        context.save(manifest, Callback::new(move |_| baseline.set(sent.clone())));
    };
    view! {<details class="settings-group" open=section==Section::General><summary>{section.title()}<span class="dirty-label">{move ||if draft.get()==baseline.get(){""}else{"Unsaved"}}</span></summary><fieldset disabled=move ||context.blocked()>{service_fields(section,draft)}<div class="form-actions"><button class="primary" disabled=move ||draft.get()==baseline.get() on:click=save>"Save Changes"</button><button on:click=move |_|draft.set(baseline.get_untracked())>"Discard edits"</button></div></fieldset></details>}
}

fn service_fields(section: Section, form: RwSignal<ServiceForm>) -> View {
    match section {
        Section::General=>view!{<div class="form-grid"><label>"Source"<select prop:value=move ||form.get().source_kind on:change=move |ev|form.update(|v|v.source_kind=event_target_value(&ev))><option value="image">"Container image"</option><option value="git">"Git / Dockerfile"</option></select></label>{move ||if form.get().source_kind=="git" {view!{<>{text_input("Repository",form,|v|v.repository.clone(),|v,s|v.repository=s)}{text_input("Branch",form,|v|v.branch.clone(),|v,s|v.branch=s)}{text_input("Commit (optional)",form,|v|v.commit.clone(),|v,s|v.commit=s)}{text_input("Dockerfile path",form,|v|v.dockerfile.clone(),|v,s|v.dockerfile=s)}{text_input("Build context",form,|v|v.context.clone(),|v,s|v.context=s)}</>}.into_view()}else{text_input("Container image",form,|v|v.image.clone(),|v,s|v.image=s)}}{text_input("Replicas",form,|v|v.replicas.clone(),|v,s|v.replicas=s)}</div>}.into_view(),
        Section::Environment=>environment_fields(form),
        Section::Process=>view!{<p class="help">"Each row is one element. Spaces are preserved; no shell parsing is applied."</p>{string_rows("Command element",form,|v|&v.command,|v|&mut v.command)}{string_rows("Argument",form,|v|&v.arguments,|v|&mut v.arguments)}}.into_view(),
        Section::Storage=>mount_fields(form),
        Section::Health=>health_fields(form),
        Section::Resources=>view!{<p class="help">"Leave a limit blank to use the runtime default."</p><div class="form-grid">{text_input("CPU (millicores)",form,|v|v.cpu.clone(),|v,s|v.cpu=s)}{text_input("Memory (bytes)",form,|v|v.memory.clone(),|v,s|v.memory=s)}</div>}.into_view(),
    }
}

fn string_rows(
    label: &'static str,
    form: RwSignal<ServiceForm>,
    read: fn(&ServiceForm) -> &Vec<String>,
    write: fn(&mut ServiceForm) -> &mut Vec<String>,
) -> View {
    view!{<div class="collection"><For each={move ||(0..form.with(|v|read(v).len())).collect::<Vec<_>>()} key=|i|*i children=move |i|view!{<div class="field-row">{text_input(label,form,move |v|read(v).get(i).cloned().unwrap_or_default(),move |v,s|{if let Some(value)=write(v).get_mut(i){*value=s;}})}<button on:click=move |_|form.update(|v|{write(v).remove(i);})>"Remove"</button></div>}/><button on:click=move |_|form.update(|v|write(v).push(String::new()))>{format!("+ Add {}",label.to_lowercase())}</button></div>}.into_view()
}
fn environment_fields(form: RwSignal<ServiceForm>) -> View {
    view!{<div class="collection"><For each={move ||(0..form.with(|v|v.environment.len())).collect::<Vec<_>>()} key=|i|*i children=move |i|view!{<div class="field-row">{text_input("Key",form,move |v|v.environment.get(i).map_or_else(String::new,|v|v.0.clone()),move |v,s|{if let Some(value)=v.environment.get_mut(i){value.0=s;}})}{text_input("Value",form,move |v|v.environment.get(i).map_or_else(String::new,|v|v.1.clone()),move |v,s|{if let Some(value)=v.environment.get_mut(i){value.1=s;}})}<button on:click=move |_|form.update(|v|{v.environment.remove(i);})>"Remove"</button></div>}/><button on:click=move |_|form.update(|v|v.environment.push((String::new(),String::new())))>"+ Add variable"</button></div>}.into_view()
}
fn mount_fields(form: RwSignal<ServiceForm>) -> View {
    view!{<p class="help">"Use a declared volume name and an absolute container path."</p><For each={move ||(0..form.with(|v|v.mounts.len())).collect::<Vec<_>>()} key=|i|*i children=move |i|view!{<div class="field-row">{text_input("Volume",form,move |v|v.mounts.get(i).map_or_else(String::new,|v|v.volume.clone()),move |v,s|{if let Some(value)=v.mounts.get_mut(i){value.volume=s;}})}{text_input("Container path",form,move |v|v.mounts.get(i).map_or_else(String::new,|v|v.target.clone()),move |v,s|{if let Some(value)=v.mounts.get_mut(i){value.target=s;}})}<label class="checkbox"><input type="checkbox" prop:checked=move ||form.with(|v|v.mounts.get(i).is_some_and(|v|v.read_only)) on:change=move |event|form.update(|v|{if let Some(value)=v.mounts.get_mut(i){value.read_only=event_target_checked(&event);}})/>"Read only"</label><button on:click=move |_|form.update(|v|{v.mounts.remove(i);})>"Remove"</button></div>}/><button on:click=move |_|form.update(|v|v.mounts.push(Mount{volume:String::new(),target:String::new(),read_only:false}))>"+ Add mount"</button>}.into_view()
}
fn health_fields(form: RwSignal<ServiceForm>) -> View {
    view!{<label class="field"><span>"Check type"</span><select prop:value=move ||form.with(|v|v.health_kind.clone()) on:change=move |event|form.update(|v|v.health_kind=event_target_value(&event))><option value="none">"None"</option><option value="http">"HTTP"</option><option value="command">"Command"</option></select></label><Show when=move ||form.with(|v|v.health_kind!="none")><div class="form-grid">{text_input("Interval (seconds)",form,|v|v.interval.clone(),|v,s|v.interval=s)}{text_input("Timeout (seconds)",form,|v|v.timeout.clone(),|v,s|v.timeout=s)}</div></Show><Show when=move ||form.with(|v|v.health_kind=="http")><div class="form-grid">{text_input("Port",form,|v|v.port.clone(),|v,s|v.port=s)}{text_input("Path",form,|v|v.path.clone(),|v,s|v.path=s)}</div></Show><Show when=move ||form.with(|v|v.health_kind=="command")>{string_rows("Health command element",form,|v|&v.health_command,|v|&mut v.health_command)}</Show>}.into_view()
}

#[component]
fn NewService() -> impl IntoView {
    let context = editor();
    let fields = create_rw_signal((String::new(), String::new()));
    let baseline = create_rw_signal(fields.get_untracked());
    dirty_group("new-service".into(), fields, baseline);
    let save = move |_| {
        let (name, image) = fields.get_untracked();
        let mut manifest = context.manifest();
        manifest.spec.services.push(Service {
            name,
            source: Source::Image { image },
            replicas: 1,
            environment: BTreeMap::new(),
            command: Vec::new(),
            arguments: Vec::new(),
            mounts: Vec::new(),
            healthcheck: None,
            resources: None,
        });
        context.save(
            manifest,
            Callback::new(move |_| fields.set((String::new(), String::new()))),
        );
    };
    view! {<section class="settings-card"><h3>"Add service"</h3><p class="help">"Save a container image service, then configure its settings below. It starts on the next deployment."</p><fieldset disabled=move ||context.blocked()><div class="form-grid">{text_input("Service name",fields,|v|v.0.clone(),|v,s|v.0=s)}{text_input("Image",fields,|v|v.1.clone(),|v,s|v.1=s)}</div><button class="primary" on:click=save>"Save Changes"</button></fieldset></section>}
}

#[component]
fn DeleteApplication() -> impl IntoView {
    let context = editor();
    let navigate = use_navigate();
    let refresh = dashboard_context().refresh;
    let delete = move |_| {
        if !window().confirm_with_message("Delete this application, its services, and all deployment history? Docker volume data will be retained.").unwrap_or(false){return;}
        let Ok(client) = mutation_client() else {
            return;
        };
        let saved = context.saved.get_untracked();
        let navigate = navigate.clone();
        let refresh = refresh.clone();
        context.busy.set(true);
        spawn_local(async move {
            match client
                .delete_application_with_generation(
                    saved.application.id.as_str(),
                    Some(saved.generation),
                )
                .await
            {
                Ok(_) => {
                    refresh();
                    navigate("/dashboard/applications", NavigateOptions::default());
                }
                Err(error) => context.failure(&error),
            }
            context.busy.set(false);
        });
    };
    view! {<section class="settings-card danger-zone"><h3>"Delete application"</h3><p class="help">"Removes services and all application history. Docker volumes and their data are retained."</p><button class="danger" disabled=move ||context.action_blocked() on:click=delete>"Delete application"</button></section>}
}

#[component]
fn DeploymentActions() -> impl IntoView {
    let context = editor();
    let preview = create_rw_signal(None::<piqueld_client::PlanView>);
    let deploy = move |_| {
        let Ok(client) = mutation_client() else {
            return;
        };
        let app = context.saved.get_untracked();
        context.busy.set(true);
        context.error.set(None);
        spawn_local(async move {
            let mut result = client
                .deploy_application(app.application.id.as_str(), app.generation)
                .await;
            if result.as_ref().is_err_and(transport_failure) {
                result = client
                    .deploy_application(app.application.id.as_str(), app.generation)
                    .await;
            }
            match result {
                Ok(_) => {
                    context.tab.set("Deployments");
                    context
                        .notice
                        .set("Deployment accepted. Follow its progress below.".into());
                    context.dashboard.with_value(|d| (d.refresh)());
                }
                Err(error) => context.failure(&error),
            }
            context.busy.set(false);
        });
    };
    let inspect = move |_| {
        let app = context.saved.get_untracked();
        let request = ApplyApplicationRequest {
            manifest: context.manifest(),
            expected_generation: Some(app.generation),
            expected_application_id: Some(app.application.id.to_string()),
        };
        context.busy.set(true);
        context.error.set(None);
        spawn_local(async move {
            match Client::browser().plan_application(&request).await {
                Ok(plan) => preview.set(Some(plan)),
                Err(error) => context.error.set(Some(client_error_message(&error))),
            }
            context.busy.set(false);
        });
    };
    create_effect(move |_| {
        context.saved.track();
        preview.set(None);
    });
    view! {<div class="deployment-actions"><button disabled=move ||context.action_blocked() on:click=inspect>"Preview"</button><button class="primary" disabled=move ||context.action_blocked() on:click=deploy>"Deploy"</button>{move ||preview.get().map(|plan|view!{<div class="preview-panel" role="region" aria-label="Deployment preview"><header><h3>"Deployment preview"</h3><button on:click=move |_|preview.set(None)>"Close preview"</button></header><p class="help">"Image tags and runtime state may change before deployment."</p><ul>{plan.changes.into_iter().map(|change|view!{<li><strong>{change.field}</strong>" · "{change.before.unwrap_or_else(||"absent".into())}" → "{change.after.unwrap_or_else(||"absent".into())}</li>}).collect_view()}</ul><ul>{plan.plan.actions.into_iter().map(|action|view!{<li>{format!("{} · {}",action.kind.name(),action.kind.resource_name())}</li>}).collect_view()}</ul>{plan.plan.diagnostics.into_iter().map(|d|view!{<p class="form-error">{format!("{}: {}",d.resource,d.message)}</p>}).collect_view()}</div>})}</div>}
}

#[component]
fn DeploymentHistory() -> impl IntoView {
    let context = editor();
    let history = create_rw_signal(Vec::<DeploymentView>::new());
    let paginated = create_rw_signal(false);
    let cursor = create_rw_signal(None::<String>);
    let error = create_rw_signal(None::<String>);
    let loading = create_rw_signal(false);
    let id = context
        .saved
        .with_untracked(|a| a.application.id.to_string());
    let active = Rc::new(Cell::new(true));
    let live = active.clone();
    on_cleanup(move || live.set(false));
    let poll_id = id.clone();
    spawn_local(async move {
        while active.get() {
            if !loading.get_untracked() && !super::document_hidden() {
                match Client::browser().deployments(&poll_id, None).await {
                    Ok(page) => {
                        if !active.get() {
                            break;
                        }
                        context.latest_deployment.set(page.items.first().cloned());
                        merge_history(history, cursor, paginated, page);
                        error.set(None);
                    }
                    Err(e) => {
                        if active.get() {
                            error.set(Some(client_error_message(&e)));
                        }
                    }
                }
            }
            gloo_timers::future::TimeoutFuture::new(2000).await;
        }
    });
    let more = move |_| {
        let id = id.clone();
        let next = cursor.get_untracked();
        loading.set(true);
        spawn_local(async move {
            match Client::browser().deployments(&id, next.as_deref()).await {
                Ok(page) => {
                    error.set(None);
                    paginated.set(true);
                    cursor.set(page.next_cursor);
                    history.update(|items| {
                        for item in page.items {
                            if !items
                                .iter()
                                .any(|old| old.operation.id == item.operation.id)
                            {
                                items.push(item);
                            }
                        }
                    });
                }
                Err(e) => error.set(Some(client_error_message(&e))),
            }
            loading.set(false);
        });
    };
    view! {<section class="deployment-history"><h3>"Deployments"</h3><p class="help">"Every Deploy captures saved configuration and refreshes images. New deployments supersede earlier work."</p>{move ||error.get().map(|e|view!{<p class="form-error" role="alert">{e}</p>})}<Show when=move ||history.with(Vec::is_empty)><p class="empty-state">"Not deployed yet. Save your configuration, then click Deploy."</p></Show><For each=move ||history.get() key=|d|d.operation.id.clone() children=move |initial| {let deployment=Signal::derive(move ||history.with(|items|items.iter().find(|d|d.operation.id==initial.operation.id).cloned().unwrap_or_else(||initial.clone())));view!{<DeploymentCard deployment=deployment/>}}/><Show when=move ||cursor.get().is_some()><button disabled=move ||loading.get() on:click=more.clone()>"Load older deployments"</button></Show></section>}
}
fn merge_history(
    history: RwSignal<Vec<DeploymentView>>,
    cursor: RwSignal<Option<String>>,
    paginated: RwSignal<bool>,
    page: Page<DeploymentView>,
) {
    history.update(|items| {
        if !paginated.get_untracked() {
            *items = page.items;
            cursor.set(page.next_cursor);
        } else {
            let has_current = page.items.iter().any(|item| item.current_target);
            let has_successful = page.items.iter().any(|item| item.last_successful);
            for item in items.iter_mut() {
                if has_current {
                    item.current_target = false;
                }
                if has_successful {
                    item.last_successful = false;
                }
            }
            for item in page.items {
                if let Some(old) = items
                    .iter_mut()
                    .find(|v| v.operation.id == item.operation.id)
                {
                    *old = item;
                } else {
                    items.insert(0, item);
                }
            }
            items.sort_by(|a, b| b.operation.id.cmp(&a.operation.id));
        }
    });
}
#[component]
fn DeploymentCard(deployment: Signal<DeploymentView>) -> impl IntoView {
    let initial = deployment.get_untracked();
    let op = initial.operation;
    let attempt_open = create_rw_signal(false);
    let errors = create_rw_signal(Vec::<piqueld_client::Operation>::new());
    let cursor = create_rw_signal(None::<String>);
    let failure = create_rw_signal(None::<String>);
    let app_id = op.application_id.to_string();
    let deployment_id = op.id.clone();
    let loading = create_rw_signal(false);
    let load = move |_| {
        let app_id = app_id.clone();
        let deployment_id = deployment_id.clone();
        let next = cursor.get_untracked();
        attempt_open.set(true);
        loading.set(true);
        spawn_local(async move {
            match Client::browser()
                .deployment_attempts(&app_id, &deployment_id, next.as_deref())
                .await
            {
                Ok(page) => {
                    failure.set(None);
                    if next.is_none() {
                        errors.set(page.items);
                    } else {
                        errors.update(|v| v.extend(page.items));
                    }
                    cursor.set(page.next_cursor);
                }
                Err(e) => failure.set(Some(client_error_message(&e))),
            }
            loading.set(false);
        });
    };
    view! {<article class="deployment-card">{move ||{let d=deployment.get();let op=d.operation;view!{<header><div><span class="deployment-state" data-state=op.state.as_str()>{op.state.as_str()}</span><strong>{format!("Configuration revision {}",op.generation)}</strong></div><div>{d.current_target.then(||view!{<span class="tag">"Current target"</span>})}{d.last_successful.then(||view!{<span class="tag">"Last successful"</span>})}</div></header><p class="help"><code>{op.id}</code></p><p>{format!("Attempt {} · {}",op.attempt,op.phase.unwrap_or_else(||"waiting".into()))}</p>{op.resource.map(|resource|view!{<p>{resource}</p>})}{op.error_message.map(|error|view!{<p class="form-error">{error}</p>})}}}}<details><summary>"Configuration snapshot"</summary>{move ||deployment.get().application.spec.services.into_iter().map(|service|{let source=match service.source {Source::Image{image}=>image,Source::Git{repository,build:piqueld_client::Build::Docker{dockerfile,context}}=>format!("Git: {} · {} · Dockerfile: {} · context: {}",repository.url,repository.commit.as_deref().unwrap_or(&repository.branch),dockerfile,context)};view!{<div class="snapshot-service"><h4>{service.name}</h4><dl><dt>"Source"</dt><dd>{source}</dd><dt>"Replicas"</dt><dd>{service.replicas}</dd><dt>"Environment"</dt><dd>{service.environment.into_iter().map(|(k,v)|view!{<p><code>{k}</code>" = "{v}</p>}).collect_view()}</dd><dt>"Command"</dt><dd>{service.command.join(" · ")}</dd><dt>"Arguments"</dt><dd>{service.arguments.join(" · ")}</dd><dt>"Mounts"</dt><dd>{service.mounts.into_iter().map(|m|view!{<p>{format!("{} → {}{}",m.volume,m.target,if m.read_only{" (read only)"}else{""})}</p>}).collect_view()}</dd><dt>"Health check"</dt><dd>{service.healthcheck.map_or_else(||"None".into(),|check|match check{piqueld_client::HealthCheck::Http{port,path,interval_seconds,timeout_seconds}=>format!("HTTP :{port}{path} · every {interval_seconds}s · timeout {timeout_seconds}s"),piqueld_client::HealthCheck::Command{command,interval_seconds,timeout_seconds}=>format!("{} · every {interval_seconds}s · timeout {timeout_seconds}s",command.join(" · "))})}</dd><dt>"Resource limits"</dt><dd>{service.resources.map_or_else(||"Runtime defaults".into(),|r|format!("CPU: {} · Memory: {}",r.cpu_millis.map_or_else(||"default".into(),|v|format!("{v} millicores")),r.memory_bytes.map_or_else(||"default".into(),|v|format!("{v} bytes"))))}</dd></dl></div>}}).collect_view()}<p>"Volumes: "{move ||deployment.get().application.spec.volumes.into_iter().map(|v|v.name).collect::<Vec<_>>().join(", ")}</p></details><button disabled=move ||loading.get() on:click=load>{move ||if !attempt_open.get(){"View attempt history"}else if cursor.get().is_some(){"Older attempts"}else{"Refresh attempts"}}</button>{move ||failure.get().map(|e|view!{<p class="form-error">{e}</p>})}{move ||errors.get().into_iter().map(|attempt|view!{<p class="attempt">{format!("Attempt {} · {} · {} {}",attempt.attempt,attempt.state,attempt.error_code.unwrap_or_default(),attempt.error_message.unwrap_or_default())}</p>}).collect_view()}</article>}
}

#[component]
pub(super) fn HostPage() -> impl IntoView {
    let settings = create_rw_signal(None);
    let error = create_rw_signal(None::<String>);
    spawn_local(async move {
        match Client::browser().system_configuration().await {
            Ok(config) => settings.set(Some(config)),
            Err(e) => error.set(Some(client_error_message(&e))),
        }
    });
    view! {<header class="application-heading"><div><p class="eyebrow">"HOST"</p><h2>"Host settings"</h2><p class="help">"Effective settings loaded by this daemon. Read only; changes require editing the host configuration file and restarting."</p></div></header>{move ||error.get().map(|e|view!{<p class="form-error">{e}</p>})}{move ||settings.get().map(|config|config.groups.into_iter().map(|(group,values)|view!{<section class="settings-card"><h3>{group}</h3><dl class="host-settings">{values.into_iter().map(|(key,value)|view!{<dt>{key}</dt><dd>{value}</dd>}).collect_view()}</dl></section>}).collect_view())}}
}
