//! Application-owned route editing and independent HTTPS readiness.
use super::super::ui::{Tone, add_button, badge, empty, notice, remove_button, route_name};
use super::{Draft, Rows, editor};
use leptos::prelude::*;
use piqueld_client::{
    ApplicationManifest, Redirect, RedirectStatus, Route, Visibility, edit::ApplicationEdit,
    system::DnsRecordState,
};

/// One editable route row. Both destinations keep their fields so switching
/// between them does not discard typed values.
#[derive(Clone, PartialEq)]
struct RouteDraft {
    hostname: String,
    /// Optional stable name; empty when unnamed.
    name: String,
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
            name: String::new(),
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
    /// The manifest's routes as editable rows.
    fn rows(manifest: &ApplicationManifest) -> Vec<Self> {
        manifest
            .spec
            .routes
            .iter()
            .cloned()
            .map(Self::from)
            .collect()
    }

    /// The route input, or a message when a number does not parse.
    fn route(self) -> Result<Route, &'static str> {
        let name = Some(self.name).filter(|name| !name.is_empty());
        let route = if self.redirect {
            let status = self
                .status
                .parse()
                .map_err(|_| "Redirect status must be 301, 302, 303, 307, or 308")?;
            Route::redirect(
                self.hostname,
                self.visibility,
                Redirect {
                    to: self.to.into(),
                    status,
                    preserve_path: self.preserve_path,
                },
            )
        } else {
            let port = self
                .port
                .parse()
                .map_err(|_| "Route port must be between 1 and 65535")?;
            Route::service(self.hostname, self.visibility, self.service, port)
        };
        Ok(route.named(name))
    }
}

