//! Application-owned public route editing and independent HTTPS readiness.
use super::{dirty_group, editor};
use leptos::{
    Callback, CollectView, For, IntoView, SignalGet, SignalGetUntracked, SignalSet, SignalUpdate,
    SignalWith, component, create_effect, create_rw_signal, event_target_value, view,
};
use piqueld_client::{Route, edit::ApplicationEdit};

#[component]
pub(super) fn RouteSettings() -> impl IntoView {
    let context = editor();
    let initial: Vec<_> = context
        .manifest()
        .spec
        .routes
        .into_iter()
        .map(|route| (route.hostname, route.service, route.port.to_string()))
        .collect();
    let draft = create_rw_signal(initial);
    let baseline = create_rw_signal(draft.get_untracked());
    dirty_group("routes".into(), draft, baseline);
    create_effect(move |_| {
        let saved_routes = context.saved.with(|saved| {
            saved
                .application
                .to_manifest()
                .spec
                .routes
                .into_iter()
                .map(|route| (route.hostname, route.service, route.port.to_string()))
                .collect::<Vec<_>>()
        });
        if draft.get_untracked() == baseline.get_untracked() {
            draft.set(saved_routes.clone());
            baseline.set(saved_routes);
        }
    });
    let save = move |_| {
        let mut routes = Vec::new();
        for (hostname, service, port) in draft.get_untracked() {
            let Ok(port) = port.parse::<u16>() else {
                context
                    .error
                    .set(Some("Route port must be between 1 and 65535".into()));
                return;
            };
            routes.push(Route {
                hostname,
                service,
                port,
            });
        }
        context.save(
            ApplicationEdit::Routes(routes),
            Callback::new(move |saved: piqueld_client::ApplicationView| {
                let normalized = saved
                    .application
                    .to_manifest()
                    .spec
                    .routes
                    .into_iter()
                    .map(|route| (route.hostname, route.service, route.port.to_string()))
                    .collect();
                draft.set(normalized);
                baseline.set(draft.get_untracked());
            }),
        );
    };
    let readiness = context
        .dashboard
        .with_value(|dashboard| dashboard.signals.readiness);
    view! {
        <section class="settings-card">
            <h3>"Public routes"</h3>
            <p class="help">"Point each domain’s DNS at this server. HTTPS certificates are managed automatically. Routes are public; your application must handle authentication. Save, then Deploy to activate changes."</p>
            {move || readiness.get().filter(|status| !status.ingress.enabled).map(|_| view!{<p class="help">"Ingress is disabled in the daemon configuration. Routes can still be saved and deployed; they become public when ingress is enabled."</p>})}
            <fieldset disabled={move || context.blocked()}>
                <For each={move || (0..draft.with(Vec::len)).collect::<Vec<_>>()} key={|index| *index} children={move |index| view! {
                    <div class="form-grid">
                        <label>"Hostname"<input type="text" placeholder="app.example.com"
                            prop:value={move || draft.with(|rows| rows.get(index).map(|r|r.0.clone()).unwrap_or_default())}
                            on:input={move |event| draft.update(|rows| if let Some(row)=rows.get_mut(index) { row.0=event_target_value(&event); })}/></label>
                        <label>"Service"<select prop:value={move || draft.with(|rows|rows.get(index).map(|r|r.1.clone()).unwrap_or_default())}
                            on:change={move |event| draft.update(|rows| if let Some(row)=rows.get_mut(index) { row.1=event_target_value(&event); })}>
                            <option value="">"Select a service"</option>
                            {move || context.saved.with(|saved| saved.application.to_manifest().spec.services.into_iter().map(|service|view!{<option value=service.name.clone()>{service.name}</option>}).collect_view())}
                        </select></label>
                        <label>"HTTP port"<input type="number" min="1" max="65535"
                            prop:value={move || draft.with(|rows|rows.get(index).map(|r|r.2.clone()).unwrap_or_default())}
                            on:input={move |event| draft.update(|rows| if let Some(row)=rows.get_mut(index) { row.2=event_target_value(&event); })}/></label>
                        <button type="button" on:click={move |_| draft.update(|rows| { rows.remove(index); })}>"Remove route"</button>
                    </div>
                }}/>
                <div class="form-actions">
                    <button type="button" disabled={move || draft.with(Vec::len)>=64} on:click={move |_| draft.update(|rows| rows.push((String::new(),String::new(),"3000".into())))}>"Add route"</button>
                    <button type="button" class="primary" disabled={move || draft.get()==baseline.get()} on:click=save>"Save routes"</button>
                    <button type="button" on:click={move |_| draft.set(baseline.get_untracked())}>"Discard edits"</button>
                </div>
            </fieldset>
            <h4>"Deployed routes"</h4>
            {move || {
                let id = context.saved.with(|saved| saved.application.id().to_string());
                readiness.get().map(|status| status.ingress.routes.into_iter().filter(|route|route.application_id==id).map(|route|view!{
                    <div class="application-row"><strong>{route.hostname}</strong><span>{format!("{}:{}",route.service,route.port)}</span><span class="tag">{route.state}</span><p class="help">{route.message}</p></div>
                }).collect_view())
            }}
        </section>
    }
}
