use super::*;
use piqueld::api::ApplicationError;
use piqueld::store::{PromotionError, StoreError};
use piqueld_core::api::{
    AcceptedPromotion, DeploymentOrigin, DeploymentView, EnvironmentAccess, PromotionSelection,
    SecretAccess, SecretProblem,
};
use piqueld_core::{ReleaseId, manifest::parse_template_toml};

/// `shop`, whose `web` service renders `ORIGIN` from each environment's
/// `domain`, with more `staging` and `production` variables, then `spec`.
/// It declares `staging`, and `production` promoted from it.
fn shop_toml(staging: &str, production: &str, spec: &str) -> String {
    format!(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='shop'\n\
         [spec.environments.staging.variables]\ndomain='staging.example.com'\n{staging}\n\
         [spec.environments.production]\n{PROMOTED}\
         [spec.environments.production.variables]\ndomain='example.com'\n{production}\n\
         [[spec.services]]\nname='web'\n[spec.services.environment]\nORIGIN='https://${{{{ vars.domain }}}}'\n{spec}"
    )
}

/// Declares `production` promoted from `staging` in [`shop_toml`].
const PROMOTED: &str = "promote_from='staging'\n";

/// The image source every `shop` deploys unless a test changes it.
const IMAGE: &str = "[spec.services.source]\ntype='image'\nimage='ghcr.io/example/notes:1.4.0'";

/// `shop` saved from `manifest`, which declares `staging` and
/// `production`.
struct Shop {
    harness: ControllerHarness,
    application: ApplicationId,
    staging: EnvironmentId,
    production: EnvironmentId,
}

impl Shop {
    async fn new(manifest: &str) -> Self {
        let harness = ControllerHarness::new().await;
        // Boxed: saving holds a whole manifest across awaits.
        let saved = Box::pin(
            harness
                .applications()
                .save(parse_template_toml(manifest).unwrap(), Some(0)),
        )
        .await
        .unwrap();
        let application = ApplicationId::parse(saved.application_id).unwrap();
        let environments = harness.store.environments(&application).await.unwrap();
        let named = |name: &str| {
            environments
                .iter()
                .find(|environment| environment.name.as_str() == name)
                .unwrap()
                .id
                .clone()
        };
        let (staging, production) = (named("staging"), named("production"));
        Self {
            harness,
            application,
            staging,
            production,
        }
    }

    /// Saves `manifest` again, converging the environments it declares.
    async fn save(&self, manifest: &str) {
        self.harness
            .applications()
            .save(parse_template_toml(manifest).unwrap(), None)
            .await
            .unwrap();
    }

    /// Deploys `staging` until it succeeds.
    async fn deploy_staging(&self) -> Operation {
        let operation = self
            .harness
            .applications()
            .deploy(&self.staging, None)
            .await
            .unwrap();
        self.harness.finish(&operation).await;
        operation
    }

    /// Promotes `selection` into `production`.
    async fn promote(
        &self,
        selection: PromotionSelection,
    ) -> Result<AcceptedPromotion, ApplicationError> {
        self.harness
            .applications()
            .promote(Daemon, &self.production, selection, None, true, None)
            .await
    }

    /// Promotes the source's current release.
    async fn promote_current(&self) -> Result<AcceptedPromotion, ApplicationError> {
        self.promote(PromotionSelection::Source { deployment: None })
            .await
    }

    /// Promotes the source's current release and runs it until it succeeds.
    async fn promoted(&self) -> AcceptedPromotion {
        let accepted = self.promote_current().await.unwrap();
        let operation = self
            .harness
            .operation(&accepted.operation.operation_id)
            .await;
        self.harness.finish(&operation).await;
        accepted
    }

    async fn deployment(&self, id: &str) -> DeploymentView {
        let environment = self
            .harness
            .store
            .operation(id)
            .await
            .unwrap()
            .environment_id;
        self.harness
            .store
            .deployments(&environment, None, 50)
            .await
            .unwrap()
            .items
            .into_iter()
            .find(|deployment| deployment.operation.id == id)
            .unwrap()
    }

