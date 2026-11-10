//! Applications with several environments: independent deploys, shared history and environment branches.

use super::*;

/// Environments refuse an invalid branch or commit like previews do, as a
/// malformed request field rather than a manifest validation failure.
#[tokio::test]
async fn environment_branches_are_validated_like_preview_branches() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::tcp(router(state(&temp).await, FakeAuth)).await;
    let saved = server
        .client()
        .apply_and_deploy(&new_application())
        .await
        .unwrap();
    let (application, environment) = (&saved.application_id, &saved.environment_id);
    let json = [("content-type", "application/json")];
    for (method, path, body, code) in [
        (
            Method::POST,
            format!("/api/v1/applications/{application}/environments"),
            r#"{"name":"staging","branch":"bad..branch","expected_generation":1}"#,
            "git_branch_invalid",
        ),
        (
            Method::PUT,
            format!("/api/v1/environments/{environment}/branch"),
            r#"{"branch":"main","commit":"abc","expected_generation":1}"#,
            "git_commit_invalid",
        ),
    ] {
        let response = server.send(method, &path, &json, body.into()).await;
        response.assert_error(StatusCode::BAD_REQUEST, code);
    }
}

#[tokio::test]
async fn environments_deploy_independently_and_runtime_commands_never_pick_one() {
    use piqueld::api::{ApplicationError, Mutation, MutationResponse};
    use piqueld::store::StoreError;
    use piqueld_core::EnvironmentName;
    let temp = tempfile::tempdir().unwrap();
    let service = state(&temp).await;
    let (saved, staging) = two_environments(&service).await;
    let application = piqueld_core::ApplicationId::parse(&saved.application_id).unwrap();
    let view = service.application(&application).await.unwrap();
    assert_eq!(view.generation, saved.generation);
    let production = view.environment("production").unwrap().id.clone();
    assert!(production.as_str().starts_with("env-"), "{production}");
    assert_eq!(view.sole_environment().unwrap_err().len(), 2);

    // Saving with a deployment never picks one of several environments.
    assert!(matches!(
        service
            .accept(Daemon,
                Mutation::save(
                    manifest().validate_template().unwrap(),
                    Some(saved.application_id.clone()),
                    true,
                ),
                Some(saved.generation),
                false,
                None,
            )
            .await,
        Err(ApplicationError::Store(StoreError::EnvironmentRequired { environments }))
            if environments.len() == 2
    ));

    let MutationResponse::Operation(deployed) = service
        .accept(
            Daemon,
            Mutation::deploy(staging.id.clone()),
            Some(saved.generation),
            false,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("deployment")
    };
    assert_eq!(deployed.environment_id, staging.id.as_str());
    assert!(
        service
            .environment_detail(&production, &piqueld_core::access::Scope::All)
            .await
            .unwrap()
            .latest_operation
            .is_none()
    );
    assert_eq!(
        service
            .environment_detail(&staging.id, &piqueld_core::access::Scope::All)
            .await
            .unwrap()
            .latest_operation
            .unwrap()
            .id,
        deployed.operation_id
    );

    // Deleting the application names every environment.
    let delete = |names: &[&str]| Mutation::DeleteApplication {
        id: application.clone(),
        environments: names
            .iter()
            .map(|name| EnvironmentName::parse(*name).unwrap())
            .collect(),
    };
    assert!(matches!(
        service
            .accept(
                Daemon,
                delete(&["production"]),
                Some(saved.generation),
                false,
                None
            )
            .await,
        Err(ApplicationError::Store(
            StoreError::ConfirmationRequired { .. }
        ))
    ));
    let MutationResponse::Deleted(deleted) = service
        .accept(
            Daemon,
            delete(&["staging", "production"]),
            Some(saved.generation),
            false,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("deleted")
    };
    assert_eq!(deleted.operations.len(), 2);
    assert_eq!(deleted.generation, saved.generation + 1);
}

#[tokio::test]
async fn application_history_spans_its_environments() {
    use piqueld::api::Mutation;
    use piqueld_core::observability::EventFilter;
    let temp = tempfile::tempdir().unwrap();
    let service = state(&temp).await;
    let (saved, staging) = two_environments(&service).await;
    let application = piqueld_core::ApplicationId::parse(&saved.application_id).unwrap();
    let production = production(&service, &saved.application_id).await;
    service
        .accept(
            Daemon,
            Mutation::Rename {
                id: application.clone(),
                name: "renamed".into(),
            },
            Some(saved.generation),
            false,
            None,
        )
        .await
        .unwrap();
    service
        .accept(
            Daemon,
            Mutation::deploy(production.clone()),
            Some(saved.generation + 1),
            false,
            None,
        )
        .await
        .unwrap();
    let events = |filter: EventFilter| {
        let service = service.clone();
        async move {
            service
                .filtered_events(&filter, &piqueld::store::Visibility::ALL, None, 100)
                .await
                .unwrap()
                .items
                .into_iter()
                .map(|event| (event.kind, event.environment_id))
                .collect::<Vec<_>>()
        }
    };

    // Application-wide events are recorded once, without an environment, and
    // the application's history includes every environment's events.
    let history = events(EventFilter {
        application_id: Some(application.to_string()),
        ..EventFilter::default()
    })
    .await;
    assert!(history.contains(&("environment_created".into(), Some(staging.id.clone()))));
    assert_eq!(
        history
            .iter()
            .filter(|(kind, _)| kind == "application_renamed")
            .collect::<Vec<_>>(),
        [&("application_renamed".to_owned(), None)]
    );
    let production_history = events(EventFilter {
        environment_id: Some(production.to_string()),
        ..EventFilter::default()
    })
    .await;
    let in_production = |(_, environment): &(String, Option<piqueld_core::EnvironmentId>)| {
        environment.as_ref() == Some(&production)
    };
    assert!(production_history.iter().any(in_production));
    assert!(production_history.iter().all(in_production));
}

