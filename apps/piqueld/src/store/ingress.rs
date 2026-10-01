//! Transactional hostname ownership and the gateway's durable routing projection.
use super::{Store, StoreError};
use piqueld_core::{
    ApplicationId,
    manifest::{Hostname, ValidatedRoute},
};
use sqlx::{Sqlite, SqliteConnection, SqliteExecutor, Transaction};
use std::collections::BTreeMap;

/// Routes the gateway serves, keyed by owning application in a stable order.
pub(crate) type RoutingTable = BTreeMap<ApplicationId, Vec<ValidatedRoute>>;

impl Store {
    /// Finalizes writes to application intent, deployment inputs/targets, or gateway state.
    /// Refresh ownership from the final transaction state before committing, so a
    /// hostname conflict rolls back the mutation, events, and replay receipt together.
    /// Write helpers must leave this to their transaction owner rather than checking
    /// intermediate state (a save can also replace the pending deployment).
    pub(super) async fn commit_application_changes<'a>(
        mut tx: Transaction<'_, Sqlite>,
        applications: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), StoreError> {
        for id in applications {
            Self::reserve_hostnames_on(&mut tx, id).await?;
        }
        tx.commit().await.map_err(StoreError::database)
    }

    /// Replaces the hostnames the installation serves itself. Routes may not
    /// claim them or their subdomains. Returns route hostnames saved before the
    /// reservation that now conflict; the gateway never publishes them.
    /// Existing reservations are reported rather than rejected, so a newly
    /// configured website hostname never fails daemon startup.
    pub(crate) async fn reserve_installation_hostnames(
        &self,
        hostnames: &[Hostname],
    ) -> Result<Vec<Hostname>, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        sqlx::query!("DELETE FROM installation_hostnames")
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        for hostname in hostnames {
            let hostname = hostname.as_str();
            sqlx::query!(
                "INSERT INTO installation_hostnames(hostname) VALUES(?1)",
                hostname
            )
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        }
        let routed = sqlx::query_scalar!("SELECT hostname FROM hostname_reservations")
            .fetch_all(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)?;
        let mut conflicts = Vec::new();
        for hostname in routed {
            let hostname = Hostname::parse(hostname).map_err(StoreError::corrupt)?;
            if hostnames.iter().any(|domain| hostname.is_within(domain)) {
                conflicts.push(hostname);
            }
        }
        Ok(conflicts)
    }

    /// Loads the hostnames reserved for the installation itself.
    async fn installation_hostnames<'e>(
        executor: impl SqliteExecutor<'e>,
    ) -> Result<Vec<Hostname>, StoreError> {
        sqlx::query_scalar!("SELECT hostname FROM installation_hostnames")
            .fetch_all(executor)
            .await
            .map_err(StoreError::database)?
            .into_iter()
            .map(|hostname| Hostname::parse(hostname).map_err(StoreError::corrupt))
            .collect()
    }

    /// Routes the gateway last accepted for an application; empty when none were
    /// ever acknowledged.
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

    /// Whether the application has desired or applied routes, i.e. whether the
    /// gateway may still hold state for it.
    pub(crate) async fn has_routes(&self, id: &ApplicationId) -> Result<bool, StoreError> {
        let id = id.as_str();
        Ok(sqlx::query_scalar!("SELECT EXISTS(SELECT 1 FROM application_routes WHERE application_id=?1 AND (json_array_length(desired_json)>0 OR json_array_length(applied_json)>0))",id)
            .fetch_one(&self.pool).await.map_err(StoreError::database)? != 0)
    }

    /// Recomputes reservations inside the transaction changing their source.
    /// Captured deployment inputs also reserve names while a newer save is pending.
    ///
    /// Collects every hostname the application could still serve (saved spec,
    /// resolved spec, latest operation target and captured input, desired and
    /// applied gateway routes), rejects any within an installation hostname, then
    /// replaces the application's `hostname_reservations` rows. A unique
    /// violation means another application owns the name and maps to
    /// `StoreError::HostnameConflict`.
    async fn reserve_hostnames_on(
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
        let installation = Self::installation_hostnames(&mut *connection).await?;
        for hostname in &names {
            let parsed = Hostname::parse(hostname.as_str()).map_err(StoreError::corrupt)?;
            if installation.iter().any(|domain| parsed.is_within(domain)) {
                return Err(StoreError::HostnameConflict {
                    hostname: hostname.clone(),
                });
            }
        }
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
    /// When `operation_id` is given, fails with `StoreError::IllegalTransition`
    /// unless it is the application's latest operation and still running, so a
    /// superseded deployment cannot publish stale routes.
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
        Self::commit_application_changes(tx, [id]).await
    }

    /// Routes the gateway should serve. Hostnames reserved by the installation
    /// are withheld even if they were saved before the reservation.
    pub(crate) async fn routing_table(&self) -> Result<RoutingTable, StoreError> {
        let installation = Self::installation_hostnames(&self.pool).await?;
        sqlx::query!(
            "SELECT application_id,desired_json FROM application_routes ORDER BY application_id"
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?
        .into_iter()
        .map(|row| {
            let mut routes: Vec<ValidatedRoute> =
                serde_json::from_str(&row.desired_json).map_err(StoreError::corrupt)?;
            routes.retain(|route| {
                !installation
                    .iter()
                    .any(|domain| route.hostname.is_within(domain))
            });
            Ok((
                ApplicationId::parse(row.application_id).map_err(StoreError::corrupt)?,
                routes,
            ))
        })
        .collect()
    }

    /// Records `table` as each application's applied routes and recomputes their
    /// hostname reservations.
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
        }
        Self::commit_application_changes(tx, table.keys().map(ApplicationId::as_str)).await
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
                    application: Box::new(application),
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
    async fn installation_hostnames_and_subdomains_are_never_routed() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
        let early = app("early", Some("app.piqueld.example.com"));
        let saved = save(&store, early.clone(), true).await.unwrap();
        let id = ApplicationId::parse(saved.application_id).unwrap();
        store
            .stage_routes(&id, &early.spec().routes, true, None)
            .await
            .unwrap();

        // Routes saved before the website hostname was configured are withheld.
        let website = Hostname::parse("piqueld.example.com").unwrap();
        let conflicts = store
            .reserve_installation_hostnames(std::slice::from_ref(&website))
            .await
            .unwrap();
        assert_eq!(conflicts, [early.spec().routes[0].hostname.clone()]);
        assert_eq!(store.routing_table().await.unwrap()[&id], []);

        for hostname in ["piqueld.example.com", "api.piqueld.example.com"] {
            assert!(matches!(
                save(&store, app("late", Some(hostname)), false).await,
                Err(StoreError::HostnameConflict { .. })
            ));
        }
        save(
            &store,
            app("sibling", Some("notpiqueld.example.com")),
            false,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn replacing_pending_deployment_releases_only_superseded_hostnames() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
        save(&store, app("one", Some("old.example.com")), true)
            .await
            .unwrap();
        save(&store, app("one", Some("new.example.com")), true)
            .await
            .unwrap();
        // Commit validates the final snapshot, after the new deployment has
        // replaced the old captured input, rather than the intermediate save.
        save(&store, app("two", Some("old.example.com")), false)
            .await
            .unwrap();
        assert!(matches!(
            save(&store, app("three", Some("new.example.com")), false).await,
            Err(StoreError::HostnameConflict { .. })
        ));
    }

    #[tokio::test]
    async fn hostname_conflict_rolls_back_deployment_and_replay_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
        save(&store, app("one", Some("taken.example.com")), false)
            .await
            .unwrap();
        let saved = save(&store, app("two", None), false).await.unwrap();
        let id = ApplicationId::parse(saved.application_id).unwrap();
        let result = store
            .accept(
                Mutation::Save {
                    application: Box::new(app("two", Some("taken.example.com"))),
                    expected_application_id: None,
                    deploy: true,
                },
                None,
                true,
                Some("conflicting-deploy"),
            )
            .await;
        assert!(matches!(result, Err(StoreError::HostnameConflict { .. })));
        assert_eq!(
            store.get(&id).await.unwrap().application.spec().routes,
            [] as [piqueld_core::manifest::ValidatedRoute; 0]
        );
        assert!(
            store
                .latest_operation_for_application(&id)
                .await
                .unwrap()
                .is_none()
        );
        // The rejected request must not consume the replay key.
        store
            .accept(
                Mutation::Save {
                    application: Box::new(app("two", Some("free.example.com"))),
                    expected_application_id: None,
                    deploy: true,
                },
                None,
                true,
                Some("conflicting-deploy"),
            )
            .await
            .unwrap();
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
        assert_eq!(
            store.routing_table().await.unwrap()[&id],
            [] as [piqueld_core::manifest::ValidatedRoute; 0]
        );
    }
}