    /// The value `environment`'s running `web` gets for `ORIGIN`, and its image.
    async fn web(&self, environment: &EnvironmentId) -> (String, String) {
        let target = self.harness.target(environment).await;
        let web = &target.services[0];
        (web.environment["ORIGIN"].clone(), web.image.to_string())
    }

    async fn release(&self, deployment: &str) -> ReleaseId {
        self.deployment(deployment).await.release.unwrap()
    }
}

/// The promotion error a request failed with.
fn refusal(result: Result<AcceptedPromotion, ApplicationError>) -> PromotionError {
    match result {
        Err(ApplicationError::Store(StoreError::Promotion(error))) => error,
        other => panic!("promotion refusal expected, got {other:?}"),
    }
}

/// The release staging built is deployed to production as recorded:
/// rendered with production's block, its image never pulled or built
/// again, except a missing registry image, pulled again by digest at
/// acceptance once the request's revision holds. Each deployment records where it came from, and the
/// release where it was promoted.
#[tokio::test]
async fn promotes_the_source_release_rendered_for_the_target_without_building() {
    let shop = Shop::new(&shop_toml("", "", IMAGE)).await;
    let promote = async |generation, request_id| {
        shop.harness
            .applications()
            .promote(
                Daemon,
                &shop.production,
                PromotionSelection::Source { deployment: None },
                Some(generation),
                false,
                request_id,
            )
            .await
    };
    let stale = async || {
        assert!(matches!(
            promote(0, None).await,
            Err(ApplicationError::Store(
                StoreError::GenerationConflict { .. }
            ))
        ));
    };
    // A stale revision is refused first, even before the source deployed.
    stale().await;
    let built = shop.deploy_staging().await;
    let release = shop.release(&built.id).await;
    let image = shop.web(&shop.staging).await.1;
    // Before its first promotion, production shows the manifest the
    // release its source runs would deploy.
    let next = shop
        .harness
        .store
        .release(&shop.application, &release)
        .await
        .unwrap()
        .release
        .template()
        .clone();
    let detail = shop
        .harness
        .applications()
        .environment_detail(&shop.production, &piqueld_core::access::Scope::All)
        .await
        .unwrap();
    assert_eq!(detail.manifest, Some(next));
    let pulls = shop.harness.pulls().await;
    // Cleanup or an operator removed the pulled image meanwhile.
    shop.harness.docker.local_images.lock().await.clear();
    // And before the image is pulled again, like a malformed idempotency
    // key.
    stale().await;
    let generation = shop.harness.store.get(&shop.production).await;
    let generation = generation.unwrap().application.generation;
    assert!(matches!(
        promote(generation, Some("not a key")).await,
        Err(ApplicationError::Store(StoreError::InvalidInput))
    ));
    assert_eq!(shop.harness.pulls().await, pulls);

    let accepted = shop.promoted().await;
    assert_eq!(accepted.release_id, release);
    let origin = DeploymentOrigin::Promotion {
        environment: shop.staging.clone(),
        deployment: built.id.clone(),
    };
    assert_eq!(accepted.origin, origin);
    assert_eq!(shop.harness.pulls().await, pulls + 1);
    assert!(!shop.harness.docker.local_images.lock().await.is_empty());

    assert_eq!(
        shop.web(&shop.production).await,
        ("https://example.com".to_owned(), image.clone())
    );
    assert_eq!(
        shop.web(&shop.staging).await,
        ("https://staging.example.com".to_owned(), image)
    );
    let promoted = shop.deployment(&accepted.operation.operation_id).await;
    assert_eq!(promoted.release, Some(release.clone()));
    assert_eq!(promoted.origin, origin);
    assert_eq!(
        shop.deployment(&built.id).await.origin,
        DeploymentOrigin::Build
    );
    let releases = shop
        .harness
        .store
        .releases(&shop.application, None, 20)
        .await
        .unwrap();
    let [listed] = releases.items.as_slice() else {
        panic!("one release expected, got {releases:?}")
    };
    assert_eq!(
        listed
            .promotions
            .iter()
            .map(|promotion| (&promotion.environment_id, &promotion.origin))
            .collect::<Vec<_>>(),
        [(&shop.production, &origin)]
    );
}

