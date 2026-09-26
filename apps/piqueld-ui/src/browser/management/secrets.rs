//! Write-only secret values and saved file references.
use super::{client_error_message, dirty_group, editor, text_input};
use leptos::{
    Callable, Callback, CollectView, For, IntoView, Show, SignalGet, SignalGetUntracked, SignalSet,
    SignalUpdate, SignalWithUntracked, component, create_rw_signal, event_target_value,
    spawn_local, store_value, view, window,
};
use piqueld_client::{
    ApplicationView, Client,
    edit::{ApplicationEdit, ServiceEdit},
};

#[component]
pub(super) fn ApplicationSecrets() -> impl IntoView {
    let context = editor();
    let metadata = create_rw_signal(Vec::<piqueld_client::SecretMetadata>::new());
    let ready = create_rw_signal(false);
    let loading = create_rw_signal(false);
    let error = create_rw_signal(None::<String>);
    let notice = create_rw_signal(String::new());
    let name = create_rw_signal(String::new());
    let value = create_rw_signal(String::new());
    let empty = create_rw_signal(String::new());
    dirty_group("secret-value".into(), value, empty);
    let id = store_value(
        context
            .saved
            .with_untracked(|a| a.application.id().to_string()),
    );
    let reload = Callback::new(move |()| {
        if context.blocked() || loading.get_untracked() {
            return;
        }
        loading.set(true);
        ready.set(false);
        let id = id.get_value();
        spawn_local(async move {
            match Client::browser().secrets(&id).await {
                Ok(items) => {
                    metadata.set(items);
                    ready.set(true);
                    error.set(None);
                }
                Err(e) => error.set(Some(client_error_message(&e))),
            }
            loading.set(false);
        });
    });
    reload.call(());
    let write = move |_| {
        if context.blocked() || !ready.get_untracked() {
            return;
        }
        let name = name.get_untracked();
        let generation = metadata.with_untracked(|items| {
            items
                .iter()
                .find(|s| s.name == name)
                .map_or(0, |s| s.generation)
        });
        if generation>0 && !window().confirm_with_message(&format!("Replace {name}? Running deployments keep their current version. A later Deploy will use the replacement.")).unwrap_or(false){return;}
        let bytes = value.get_untracked().into_bytes();
        value.set(String::new());
        let id = id.get_value();
        context.busy.set(true);
        notice.set(String::new());
        error.set(None);
        spawn_local(async move {
            match Client::browser()
                .put_secret(&id, &name, generation, bytes)
                .await
            {
                Ok(secret) => {
                    metadata.update(|items| {
                        items.retain(|s| s.name != secret.name);
                        items.push(secret);
                        items.sort_by(|a, b| a.name.cmp(&b.name));
                    });
                    notice
                        .set("Secret saved. Deploy the application to use its new version.".into());
                }
                Err(e) => {
                    ready.set(false);
                    error.set(Some(format!("{} Refresh metadata before another secret change. The submitted value has been cleared.", client_error_message(&e))));
                }
            }
            context.busy.set(false);
        });
    };
    let remove = Callback::new(move |secret: piqueld_client::SecretMetadata| {
        if context.blocked() || !ready.get_untracked() {
            return;
        }
        if !window()
            .confirm_with_message(&format!(
                "Delete {} and its retained versions? Referenced secrets cannot be deleted.",
                secret.name
            ))
            .unwrap_or(false)
        {
            return;
        }
        let id = id.get_value();
        context.busy.set(true);
        notice.set(String::new());
        error.set(None);
        spawn_local(async move {
            match Client::browser()
                .delete_secret(&id, &secret.name, secret.generation)
                .await
            {
                Ok(()) => {
                    metadata.update(|items| items.retain(|s| s.name != secret.name));
                    notice.set("Secret deleted.".into());
                    error.set(None);
                }
                Err(e) => {
                    ready.set(false);
                    error.set(Some(format!(
                        "{} Refresh metadata, then retry deletion if cleanup is pending.",
                        client_error_message(&e)
                    )));
                }
            }
            context.busy.set(false);
        });
    });
    view! {<section class="settings-card"><h3>"Application secrets"</h3>
        <p class="help">"Values are write-only. File references below are saved configuration; deploy after saving to use them."</p>
        <button disabled=move ||context.blocked() || loading.get() on:click=move |_|reload.call(())>"Refresh metadata"</button>
        {move ||error.get().map(|e|view!{<p class="form-error" role="alert">{e}</p>})}<p role="status">{move ||notice.get()}</p>
        {move ||metadata.get().into_iter().map(|secret|{let selected=secret.name.clone();view!{
            <div class="form-actions"><strong>{secret.name.clone()}</strong><span>{format!("Version {}{}",secret.generation,if secret.deleting { " · deletion pending" } else { "" })}</span>
                <button disabled=move ||context.blocked() || !ready.get() || secret.deleting on:click=move |_|name.set(selected.clone())>"Replace value"</button>
                <button disabled=move ||context.blocked() || !ready.get() on:click=move |_|remove.call(secret.clone())>"Delete"</button>
            </div>
        }}).collect_view()}
        <fieldset disabled=move ||context.blocked() || !ready.get()>
            {text_input("Secret name",name,String::clone,|v,s|*v=s)}
            <label>"New value"<textarea autocomplete="off" spellcheck="false" rows="3" prop:value=move ||value.get() on:input=move |e|value.set(event_target_value(&e))></textarea></label>
            <p class="help">"The value is cleared when submitted and cannot be read back. Use the CLI for binary files."</p>
            <button class="primary" disabled=move ||name.get().is_empty() || value.get().is_empty() || metadata.get().iter().any(|s|s.name==name.get() && s.deleting) on:click=write>"Save secret"</button>
        </fieldset>
    </section>
    <Show when=move ||context.saved.get().application.spec().manifest.is_some()><p class="help">"Edit secret file references in the repository manifest."</p></Show>
    <fieldset disabled=move ||context.blocked() || context.saved.get().application.spec().manifest.is_some()>
        <For each={move ||context.saved.get().application.spec().services.iter().map(|s|s.name.to_string()).collect::<Vec<_>>()} key=|name|name.clone() children=move |name|view!{<SecretFiles service_name=name/>}/>
    </fieldset>}
}

