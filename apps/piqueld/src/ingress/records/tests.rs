use super::*;
use crate::{
    config::TunnelCredentials,
    dns::provider::{
        DnsProvider, DnsProviderConfig,
        test_provider::{Mimics, Outage, TestProvider},
    },
    ingress::tests::{Engine, application, private_config, request_deployment},
    store::Store,
};
use piqueld_core::{
    EnvironmentId,
    manifest::{RouteHostname, ValidatedRoute},
};
use std::sync::Arc;

const PUBLIC: &str = "203.0.113.10";
const TUNNEL: &str = "6ff42ae2-765d-4adf-8112-31c55c1551ef";

/// An ingress whose only provider keeps `example.com` in memory, with
/// private ingress enabled and Docker never called.
struct Harness {
    engine: Engine,
    store: Arc<Store>,
    provider: TestProvider,
    ingress: Ingress,
}

impl Harness {
    async fn new(manage_records: bool) -> Self {
        let engine = Engine::unused().await;
        let provider = TestProvider::new(&["example.com"]);
        let ingress = Self::ingress(&engine, &provider, manage_records, None).await;
        ingress.converged();
        Self {
            store: Arc::clone(&engine.store),
            engine,
            provider,
            ingress,
        }
    }

    /// A manager of `engine`'s store and `provider`'s zone.
    async fn ingress(
        engine: &Engine,
        provider: &TestProvider,
        manage_records: bool,
        tunnel: Option<TunnelCredentials>,
    ) -> Ingress {
        let ingress = engine
            .ingress(true)
            .with_dns(
                &crate::config::DnsConfig {
                    providers: vec![DnsProviderConfig {
                        provider: DnsProvider::Test(provider.clone()),
                        manage_records,
                    }],
                },
                &crate::config::AcmeConfig::default(),
            )
            .unwrap()
            .with_private(&private_config())
            .with_tunnel(&crate::config::TunnelConfig {
                credentials: tunnel,
            })
            .with_public_addresses(&[PUBLIC.parse().unwrap()]);
        ingress.certificates.dns.discover().await;
        ingress
    }

    /// Another manager of this store and zone, as after a restart, before its
    /// gateway has converged.
    async fn restart(&self, tunnel: Option<TunnelCredentials>) -> Ingress {
        Self::ingress(&self.engine, &self.provider, true, tunnel).await
    }

    /// Sets the apps node's tailnet addresses.
    async fn node_addresses(&self, addresses: &[&str]) {
        self.ingress.health.write().await.private.addresses =
            addresses.iter().map(|a| (*a).to_owned()).collect();
    }

    /// Saves an application, returning its environment and the route to
    /// `host` with `visibility`.
    async fn route(&self, host: &str, visibility: &str) -> (EnvironmentId, ValidatedRoute) {
        let name = host.trim_start_matches("*.").split('.').next().unwrap();
        let app = application(name, host, "body");
        let (id, _) = request_deployment(&self.store, app.clone()).await;
        let mut manifest = app.to_manifest();
        manifest.spec.routes[0].visibility = visibility.parse().unwrap();
        let route = manifest.validate().unwrap().spec().routes[0].clone();
        (id, route)
    }

    /// Stages `routes` as a deployment does, `ready` or not.
    async fn stage(&self, id: &EnvironmentId, routes: &[ValidatedRoute], ready: bool) {
        self.store
            .stage_routes(id, routes, ready, None)
            .await
            .unwrap();
    }

    /// Acknowledges the desired table, as the gateway does once applied.
    async fn apply(&self) {
        let table = self.store.routing_table().await.unwrap();
        self.store.acknowledge_routes(&table).await.unwrap();
    }

    /// One reconciliation pass of `ingress`, returning whether it completed.
    async fn pass_of(ingress: &Ingress) -> bool {
        let plan = ingress.plan_records().await.unwrap();
        ingress
            .reconcile_records(&plan, &CancellationToken::new())
            .await
    }

    async fn pass(&self) -> bool {
        Self::pass_of(&self.ingress).await
    }

    fn state(&self, host: &str) -> RouteDns {
        self.ingress.route_dns(&RouteHostname::parse(host).unwrap())
    }