/// A later source deployment never retargets an accepted promotion, a
/// reviewed deployment that is no longer current is refused, and an
/// earlier release can be deployed again by ID.
#[tokio::test]
async fn the_release_is_pinned_at_acceptance() {
    let shop = Shop::new(&shop_toml("", "", IMAGE)).await;
    let first = shop.deploy_staging().await;
    let release = shop.release(&first.id).await;
    let applications = shop.harness.applications();
    let promote = |request| {
        applications.promote(
            Daemon,
            &shop.production,
            PromotionSelection::Source {
                deployment: Some(first.id.clone()),
            },
            None,
            true,
            Some(request),
        )
    };
    let accepted = promote("promote-first").await.unwrap();

    // The tag now resolves to another image, so staging's next deployment
    // records another release. Running it also runs the promotion.
    shop.harness
        .docker
        .registry
        .lock()
        .await
        .digests
        .insert("ghcr.io/example/notes:1.4.0".into(), "b".repeat(64));
    let second = shop.deploy_staging().await;
    assert_ne!(shop.release(&second.id).await, release);
    let promoted = shop
        .harness
        .operation(&accepted.operation.operation_id)
        .await;
    assert_eq!(promoted.state, OperationState::Succeeded);
    assert_eq!(
        shop.deployment(&promoted.id).await.release,
        Some(release.clone())
    );
    assert!(
        shop.web(&shop.production)
            .await
            .1
            .ends_with(&"a".repeat(64))
    );

    assert!(matches!(
        refusal(
            shop.promote(PromotionSelection::Source {
                deployment: Some(first.id.clone()),
            })
            .await
        ),
        PromotionError::SourceChanged { deployment, .. } if deployment == first.id
    ));
    // Repeating the accepted request returns its response all the same.
    let repeated = promote("promote-first").await.unwrap();
    assert_eq!(
        repeated.operation.operation_id,
        accepted.operation.operation_id
    );

    let again = shop
        .promote(PromotionSelection::Release {
            release: release.clone(),
        })
        .await
        .unwrap();
    let operation = shop.harness.operation(&again.operation.operation_id).await;
    shop.harness.finish(&operation).await;
    assert_eq!(again.origin, DeploymentOrigin::Release);
    assert_eq!(
        shop.deployment(&operation.id).await.origin,
        DeploymentOrigin::Release
    );
    assert!(
        shop.web(&shop.production)
            .await
            .1
            .ends_with(&"a".repeat(64))
    );
}

/// Every precondition refuses the promotion before anything is captured:
/// a source without a succeeded deployment or with unhealthy services, a
/// build input that renders differently, and missing or inaccessible
/// secrets, all listed at once (also by the plan).
#[tokio::test]
async fn preconditions_are_checked_before_anything_is_captured() {
    let manifest = shop_toml(
        "api='staging-api'",
        "api='production-api'",
        &format!(
            "{IMAGE}\n[[spec.services.secrets]]\nname='${{{{ vars.api }}}}'\ntarget='/run/secrets/api'\n\
             [[spec.services.secrets]]\nname='${{{{ vars.db }}}}'\ntarget='/run/secrets/db'\n\
             [spec.variables]\ndb='shared-db'"
        ),
    );
    let shop = Shop::new(&manifest).await;
    assert!(matches!(
        refusal(shop.promote_current().await),
        PromotionError::SourceNotReady { .. }
    ));
    let applications = shop.harness.applications();
    applications
        .put_stored_secret(
            Daemon,
            &shop.application,
            "staging-api",
            0,
            b"api".to_vec(),
            None,
        )
        .await
        .unwrap();
    let staging_only = SecretAccess {
        environments: EnvironmentAccess::Only([shop.staging.clone()].into()),
        previews: false,
    };
    applications
        .put_stored_secret(
            Daemon,
            &shop.application,
            "shared-db",
            0,
            b"db".to_vec(),
            Some(&staging_only),
        )
        .await
        .unwrap();
    shop.deploy_staging().await;

    let expected = [
        SecretProblem::Missing {
            secret: "production-api".into(),
        },
        SecretProblem::AccessDenied {
            secret: "shared-db".into(),
        },
    ];
    match refusal(shop.promote_current().await) {
        PromotionError::Secrets { secrets, .. } => assert_eq!(secrets, expected),
        other => panic!("secrets refusal expected, got {other:?}"),
    }
    let plan = applications
        .plan_release(
            &shop.production,
            &PromotionSelection::Source { deployment: None },
        )
        .await
        .unwrap();
    assert_eq!(plan.release.unwrap().secrets, expected);

    // Unhealthy right now, though its deployment succeeded; then healthy,
    // but running another image than its release's.
    shop.harness.docker.observed.lock().await.services[0].convergence = Convergence::Degraded;
    assert!(matches!(
        refusal(shop.promote_current().await),
        PromotionError::SourceNotReady { .. }
    ));
    {
        let mut observed = shop.harness.docker.observed.lock().await;
        let web = &mut observed.services[0];
        web.convergence = Convergence::Converged;
        web.image = format!("ghcr.io/example/notes@sha256:{}", "f".repeat(64));
    }
    assert!(matches!(
        refusal(shop.promote_current().await),
        PromotionError::SourceNotReady { .. }
    ));
    assert!(
        shop.harness
            .store
            .latest_operation_for_environment(&shop.production)
            .await
            .unwrap()
            .is_none()
    );
}

