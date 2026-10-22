//! Deployment results for agents, shared by `env` and `preview`: what every
//! deploy and create result carries, `wait` for a deployment to be ready, and
//! `url` to read its URLs. Readiness comes from the daemon; the CLI never
//! probes a URL.
use crate::{
    commands::{Settled, finish_operation, wait_for_operation, wait_for_operation_until},
    error::{CliCode, CliError, ErrorKind, Result},
    output::{
        Console,
        reports::{AcceptedDeploymentReport, Named, OperationOutcomeReport},
    },
    support::{join_or, retry_transport},
};
use clap::{Args, ValueEnum};
use piqueld_client::{
    AcceptedOperation, ApplicationId, ApplicationView, Client, DeploymentOrigin, DeploymentView,
    EnvironmentDetailView, EnvironmentId, EnvironmentName, EnvironmentView, LocalSnapshot,
    Operation, OperationKind, OperationState, ReleaseId, RouteUrl, SystemVariable, UrlState,
};
use serde::Serialize;
use serde_json::json;

/// One deployment of an environment or preview, in the fields agents need.
/// Deploy, create, and `wait` results flatten it into their own objects.
#[derive(Serialize)]
pub(crate) struct Deployed {
    pub(crate) application_id: ApplicationId,
    pub(crate) environment_id: EnvironmentId,
    /// The environment's name, or the preview's slug: `env.slug`.
    pub(crate) slug: EnvironmentName,
    /// Deployments are identified by their operation.
    pub(crate) deployment_id: String,
    pub(crate) operation_id: String,
    /// The release its prepared target runs; null until prepared, and for previews.
    pub(crate) release_id: Option<ReleaseId>,
    /// Branch and commit its manifest was read from; null until fetched, and
    /// for saved manifests. For a local snapshot, the snapshot commit.
    pub(crate) branch: Option<String>,
    pub(crate) commit: Option<String>,
    /// Whether it deploys a snapshot of a local worktree.
    pub(crate) local: bool,
    /// The commit of the application's repository a local snapshot was
    /// uploaded against; null for complete uploads and other deployments.
    pub(crate) base: Option<String>,
    /// How it ended; omitted with `--no-wait`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) outcome: Option<OperationState>,
    /// Its URLs and their state, once it runs; omitted with `--no-wait` and
    /// when it does not run (superseded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) urls: Option<Vec<RouteUrl>>,
}

impl Deployed {
    /// A deployment accepted but not awaited: nothing is fetched yet.
    pub(crate) fn accepted(environment: &EnvironmentView, operation_id: &str) -> Self {
        Self {
            application_id: environment.application_id.clone(),
            environment_id: environment.id.clone(),
            slug: environment.name.clone(),
            deployment_id: operation_id.to_owned(),
            operation_id: operation_id.to_owned(),
            release_id: None,
            branch: None,
            commit: None,
            local: false,
            base: None,
            outcome: None,
            urls: None,
        }
    }

    /// Marks the deployment as one of `snapshot`, uploaded from a local
    /// worktree of `branch`.
    pub(crate) fn snapshot(self, branch: &str, snapshot: &LocalSnapshot) -> Self {
        Self {
            branch: Some(branch.to_owned()),
            commit: Some(snapshot.commit.clone()),
            local: true,
            base: snapshot.base.clone(),
            ..self
        }
    }

