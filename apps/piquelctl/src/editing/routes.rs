//! Editing routes: adding, redirecting, renaming and their visibility.

use super::{EditFlags, visibility};
use crate::{
    commands::resolve_application,
    error::{CliError, ErrorKind, Result},
    output::{Console, reports::RouteRow},
};
use clap::{Args, Subcommand};
use piqueld_client::{
    Client, Redirect, RedirectStatus, Route, Template, Visibility, edit::ApplicationEdit,
};

#[derive(Debug, Args)]
pub(crate) struct RouteTarget {
    /// Application name or stable ID.
    app: String,
    /// Hostname of the route: an exact name, or a wildcard such as
    /// `*.example.com`. It may reference variables, such as
    /// `${{ vars.domain }}`.
    hostname: String,
    #[command(flatten)]
    flags: EditFlags,
}
#[derive(Debug, Args)]
pub(crate) struct AddRouteArgs {
    #[command(flatten)]
    target: RouteTarget,
    /// Service that serves the route.
    service: String,
    /// HTTP port the service listens on inside its container.
    port: u16,
    /// `public` serves everyone, `private` only devices on the tailnet. The
    /// environment's visibility ceiling can make it stricter.
    #[arg(long, default_value = "private", value_parser = visibility())]
    visibility: Visibility,
    /// Stable route name, unique within the application. It selects the
    /// route in every environment and preview, such as with
    /// `env url --route`.
    #[arg(long)]
    name: Option<String>,
}
#[derive(Debug, Args)]
pub(crate) struct RedirectRouteArgs {
    #[command(flatten)]
    target: RouteTarget,
    /// Destination: an absolute `http` or `https` URL, without query or
    /// fragment. It may reference variables.
    #[arg(value_name = "URL")]
    to: Template,
    /// HTTP status of the redirect: `301`, `302`, `303`, `307` or `308`.
    #[arg(long, default_value_t = RedirectStatus::PermanentRedirect.into())]
    status: u16,
    /// Drop the request path and query instead of appending them to the
    /// destination.
    #[arg(long)]
    no_preserve_path: bool,
    /// `public` serves everyone, `private` only devices on the tailnet. The
    /// environment's visibility ceiling can make it stricter.
    #[arg(long, default_value = "private", value_parser = visibility())]
    visibility: Visibility,
    /// Stable route name, unique within the application. It selects the
    /// route in every environment and preview, such as with
    /// `env url --route`.
    #[arg(long)]
    name: Option<String>,
}
#[derive(Debug, Args)]
pub(crate) struct RouteNameArgs {
    #[command(flatten)]
    target: RouteTarget,
    /// New route name, unique within the application. Omit it to remove the
    /// name.
    name: Option<String>,
}
#[derive(Debug, Args)]
pub(crate) struct RouteVisibilityArgs {
    #[command(flatten)]
    target: RouteTarget,
    /// `public` serves everyone, `private` only devices on the tailnet.
    #[arg(value_parser = visibility())]
    visibility: Visibility,
}
#[derive(Debug, Subcommand)]
pub(crate) enum RouteCommand {
    /// List the application's deployed routes in every environment.
    ///
    /// Each route shows its visibility, its state, and the DNS records its
    /// hostname needs.
    List {
        /// Application name or stable ID.
        app: String,
    },
    /// Add an HTTPS route to a service.
    Add(AddRouteArgs),
    /// Add an HTTPS route that the gateway answers with a redirect.
    Redirect(RedirectRouteArgs),
    /// Change who can connect to a route.
    ///
    /// The next deployment withdraws the route from its old listener as it
    /// starts.
    Visibility(RouteVisibilityArgs),
    /// Name a route, or remove its name.
    ///
    /// A name selects the same route in every environment and preview, such
    /// as with `env url --route NAME`.
    Name(RouteNameArgs),
    /// Remove a route.
    Remove(RouteTarget),
}
impl RouteCommand {
    /// Lists deployed routes, or edits routes client-side and saves the whole
    /// list: loads the application, appends a route or changes or removes one
    /// (by normalized hostname), then saves against the same loaded generation.
    /// Changing or removing an unknown hostname is an input error.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let target = match self {
            Self::List { app } => return list_routes(client, console, app).await,
            Self::Add(args) => &args.target,
            Self::Redirect(args) => &args.target,
            Self::Visibility(args) => &args.target,
            Self::Name(args) => &args.target,
            Self::Remove(target) => target,
        };
        let current = resolve_application(client, &target.app).await?;
        let mut routes = current.application.to_manifest().spec.routes;
        // Literal hostnames are saved canonically; references as written.
        let hostname = if Template::mentions_reference(&target.hostname) {
            target.hostname.clone()
        } else {
            target.hostname.trim_end_matches('.').to_ascii_lowercase()
        };
        let not_found = || {
            CliError::new(
                ErrorKind::Input,
                format!("route {:?} was not found", target.hostname),
            )
        };
        match self {
            Self::List { .. } => unreachable!("listing returned above"),
            Self::Add(args) => routes.push(
                Route::service(
                    target.hostname.clone(),
                    args.visibility,
                    args.service.clone(),
                    args.port,
                )
                .named(args.name.clone()),
            ),
            Self::Redirect(args) => routes.push(
                Route::redirect(
                    target.hostname.clone(),
                    args.visibility,
                    Redirect {
                        to: args.to.clone(),
                        status: args.status,
                        preserve_path: !args.no_preserve_path,
                    },
                )
                .named(args.name.clone()),
            ),
            Self::Visibility(args) => {
                routes
                    .iter_mut()
                    .find(|route| route.hostname.as_str() == hostname)
                    .ok_or_else(not_found)?
                    .visibility = args.visibility;
            }
            Self::Name(args) => {
                routes
                    .iter_mut()
                    .find(|route| route.hostname.as_str() == hostname)
                    .ok_or_else(not_found)?
                    .name
                    .clone_from(&args.name);
            }
            Self::Remove(_) => {
                let count = routes.len();
                routes.retain(|route| route.hostname.as_str() != hostname);
                if routes.len() == count {
                    return Err(not_found());
                }
            }
        }
        let edit = ApplicationEdit::Routes(routes);
        target
            .flags
            .save_loaded(client, console, &current, &edit)
            .await
    }
}

/// Lists the deployed routes of every environment of `app`, from the
/// daemon's ingress status.
async fn list_routes(client: &Client, console: &mut Console, app: &str) -> Result<()> {
    let application = resolve_application(client, app).await?;
    let readiness = client.system_readiness().await?;
    let rows: Vec<_> = readiness
        .ingress
        .routes
        .into_iter()
        .filter_map(|route| {
            let environment = application.environment(&route.environment_id)?;
            Some(RouteRow {
                environment: environment.name.to_string(),
                route,
            })
        })
        .collect();
    console.emit(&rows)
}