/// The environment's declared secrets are generated for a promotion, but
/// stored only once it is accepted.
#[tokio::test]
async fn generated_secrets_are_stored_with_the_accepted_promotion() {
    let manifest = shop_toml(
        "",
        "",
        &format!(
            "{IMAGE}\n[[spec.services.secrets]]\nname='session'\ntarget='/run/secrets/session'\n\
             [[spec.secrets]]\nname='session'\n[spec.secrets.generate]\ntype='random'\nbytes=16"
        ),
    );
    let shop = Shop::new(&manifest).await;
    shop.deploy_staging().await;
    let applications = shop.harness.applications();
    let current = PromotionSelection::Source { deployment: None };
    assert!(matches!(
        applications
            .promote(
                Daemon,
                &shop.production,
                current.clone(),
                Some(0),
                false,
                None
            )
            .await,
        Err(ApplicationError::Store(
            StoreError::GenerationConflict { .. }
        ))
    ));
    assert!(
        shop.harness
            .store
            .secrets(&shop.production)
            .await
            .unwrap()
            .is_empty()
    );
    shop.promoted().await;
    let stored = shop.harness.store.secrets(&shop.production).await.unwrap();
    assert_eq!(
        stored
            .iter()
            .map(|secret| secret.name.as_str())
            .collect::<Vec<_>>(),
        ["session"]
    );
    // Planning the same release again changes nothing: services keep the
    // secret version they run.
    let plan = applications
        .plan_release(&shop.production, &current)
        .await
        .unwrap();
    assert!(plan.identical, "{:?}", plan.changes);
    assert!(
        !plan
            .plan
            .actions
            .iter()
            .any(|action| matches!(action.kind, piqueld_core::ActionKind::EnsureService { .. })),
        "{:?}",
        plan.plan.actions
    );
}

/// A build input production renders differently can't reuse the
/// release's image.
#[tokio::test]
async fn a_release_whose_build_inputs_render_differently_is_refused() {
    let tagged = shop_toml(
        "",
        "tag='1.5.0'",
        "[spec.services.source]\ntype='image'\nimage='ghcr.io/example/notes:${{ vars.tag }}'\n\
         [spec.variables]\ntag='1.4.0'",
    );
    let shop = Shop::new(&tagged).await;
    shop.deploy_staging().await;
    match shop.promote_current().await {
        Err(ApplicationError::Store(StoreError::Validation(errors))) => assert_eq!(
            errors
                .0
                .iter()
                .map(|error| (error.code.as_str(), error.path.as_str()))
                .collect::<Vec<_>>(),
            [("release_incompatible", "web.source.image")]
        ),
        other => panic!("release_incompatible expected, got {other:?}"),
    }
}