    /// A finished deployment: its release and revision, the local snapshot
    /// it runs, restored or not, and, when `detail` shows it runs, its URLs.
    async fn finished(
        client: &Client,
        environment: &EnvironmentView,
        deployment: &DeploymentView,
        detail: &EnvironmentDetailView,
    ) -> Result<Self> {
        let id = &deployment.operation.id;
        let mut origin = deployment.origin.clone();
        // Restores run what they restored, which may be a restore too.
        while let DeploymentOrigin::Restore { deployment } = &origin {
            origin = find_deployment(client, environment, deployment)
                .await?
                .origin;
        }
        let snapshot = match origin {
            DeploymentOrigin::Local(snapshot) => Some(snapshot),
            _ => None,
        };
        Ok(Self {
            local: snapshot.is_some(),
            base: snapshot.and_then(|snapshot| snapshot.base),
            release_id: deployment.release.clone(),
            branch: deployment
                .system_value(SystemVariable::GitBranch)
                .map(str::to_owned),
            commit: deployment
                .system_value(SystemVariable::GitSha)
                .map(str::to_owned),
            outcome: Some(deployment.operation.state),
            urls: (deployment.operation.state == OperationState::Succeeded && runs(detail, id))
                .then(|| detail.urls.clone())
                .flatten(),
            ..Self::accepted(environment, id)
        })
    }

    /// Reads the deployment that ended as `operation`, which decided the
    /// command's outcome, and the environment's detail, for [`Self::finished`].
    pub(crate) async fn read(
        client: &Client,
        environment: &EnvironmentView,
        operation: Operation,
    ) -> Result<Self> {
        let mut deployment = find_deployment(client, environment, &operation.id).await?;
        deployment.operation = operation;
        let detail = client.environment_detail(environment.id.as_str()).await?;
        Self::finished(client, environment, &deployment, &detail).await
    }
}

/// Whether the environment's runtime target is `deployment`'s: no newer
/// operation was accepted since. Every operation but a deletion is a
/// deployment, and a deletion ends the deployment too.
fn runs(detail: &EnvironmentDetailView, deployment: &str) -> bool {
    detail
        .latest_operation
        .as_ref()
        .is_some_and(|latest| latest.id == deployment)
}

/// The detail's URLs. Daemons that predate URL readiness report none, which
/// must not read as "no routes".
fn urls(detail: &EnvironmentDetailView) -> Result<&[RouteUrl]> {
    detail.urls.as_deref().ok_or_else(|| {
        CliError::new(
            ErrorKind::General,
            "the daemon does not report URL readiness; upgrade it",
        )
    })
}

/// Finds a deployment of `environment` by ID, newest pages first.
async fn find_deployment(
    client: &Client,
    environment: &EnvironmentView,
    id: &str,
) -> Result<DeploymentView> {
    let mut cursor = None;
    loop {
        let page = client
            .deployments(environment.id.as_str(), cursor.as_deref())
            .await?;
        if let Some(deployment) = page
            .items
            .into_iter()
            .find(|deployment| deployment.operation.id == id)
        {
            return Ok(deployment);
        }
        cursor = Some(page.next_cursor.ok_or_else(|| {
            CliError::new(
                ErrorKind::Input,
                format!("deployment {id:?} of {} was not found", environment.name),
            )
        })?);
    }
}

/// Emits an accepted deployment as is with `no_wait`, otherwise waits for it
/// and emits how it ended.
pub(crate) async fn wait_for_accepted(
    console: &mut Console,
    client: &Client,
    environment: &EnvironmentView,
    no_wait: bool,
    accepted: &AcceptedOperation,
) -> Result<()> {
    if no_wait {
        return console.emit(&AcceptedDeploymentReport {
            generation: accepted.generation,
            deployed: Deployed::accepted(environment, &accepted.operation_id),
        });
    }
    let operation = wait_for_operation(console, client, &accepted.operation_id).await?;
    console.emit(&OperationOutcomeReport {
        accepted,
        deployed: Deployed::read(client, environment, operation.clone()).await?,
        operation,
    })
}

/// How ready `wait` waits for a deployment to be.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum Readiness {
    /// The deployment succeeded, and its runtime is observed with every
    /// service healthy.
    Runtime,
    /// Runtime, and every URL of its rendered routes is ready.
    Routes,
}

