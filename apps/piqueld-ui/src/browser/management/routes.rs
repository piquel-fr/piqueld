//! Application-owned route editing and independent HTTPS readiness.
use super::super::ui::{Icon, Tone, badge, empty, icon, notice, remove_button};
use super::{dirty_group, editor, save_actions};
use leptos::prelude::*;
use piqueld_client::{Redirect, RedirectStatus, Route, Visibility, edit::ApplicationEdit};

/// One editable route row. Both destinations keep their fields so switching
/// between them does not discard typed values.
#[derive(Clone, PartialEq)]
struct RouteDraft {
    hostname: String,
    visibility: Visibility,
    redirect: bool,
    service: String,
    port: String,
    to: String,
    status: String,
    preserve_path: bool,
}

impl Default for RouteDraft {
    fn default() -> Self {
        Self {
            hostname: String::new(),
            visibility: Visibility::Private,
            redirect: false,
            service: String::new(),
            port: "3000".into(),
            to: String::new(),
            status: u16::from(RedirectStatus::PermanentRedirect).to_string(),
            preserve_path: true,
        }
    }
}

impl RouteDraft {
    /// Saved routes as editable rows.
    fn rows(routes: Vec<Route>) -> Vec<Self> {
        routes.into_iter().map(Self::from).collect()
    }

    /// The route input, or a message when a number does not parse.
    fn route(self) -> Result<Route, &'static str> {
        if self.redirect {
            let status = self
                .status
                .parse()
                .map_err(|_| "Redirect status must be 301, 302, 303, 307, or 308")?;
            Ok(Route::redirect(
                self.hostname,
                self.visibility,
                Redirect {
                    to: self.to.into(),
                    status,
                    preserve_path: self.preserve_path,
                },
            ))
        } else {
            let port = self
                .port
                .parse()
                .map_err(|_| "Route port must be between 1 and 65535")?;
            Ok(Route::service(
                self.hostname,
                self.visibility,
                self.service,
                port,
            ))
        }
    }
}

impl From<Route> for RouteDraft {
    fn from(route: Route) -> Self {
        let mut draft = Self {
            hostname: route.hostname.to_string(),
            visibility: route.visibility,
            service: route.service.unwrap_or_default(),
            ..Self::default()
        };
        if let Some(port) = route.port {
            draft.port = port.to_string();
        }
        if let Some(redirect) = route.redirect {
            draft.redirect = true;
            draft.to = redirect.to.to_string();
            draft.status = redirect.status.to_string();
            draft.preserve_path = redirect.preserve_path;
        }
        draft
    }
}

/// Reads one field of a row, empty when the row was removed.
fn field<T: Default>(
    draft: RwSignal<Vec<RouteDraft>>,
    index: usize,
    read: impl Fn(&RouteDraft) -> T,
) -> T {
    draft.with(|rows| rows.get(index).map(read).unwrap_or_default())
}

/// Updates one row in place.
fn edit(draft: RwSignal<Vec<RouteDraft>>, index: usize, write: impl FnOnce(&mut RouteDraft)) {
    draft.update(|rows| {
        if let Some(row) = rows.get_mut(index) {
            write(row);
        }
    });
}

const fn route_tone(state: &str) -> Tone {
    match state.as_bytes() {
        b"ready" => Tone::Ok,
        b"failed" => Tone::Bad,
        b"pending" => Tone::Pending,
        _ => Tone::Neutral,
    }
}

