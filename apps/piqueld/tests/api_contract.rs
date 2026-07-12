#![allow(missing_docs)]

use async_trait::async_trait;
use piqueld::api::{
    ApiState, BoundaryError, PreparedApplication, RuntimeBoundary, RuntimeCapabilities, router,
};
use piqueld::store::{SqliteStore, StoredApplication};
use piqueld_client::{
    Client, CreateApplicationRequest, DeleteApplicationRequest, ListApplicationsOptions,
    PlanApplicationRequest, ReplaceApplicationRequest, ReplacePlanRequest,
};
use piqueld_core::{
    InstanceId, NormalizedApplication, ObservedApplication, ResolutionSet, compile_application,
    manifest::{ApplicationManifest, Source},
    resource::ResolvedSource,
};
use std::{collections::BTreeMap, sync::Arc};
use tempfile::TempDir;
use tokio::net::{TcpListener, UnixListener};

struct FakeRuntime {
    instance: InstanceId,
}

#[async_trait]
impl RuntimeBoundary for FakeRuntime {
    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            source_resolution: true,
            runtime_observation: true,
            runtime_execution: true,
            reason: None,
        }
    }

    async fn prepare(
        &self,
        application: &NormalizedApplication,
    ) -> Result<PreparedApplication, BoundaryError> {
        let sources = application
            .spec
            .services
            .iter()
            .map(|service| {
                let Source::Image { image } = &service.source else {
                    return Err(BoundaryError::Failed);
                };
                let repository = image.rsplit_once(':').map_or(image.as_str(), |v| v.0);
                Ok((
                    service.name.clone(),
                    ResolvedSource::Image {
                        requested: image.clone(),
                        digest_reference: format!("{repository}@sha256:{}", "a".repeat(64)),
                    },
                ))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let resolved = compile_application(
            application,
            self.instance.clone(),
            "piqueld-ingress",
            &ResolutionSet {
                sources,
                secrets: BTreeMap::new(),
            },
        )
        .map_err(|_| BoundaryError::Failed)?;
        Ok(PreparedApplication {
            resolved,
            observed: ObservedApplication::default(),
        })
    }

    async fn observe(&self, _: &StoredApplication) -> Result<ObservedApplication, BoundaryError> {
        Ok(ObservedApplication::default())
    }
}

fn manifest() -> ApplicationManifest {
    serde_json::from_value(serde_json::json!({
        "api_version":"piqueld.dev/v1alpha1",
        "kind":"Application",
        "metadata":{"name":"notes"},
        "spec":{"services":[{"name":"web","source":{"type":"image","image":"ghcr.io/example/notes:1"}}]}
    }))
    .unwrap()
}

#[allow(clippy::too_many_lines)]
async fn exercise(client: Client) {
    assert_eq!(client.system_status().await.unwrap().api_version, "v1");
    assert!(client.capabilities().await.unwrap().runtime_execution);
    assert!(
        client.openapi().await.unwrap()["paths"]
            .as_object()
            .unwrap()
            .len()
            >= 12
    );
    let preview = client
        .plan_create(&PlanApplicationRequest {
            manifest: manifest(),
        })
        .await
        .unwrap();
    assert_eq!(preview.proposed_generation, 1);
    let toml = r#"api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
[[spec.services]]
name = "web"
[spec.services.source]
type = "image"
image = "ghcr.io/example/notes:1"
"#;
    assert_eq!(
        client
            .plan_create_toml(toml)
            .await
            .unwrap()
            .proposed_generation,
        1
    );
    let created = client
        .create_application(
            &CreateApplicationRequest {
                manifest: manifest(),
            },
            "transport-contract",
        )
        .await
        .unwrap();
    assert_eq!(client.applications().await.unwrap().items.len(), 1);
    assert_eq!(
        client
            .application(&created.application_id)
            .await
            .unwrap()
            .generation,
        1
    );
    assert_eq!(
        client
            .application_status(&created.application_id)
            .await
            .unwrap()
            .state,
        "pending"
    );
    assert_eq!(
        client
            .plan_replace(
                &created.application_id,
                &ReplacePlanRequest {
                    expected_generation: 1,
                    manifest: manifest(),
                },
            )
            .await
            .unwrap()
            .proposed_generation,
        2
    );
    assert_eq!(
        client
            .plan_replace_toml(&created.application_id, toml, 1)
            .await
            .unwrap()
            .proposed_generation,
        2
    );
    let replaced = client
        .replace_application(
            &created.application_id,
            &ReplaceApplicationRequest {
                expected_generation: 1,
                manifest: manifest(),
            },
        )
        .await
        .unwrap();
    assert_eq!(replaced.generation, 2);
    let replaced = client
        .replace_application_toml(&created.application_id, toml, 2)
        .await
        .unwrap();
    assert_eq!(replaced.generation, 3);
    let reconciled = client.reconcile(&created.application_id, 3).await.unwrap();
    assert_eq!(
        client
            .operation(&reconciled.operation_id)
            .await
            .unwrap()
            .kind,
        "reconcile"
    );
    let mut operation_events = client.watch_operation(&reconciled.operation_id, None);
    let operation_event =
        tokio::time::timeout(std::time::Duration::from_secs(2), operation_events.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    assert_eq!(operation_event.event.as_deref(), Some("operation"));
    let mut events = client.watch_application(&created.application_id, None);
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(event.id.is_some());
    assert_eq!(event.event.as_deref(), Some("application"));

    let second_toml = toml.replace("name = \"notes\"", "name = \"notes-two\"");
    let second = client
        .create_application_toml(&second_toml, "transport-contract-toml")
        .await
        .unwrap();
    assert_eq!(second.generation, 1);
    let page = client
        .applications_with(&ListApplicationsOptions {
            cursor: None,
            limit: Some(1),
        })
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(page.next_cursor.is_some());

    let deleted = client
        .delete_application(
            &created.application_id,
            &DeleteApplicationRequest {
                expected_generation: 3,
                force: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(deleted.generation, 4);
    assert_eq!(
        client.operation(&deleted.operation_id).await.unwrap().kind,
        "delete"
    );
}

async fn state(temp: &TempDir) -> ApiState {
    let store = Arc::new(
        SqliteStore::open(temp.path().join("state.db"))
            .await
            .unwrap(),
    );
    let instance = InstanceId::parse(store.instance_id().to_owned()).unwrap();
    ApiState::new(store, Arc::new(FakeRuntime { instance }))
}

#[tokio::test]
async fn typed_client_exercises_router_over_tcp() {
    let temp = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(axum::serve(listener, router(state(&temp).await)).into_future());
    exercise(Client::tcp(&format!("http://{address}/")).unwrap()).await;
    server.abort();
}

#[tokio::test]
async fn typed_client_exercises_router_over_unix_socket() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("api.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(axum::serve(listener, router(state(&temp).await)).into_future());
    exercise(Client::unix(path)).await;
    server.abort();
}
