//! Transactional hostname ownership and the gateway's durable routing projection.
use super::{Store, StoreError, access::scope_json};
use piqueld_core::{
    EnvironmentId,
    access::Scope,
    api::RouteStatus,
    manifest::{Hostname, ValidatedRoute},
};
use sqlx::{Sqlite, SqliteConnection, SqliteExecutor, Transaction};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// Routes the gateway serves, keyed by owning environment in a stable order.
pub(crate) type RoutingTable = BTreeMap<EnvironmentId, Vec<ValidatedRoute>>;

impl Store {
    /// Finalizes writes to application intent, deployment inputs/targets, or
    /// gateway state, given the environments whose hostnames they may change.
    /// Refresh ownership from the final transaction state before committing, so a
    /// hostname conflict rolls back the mutation, events, and replay receipt together.
    /// Write helpers must leave this to their transaction owner rather than checking
    /// intermediate state (a save can also replace the pending deployment).
    pub(super) async fn commit_environment_changes<'a>(
        mut tx: Transaction<'_, Sqlite>,
        environments: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), StoreError> {
        for id in environments {
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
        id: &EnvironmentId,
    ) -> Result<Vec<ValidatedRoute>, StoreError> {
        let id = id.as_str();
        let json = sqlx::query_scalar!(
            "SELECT applied_json FROM environment_routes WHERE environment_id=?1",
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
    pub(crate) async fn has_routes(&self, id: &EnvironmentId) -> Result<bool, StoreError> {
        let id = id.as_str();
        Ok(sqlx::query_scalar!("SELECT EXISTS(SELECT 1 FROM environment_routes WHERE environment_id=?1 AND (json_array_length(desired_json)>0 OR json_array_length(applied_json)>0))",id)
            .fetch_one(&self.pool).await.map_err(StoreError::database)? != 0)
    }

    /// Recomputes reservations inside the transaction changing their source.
    /// Captured deployments also reserve names while a newer save is pending.
    ///
    /// Collects every hostname the environment could still serve (the routes
    /// of the manifest it deploys, rendered for this environment, its resolved
    /// spec, its latest operation's target and rendered deployment manifest,
    /// desired and applied gateway routes), rejects any within an installation
    /// hostname, then replaces the environment's `hostname_reservations` rows.
    /// A unique violation means another environment owns the name. Sibling
    /// conflicts identify that environment of the same application.
    async fn reserve_hostnames_on(
        connection: &mut SqliteConnection,
        environment_id: &str,
    ) -> Result<(), StoreError> {
        let mut names = sqlx::query_scalar!(r#"
            SELECT DISTINCT json_extract(r.value, '$.hostname') AS "hostname!: String" FROM (
                SELECT json_extract(resolved_json,'$.routes') AS routes FROM environments WHERE id=?1
                UNION ALL SELECT json_extract(target_json,'$.routes') FROM operations WHERE id=(SELECT id FROM operations WHERE environment_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 1)
                UNION ALL SELECT json_extract(manifest_json,'$.spec.routes') FROM deployments WHERE id=(SELECT id FROM operations WHERE environment_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 1)
                UNION ALL SELECT desired_json FROM environment_routes WHERE environment_id=?1
                UNION ALL SELECT applied_json FROM environment_routes WHERE environment_id=?1
            ) AS sources, json_each(sources.routes) AS r
        "#, environment_id).fetch_all(&mut *connection).await.map_err(StoreError::database)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        // Each environment's own manifest renders its routes, so environments
        // of one application can serve different hostnames, and environments
        // following different branches never reserve each other's.
        if let Some(environment) = Self::environment_on(&mut *connection, environment_id).await?
            && let Some(manifest) = environment.manifest()
        {
            names.extend(
                manifest
                    .hostnames(&environment.environment.target())
                    .into_iter()
                    .map(String::from),
            );
        }
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
            "DELETE FROM hostname_reservations WHERE environment_id=?1",
            environment_id
        )
        .execute(&mut *connection)
        .await
        .map_err(StoreError::database)?;
        for hostname in names {
            let result = sqlx::query!(
                "INSERT INTO hostname_reservations(hostname,environment_id) VALUES(?1,?2)",
                hostname,
                environment_id
            )
            .execute(&mut *connection)
            .await;
            if let Err(error) = result {
                if !error
                    .as_database_error()
                    .is_some_and(sqlx::error::DatabaseError::is_unique_violation)
                {
                    return Err(StoreError::database(error));
                }
                let sibling = sqlx::query_scalar!(
                    r#"SELECT owner.name AS "name!" FROM hostname_reservations r JOIN environments owner ON owner.id=r.environment_id JOIN environments contender ON contender.id=?2 WHERE r.hostname=?1 AND owner.application_id=contender.application_id"#,
                    hostname,
                    environment_id
                ).fetch_optional(&mut *connection).await.map_err(StoreError::database)?;
                return Err(match sibling {
                    Some(name) => StoreError::SharedHostnameConflict {
                        hostname,
                        environment: piqueld_core::EnvironmentName::parse(name)
                            .map_err(StoreError::corrupt)?,
                    },
                    None => StoreError::HostnameConflict { hostname },
                });
            }
        }
        Ok(())
    }

    /// Persists a ready cutover, or withdraws removed routes while retaining
    /// existing destinations until their replacements are ready. A route whose
    /// visibility changes counts as removed, so it leaves its old listener as
    /// the deployment starts and joins the new one once backends are ready.
    /// When `operation_id` is given, fails with `StoreError::IllegalTransition`
    /// unless it is the application's latest operation and still running, so a
    /// superseded deployment cannot publish stale routes.
    pub(crate) async fn stage_routes(
        &self,
        environment_id: &EnvironmentId,
        routes: &[ValidatedRoute],
        ready: bool,
        operation_id: Option<&str>,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let id = environment_id.as_str();
        if let Some(operation_id) = operation_id {
            let current = sqlx::query_scalar!("SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND environment_id=?2 AND state='running' AND id=(SELECT id FROM operations WHERE environment_id=?2 ORDER BY created_at_ms DESC,id DESC LIMIT 1))",operation_id,id)
                .fetch_one(&mut *tx).await.map_err(StoreError::database)?;
            if current == 0 {
                return Err(StoreError::IllegalTransition);
            }
        }
        let previous = sqlx::query_scalar!(
            "SELECT desired_json FROM environment_routes WHERE environment_id=?1",
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
            desired.retain(|old| routes.iter().any(|new| new.same_listener(old)));
        }
        let json = serde_json::to_string(&desired).map_err(StoreError::corrupt)?;
        sqlx::query!("INSERT INTO environment_routes(environment_id,desired_json) VALUES(?1,?2) ON CONFLICT(environment_id) DO UPDATE SET desired_json=excluded.desired_json",id,json)
            .execute(&mut *tx).await.map_err(StoreError::database)?;
        Self::commit_environment_changes(tx, [id]).await
    }

    /// Routes the gateway should serve. Hostnames reserved by the installation
    /// are withheld even if they were saved before the reservation.
    pub(crate) async fn routing_table(&self) -> Result<RoutingTable, StoreError> {
        let installation = Self::installation_hostnames(&self.pool).await?;
        sqlx::query!(
            "SELECT environment_id,desired_json FROM environment_routes ORDER BY environment_id"
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
                EnvironmentId::parse(row.environment_id).map_err(StoreError::corrupt)?,
                routes,
            ))
        })
        .collect()
    }

    /// The `routes` whose environment belongs to a `readable` application.
    /// Route statuses name hostnames and backends, so other applications'
    /// routes stay hidden like the applications themselves.
    pub(crate) async fn readable_routes(
        &self,
        readable: &Scope,
        mut routes: Vec<RouteStatus>,
    ) -> Result<Vec<RouteStatus>, StoreError> {
        let Some(applications) = scope_json(readable) else {
            return Ok(routes);
        };
        let environments: HashSet<String> = sqlx::query_scalar!(
            r#"SELECT id AS "id!" FROM environments WHERE application_id IN (SELECT value FROM json_each(?1))"#,
            applications
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?
        .into_iter()
        .collect();
        routes.retain(|route| environments.contains(&route.environment_id));
        Ok(routes)
    }

    /// Records `table` as each application's applied routes and recomputes their
    /// hostname reservations.
    /// Called only after this exact table is accepted (or the gateway is stopped).
    pub(crate) async fn acknowledge_routes(&self, table: &RoutingTable) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        for (environment_id, routes) in table {
            let id = environment_id.as_str();
            let json = serde_json::to_string(routes).map_err(StoreError::corrupt)?;
            sqlx::query!(
                "UPDATE environment_routes SET applied_json=?1 WHERE environment_id=?2",
                json,
                id
            )
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        }
        Self::commit_environment_changes(tx, table.keys().map(EnvironmentId::as_str)).await
    }

    /// The routes the gateway last acknowledged, for every environment. DNS
    /// records follow this table, so they change only after the gateway has
    /// applied or withdrawn a route.
    pub(crate) async fn applied_table(&self) -> Result<RoutingTable, StoreError> {
        sqlx::query!(
            "SELECT environment_id,applied_json FROM environment_routes ORDER BY environment_id"
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?
        .into_iter()
        .map(|row| {
            Ok((
                EnvironmentId::parse(row.environment_id).map_err(StoreError::corrupt)?,
                serde_json::from_str(&row.applied_json).map_err(StoreError::corrupt)?,
            ))
        })
        .collect()
    }

    /// Hostnames whose DNS records piqueld manages, including those of
    /// removed routes until their records are deleted.
    pub(crate) async fn dns_records(&self) -> Result<BTreeSet<Hostname>, StoreError> {
        sqlx::query_scalar!("SELECT hostname FROM dns_records")
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::database)?
            .into_iter()
            .map(|hostname| Hostname::parse(hostname).map_err(StoreError::corrupt))
            .collect()
    }

    /// Records that piqueld manages `hostname`'s DNS records, before it writes
    /// the first one, and whether changes it staged are `unpublished`.
    pub(crate) async fn track_dns_records(
        &self,
        hostname: &Hostname,
        unpublished: bool,
    ) -> Result<(), StoreError> {
        let hostname = hostname.as_str();
        sqlx::query!(
            "INSERT INTO dns_records(hostname,unpublished) VALUES(?1,?2) ON CONFLICT(hostname) DO UPDATE SET unpublished=excluded.unpublished",
            hostname,
            unpublished
        )
        .execute(&self.pool)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// Whether changes staged for `hostname` may not be published yet.
    pub(crate) async fn dns_records_unpublished(
        &self,
        hostname: &Hostname,
    ) -> Result<bool, StoreError> {
        let hostname = hostname.as_str();
        Ok(sqlx::query_scalar!(
            "SELECT unpublished FROM dns_records WHERE hostname=?1",
            hostname
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
            == Some(1))
    }

    /// Forgets `hostname` once its DNS records are deleted.
    pub(crate) async fn release_dns_records(&self, hostname: &Hostname) -> Result<(), StoreError> {
        let hostname = hostname.as_str();
        sqlx::query!("DELETE FROM dns_records WHERE hostname=?1", hostname)
            .execute(&self.pool)
            .await
            .map_err(StoreError::database)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Mutation, MutationResponse};
    use piqueld_core::manifest::{ApplicationTemplate, Variable};
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
            .normalize(piqueld_core::ApplicationId::parse("input-app").unwrap())
    }

    async fn save(
        store: &Store,
        application: NormalizedApplication,
        deploy: bool,
    ) -> Result<piqueld_core::api::SavedApplication, StoreError> {
        let (response, _) = store
            .accept(
                crate::api::Actor::Daemon,
                Mutation::Save {
                    application: Box::new(ApplicationTemplate::from(&application)),
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
        let id = EnvironmentId::parse(saved.application_id).unwrap();
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
    async fn environments_of_one_application_cannot_share_a_hostname() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
        let saved = save(&store, app("one", Some("site.example.com")), false)
            .await
            .unwrap();
        let staging = Mutation::CreateEnvironment {
            application: piqueld_core::ApplicationId::parse(&saved.application_id).unwrap(),
            name: piqueld_core::EnvironmentName::parse("staging").unwrap(),
            source: None,
        };
        assert!(matches!(
            store.accept(crate::store::Actor::Daemon, staging, None, true, None).await,
            Err(StoreError::SharedHostnameConflict { hostname, environment })
                if hostname == "site.example.com" && environment.as_str() == "production"
        ));
        assert_eq!(
            store
                .environments(&piqueld_core::ApplicationId::parse(saved.application_id).unwrap())
                .await
                .unwrap()
                .len(),
            1,
            "failed creation rolls back the environment"
        );
    }

    #[tokio::test]
    async fn environments_render_their_own_hostnames_from_variables() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
        let template = piqueld_core::manifest::parse_template_toml(
            "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='notes'\n[spec.variables]\ndomain='piquel.fr'\n[spec.environments.staging.variables]\ndomain='staging.piquel.fr'\n[[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='nginx:alpine'\n[[spec.routes]]\nhostname='${{ vars.domain }}'\nservice='web'\nport=80",
        )
        .unwrap();
        let (MutationResponse::Saved(saved), _) = store
            .accept(
                crate::api::Actor::Daemon,
                Mutation::save(template, None, false),
                Some(0),
                false,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("saved response")
        };
        let application = piqueld_core::ApplicationId::parse(&saved.application_id).unwrap();
        let staging = Mutation::CreateEnvironment {
            application: application.clone(),
            name: piqueld_core::EnvironmentName::parse("staging").unwrap(),
            source: None,
        };
        let (MutationResponse::Environment(staging), _) = store
            .accept(crate::api::Actor::Daemon, staging, None, true, None)
            .await
            .unwrap()
        else {
            panic!("environment response")
        };
        let production = EnvironmentId::default_for(&application);
        let mut deployed = Vec::new();
        for environment in [&production, &staging.id] {
            let (MutationResponse::Operation(operation), _) = store
                .accept(
                    crate::api::Actor::Daemon,
                    Mutation::deploy(environment.clone()),
                    None,
                    true,
                    None,
                )
                .await
                .unwrap()
            else {
                panic!("operation response")
            };
            let rendering = store
                .deployment_snapshot(&operation.operation_id)
                .await
                .unwrap()
                .rendering
                .unwrap();
            deployed.push(rendering.application.spec().routes[0].hostname.to_string());
        }
        assert_eq!(deployed, ["piquel.fr", "staging.piquel.fr"]);
        assert!(matches!(
            save(&store, app("other", Some("staging.piquel.fr")), false).await,
            Err(StoreError::HostnameConflict { .. })
        ));

        // Environments that render the same hostname still conflict.
        let same = piqueld_core::edit::Variables {
            defaults: [("domain".into(), Variable::String("piquel.fr".into()))].into(),
            ..Default::default()
        };
        let edit = Mutation::Edit {
            id: application,
            edit: Box::new(piqueld_core::edit::ApplicationEdit::Variables(same)),
            deploy: false,
        };
        assert!(matches!(
            store.accept(crate::api::Actor::Daemon, edit, None, true, None).await,
            Err(StoreError::SharedHostnameConflict { hostname, environment })
                if hostname == "piquel.fr" && environment.as_str() == "production"
        ));
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
        let id = EnvironmentId::parse(saved.application_id).unwrap();
        let result = store
            .accept(
                crate::api::Actor::Daemon,
                Mutation::Save {
                    application: Box::new(ApplicationTemplate::from(&app(
                        "two",
                        Some("taken.example.com"),
                    ))),
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
            store
                .get(&id)
                .await
                .unwrap()
                .manifest()
                .unwrap()
                .spec()
                .routes,
            [] as [piqueld_core::manifest::Route; 0]
        );
        assert!(
            store
                .latest_operation_for_environment(&id)
                .await
                .unwrap()
                .is_none()
        );
        // The rejected request must not consume the replay key.
        store
            .accept(
                crate::api::Actor::Daemon,
                Mutation::Save {
                    application: Box::new(ApplicationTemplate::from(&app(
                        "two",
                        Some("free.example.com"),
                    ))),
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
        let id = EnvironmentId::parse(saved.application_id).unwrap();
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
        let environment = store.get(&operation.environment_id).await.unwrap();
        let fetched = ApplicationTemplate::from(&app("one", Some("taken.example.com")))
            .with_id(environment.application.application.id().clone());
        let rendering = environment.render(&fetched, &operation.id).unwrap();
        assert!(matches!(
            store
                .save_deployment_input(&operation, &fetched, &rendering, (None, None), &[])
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
        let id = EnvironmentId::parse(saved.application_id).unwrap();
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
        let id = EnvironmentId::parse(saved.application_id).unwrap();
        store
            .stage_routes(&id, &input.spec().routes, true, None)
            .await
            .unwrap();
        let mut changed = input.spec().routes.clone();
        changed[0].target =
            serde_json::from_value(serde_json::json!({"service":"web","port":3000})).unwrap();
        store
            .stage_routes(&id, &changed, false, None)
            .await
            .unwrap();
        assert_eq!(
            store.routing_table().await.unwrap()[&id][0]
                .target
                .to_string(),
            "web:80"
        );
        store.stage_routes(&id, &changed, true, None).await.unwrap();
        assert_eq!(
            store.routing_table().await.unwrap()[&id][0]
                .target
                .to_string(),
            "web:3000"
        );
        store.stage_routes(&id, &[], false, None).await.unwrap();
        assert_eq!(
            store.routing_table().await.unwrap()[&id],
            [] as [piqueld_core::manifest::ValidatedRoute; 0]
        );
    }

    #[tokio::test]
    async fn changing_visibility_withdraws_the_route_until_backends_are_ready() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
        let input = app("one", Some("site.example.com"));
        let saved = save(&store, input.clone(), false).await.unwrap();
        let id = EnvironmentId::parse(saved.application_id).unwrap();
        let mut public = input.spec().routes.clone();
        public[0].visibility = piqueld_core::manifest::Visibility::Public;
        store.stage_routes(&id, &public, true, None).await.unwrap();
        // Public -> private leaves the public listener as the deployment
        // starts; the private route joins once backends are ready.
        let private = input.spec().routes.clone();
        store
            .stage_routes(&id, &private, false, None)
            .await
            .unwrap();
        assert_eq!(
            store.routing_table().await.unwrap()[&id],
            [] as [piqueld_core::manifest::ValidatedRoute; 0]
        );
        store.stage_routes(&id, &private, true, None).await.unwrap();
        assert_eq!(store.routing_table().await.unwrap()[&id], private);
    }

    #[tokio::test]
    async fn route_statuses_are_limited_to_readable_applications() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
        let mut statuses = Vec::new();
        let mut applications = Vec::new();
        for (name, hostname) in [("one", "one.example.com"), ("two", "two.example.com")] {
            let input = app(name, Some(hostname));
            let saved = save(&store, input.clone(), false).await.unwrap();
            let route = &input.spec().routes[0];
            statuses.push(RouteStatus {
                name: None,
                environment_id: saved.application_id.clone(),
                hostname: hostname.into(),
                visibility: route.visibility,
                dns: piqueld_core::api::DnsRecords::ServerAddresses {
                    addresses: Vec::new(),
                },
                dns_state: piqueld_core::api::DnsRecordState::Manual,
                target: route.target.clone(),
                state: "ready".into(),
                message: String::new(),
            });
            applications.push(piqueld_core::ApplicationId::parse(saved.application_id).unwrap());
        }
        let hostnames = |routes: Vec<RouteStatus>| {
            routes
                .into_iter()
                .map(|route| route.hostname)
                .collect::<Vec<_>>()
        };
        let all = store.readable_routes(&Scope::All, statuses.clone()).await;
        assert_eq!(
            hostnames(all.unwrap()),
            ["one.example.com", "two.example.com"]
        );
        let one = Scope::one(applications[0].clone());
        let limited = store.readable_routes(&one, statuses.clone()).await;
        assert_eq!(hostnames(limited.unwrap()), ["one.example.com"]);
        let none = store.readable_routes(&Scope::NONE, statuses).await;
        assert_eq!(hostnames(none.unwrap()), [] as [String; 0]);
    }
}