impl Readiness {
    /// What `detail` still lacks, or nothing once it is this ready.
    fn missing(self, detail: &EnvironmentDetailView) -> Result<Option<String>> {
        if !detail.runtime_ready() {
            return Ok(Some(
                "waiting for the runtime to be observed healthy".into(),
            ));
        }
        if matches!(self, Self::Runtime) {
            return Ok(None);
        }
        let pending: Vec<_> = urls(detail)?
            .iter()
            .filter(|url| url.state == UrlState::Pending)
            .map(|url| url.url.as_str())
            .collect();
        Ok((!pending.is_empty()).then(|| format!("waiting for {}", pending.join(", "))))
    }
}

/// Selects the environment or preview a command acts on.
pub(crate) trait Target {
    /// Loads the application and selects the environment or preview.
    async fn resolve(&self, client: &Client) -> Result<(ApplicationView, EnvironmentView)>;

    /// The selected environment or preview.
    async fn environment(&self, client: &Client) -> Result<EnvironmentView> {
        Ok(self.resolve(client).await?.1)
    }
}

// `wait` arguments shared by `env wait` and `preview wait`.
#[derive(Debug, Args)]
pub(crate) struct WaitArgs<T: Args> {
    #[command(flatten)]
    target: T,
    /// Deployment to wait for; by default the latest one when `wait` starts.
    /// A newer deployment ends the wait with exit code 3.
    #[arg(long)]
    deployment: Option<String>,
    /// `runtime`: it succeeded and its runtime is observed with every service
    /// healthy;
    /// `routes`: also every URL of its routes is ready.
    #[arg(long, value_enum, default_value_t = Readiness::Runtime)]
    ready: Readiness,
}

impl<T: Args + Target> WaitArgs<T> {
    /// Waits for one deployment of the target to be ready, then emits it.
    /// Never follows a newer deployment: if one supersedes it, fails with a
    /// conflict. A failed deployment fails with an operation error, and the
    /// command `--timeout` ends only this wait.
    pub(crate) async fn run(&self, console: &mut Console, client: &Client) -> Result<()> {
        let environment = &self.target.environment(client).await?;
        let id = self.deployment(client, environment).await?;
        let (deployment, detail) =
            wait_for_operation_until(console, client, &id, async |operation| {
                if operation.state == OperationState::Superseded {
                    return Err(superseded(&id, None));
                }
                // Read before the detail, so the detail is the newest evidence.
                let mut deployment = find_deployment(client, environment, &id).await?;
                let detail = client.environment_detail(environment.id.as_str()).await?;
                // The detail's copy of the operation is the one its runtime
                // state goes with: a retry may have started or failed since.
                let latest = match &detail.latest_operation {
                    Some(latest) if latest.id == id => latest.clone(),
                    newer => {
                        let newer = newer.as_ref().map(|latest| latest.id.as_str());
                        return Err(superseded(&id, newer));
                    }
                };
                if !latest.state.terminal() {
                    return Ok(Settled::Waiting(format!("{} again", latest.state)));
                }
                let latest = finish_operation(latest)?;
                if latest.state == OperationState::Superseded {
                    return Err(superseded(&id, None));
                }
                deployment.operation = latest;
                Ok(match self.ready.missing(&detail)? {
                    Some(missing) => Settled::Waiting(missing),
                    None => Settled::Done((deployment, detail)),
                })
            })
            .await?;
        console.emit(&Deployed::finished(client, environment, &deployment, &detail).await?)
    }

    /// The deployment to wait for: `--deployment`, checked to be one of
    /// `environment`'s, or its latest one.
    async fn deployment(&self, client: &Client, environment: &EnvironmentView) -> Result<String> {
        let Some(id) = &self.deployment else {
            let page = client.deployments(environment.id.as_str(), None).await?;
            return page
                .items
                .into_iter()
                .next()
                .map(|deployment| deployment.operation.id)
                .ok_or_else(|| {
                    CliError::new(
                        ErrorKind::Input,
                        format!("{} has no deployment to wait for", environment.name),
                    )
                });
        };
        let operation = client.operation(id).await?;
        if operation.environment_id != environment.id || operation.kind == OperationKind::Delete {
            return Err(CliError::new(
                ErrorKind::Input,
                format!("{id:?} is not a deployment of {}", environment.name),
            ));
        }
        Ok(id.clone())
    }
}

