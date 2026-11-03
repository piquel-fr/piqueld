//! Application-owned routes, each a row added and edited in a dialog, with
//! the state of its deployments, and where each deployed route stands with
//! DNS and HTTPS.
use super::super::confirm;
use super::super::ui::{
    FormModal, Icon, Tone, badge, empty, icon, notice, route_name, row_actions,
};
use super::{RowDialog, editor};
use leptos::prelude::*;
use piqueld_client::{
    ApplicationManifest, Redirect, RedirectStatus, Route, Visibility,
    edit::ApplicationEdit,
    system::{DnsRecordState, RouteStatus},
};

/// One route as the dialog edits it. Both destinations keep their fields
/// so switching between them does not discard typed values.
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

/// Where a saved route leads, in a few words.
fn destination(route: &Route) -> String {
    match &route.redirect {
        Some(redirect) => format!(
            "redirects to {} ({}{})",
            redirect.to,
            redirect.status,
            if redirect.preserve_path {
                ", keeping the path"
            } else {
                ""
            }
        ),
        None => format!(
            "to service {}, port {}",
            route.service.as_deref().unwrap_or("?"),
            route
                .port
                .map_or_else(|| "?".to_owned(), |port| port.to_string()),
        ),
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

/// Whether `deployed` serves the saved `route`: the same name when the route
/// has one, else the same hostname (a hostname with a variable renders
/// differently in each environment, which is what names are for).
fn serves(deployed: &RouteStatus, route: &Route) -> bool {
    match &route.name {
        Some(name) => deployed.name.as_ref().map(ToString::to_string).as_ref() == Some(name),
        None => deployed.hostname == route.hostname.to_string(),
    }
}

/// Route editor: one row per saved route, with the state of each of its
/// deployments, added and edited in a dialog that saves at once. Below it,
/// every deployed route's DNS and HTTPS state, from the dashboard readiness
/// signal.
#[component]
#[expect(clippy::too_many_lines, reason = "flat view! markup")]
pub(super) fn RouteSettings() -> impl IntoView {
    let context = editor();
    let dialog = RowDialog::<RouteDraft>::new();
    let draft = dialog.draft;
    let readiness = context.signals().readiness;
    // Deployed routes of every environment and preview, with its name.
    let deployed = Memo::new(move |_| {
        readiness
            .get()
            .map(|status| {
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
            .unwrap_or_default()
    });
    // Saves `rows` as the application's routes, then closes the dialog.
    let save = move |rows: Vec<RouteDraft>| match rows
        .into_iter()
        .map(RouteDraft::route)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(routes) => context.save(ApplicationEdit::Routes(routes), move |_| {
            dialog.opened.set(false);
        }),
        Err(message) => context.set_error(Some(message.into())),
    };
    let remove = move |index: usize| {
        if !confirm(
            "Remove this route from the saved configuration? It is served until the next deployment.",
        ) {
            return;
        }
        let mut rows = RouteDraft::rows(&context.manifest());
        if index < rows.len() {
            rows.remove(index);
        }
        save(rows);
    };
    view! {
        <div class="stack">
            <section class="card">
                <header>
                    <div>
                        <h3>"Routes"</h3>
                        <p>
                            "Hostnames that reach a service of this application, or redirect elsewhere. A public route is reachable from the internet; a private one, the default, only from the tailnet. Save a route, then deploy to serve it."
                        </p>
                    </div>
                    <button
                        type="button"
                        class="btn btn-sm"
                        disabled={move || context.blocked()}
                        on:click={move |_| dialog.add()}
                    >
                        {icon(Icon::Plus)}
                        "Add route"
                    </button>
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
                    }}
                    {move || {
                        let routes = context.manifest().spec.routes;
                        if routes.is_empty() {
                            return empty("No routes yet. Add one to reach a service by hostname.");
                        }
                        let deployed = deployed.get();
                        view! {
                            <ul class="list" aria-label="Routes">
                                {routes
                                    .into_iter()
                                    .enumerate()
                                    .map(|(index, route)| {
                                        let states = deployed
                                            .iter()
                                            .filter(|(_, status)| serves(status, &route))
                                            .map(|(environment, status)| {
                                                view! {
                                                    <span class="route-state" title={status.message.clone()}>
                                                        {environment.clone()}
                                                        {badge(route_tone(&status.state), status.state.clone())}
                                                    </span>
                                                }
                                            })
                                            .collect_view();
                                        let edited = RouteDraft::from(route.clone());
                                        view! {
                                            <li class="list-row">
                                                <span class="title">
                                                    {route.hostname.to_string()}
                                                    <small>{destination(&route)}</small>
                                                </span>
                                                {route
                                                    .name
                                                    .clone()
                                                    .map(|name| view! { <span class="tag">{name}</span> })}
                                                {badge(Tone::Neutral, route.visibility.to_string())}
                                                <span class="route-states">{states}</span>
                                                {row_actions(
                                                    move || dialog.edit(index, edited.clone()),
                                                    move || remove(index),
                                                )}
                                            </li>
                                        }
                                    })
                                    .collect_view()}
                            </ul>
                        }
                            .into_any()
                    }}
                </div>
                <FormModal
                    title={dialog.title("Add route", "Edit route")}
                    opened={dialog.opened}
                    busy={context.busy}
                    error={context.error}
                    submit={dialog.title("Add route", "Save route")}
                    on_submit={Callback::new(move |()| {
                        save(dialog.apply(RouteDraft::rows(&context.manifest())));
                    })}
                >
                    <label class="field">
                        <span>"Hostname"</span>
                        <input
                            type="text"
                            placeholder="app.example.com"
                            prop:value={move || draft.with(|r| r.hostname.clone())}
                            on:input={move |event| {
                                draft.update(|r| r.hostname = event_target_value(&event));
                            }}
                        />
                    </label>
                    <p class="hint">
                        "A wildcard such as *.example.com routes every name one label below it. ${{ vars.<name> }} takes a variable, so each environment gets its own hostname."
                    </p>
                    <div class="form-grid">
                        <label class="field">
                            <span>"Name"</span>
                            <input
                                type="text"
                                placeholder="optional"
                                prop:value={move || draft.with(|r| r.name.clone())}
                                on:input={move |event| {
                                    draft.update(|r| r.name = event_target_value(&event));
                                }}
                            />
                        </label>
                        <label class="field">
                            <span>"Visibility"</span>
                            <select
                                prop:value={move || draft.with(|r| r.visibility).to_string()}
                                on:change={move |event| {
                                    if let Ok(visibility) = event_target_value(&event).parse() {
                                        draft.update(|r| r.visibility = visibility);
                                    }
                                }}
                            >
                                <option value="private">"Private"</option>
                                <option value="public">"Public"</option>
                            </select>
                        </label>
                    </div>
                    <p class="hint">
                        "A name selects the same route in every environment and preview, whatever hostname it renders."
                    </p>
                    <label class="field">
                        <span>"Destination"</span>
                        <select
                            prop:value={move || {
                                if draft.with(|r| r.redirect) { "redirect" } else { "service" }
                            }}
                            on:change={move |event| {
                                draft
                                    .update(|r| {
                                        r.redirect = event_target_value(&event) == "redirect";
                                    });
                            }}
                        >
                            <option value="service">"A service"</option>
                            <option value="redirect">"A redirect"</option>
                        </select>
                    </label>
                    <Show
                        when={move || draft.with(|r| r.redirect)}
                        fallback={move || {
                            view! {
                                <div class="form-grid">
                                    <label class="field">
                                        <span>"Service"</span>
                                        <select
                                            prop:value={move || draft.with(|r| r.service.clone())}
                                            on:change={move |event| {
                                                draft.update(|r| r.service = event_target_value(&event));
                                            }}
                                        >
                                            {context.service_options()}
                                        </select>
                                    </label>
                                    <label class="field">
                                        <span>"HTTP port"</span>
                                        <input
                                            type="number"
                                            min="1"
                                            max="65535"
                                            prop:value={move || draft.with(|r| r.port.clone())}
                                            on:input={move |event| {
                                                draft.update(|r| r.port = event_target_value(&event));
                                            }}
                                        />
                                    </label>
                                </div>
                            }
                        }}
                    >
                        <label class="field">
                            <span>"Redirect to"</span>
                            <input
                                type="url"
                                placeholder="https://example.com"
                                prop:value={move || draft.with(|r| r.to.clone())}
                                on:input={move |event| {
                                    draft.update(|r| r.to = event_target_value(&event));
                                }}
                            />
                        </label>
                        <div class="form-grid">
                            <label class="field">
                                <span>"Status"</span>
                                <select
                                    prop:value={move || draft.with(|r| r.status.clone())}
                                    on:change={move |event| {
                                        draft.update(|r| r.status = event_target_value(&event));
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
                                    prop:checked={move || draft.with(|r| r.preserve_path)}
                                    on:change={move |event| {
                                        draft
                                            .update(|r| r.preserve_path = event_target_checked(&event));
                                    }}
                                />
                                "Keep path and query"
                            </label>
                        </div>
                    </Show>
                </FormModal>
            </section>
            {move || {
                let deployed = deployed.get();
                (!deployed.is_empty()).then(|| deployed_routes(deployed))
            }}
        </div>
    }
}

/// The application's deployed routes in every environment and preview, with
/// the DNS records each needs and its DNS and HTTPS state. HTTPS readiness
/// is probed independently of application rollout.
fn deployed_routes(routes: Vec<(String, RouteStatus)>) -> AnyView {
    view! {
        <section class="card card-flush">
            <header>
                <div>
                    <h3>"Where routes are served"</h3>
                    <p>
                        "Each deployed route, the DNS records its hostname needs, and whether HTTPS answers on it."
                    </p>
                </div>
            </header>
            <table class="table">
                <thead>
                    <tr>
                        <th>"Hostname"</th>
                        <th>"Environment"</th>
                        <th>"Visibility"</th>
                        <th>"Destination"</th>
                        <th>"DNS records"</th>
                        <th>"DNS"</th>
                        <th>"HTTPS"</th>
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
                                        {route
                                            .name
                                            .map(|name| {
                                                view! {
                                                    " "
                                                    <span class="tag">{route_name(Some(name))}</span>
                                                }
                                            })}
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
                                        {badge(
                                            dns_tone(route.dns_state),
                                            route.dns_state.to_string(),
                                        )}
                                    </td>
                                    <td>{badge(route_tone(&route.state), route.state.clone())}</td>
                                    <td class="muted">{route.message}</td>
                                </tr>
                            }
                        })
                        .collect_view()}
                </tbody>
            </table>
        </section>
    }
    .into_any()
}