/// An environment can't become promoted while a deployment may be
/// fetching or building: its declaration waits for the next convergence.
/// Retrying a deployment that never prepared fails instead of fetching or
/// building once it is promoted.
#[tokio::test]
async fn a_promoted_environment_never_builds_a_deployment_requested_before() {
    let promoted = shop_toml("", "", IMAGE);
    let tracking = promoted.replace(PROMOTED, "");
    let shop = Shop::new(&tracking).await;
    let requested = shop
        .harness
        .applications()
        .deploy(&shop.production, None)
        .await
        .unwrap();
    let source = || async {
        shop.harness
            .store
            .get(&shop.production)
            .await
            .unwrap()
            .environment
            .source
            .promoted_from()
            .cloned()
    };
    shop.save(&promoted).await;
    assert_eq!(source().await, None);

    // It fails before preparing anything, then the environment is promoted.
    shop.harness
        .docker
        .incompatible_swarm
        .store(true, Ordering::SeqCst);
    shop.harness.scan().await;
    let failed = shop.harness.operation(&requested.id).await;
    assert_eq!(failed.state, OperationState::Failed);
    shop.harness
        .docker
        .incompatible_swarm
        .store(false, Ordering::SeqCst);
    shop.save(&promoted).await;
    assert_eq!(source().await, Some(shop.staging.clone()));

    let pulls = shop.harness.pulls().await;
    let retried = shop
        .harness
        .applications()
        .reconcile(&shop.production, None)
        .await
        .unwrap();
    assert_eq!(retried.id, requested.id);
    shop.harness.scan().await;
    let failed = shop.harness.operation(&requested.id).await;
    assert_eq!(failed.state, OperationState::Failed);
    assert_eq!(failed.error_code.as_deref(), Some("environment_promoted"));
    assert_eq!(shop.harness.pulls().await, pulls);
}

/// An environment made promoted still repairs drift in the build it
/// runs: converging its active target again rolls nothing new in.
#[tokio::test]
async fn a_promoted_environment_repairs_the_build_it_runs() {
    let promoted = shop_toml("", "", IMAGE);
    let tracking = promoted.replace(PROMOTED, "");
    let shop = Shop::new(&tracking).await;
    let deployed = shop
        .harness
        .applications()
        .deploy(&shop.production, None)
        .await
        .unwrap();
    let deployed = shop.harness.operation(&deployed.id).await;
    shop.harness.finish(&deployed).await;
    shop.save(&promoted).await;
    let production = shop.production.to_string();
    let web = async || {
        let mut observed = shop.harness.docker.observed.lock().await;
        let web = observed
            .services
            .iter_mut()
            .find(|service| service.labels.get(APPLICATION_LABEL) == Some(&production))
            .unwrap();
        let replicas = web.replicas;
        web.replicas = 0;
        web.convergence = Convergence::Degraded;
        replicas
    };
    web().await;
    shop.harness.finish(&deployed).await;
    assert_eq!(web().await, 1);
}

/// A build that prepared its target before the environment was made
/// promoted doesn't roll out into it when retried: a promoted
/// environment only runs releases.
#[tokio::test]
async fn a_promoted_environment_never_rolls_out_a_build_prepared_before() {
    let promoted = shop_toml("", "", IMAGE);
    let tracking = promoted.replace(PROMOTED, "");
    let mut shop = Shop::new(&tracking).await;
    shop.harness.set_convergence_timeout(Duration::from_secs(1));
    let requested = shop
        .harness
        .applications()
        .deploy(&shop.production, None)
        .await
        .unwrap();
    // It prepares, then fails before rolling out.
    shop.harness
        .docker
        .fail_observations
        .store(true, Ordering::SeqCst);
    shop.harness.scan().await;
    assert_eq!(
        shop.harness.operation(&requested.id).await.state,
        OperationState::Failed
    );
    assert!(
        shop.harness
            .store
            .prepared_target(&requested.id)
            .await
            .unwrap()
            .is_some()
    );
    shop.harness
        .docker
        .fail_observations
        .store(false, Ordering::SeqCst);
    shop.save(&promoted).await;

    let retried = shop
        .harness
        .applications()
        .reconcile(&shop.production, None)
        .await
        .unwrap();
    assert_eq!(retried.id, requested.id);
    shop.harness.scan().await;
    let failed = shop.harness.operation(&requested.id).await;
    assert_eq!(failed.state, OperationState::Failed);
    assert_eq!(failed.error_code.as_deref(), Some("environment_promoted"));
    assert!(
        shop.harness
            .store
            .get(&shop.production)
            .await
            .unwrap()
            .resolved
            .is_none()
    );
}