#[component]
fn SecretFiles(service_name: String) -> impl IntoView {
    let context = editor();
    let mounts = create_rw_signal(context.saved.with_untracked(|a| {
        a.application
            .spec()
            .services
            .iter()
            .find(|s| s.name.as_str() == service_name)
            .map(|s| s.secrets.clone())
            .unwrap_or_default()
    }));
    let baseline = create_rw_signal(mounts.get_untracked());
    dirty_group(format!("secret-files:{service_name}"), mounts, baseline);
    let service = store_value(service_name.clone());
    let save = move |_| {
        context.save(
            ApplicationEdit::Service {
                name: service.get_value(),
                edit: ServiceEdit::Secrets(mounts.get_untracked()),
            },
            Callback::new(move |saved: ApplicationView| {
                if let Some(target) = saved
                    .application
                    .spec()
                    .services
                    .iter()
                    .find(|s| s.name.as_str() == service.get_value())
                {
                    mounts.set(target.secrets.clone());
                    baseline.set(target.secrets.clone());
                }
            }),
        );
    };
    view! {<section class="settings-card"><h3>{format!("{} · Secret files",service_name)}</h3>
        <For each={move ||(0..mounts.get().len()).collect::<Vec<_>>()} key=|i|*i children=move |index|view!{
            <div class="form-grid"><label>"Secret name"<input prop:value=move ||mounts.get().get(index).map(|m|m.name.clone()).unwrap_or_default() on:input=move |e|mounts.update(|items|items[index].name=event_target_value(&e))/></label>
            <label>"Container path"<input prop:value=move ||mounts.get().get(index).map(|m|m.target.clone()).unwrap_or_default() on:input=move |e|mounts.update(|items|items[index].target=event_target_value(&e))/></label>
            <button on:click=move |_|mounts.update(|items|{items.remove(index);})>"Remove reference"</button></div>
        }/>
        <button on:click=move |_|mounts.update(|items|items.push(piqueld_client::SecretMount{name:String::new(),target:"/run/secrets/".into()}))>"Add secret file"</button>
        <p class="help">"Paths must be under /run/secrets. This saves references, never secret values."</p>
        <div class="form-actions"><button class="primary" disabled=move ||mounts.get()==baseline.get() on:click=save>"Save Changes"</button><button on:click=move |_|mounts.set(baseline.get_untracked())>"Discard edits"</button></div>
    </section>}
}
