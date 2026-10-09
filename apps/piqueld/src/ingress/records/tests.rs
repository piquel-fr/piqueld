use super::*;
use crate::{
    config::TunnelCredentials,
    dns::{DnsProvider, DnsProviderConfig, memory::Memory},
    ingress::tests::{application, request_deployment},
    store::Store,
};
use piqueld_core::EnvironmentId;
use std::sync::Arc;

const PUBLIC: &str = "203.0.113.10";
const TUNNEL: &str = "6ff42ae2-765d-4adf-8112-31c55c1551ef";

/// An ingress whose only provider keeps `example.com` in memory, with
/// private ingress enabled and Docker never called.
struct Harness {
    directory: tempfile::TempDir,
    store: Arc<Store>,
    provider: Memory,
    ingress: Ingress,
}

impl Harness {
    async fn new(manage_records: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(directory.path().join("db")).await.unwrap());
        let provider = Memory::new(&["example.com"]);
        let ingress = Self::ingress(&directory, &store, &provider, manage_records, None).await;
        Self {
            directory,
            store,
            provider,
            ingress,
        }
    }

    /// Another manager of the same store and zone, as after a restart.
    async fn ingress(
        directory: &tempfile::TempDir,
        store: &Arc<Store>,
        provider: &Memory,
        manage_records: bool,
        tunnel: Option<TunnelCredentials>,
    ) -> Ingress {
        // Docker clients require the socket file to exist; nothing listens on it.
        let socket = directory.path().join("docker.sock");
        if !socket.exists() {
            drop(tokio::net::UnixListener::bind(&socket).unwrap());
        }
        let ingress = Ingress::new(true, &socket, directory.path(), Arc::clone(store))
            .unwrap()
            .with_dns(
                &crate::config::DnsConfig {
                    providers: vec![DnsProviderConfig {
                        provider: DnsProvider::Memory(provider.clone()),
                        manage_records,
                    }],
                },
                &crate::config::AcmeConfig::default(),
            )
            .unwrap()
            .with_private(&crate::config::PrivateIngressConfig {
                enabled: true,
                ..Default::default()
            })
            .with_tunnel(&crate::config::TunnelConfig {
                credentials: tunnel,
            })
            .with_public_addresses(&[PUBLIC.parse().unwrap()]);
        ingress.certificates.dns.discover().await;
        ingress
    }

    /// Another manager of this store and zone, as after a restart.
    async fn restart(&self, tunnel: Option<TunnelCredentials>) -> Ingress {
        Self::ingress(&self.directory, &self.store, &self.provider, true, tunnel).await
    }

    /// Sets the apps node's tailnet addresses.
    async fn node_addresses(&self, addresses: &[&str]) {
        self.ingress.health.write().await.private.addresses =
            addresses.iter().map(|a| (*a).to_owned()).collect();
    }

    /// Saves an application, returning its environment and the route to
    /// `host` with `visibility`.
    async fn route(&self, host: &str, visibility: &str) -> (EnvironmentId, ValidatedRoute) {
        let app = application(host.split('.').next().unwrap(), host, "body");
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
        self.ingress.route_dns(&Hostname::parse(host).unwrap())
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
        format!("_piqueld.{host} TXT {:?}", self.store.instance_id())
    }
}

#[test]
fn changes_update_in_place_and_delete_before_creating() {
    let found = |id: &str, record: Record| Found {
        id: RecordId(id.into()),
        record,
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
    let drifted = harness.provider.records("www.example.com").unwrap();
    harness
        .provider
        .upsert(
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
    harness.provider.changes();
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
    harness.provider.fail(true);
    assert!(!harness.pass().await, "a failed pass is retried sooner");
    let failed = harness.state("www.example.com");
    assert_eq!(failed.state, DnsRecordState::Pending);
    assert!(failed.message.unwrap().contains("retries"));
    harness.provider.fail(false);
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
    disabled.health.write().await.healthy = true;
    assert!(Harness::pass_of(&disabled).await);
    assert_eq!(harness.provider.dump(), Vec::<String>::new());
}
