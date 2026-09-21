//! Transactional hostname ownership and the gateway's durable routing projection.
use super::{Store, StoreError};
use piqueld_core::{ApplicationId, manifest::ValidatedRoute};
use sqlx::SqliteConnection;
use std::collections::BTreeMap;

pub(crate) type RoutingTable = BTreeMap<ApplicationId, Vec<ValidatedRoute>>;

impl Store {
    pub(crate) async fn applied_routes(
        &self,
        id: &ApplicationId,
    ) -> Result<Vec<ValidatedRoute>, StoreError> {
        let id = id.as_str();
        let json = sqlx::query_scalar!(
            "SELECT applied_json FROM application_routes WHERE application_id=?1",
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?;
        json.as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(StoreError::corrupt)
            .map(Option::unwrap_or_default)
    }

    pub(crate) async fn has_routes(&self, id: &ApplicationId) -> Result<bool, StoreError> {
        let id = id.as_str();
        Ok(sqlx::query_scalar!("SELECT EXISTS(SELECT 1 FROM application_routes WHERE application_id=?1 AND (json_array_length(desired_json)>0 OR json_array_length(applied_json)>0))",id)
            .fetch_one(&self.pool).await.map_err(StoreError::database)? != 0)
    }

    /// Recomputes reservations inside the transaction changing their source.
    /// Captured deployment inputs also reserve names while a newer save is pending.
    pub(super) async fn reserve_hostnames_on(
        connection: &mut SqliteConnection,
        application_id: &str,
    ) -> Result<(), StoreError> {
        let names = sqlx::query_scalar!(r#"
            SELECT DISTINCT json_extract(r.value, '$.hostname') AS "hostname!: String" FROM (
                SELECT json_extract(desired_json,'$.spec.routes') AS routes FROM applications WHERE id=?1
                UNION ALL SELECT json_extract(resolved_json,'$.routes') FROM applications WHERE id=?1
                UNION ALL SELECT json_extract(target_json,'$.routes') FROM operations WHERE id=(SELECT id FROM operations WHERE application_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 1)
                UNION ALL SELECT json_extract(application_json,'$.spec.routes') FROM deployment_inputs WHERE operation_id=(SELECT id FROM operations WHERE application_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 1)
                UNION ALL SELECT desired_json FROM application_routes WHERE application_id=?1
                UNION ALL SELECT applied_json FROM application_routes WHERE application_id=?1
            ) AS sources, json_each(sources.routes) AS r
        "#, application_id).fetch_all(&mut *connection).await.map_err(StoreError::database)?;
        sqlx::query!(
            "DELETE FROM hostname_reservations WHERE application_id=?1",
            application_id
        )
        .execute(&mut *connection)
        .await
        .map_err(StoreError::database)?;
        for hostname in names {
            sqlx::query!(
                "INSERT INTO hostname_reservations(hostname,application_id) VALUES(?1,?2)",
                hostname,
                application_id
            )
            .execute(&mut *connection)
            .await
            .map_err(|error| {
                if error
                    .as_database_error()
                    .is_some_and(sqlx::error::DatabaseError::is_unique_violation)
                {
                    StoreError::HostnameConflict { hostname }
                } else {
                    StoreError::database(error)
                }
            })?;
        }
        Ok(())
    }

    /// Persists a ready cutover, or withdraws removed hostnames while retaining
    /// existing destinations until their replacements are ready.
    pub(crate) async fn stage_routes(
        &self,
        application_id: &ApplicationId,
        routes: &[ValidatedRoute],
        ready: bool,
        operation_id: Option<&str>,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let id = application_id.as_str();
        if let Some(operation_id) = operation_id {
            let current = sqlx::query_scalar!("SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND application_id=?2 AND state='running' AND id=(SELECT id FROM operations WHERE application_id=?2 ORDER BY created_at_ms DESC,id DESC LIMIT 1))",operation_id,id)
                .fetch_one(&mut *tx).await.map_err(StoreError::database)?;
            if current == 0 {
                return Err(StoreError::IllegalTransition);
            }
        }
        let previous = sqlx::query_scalar!(
            "SELECT desired_json FROM application_routes WHERE application_id=?1",
            id
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let mut desired: Vec<ValidatedRoute> = previous
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(StoreError::corrupt)?
            .unwrap_or_default();
        if ready {
            desired = routes.to_vec();
        } else {
            desired.retain(|old| routes.iter().any(|new| new.hostname == old.hostname));
        }
        let json = serde_json::to_string(&desired).map_err(StoreError::corrupt)?;
        sqlx::query!("INSERT INTO application_routes(application_id,desired_json) VALUES(?1,?2) ON CONFLICT(application_id) DO UPDATE SET desired_json=excluded.desired_json",id,json)
            .execute(&mut *tx).await.map_err(StoreError::database)?;
        Self::reserve_hostnames_on(&mut tx, id).await?;
        tx.commit().await.map_err(StoreError::database)
    }

    pub(crate) async fn routing_table(&self) -> Result<RoutingTable, StoreError> {
        sqlx::query!(
            "SELECT application_id,desired_json FROM application_routes ORDER BY application_id"
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?
        .into_iter()
        .map(|row| {
            Ok((
                ApplicationId::parse(row.application_id).map_err(StoreError::corrupt)?,
                serde_json::from_str(&row.desired_json).map_err(StoreError::corrupt)?,
            ))
        })
        .collect()
    }

    /// Called only after this exact table is accepted (or the gateway is stopped).
    pub(crate) async fn acknowledge_routes(&self, table: &RoutingTable) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        for (application_id, routes) in table {
            let id = application_id.as_str();
            let json = serde_json::to_string(routes).map_err(StoreError::corrupt)?;
            sqlx::query!(
                "UPDATE application_routes SET applied_json=?1 WHERE application_id=?2",
                json,
                id
            )
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
            Self::reserve_hostnames_on(&mut tx, id).await?;
        }
        tx.commit().await.map_err(StoreError::database)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Mutation, MutationResponse};
    use piqueld_core::{NormalizedApplication, OperationState};
    use std::fmt::Write as _;

    fn app(name: &str, hostname: Option<&str>) -> NormalizedApplication {
        let mut text = format!(
            "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='{name}'\n[[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='nginx:alpine'"
        );
        if let Some(hostname) = hostname {
            write!(
                text,
                "\n[[spec.routes]]\nhostname='{hostname}'\nservice='web'\nport=80"
            )
            .unwrap();
        }
        piqueld_core::parse_toml(&text)
            .unwrap()
            .normalize(ApplicationId::parse("input-app").unwrap())
    }

    async fn save(
        store: &Store,
        application: NormalizedApplication,
        deploy: bool,
    ) -> Result<piqueld_core::api::SavedApplication, StoreError> {
        let (response, _) = store
            .accept(
                Mutation::Save {
                    application,
                    expected_application_id: None,
                    deploy,
                },
                None,
                true,
                None,
            )
            .await?;
        let MutationResponse::Saved(saved) = response else {
            panic!("saved response")
        };
        Ok(saved)
    }

    #[tokio::test]
    async fn concurrent_saves_cannot_claim_the_same_hostname() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
        let (one, two) = tokio::join!(
            save(&store, app("one", Some("site.example.com")), false),
            save(&store, app("two", Some("SITE.EXAMPLE.COM.")), false)
        );
        assert!(matches!(
            (&one, &two),
            (Ok(_), Err(StoreError::HostnameConflict { .. }))
                | (Err(StoreError::HostnameConflict { .. }), Ok(_))
        ));
        assert_eq!(
            store.list(None, 10).await.unwrap().items.len(),
            1,
            "failed reservation rolls back the entire save"
        );
    }

    #[tokio::test]
    async fn deployed_names_survive_disabled_ingress_and_failed_gateway_withdrawal() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
        let input = app("one", Some("site.example.com"));
        let saved = save(&store, input.clone(), false).await.unwrap();
        let id = ApplicationId::parse(saved.application_id).unwrap();
        store
            .stage_routes(&id, &input.spec().routes, true, None)
            .await
            .unwrap();
        store
            .acknowledge_routes(&store.routing_table().await.unwrap())
            .await
            .unwrap();
        save(&store, app("one", None), false).await.unwrap();
        assert!(matches!(
            save(&store, app("two", Some("site.example.com")), false).await,
            Err(StoreError::HostnameConflict { .. })
        ));
        store.stage_routes(&id, &[], true, None).await.unwrap();
        assert!(
            matches!(
                save(&store, app("two", Some("site.example.com")), false).await,
                Err(StoreError::HostnameConflict { .. })
            ),
            "last accepted configuration still reserves its host"
        );
        store
            .acknowledge_routes(&store.routing_table().await.unwrap())
            .await
            .unwrap();
        save(&store, app("two", Some("site.example.com")), false)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn captured_and_fetched_deployments_reserve_hostnames() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
        let saved = save(&store, app("one", Some("site.example.com")), true)
            .await
            .unwrap();
        save(&store, app("one", None), false).await.unwrap();
        assert!(matches!(
            save(&store, app("two", Some("site.example.com")), false).await,
            Err(StoreError::HostnameConflict { .. })
        ));
        save(&store, app("two", Some("taken.example.com")), false)
            .await
            .unwrap();
        let operation = store.operation(&saved.operation_id.unwrap()).await.unwrap();
        store
            .transition_operation(
                &operation.id,
                OperationState::Requested,
                OperationState::Running,
                None,
            )
            .await
            .unwrap();
        let fetched =
            app("one", Some("taken.example.com")).with_id(operation.application_id.clone());
        assert!(matches!(
            store
                .save_deployment_input(&operation, &fetched, None)
                .await,
            Err(StoreError::HostnameConflict { .. })
        ));
    }

    #[tokio::test]
    async fn superseded_operations_cannot_publish_routes() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
        let input = app("one", Some("site.example.com"));
        let saved = save(&store, input.clone(), true).await.unwrap();
        let operation = saved.operation_id.unwrap();
        store
            .transition_operation(
                &operation,
                OperationState::Requested,
                OperationState::Running,
                None,
            )
            .await
            .unwrap();
        save(&store, app("one", Some("new.example.com")), true)
            .await
            .unwrap();
        let id = ApplicationId::parse(saved.application_id).unwrap();
        assert!(matches!(
            store
                .stage_routes(&id, &input.spec().routes, true, Some(&operation))
                .await,
            Err(StoreError::IllegalTransition)
        ));
        assert!(store.routing_table().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn withdrawals_precede_cutover_and_repointing_waits_for_readiness() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
        let input = app("one", Some("site.example.com"));
        let saved = save(&store, input.clone(), false).await.unwrap();
        let id = ApplicationId::parse(saved.application_id).unwrap();
        store
            .stage_routes(&id, &input.spec().routes, true, None)
            .await
            .unwrap();
        let mut changed = input.spec().routes.clone();
        changed[0].port = std::num::NonZeroU16::new(3000).unwrap();
        store
            .stage_routes(&id, &changed, false, None)
            .await
            .unwrap();
        assert_eq!(store.routing_table().await.unwrap()[&id][0].port.get(), 80);
        store.stage_routes(&id, &changed, true, None).await.unwrap();
        assert_eq!(
            store.routing_table().await.unwrap()[&id][0].port.get(),
            3000
        );
        store.stage_routes(&id, &[], false, None).await.unwrap();
        assert!(store.routing_table().await.unwrap()[&id].is_empty());
    }
}
