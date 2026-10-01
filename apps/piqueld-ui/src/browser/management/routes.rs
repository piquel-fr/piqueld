//! Application-owned public route editing and independent HTTPS readiness.
use super::super::ui::{Icon, Tone, badge, empty, icon, notice, remove_button};
use super::{dirty_group, editor, save_actions};
use leptos::prelude::*;
use piqueld_client::{Route, edit::ApplicationEdit};

type RouteRow = (String, String, String);

fn route_rows(routes: Vec<Route>) -> Vec<RouteRow> {
    routes
        .into_iter()
        .map(|route| (route.hostname, route.service, route.port.to_string()))
        .collect()
}

const fn route_tone(state: &str) -> Tone {
    match state.as_bytes() {
        b"ready" => Tone::Ok,
        b"failed" => Tone::Bad,
        b"pending" => Tone::Pending,
        _ => Tone::Neutral,
    }
}

/// Public route editor. Rows are drafted as `(hostname, service, port)` text and
/// re-synced from saved configuration only while there are no local edits. Saving
/// validates ports and replaces all routes. Also lists this application's deployed
/// routes and their ingress state from the dashboard readiness signal.
#[component]
pub(super) fn RouteSettings() -> impl IntoView {
    let context = editor();
    let draft = RwSignal::new(route_rows(context.manifest().spec.routes));
    let baseline = RwSignal::new(draft.get_untracked());
    dirty_group("routes".into(), draft, baseline);
    Effect::new(move |_| {
        let saved_routes = context
            .saved
            .with(|saved| route_rows(saved.application.to_manifest().spec.routes));
        if draft.get_untracked() == baseline.get_untracked() {
            draft.set(saved_routes.clone());
            baseline.set(saved_routes);
        }
    });
    let save = move || {
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
                draft.set(route_rows(saved.application.to_manifest().spec.routes));
                baseline.set(draft.get_untracked());
            }),
        );
    };
    let readiness = context
        .dashboard
        .with_value(|dashboard| dashboard.signals.readiness);
    let deployed = move || {
        let id = context.id();
        readiness.get().map(|status| {
            status
                .ingress
                .routes
                .into_iter()
                .filter(|route| route.application_id == id)
                .collect::<Vec<_>>()
        })
    };
    view! {
        <div class="stack">
            <section class="card">
                <header>
                    <div>
                        <h3>"Public routes"</h3>
                        <p>
                            "Point each hostname’s DNS at this server; HTTPS certificates are managed automatically. Routes are public, so your application must handle authentication. Save, then deploy to activate changes."
                        </p>
                    </div>
                </header>
                <div class="stack-sm">
                    {move || {
                        readiness
                            .get()
                            .filter(|status| !status.ingress.enabled)
                            .map(|_| {
                                notice(
                                    Tone::Info,
                                    "Ingress is disabled in the daemon configuration. Routes can still be saved and deployed; they become public when ingress is enabled.",
                                )
                            })
                    }} <fieldset disabled={move || context.blocked()}>
                        <div class="form-list">
                            <For
                                each={move || (0..draft.with(Vec::len)).collect::<Vec<_>>()}
                                key={|index| *index}
                                children={move |index| {
                                    view! {
                                        <div class="form-row">
                                            <label class="field">
                                                <span>"Hostname"</span>
                                                <input
                                                    type="text"
                                                    placeholder="app.example.com"
                                                    prop:value={move || {
                                                        draft
                                                            .with(|rows| {
                                                                rows.get(index).map(|r| r.0.clone()).unwrap_or_default()
                                                            })
                                                    }}
                                                    on:input={move |event| {
                                                        draft
                                                            .update(|rows| {
                                                                if let Some(row) = rows.get_mut(index) {
                                                                    row.0 = event_target_value(&event);
                                                                }
                                                            });
                                                    }}
                                                />
                                            </label>
                                            <label class="field">
                                                <span>"Service"</span>
                                                <select
                                                    prop:value={move || {
                                                        draft
                                                            .with(|rows| {
                                                                rows.get(index).map(|r| r.1.clone()).unwrap_or_default()
                                                            })
                                                    }}
                                                    on:change={move |event| {
                                                        draft
                                                            .update(|rows| {
                                                                if let Some(row) = rows.get_mut(index) {
                                                                    row.1 = event_target_value(&event);
                                                                }
                                                            });
                                                    }}
                                                >
                                                    <option value="">"Select a service"</option>
                                                    {move || {
                                                        context
                                                            .manifest()
                                                            .spec
                                                            .services
                                                            .into_iter()
                                                            .map(|service| {
                                                                view! {
                                                                    <option value={service
                                                                        .name
                                                                        .clone()}>{service.name.clone()}</option>
                                                                }
                                                            })
                                                            .collect_view()
                                                    }}
                                                </select>
                                            </label>
                                            <label class="field" style="max-width:120px">
                                                <span>"HTTP port"</span>
                                                <input
                                                    type="number"
                                                    min="1"
                                                    max="65535"
                                                    prop:value={move || {
                                                        draft
                                                            .with(|rows| {
                                                                rows.get(index).map(|r| r.2.clone()).unwrap_or_default()
                                                            })
                                                    }}
                                                    on:input={move |event| {
                                                        draft
                                                            .update(|rows| {
                                                                if let Some(row) = rows.get_mut(index) {
                                                                    row.2 = event_target_value(&event);
                                                                }
                                                            });
                                                    }}
                                                />
                                            </label>
                                            {remove_button(move || {
                                                draft
                                                    .update(|rows| {
                                                        rows.remove(index);
                                                    });
                                            })}
                                        </div>
                                    }
                                }}
                            />
                        </div>
                        <button
                            type="button"
                            class="btn btn-sm"
                            disabled={move || draft.with(Vec::len) >= 64}
                            on:click={move |_| {
                                draft
                                    .update(|rows| {
                                        rows.push((String::new(), String::new(), "3000".into()))
                                    });
                            }}
                        >
                            {icon(Icon::Plus)}
                            "Add route"
                        </button>
                        {save_actions(draft, baseline, save, || false)}
                    </fieldset>
                </div>
            </section>
            <section class="card card-flush">
                <header>
                    <div>
                        <h3>"Deployed routes"</h3>
                        <p>"HTTPS readiness is probed independently of application rollout."</p>
                    </div>
                </header>
                {move || {
                    match deployed() {
                        None => empty("Waiting for the ingress status…"),
                        Some(routes) if routes.is_empty() => {
                            empty("No routes are deployed for this application.")
                        }
                        Some(routes) => {
                            view! {
                                <table class="table">
                                    <thead>
                                        <tr>
                                            <th>"Hostname"</th>
                                            <th>"Backend"</th>
                                            <th>"State"</th>
                                            <th>"Details"</th>
                                        </tr>
                                    </thead>
                                    <tbody>
                                        {routes
                                            .into_iter()
                                            .map(|route| {
                                                view! {
                                                    <tr>
                                                        <td>
                                                            <strong>{route.hostname}</strong>
                                                        </td>
                                                        <td>
                                                            <code>{format!("{}:{}", route.service, route.port)}</code>
                                                        </td>
                                                        <td>
                                                            {badge(route_tone(&route.state), route.state.clone())}
                                                        </td>
                                                        <td class="muted">{route.message}</td>
                                                    </tr>
                                                }
                                            })
                                            .collect_view()}
                                    </tbody>
                                </table>
                            }
                                .into_any()
                        }
                    }
                }}
            </section>
        </div>
    }
}