    /// Phases of the journal actions that started since the last call.
    async fn journaled(&self, since: &mut i64) -> Vec<String> {
        let mut events = self.store.events(None, None, 100).await.unwrap().items;
        events.retain(|event| event.id > *since);
        *since = events.iter().map(|event| event.id).max().unwrap_or(*since);
        events.sort_by_key(|event| event.id);
        events
            .into_iter()
            .filter(|event| event.kind == "action_started")
            .map(|event| event.phase.unwrap_or_default())
            .collect()
    }

    /// Asserts that nothing changed at the provider since the last check.
    fn unchanged(&self) {
        assert_eq!(self.provider.changes(), Vec::<String>::new());
    }

    /// The ownership record of `host` for this installation.
    fn owner(&self, host: &str) -> String {
        format!(
            "_piqueld.{host} TXT {:?}",
            self.store.instance_id().as_str()
        )
    }
}

impl Ingress {
    /// Marks the gateway as converged with this configuration.
    fn converged(&self) {
        self.gateway_converged
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[test]
fn changes_update_in_place_and_delete_before_creating() {
    let found = |id: &str, record: Record| Found {
        id: RecordId(id.into()),
        record,
        proxied: false,
    };
    let a = |last: u8| Record::A(std::net::Ipv4Addr::new(192, 0, 2, last));
    let cname = Record::Cname {
        target: Hostname::parse("x.cfargotunnel.com").unwrap(),
        proxied: true,
    };
    // One address changes and a duplicate goes: one update, one deletion.
    let current = [found("1", a(1)), found("2", a(2)), found("3", a(2))];
    assert_eq!(
        Changes::between(&current, &BTreeSet::from([a(2), a(3)])),
        Changes {
            update: vec![(RecordId("1".into()), a(3))],
            delete: vec![RecordId("3".into())],
            create: Vec::new(),
        }
    );
    // A CNAME replaces addresses: they are deleted, then it is created.
    assert_eq!(
        Changes::between(&current[..1], &BTreeSet::from([cname.clone()])),
        Changes {
            update: Vec::new(),
            delete: vec![RecordId("1".into())],
            create: vec![cname],
        }
    );
    assert!(Changes::between(&current[..2], &BTreeSet::from([a(1), a(2)])).is_empty());
    // An address proxied by hand is rewritten unproxied.
    let proxied = Found {
        proxied: true,
        ..found("4", a(1))
    };
    assert_eq!(
        Changes::between(&[proxied], &BTreeSet::from([a(1)])),
        Changes {
            update: vec![(RecordId("4".into()), a(1))],
            ..Changes::default()
        }
    );
}

#[tokio::test]
async fn records_are_written_once_applied_and_never_on_foreign_names() {
    let harness = Harness::new(true).await;
    let (id, public) = harness.route("www.example.com", "public").await;

    // Staged but not applied: nothing is written yet.
    harness
        .stage(&id, std::slice::from_ref(&public), true)
        .await;
    assert!(harness.pass().await);
    assert_eq!(
        harness.state("www.example.com").state,
        DnsRecordState::Pending
    );
    harness.unchanged();

    // Applied: the ownership record, then the A record, in one action.
    harness.apply().await;
    assert!(harness.pass().await);
    assert_eq!(
        harness.provider.dump(),
        [
            harness.owner("www.example.com"),
            format!("www.example.com A {PUBLIC}")
        ]
    );
    assert_eq!(
        harness.state("www.example.com").state,
        DnsRecordState::Managed
    );
    let mut since = 0;
    assert_eq!(harness.journaled(&mut since).await, ["ingress_dns_create"]);
    // An unchanged pass changes nothing and records no history.
    harness.provider.changes();
    assert!(harness.pass().await);
    harness.unchanged();
    assert_eq!(harness.journaled(&mut since).await, Vec::<String>::new());

    // Drift is repaired: an address piqueld owns is changed by hand.
    let drifted = harness.provider.found("www.example.com").unwrap();
    harness
        .provider
        .set(
            "www.example.com",
            Some(&drifted[0].id),
            &Record::A("198.51.100.7".parse().unwrap()),
        )
        .unwrap();
    harness.provider.changes();
    assert!(harness.pass().await);
    assert_eq!(
        harness.provider.changes(),
        [format!("write www.example.com A {PUBLIC}")]
    );
    assert_eq!(harness.journaled(&mut since).await, ["ingress_dns_update"]);

    // A name holding a record piqueld does not own is never written.
    let (other, taken) = harness.route("taken.example.com", "public").await;
    harness.provider.insert(
        "taken.example.com",
        Record::A("198.51.100.1".parse().unwrap()),
    );
    harness.stage(&other, &[taken], true).await;
    harness.apply().await;
    assert!(harness.pass().await);
    harness.unchanged();
    let conflict = harness.state("taken.example.com");
    assert_eq!(conflict.state, DnsRecordState::DnsConflict);
    assert!(
        conflict.message.unwrap().contains("A 198.51.100.1"),
        "conflict names the record"
    );
}

#[tokio::test]
async fn wildcards_get_records_with_ownership_beside_their_parent() {
    let harness = Harness::new(true).await;
    let (id, wildcard) = harness.route("*.tenants.example.com", "public").await;
    harness.stage(&id, &[wildcard], true).await;
    harness.apply().await;
    assert!(harness.pass().await);
    // Providers accept `*` only as a name's first label.
    assert_eq!(
        harness.provider.dump(),
        [
            format!(
                "_piqueld._wildcard.tenants.example.com TXT {:?}",
                harness.store.instance_id().as_str()
            ),
            format!("*.tenants.example.com A {PUBLIC}"),
        ]
    );
    assert_eq!(
        harness.state("*.tenants.example.com").state,
        DnsRecordState::Managed
    );
}

#[tokio::test]
async fn visibility_changes_withdraw_before_repointing() {
    let harness = Harness::new(true).await;
    let (id, public) = harness.route("www.example.com", "public").await;
    harness
        .stage(&id, std::slice::from_ref(&public), true)
        .await;
    harness.apply().await;
    assert!(harness.pass().await);
    harness.provider.changes();

    // Public -> private: the deployment withdraws the route first. Until the
    // gateway acknowledges that, the records stay.
    let mut private = public.clone();
    private.visibility = Visibility::Private;
    harness
        .node_addresses(&["100.64.0.5", "fd7a:115c:a1e0::5"])
        .await;
    harness
        .stage(&id, std::slice::from_ref(&private), false)
        .await;
    assert!(harness.pass().await);
    harness.unchanged();
    // Withdrawn: the records, then the ownership record, are deleted.
    harness.apply().await;
    assert!(harness.pass().await);
    assert_eq!(
        harness.provider.changes(),
        [
            format!("delete www.example.com A {PUBLIC}"),
            format!("delete {}", harness.owner("www.example.com")),
        ]
    );
    let mut since = 0;
    assert_eq!(
        harness.journaled(&mut since).await,
        ["ingress_dns_create", "ingress_dns_delete"]
    );
    // Served privately: only the apps node's tailnet addresses.
    harness
        .stage(&id, std::slice::from_ref(&private), true)
        .await;
    harness.apply().await;
    assert!(harness.pass().await);
    assert_eq!(
        harness.provider.dump(),
        [
            harness.owner("www.example.com"),
            "www.example.com A 100.64.0.5".into(),
            "www.example.com AAAA fd7a:115c:a1e0::5".into(),
        ]
    );
    // The node's address changes: the record is updated in place.
    harness
        .node_addresses(&["100.64.0.9", "fd7a:115c:a1e0::5"])
        .await;
    harness.provider.changes();
    assert!(harness.pass().await);
    assert_eq!(
        harness.provider.changes(),
        ["write www.example.com A 100.64.0.9"]
    );
    // A node without addresses keeps the records.
    harness.node_addresses(&[]).await;
    assert!(harness.pass().await);
    harness.unchanged();
    assert_eq!(
        harness.state("www.example.com").state,
        DnsRecordState::Pending
    );

    // A removed route's records are deleted, even by a restarted daemon.
    harness.stage(&id, &[], true).await;
    harness.apply().await;
    let restarted = harness.restart(None).await;
    restarted.converged();
    assert!(Harness::pass_of(&restarted).await);
    assert_eq!(harness.provider.dump(), Vec::<String>::new());
    assert!(harness.store.dns_records().await.unwrap().is_empty());
}

#[tokio::test]
async fn tunnel_mode_replaces_addresses_with_a_proxied_cname() {
    let harness = Harness::new(true).await;
    let (id, route) = harness.route("www.example.com", "public").await;
    harness.stage(&id, &[route], true).await;
    harness.apply().await;
    assert!(harness.pass().await);
    let tunnel = harness
        .restart(Some(TunnelCredentials {
            id: TUNNEL.parse().unwrap(),
            file: "{}".into(),
        }))
        .await;
    // Until the gateway runs in tunnel mode, the records stay.
    harness.provider.changes();
    assert!(Harness::pass_of(&tunnel).await);
    harness.unchanged();
    tunnel.converged();
    assert!(Harness::pass_of(&tunnel).await);
    assert_eq!(
        harness.provider.changes(),
        [
            format!("delete www.example.com A {PUBLIC}"),
            format!("write www.example.com CNAME (proxied) {TUNNEL}.cfargotunnel.com"),
        ]
    );
}

#[tokio::test]
async fn failures_keep_records_and_unmanaged_zones_stay_manual() {
    let harness = Harness::new(true).await;
    let (id, route) = harness.route("www.example.com", "public").await;
    harness.stage(&id, std::slice::from_ref(&route), true).await;
    harness.apply().await;
    harness.provider.fail(Some(Outage::All));
    assert!(!harness.pass().await, "a failed pass is retried sooner");
    let failed = harness.state("www.example.com");
    assert_eq!(failed.state, DnsRecordState::Pending);
    assert!(failed.message.unwrap().contains("retries"));
    harness.provider.fail(None);
    assert!(harness.pass().await);
    assert_eq!(
        harness.state("www.example.com").state,
        DnsRecordState::Managed
    );

    // Without `manage_records`, the provider is never called.
    let manual = Harness::new(false).await;
    let (id, route) = manual.route("www.example.com", "public").await;
    manual.stage(&id, &[route], true).await;
    manual.apply().await;
    assert!(manual.pass().await);
    manual.unchanged();
    assert_eq!(
        manual.state("www.example.com").state,
        DnsRecordState::Manual
    );
}

#[tokio::test]
async fn disabled_ingress_deletes_records_once_the_gateway_stopped() {
    let harness = Harness::new(true).await;
    let (id, route) = harness.route("www.example.com", "public").await;
    harness.stage(&id, &[route], true).await;
    harness.apply().await;
    assert!(harness.pass().await);
    harness.provider.changes();
    let mut disabled = harness.restart(None).await;
    disabled.enabled = false;
    // The gateway's stop is not confirmed yet: its records stay.
    assert!(Harness::pass_of(&disabled).await);
    harness.unchanged();
    disabled.converged();
    assert!(Harness::pass_of(&disabled).await);
    assert_eq!(harness.provider.dump(), Vec::<String>::new());
}

#[tokio::test]
async fn names_another_installation_claims_are_never_changed() {
    let harness = Harness::new(true).await;
    let (id, route) = harness.route("www.example.com", "public").await;
    harness.stage(&id, &[route], true).await;
    harness.apply().await;
    assert!(harness.pass().await);
    // Another installation also claims the name, and its address drifts.
    harness
        .provider
        .insert("_piqueld.www.example.com", Record::Txt("other".into()));
    let drifted = harness.provider.found("www.example.com").unwrap();
    harness
        .provider
        .set(
            "www.example.com",
            Some(&drifted[0].id),
            &Record::A("198.51.100.7".parse().unwrap()),
        )
        .unwrap();
    harness.provider.changes();
    assert!(harness.pass().await);
    harness.unchanged();
    assert_eq!(
        harness.state("www.example.com").state,
        DnsRecordState::DnsConflict
    );
    // Removing the route deletes only this installation's ownership record.
    harness.stage(&id, &[], true).await;
    harness.apply().await;
    assert!(harness.pass().await);
    assert_eq!(
        harness.provider.changes(),
        [format!("delete {}", harness.owner("www.example.com"))]
    );
    assert_eq!(
        harness.provider.dump(),
        [
            "www.example.com A 198.51.100.7",
            "_piqueld.www.example.com TXT \"other\"",
        ]
    );
}

#[tokio::test]
async fn staged_changes_are_published_after_a_failed_publish() {
    let harness = Harness::new(true).await;
    harness.provider.mimic(Mimics::Ovh);
    let (id, route) = harness.route("www.example.com", "public").await;
    harness.stage(&id, &[route], true).await;
    harness.apply().await;
    assert!(harness.pass().await);
    harness.provider.changes();
    // The route is removed, but publishing its deletion fails.
    harness.stage(&id, &[], true).await;
    harness.apply().await;
    harness.provider.fail(Some(Outage::Publish));
    assert!(!harness.pass().await);
    harness.provider.changes();
    // Nothing is left to delete; the deletion is still published, once.
    harness.provider.fail(None);
    assert!(harness.pass().await);
    assert_eq!(harness.provider.changes(), ["publish"]);
    assert!(harness.store.dns_records().await.unwrap().is_empty());
    assert!(harness.pass().await);
    harness.unchanged();
}

#[tokio::test]
async fn a_route_changed_during_a_pass_waits_for_the_next_one() {
    let harness = Harness::new(true).await;
    let (id, route) = harness.route("www.example.com", "public").await;
    harness.stage(&id, &[route], true).await;
    harness.apply().await;
    // The pass was planned while the route was applied, then the gateway
    // withdrew it: the stale plan writes nothing.
    let plan = harness.ingress.plan_records().await.unwrap();
    harness.stage(&id, &[], true).await;
    harness.apply().await;
    harness
        .ingress
        .reconcile_records(&plan, &CancellationToken::new())
        .await;
    harness.unchanged();
    assert_eq!(
        harness.state("www.example.com").state,
        DnsRecordState::Pending
    );
}

#[tokio::test]
async fn tunnel_routes_wait_in_zones_that_cannot_proxy() {
    let harness = Harness::new(true).await;
    harness.provider.mimic(Mimics::Ovh);
    let (id, route) = harness.route("www.example.com", "public").await;
    harness.stage(&id, &[route], true).await;
    harness.apply().await;
    let tunnel = harness
        .restart(Some(TunnelCredentials {
            id: TUNNEL.parse().unwrap(),
            file: "{}".into(),
        }))
        .await;
    tunnel.converged();
    // Nothing is claimed or written, and the pass does not fail.
    assert!(Harness::pass_of(&tunnel).await);
    harness.unchanged();
    let state = tunnel.route_dns(&RouteHostname::parse("www.example.com").unwrap());
    assert_eq!(state.state, DnsRecordState::Pending);
    assert!(state.message.unwrap().contains("Cloudflare"));
}

#[tokio::test]
async fn the_webhook_hostname_gets_a_public_routes_records() {
    let harness = Harness::new(true).await;
    let hooks = Hostname::parse("hooks.example.com").unwrap();
    let direct = harness
        .restart(None)
        .await
        .with_webhooks(Some(hooks.clone()));
    direct.converged();
    assert!(Harness::pass_of(&direct).await);
    assert_eq!(
        harness.provider.dump(),
        [
            harness.owner("hooks.example.com"),
            format!("hooks.example.com A {PUBLIC}")
        ]
    );
    // In tunnel mode, the proxied CNAME to the tunnel replaces it.
    let tunnel = TunnelCredentials {
        id: TUNNEL.parse().unwrap(),
        file: "{}".into(),
    };
    let tunneled = harness
        .restart(Some(tunnel))
        .await
        .with_webhooks(Some(hooks));
    tunneled.converged();
    harness.provider.changes();
    assert!(Harness::pass_of(&tunneled).await);
    assert_eq!(
        harness.provider.changes(),
        [
            format!("delete hooks.example.com A {PUBLIC}"),
            format!("write hooks.example.com CNAME (proxied) {TUNNEL}.cfargotunnel.com"),
        ]
    );
}