/// Route editor. Rows are drafted as [`RouteDraft`] text and re-synced
/// from saved configuration only while there are no local edits. Saving
/// validates numbers and replaces all routes. Also lists this application's
/// deployed routes and their ingress state from the dashboard readiness signal.
#[component]
pub(super) fn RouteSettings() -> impl IntoView {
    let context = editor();
    let draft = RwSignal::new(RouteDraft::rows(context.manifest().spec.routes));
    let baseline = RwSignal::new(draft.get_untracked());
    dirty_group("routes".into(), draft, baseline);
    Effect::new(move |_| {
        let saved_routes = context
            .saved
            .with(|saved| RouteDraft::rows(saved.application.to_manifest().spec.routes));
        if draft.get_untracked() == baseline.get_untracked() {
            draft.set(saved_routes.clone());
            baseline.set(saved_routes);
        }
    });
    let save = move || {
        let routes = match draft
            .get_untracked()
            .into_iter()
            .map(RouteDraft::route)
            .collect()
        {
            Ok(routes) => routes,
            Err(message) => {
                context.error.set(Some(message.into()));
                return;
            }
        };
        context.save(
            ApplicationEdit::Routes(routes),
            Callback::new(move |saved: piqueld_client::ApplicationView| {
                draft.set(RouteDraft::rows(
                    saved.application.to_manifest().spec.routes,
                ));
                baseline.set(draft.get_untracked());
            }),
        );
    };
    let readiness = context
        .dashboard
        .with_value(|dashboard| dashboard.signals.readiness);
    // Deployed routes of every environment, with the environment's name.
    let deployed = move || {
        let environments = context.saved.with(|saved| saved.environments.clone());
        readiness.get().map(|status| {
            status
                .ingress
                .routes
                .into_iter()
                .filter_map(|route| {
                    environments
                        .iter()
                        .find(|environment| environment.id.as_str() == route.environment_id)
                        .map(|environment| (environment.name.to_string(), route))
                })
                .collect::<Vec<_>>()
        })
    };
    view! {
        <div class="stack">
            <section class="card">
                <header>
                    <div>
                        <h3>"Routes"</h3>
                        <p>
                            "Public routes are reachable from the internet: point their DNS at this server (or, with a Cloudflare Tunnel, a proxied CNAME at the tunnel), and their certificates are managed automatically. Private routes, the default, are reachable only from the tailnet on the same hostname: point their DNS at the apps node’s tailnet addresses, and their certificates come from a DNS provider. An environment’s visibility can make its routes private. Redirects are answered by the gateway and need no service. Save, then deploy to activate changes."
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
                                    "Ingress is disabled in the daemon configuration. Routes can still be saved and deployed; they are served once ingress is enabled.",
                                )
                            })
                    }}
                    {move || {
                        readiness
                            .get()
                            .filter(|status| status.ingress.enabled && !status.ingress.private.enabled)
                            .map(|_| {
                                notice(
                                    Tone::Info,
                                    "Private ingress is disabled in the daemon configuration ([ingress.private]). Private routes can still be saved and deployed; they are served on the tailnet once it is enabled.",
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
                                                    prop:value={move || field(draft, index, |r| r.hostname.clone())}
                                                    on:input={move |event| {
                                                        edit(draft, index, |r| r.hostname = event_target_value(&event));
                                                    }}
                                                />
                                            </label>
                                            <label class="field" style="max-width:130px">
                                                <span>"Visibility"</span>
                                                <select
                                                    prop:value={move || field(draft, index, |r| r.visibility).to_string()}
                                                    on:change={move |event| {
                                                        if let Ok(visibility) = event_target_value(&event).parse() {
                                                            edit(draft, index, |r| r.visibility = visibility);
                                                        }
                                                    }}
                                                >
                                                    <option value="private">"Private"</option>
                                                    <option value="public">"Public"</option>
                                                </select>
                                            </label>
                                            <label class="field" style="max-width:140px">
                                                <span>"Destination"</span>
                                                <select
                                                    prop:value={move || {
                                                        if field(draft, index, |r| r.redirect) { "redirect" } else { "service" }
                                                    }}
                                                    on:change={move |event| {
                                                        edit(draft, index, |r| r.redirect = event_target_value(&event) == "redirect");
                                                    }}
                                                >
                                                    <option value="service">"Service"</option>
                                                    <option value="redirect">"Redirect"</option>
                                                </select>
                                            </label>
                                            {move || {
                                                if field(draft, index, |r| r.redirect) {
                                                    view! {
                                                        <label class="field">
                                                            <span>"Redirect to"</span>
                                                            <input
                                                                type="url"
                                                                placeholder="https://example.com"
                                                                prop:value={move || field(draft, index, |r| r.to.clone())}
                                                                on:input={move |event| {
                                                                    edit(draft, index, |r| r.to = event_target_value(&event));
                                                                }}
                                                            />
                                                        </label>
                                                        <label class="field" style="max-width:180px">
                                                            <span>"Status"</span>
                                                            <select
                                                                prop:value={move || field(draft, index, |r| r.status.clone())}
                                                                on:change={move |event| {
                                                                    edit(draft, index, |r| r.status = event_target_value(&event));
                                                                }}
                                                            >
                                                                <option value="308">"308 Permanent"</option>
                                                                <option value="301">"301 Moved permanently"</option>
                                                                <option value="307">"307 Temporary"</option>
                                                                <option value="302">"302 Found"</option>
                                                                <option value="303">"303 See other"</option>
                                                            </select>
                                                        </label>
                                                        <label class="checkbox">
                                                            <input
                                                                type="checkbox"
                                                                prop:checked={move || field(draft, index, |r| r.preserve_path)}
                                                                on:change={move |event| {
                                                                    edit(draft, index, |r| r.preserve_path = event_target_checked(&event));
                                                                }}
                                                            />
                                                            "Keep path and query"
                                                        </label>
                                                    }
                                                        .into_any()
                                                } else {
                                                    view! {
                                                        <label class="field">
                                                            <span>"Service"</span>
                                                            <select
                                                                prop:value={move || field(draft, index, |r| r.service.clone())}
                                                                on:change={move |event| {
                                                                    edit(draft, index, |r| r.service = event_target_value(&event));
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
                                                                prop:value={move || field(draft, index, |r| r.port.clone())}
                                                                on:input={move |event| {
                                                                    edit(draft, index, |r| r.port = event_target_value(&event));
                                                                }}
                                                            />
                                                        </label>
                                                    }
                                                        .into_any()
                                                }
                                            }}
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
                                        rows.push(RouteDraft::default())
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
                                            <th>"Environment"</th>
                                            <th>"Visibility"</th>
                                            <th>"Destination"</th>
                                            <th>"DNS records"</th>
                                            <th>"State"</th>
                                            <th>"Details"</th>
                                        </tr>
                                    </thead>
                                    <tbody>
                                        {routes
                                            .into_iter()
                                            .map(|(environment, route)| {
                                                view! {
                                                    <tr>
                                                        <td>
                                                            <strong>{route.hostname}</strong>
                                                        </td>
                                                        <td>{environment}</td>
                                                        <td>{route.visibility.to_string()}</td>
                                                        <td>
                                                            <code>{route.target.to_string()}</code>
                                                        </td>
                                                        <td>
                                                            <code>{route.dns.to_string()}</code>
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