// `history` arguments shared by `env history` and `preview history`.
#[derive(Debug, Args)]
pub(crate) struct HistoryArgs<T: Args> {
    #[command(flatten)]
    target: T,
    /// Cursor from the previous page.
    #[arg(long)]
    cursor: Option<String>,
}

impl<T: Args + Target> HistoryArgs<T> {
    /// Emits one page of the target's deployments, newest first.
    pub(crate) async fn run(&self, console: &mut Console, client: &Client) -> Result<()> {
        let (application, environment) = self.target.resolve(client).await?;
        let page = client
            .deployments(environment.id.as_str(), self.cursor.as_deref())
            .await?;
        console.emit(&Named::new(&page, &application))
    }
}

// `restore` arguments shared by `env restore` and `preview restore`.
#[derive(Debug, Args)]
pub(crate) struct RestoreArgs<T: Args> {
    #[command(flatten)]
    target: T,
    /// The deployment to restore, as `history` lists it; it must have succeeded.
    deployment: String,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    yes: bool,
    /// Return after the daemon accepts the restore.
    #[arg(long)]
    no_wait: bool,
}

impl<T: Args + Target> RestoreArgs<T> {
    /// Confirms and restores the deployment exactly as it ran, as a new
    /// deployment, then waits for it unless `--no-wait`.
    pub(crate) async fn run(&self, console: &mut Console, client: &Client) -> Result<()> {
        let environment = self.target.environment(client).await?;
        console
            .confirm(
                self.yes,
                &format!(
                    "Restore deployment {} of {}? Its jobs don't run again, and volumes and migrations stay as they are.",
                    self.deployment, environment.name
                ),
            )
            .await?;
        let accepted =
            retry_transport(|| client.restore_deployment(&environment, &self.deployment)).await?;
        wait_for_accepted(console, client, &environment, self.no_wait, &accepted).await
    }
}

/// `deployment` no longer runs: a newer operation was accepted.
fn superseded(deployment: &str, newer: Option<&str>) -> CliError {
    let by = newer.map_or_else(String::new, |newer| format!(" by {newer}"));
    CliError::new(
        ErrorKind::Conflict,
        format!("deployment {deployment} was superseded{by}"),
    )
    .code(CliCode::DeploymentSuperseded)
    .with_details(json!({"deployment_id": deployment, "superseded_by": newer}))
}

// `url` arguments shared by `env url` and `preview url`.
#[derive(Debug, Args)]
pub(crate) struct UrlArgs<T: Args> {
    #[command(flatten)]
    target: T,
    /// Only the URL of the route with this name.
    #[arg(long)]
    route: Option<String>,
}

impl<T: Args + Target> UrlArgs<T> {
    /// Emits the URLs of the target's rendered routes and their state, as
    /// the daemon derives it, or the one named `--route`, warning while it is
    /// pending.
    pub(crate) async fn run(&self, console: &mut Console, client: &Client) -> Result<()> {
        let environment = &self.target.environment(client).await?;
        let detail = client.environment_detail(environment.id.as_str()).await?;
        let urls = urls(&detail)?;
        let Some(name) = &self.route else {
            return console.emit(&urls.to_vec());
        };
        let url = urls
            .iter()
            .find(|url| {
                url.name
                    .as_ref()
                    .is_some_and(|route| route.as_str() == name)
            })
            .ok_or_else(|| {
                let names = urls.iter().filter_map(|url| url.name.as_ref());
                CliError::new(
                    ErrorKind::Input,
                    format!(
                        "{} renders no route named {name:?}; its named routes: {}",
                        environment.name,
                        join_or(names, "none")
                    ),
                )
            })?;
        console.emit(url)?;
        if url.state == UrlState::Pending {
            console.warning(format_args!(
                "{} is pending: {}",
                url.url,
                url.waiting_for()
            ))?;
        }
        Ok(())
    }
}