impl From<Route> for RouteDraft {
    fn from(route: Route) -> Self {
        let mut draft = Self {
            hostname: route.hostname.to_string(),
            name: route.name.unwrap_or_default(),
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

/// Badge tone of a deployed route's HTTPS state.
fn route_tone(state: &str) -> Tone {
    match state {
        "ready" => Tone::Ok,
        "failed" => Tone::Bad,
        "pending" => Tone::Pending,
        _ => Tone::Neutral,
    }
}

/// Badge tone of a route's DNS record state.
const fn dns_tone(state: DnsRecordState) -> Tone {
    match state {
        DnsRecordState::Managed => Tone::Ok,
        DnsRecordState::Pending => Tone::Pending,
        DnsRecordState::DnsConflict => Tone::Bad,
        DnsRecordState::Manual => Tone::Neutral,
    }
}

/// Route editor. Rows are drafted as [`RouteDraft`] text and re-synced
/// from saved configuration only while there are no local edits. Saving
/// validates numbers and replaces all routes. Also lists this application's
/// deployed routes and their ingress state from the dashboard readiness signal.
#[component]
pub(super) fn RouteSettings() -> impl IntoView {
    let context = editor();
    let draft = Draft::synced("routes", RouteDraft::rows);
    let routes = draft.value;
    let save = move || match routes
        .get_untracked()
        .into_iter()
        .map(RouteDraft::route)
        .collect()
    {
        Ok(saved) => draft.save(ApplicationEdit::Routes(saved), RouteDraft::rows),
        Err(message) => context.error.set(Some(message.into())),
    };
    let readiness = context.signals().readiness;
    view! {
        <div class="stack">
            <section class="card">
                <header>
                    <div>
                        <h3>"Routes"</h3>
                        <p>
                            "Public routes are reachable from the internet: point their DNS at this server (or, with a Cloudflare Tunnel, a proxied CNAME at the tunnel), and their certificates are managed automatically. Private routes, the default, are reachable only from the tailnet on the same hostname: point their DNS at the apps node’s tailnet addresses, and their certificates come from a DNS provider. A wildcard hostname such as *.example.com routes every name one label below example.com, except names with their own route, and gets its certificate from a DNS provider, unless it is public behind a Cloudflare Tunnel, which terminates its TLS. An environment’s visibility can make its routes private. Redirects are answered by the gateway and need no service. Save, then deploy to activate changes."
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
                            .filter(|status| {
                                status.ingress.enabled && !status.ingress.private.enabled
                            })
                            .map(|_| {
                                notice(
                                    Tone::Info,
                                    "Private ingress is disabled in the daemon configuration ([ingress.private]). Private routes can still be saved and deployed; they are served on the tailnet once it is enabled.",
                                )
                            })
                    }} <fieldset disabled={move || context.blocked()}>
                        <div class="form-list">
                            <For
                                each={move || routes.indices()}
                                key={|index| *index}
                                children={move |index| route_row(routes, index)}
                            />
                        </div>
                        {add_button(
                            "Add route",
                            move || routes.with(Vec::len) >= 64,
                            move || routes.update(|rows| rows.push(RouteDraft::default())),
                        )}
                        {draft.actions(save, || false)}
                    </fieldset>
                </div>
            </section>
            <DeployedRoutes />
        </div>
    }
}

/// Fields of route `index`: its hostname, name, visibility, and either the
/// redirect or the service it leads to.
fn route_row(routes: RwSignal<Vec<RouteDraft>>, index: usize) -> AnyView {
    let context = editor();
    let destination = move || {
        if routes.field(index, |r| r.redirect) {
            view! {
                <label class="field">
                    <span>"Redirect to"</span>
                    <input
                        type="url"
                        placeholder="https://example.com"
                        prop:value={move || routes.field(index, |r| r.to.clone())}
                        on:input={move |event| {
                            routes.edit(index, |r| r.to = event_target_value(&event));
                        }}
                    />
                </label>
                <label class="field" style="max-width:180px">
                    <span>"Status"</span>
                    <select
                        prop:value={move || routes.field(index, |r| r.status.clone())}
                        on:change={move |event| {
                            routes.edit(index, |r| r.status = event_target_value(&event));
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
                        prop:checked={move || routes.field(index, |r| r.preserve_path)}
                        on:change={move |event| {
                            routes.edit(index, |r| r.preserve_path = event_target_checked(&event));
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
                        prop:value={move || routes.field(index, |r| r.service.clone())}
                        on:change={move |event| {
                            routes.edit(index, |r| r.service = event_target_value(&event));
                        }}
                    >
                        {context.service_options()}
                    </select>
                </label>
                <label class="field" style="max-width:120px">
                    <span>"HTTP port"</span>
                    <input
                        type="number"
                        min="1"
                        max="65535"
                        prop:value={move || routes.field(index, |r| r.port.clone())}
                        on:input={move |event| {
                            routes.edit(index, |r| r.port = event_target_value(&event));
                        }}
                    />
                </label>
            }
            .into_any()
        }
    };
    view! {
        <div class="form-row">
            <label class="field">
                <span>"Hostname"</span>
                <input
                    type="text"
                    placeholder="app.example.com"
                    prop:value={move || routes.field(index, |r| r.hostname.clone())}
                    on:input={move |event| {
                        routes.edit(index, |r| r.hostname = event_target_value(&event));
                    }}
                />
            </label>
            <label class="field" style="max-width:130px">
                <span>"Name"</span>
                <input
                    type="text"
                    placeholder="optional"
                    title="Stable name selecting this route in every environment and preview"
                    prop:value={move || routes.field(index, |r| r.name.clone())}
                    on:input={move |event| {
                        routes.edit(index, |r| r.name = event_target_value(&event));
                    }}
                />
            </label>
            <label class="field" style="max-width:130px">
                <span>"Visibility"</span>
                <select
                    prop:value={move || routes.field(index, |r| r.visibility).to_string()}
                    on:change={move |event| {
                        if let Ok(visibility) = event_target_value(&event).parse() {
                            routes.edit(index, |r| r.visibility = visibility);
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
                        if routes.field(index, |r| r.redirect) { "redirect" } else { "service" }
                    }}
                    on:change={move |event| {
                        routes
                            .edit(index, |r| r.redirect = event_target_value(&event) == "redirect");
                    }}
                >
                    <option value="service">"Service"</option>
                    <option value="redirect">"Redirect"</option>
                </select>
            </label>
            {destination}
            {remove_button(move || routes.remove(index))}
        </div>
    }
    .into_any()
}

/// The application's deployed routes in every environment and preview, with
/// their DNS and HTTPS state, from the dashboard readiness signal.
#[component]
fn DeployedRoutes() -> impl IntoView {
    let context = editor();
    let readiness = context.signals().readiness;
    // Deployed routes of every environment and preview, with its name.
    let deployed = move || {
        readiness.get().map(|status| {
            context.saved.with(|saved| {
                status
                    .ingress
                    .routes
                    .into_iter()
                    .filter_map(|route| {
                        saved
                            .deployable(&route.environment_id)
                            .map(|environment| (environment.name.to_string(), route))
                    })
                    .collect::<Vec<_>>()
            })
        })
    };
    view! {
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
                                        <th>"Name"</th>
                                        <th>"Environment"</th>
                                        <th>"Visibility"</th>
                                        <th>"Destination"</th>
                                        <th>"DNS records"</th>
                                        <th>"DNS"</th>
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
                                                    <td>{route_name(route.name)}</td>
                                                    <td>{environment}</td>
                                                    <td>{route.visibility.to_string()}</td>
                                                    <td>
                                                        <code>{route.target.to_string()}</code>
                                                    </td>
                                                    <td>
                                                        <code>{route.dns.to_string()}</code>
                                                    </td>
                                                    <td>
                                                        {badge(
                                                            dns_tone(route.dns_state),
                                                            route.dns_state.to_string(),
                                                        )}
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
    }
}