#[tokio::test]
async fn previews_compare_with_the_selected_environment() {
    use piqueld::api::{ApplicationError, Mutation};
    use piqueld::store::StoreError;
    use piqueld_core::api::RuntimeCoverage;
    let temp = tempfile::tempdir().unwrap();
    let service = state(&temp).await;
    let (saved, staging) = two_environments(&service).await;
    let production = production(&service, &saved.application_id).await;
    service
        .accept(
            Daemon,
            Mutation::deploy(staging.id.clone()),
            Some(saved.generation),
            false,
            None,
        )
        .await
        .unwrap();
    let plan = |environment: Option<piqueld_core::EnvironmentId>| {
        let service = service.clone();
        async move {
            service
                .plan(
                    &piqueld_core::access::Grants::admin(),
                    manifest().validate_template().unwrap(),
                    None,
                    None,
                    environment.as_ref(),
                )
                .await
        }
    };

    // Without a selection, several environments leave no runtime baseline.
    let unselected = plan(None).await.unwrap();
    assert!(unselected.operation.is_none() && unselected.plan.actions.is_empty());
    assert_eq!(unselected.runtime_coverage, RuntimeCoverage::Omitted);

    let deployed = plan(Some(staging.id.clone())).await.unwrap();
    assert!(deployed.identical);
    assert!(deployed.operation.is_some());
    let resolves_image = |plan: &piqueld_core::api::PlanView| {
        plan.plan
            .actions
            .iter()
            .any(|action| matches!(action.kind, ActionKind::ResolveImage { .. }))
    };
    assert!(resolves_image(&deployed));
    // The image tag must be resolved before the rest can be planned.
    assert_eq!(deployed.runtime_coverage, RuntimeCoverage::Resolution);

    let undeployed = plan(Some(production)).await.unwrap();
    assert!(!undeployed.identical && undeployed.operation.is_none());
    assert!(!undeployed.changes.is_empty() && resolves_image(&undeployed));

    // An environment of another application is never used as a baseline.
    let mut other = manifest();
    other.metadata.name = "other".into();
    save(&service, other).await;
    let foreign = service
        .applications(&piqueld_core::access::Scope::All, None, None, None)
        .await
        .unwrap()
        .items;
    let foreign = foreign
        .iter()
        .find(|application| application.name == "other")
        .unwrap()
        .environments[0]
        .id
        .clone();
    assert!(matches!(
        plan(Some(foreign)).await,
        Err(ApplicationError::Store(StoreError::NotFound))
    ));
}

#[tokio::test]
async fn previews_render_variables_for_the_selected_environment() {
    use piqueld::api::ApplicationError;
    use piqueld::store::StoreError;
    use piqueld_core::manifest::{EnvironmentConfig, Variable, VariableValue};
    let temp = tempfile::tempdir().unwrap();
    let service = state(&temp).await;
    let (saved, staging) = two_environments(&service).await;
    let production = production(&service, &saved.application_id).await;
    let level = |value: &str| EnvironmentConfig {
        variables: [("level".into(), Variable::String(value.into()))].into(),
        ..EnvironmentConfig::default()
    };
    let mut manifest = manifest();
    manifest.spec.services[0]
        .environment
        .insert("LEVEL".into(), "${{ vars.level }}".into());
    manifest.spec.environments.extend([
        ("staging".into(), level("debug")),
        ("preview".into(), level("trace")),
    ]);
    let plan = |environment: piqueld_core::EnvironmentId| {
        let (service, manifest) = (service.clone(), manifest.clone());
        async move {
            service
                .plan(
                    &piqueld_core::access::Grants::admin(),
                    manifest.validate_template().unwrap(),
                    None,
                    None,
                    Some(&environment),
                )
                .await
        }
    };

    let rendered = plan(staging.id.clone()).await.unwrap();
    assert_eq!(
        rendered.variables["vars.level"],
        VariableValue::String("debug".into())
    );
    assert_eq!(
        rendered.variables["env.name"],
        VariableValue::String("staging".into())
    );
    // Blocks declare environments: saving it would create `preview`, and
    // `production` would no longer be declared.
    let name = |name: &str| piqueld_core::EnvironmentName::parse(name).unwrap();
    assert_eq!(
        rendered.environments,
        [
            piqueld_core::manifest::EnvironmentChange::Create {
                environment: name("preview"),
                source: piqueld_core::manifest::DeclaredSource::Saved,
                sync: false,
            },
            piqueld_core::manifest::EnvironmentChange::Undeclared {
                environment: name("production"),
            },
        ]
    );
    assert!(!rendered.plan.is_blocked());

    // Production has no value for `level`.
    let Err(ApplicationError::Store(StoreError::Validation(errors))) = plan(production).await
    else {
        panic!("missing value expected")
    };
    assert_eq!(
        (errors.0[0].code.as_str(), errors.0[0].path.as_str()),
        (
            piqueld_core::codes::VARIABLE_VALUE_MISSING,
            "spec.services[0].environment.LEVEL"
        )
    );
}
