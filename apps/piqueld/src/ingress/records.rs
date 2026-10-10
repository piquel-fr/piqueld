//! Routes' DNS records, managed in the zones of providers that set
//! `manage_records`:
//!
//! ```text
//! private route           A/AAAA -> the apps node's tailnet addresses
//! public route, tunnel    CNAME (proxied) -> <tunnel-id>.cfargotunnel.com
//! public route, direct    A/AAAA -> [ingress] public_addresses (else manual)
//! ```
//!
//! The webhook hostname, `[ingress] webhook_hostname`, gets a public route's
//! records.
//!
//! **Ordering.** Records follow the routes the gateway has acknowledged
//! ([`Store::applied_table`](crate::store::Store::applied_table)), so they
//! are written once the gateway serves a route and deleted or repointed only
//! after it has withdrawn it. A route waiting for the gateway keeps its
//! current records, and so does every route until the gateway has converged
//! with this daemon's configuration, such as a new ingress mode. Each
//! hostname's plan is checked again just before its records change.
//!
//! **Ownership.** piqueld only changes the records of names it owns: those
//! with a `_piqueld.<hostname>` TXT record holding the installation ID, and
//! no other installation's. A name already holding A/AAAA/CNAME records, or
//! another installation's ownership record, is a `dns_conflict` and never
//! written. Claimed names are
//! kept in the store until their records are deleted, so a removed route's
//! records are found after a restart.
//!
//! **Reconciliation.** Every 10 seconds the desired records are recomputed
//! from the store and the apps node's addresses, without calling providers.
//! A pass runs when they changed, every 5 minutes to repair drift, and a
//! minute after a failed pass. Changes OVH stages are marked unpublished in
//! the store until published, so a failed publish is retried. Changes are
//! daemon-scoped journal actions
//! with `ingress_dns_*` phases; a pass that changes nothing records nothing.
use super::{Ingress, wire::Journaled};
use crate::dns::provider::{DnsProvider, Found, Record, RecordId, Zone};
use anyhow::{Context, Result};
use piqueld_core::{
    api::DnsRecordState,
    manifest::{Hostname, Visibility},
    observability::DiagnosticCode,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

/// Passes run at least this often, repairing drift.
const REPAIR: Duration = Duration::from_mins(5);
/// A pass that failed for some hostname is retried after this long.
const RETRY: Duration = Duration::from_mins(1);

/// What one hostname's records should be.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Desired {
    /// Exactly these A/AAAA/CNAME records.
    Records(BTreeSet<Record>),
    /// Not known yet; current records are kept.
    Pending(&'static str),
    /// No route needs the hostname: the records piqueld owns are deleted.
    Removed,
}

impl Desired {
    /// Whether it includes a proxied CNAME record.
    fn proxied(&self) -> bool {
        matches!(self, Self::Records(records)
            if records.iter().any(|record| matches!(record, Record::Cname { proxied: true, .. })))
    }
}

/// What piqueld does with one hostname's records.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Plan {
    /// The operator manages them.
    Manual,
    /// piqueld manages them in `zone` of provider `provider`.
    Managed {
        provider: usize,
        zone: Zone,
        desired: Desired,
    },
}

/// The DNS state of one route hostname, as route status reports it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct RouteDns {
    pub(super) state: DnsRecordState,
    /// Safe diagnostic while records are pending or conflict.
    pub(super) message: Option<String>,
}

impl RouteDns {
    fn new(state: DnsRecordState, message: impl Into<Option<String>>) -> Self {
        Self {
            state,
            message: message.into(),
        }
    }
}

/// Changes turning a name's current records into the desired ones.
#[derive(Debug, Default, Eq, PartialEq)]
struct Changes {
    /// Records replaced in place by one of the same type.
    update: Vec<(RecordId, Record)>,
    delete: Vec<RecordId>,
    create: Vec<Record>,
}

impl Changes {
    /// Pairs surplus records with missing ones of the same type, which are
    /// updated in place; the rest are deleted, then the missing created. A
    /// CNAME cannot coexist with A/AAAA records, so deletions come first. A
    /// record whose proxying was changed counts as surplus.
    fn between(current: &[Found], desired: &BTreeSet<Record>) -> Self {
        let mut missing: Vec<&Record> = desired
            .iter()
            .filter(|record| !current.iter().any(|found| found.exact() == Some(*record)))
            .collect();
        let mut kept = BTreeSet::new();
        let mut changes = Self::default();
        for found in current {
            if let Some(record) = found.exact()
                && desired.contains(record)
                && kept.insert(record)
            {
                continue;
            }
            match missing
                .iter()
                .position(|record| record.kind() == found.record.kind())
            {
                Some(index) => changes
                    .update
                    .push((found.id.clone(), missing.remove(index).clone())),
                None => changes.delete.push(found.id.clone()),
            }
        }
        changes.create = missing.into_iter().cloned().collect();
        changes
    }

    fn is_empty(&self) -> bool {
        self.update.is_empty() && self.delete.is_empty() && self.create.is_empty()
    }
}

impl Ingress {
    /// Every 10 seconds, plans each hostname's records and reconciles them
    /// when the plan changed, every 5 minutes, or a minute after a failure.
    /// Does nothing while no provider manages records.
    pub(super) async fn run_records(&self, cancellation: &CancellationToken) {
        let dns = &self.certificates.dns;
        if !dns.manages_any_records() {
            return;
        }
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last: Option<(BTreeMap<Hostname, Plan>, Instant, bool)> = None;
        loop {
            tokio::select! { ()=cancellation.cancelled()=>return, _=tick.tick()=>{} }
            if cancellation
                .run_until_cancelled(dns.refresh())
                .await
                .is_none()
            {
                return;
            }
            let plan = match self.plan_records().await {
                Ok(plan) => plan,
                Err(error) => {
                    tracing::warn!(error=?error, "could not plan route DNS records");
                    continue;
                }
            };
            let due = last.as_ref().is_none_or(|(previous, at, complete)| {
                previous != &plan || at.elapsed() >= if *complete { REPAIR } else { RETRY }
            });
            if due {
                let complete = self.reconcile_records(&plan, cancellation).await;
                last = Some((plan, Instant::now(), complete));
            }
        }
    }

    /// Each hostname's plan: applied routes' records, routes waiting for the
    /// gateway kept as they are, and claimed names without a route removed.
    /// Disabled ingress removes every record. Until the gateway has converged
    /// (or stopped) with this daemon's configuration, such as a new ingress
    /// mode after a restart, every record is kept. Hostnames outside the zones
    /// of providers managing records are manual.
    async fn plan_records(&self) -> Result<BTreeMap<Hostname, Plan>> {
        // `None` is manual.
        let mut wanted: BTreeMap<Hostname, Option<Desired>> = BTreeMap::new();
        let addresses: Vec<IpAddr> = self
            .health
            .read()
            .await
            .private
            .addresses
            .iter()
            .filter_map(|address| address.parse().ok())
            .collect();
        if self.enabled {
            for route in self.store.applied_table().await?.into_values().flatten() {
                let desired = self.desired_records(route.visibility, &addresses);
                wanted.insert(route.hostname, desired);
            }
            // The webhook hostname is served on the public listener, like a
            // public route, from the gateway's own configuration.
            if let Some(hostname) = self.webhook_hostname() {
                let desired = self.desired_records(Visibility::Public, &addresses);
                wanted.insert(hostname.clone(), desired);
            }
            for route in self.store.routing_table().await?.into_values().flatten() {
                wanted
                    .entry(route.hostname)
                    .or_insert(Some(Desired::Pending(
                        "Waiting for the gateway to apply the route",
                    )));
            }
        }
        for hostname in self.store.dns_records().await? {
            wanted.entry(hostname).or_insert(Some(Desired::Removed));
        }
        if !self
            .gateway_converged
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            for desired in wanted.values_mut().flatten() {
                *desired = Desired::Pending(
                    "Waiting for the gateway to apply this daemon's configuration",
                );
            }
        }
        let dns = &self.certificates.dns;
        let mut plan = BTreeMap::new();
        for (hostname, desired) in wanted {
            let owner = dns.zone_for(&hostname).await;
            let entry = match (desired, owner) {
                (Some(desired), Ok((provider, zone))) if dns.manages_records(provider) => {
                    // Writing them would fail on every pass.
                    let desired = if desired.proxied() && !dns.provider(provider).proxies() {
                        Desired::Pending(
                            "Tunnel routes need a proxied CNAME, which only a Cloudflare zone can hold. Move the zone to Cloudflare",
                        )
                    } else {
                        desired
                    };
                    Plan::Managed {
                        provider,
                        zone,
                        desired,
                    }
                }
                _ => Plan::Manual,
            };
            plan.insert(hostname, entry);
        }
        Ok(plan)
    }

    /// The records a hostname served with `visibility` needs, or `None` when
    /// they are manual: a direct public one without `[ingress]
    /// public_addresses`. Private ones only ever get the apps node's tailnet
    /// `addresses`.
    fn desired_records(&self, visibility: Visibility, addresses: &[IpAddr]) -> Option<Desired> {
        let addresses = match (visibility, &self.tunnel) {
            (Visibility::Private, _) if self.node.is_none() => {
                return Some(Desired::Pending(
                    "Private ingress is disabled, so the route is not served",
                ));
            }
            (Visibility::Private, _) if addresses.is_empty() => {
                return Some(Desired::Pending(
                    "Waiting for the apps node's tailnet addresses",
                ));
            }
            (Visibility::Private, _) => addresses,
            (Visibility::Public, Some(tunnel)) => {
                let target = Hostname::parse(tunnel.hostname()).ok()?;
                return Some(Desired::Records(BTreeSet::from([Record::Cname {
                    target,
                    proxied: true,
                }])));
            }
            (Visibility::Public, None) if self.public_addresses.is_empty() => return None,
            (Visibility::Public, None) => &self.public_addresses,
        };
        Some(Desired::Records(
            addresses.iter().copied().map(Record::address).collect(),
        ))
    }

    /// Reconciles every managed hostname, one at a time, and publishes their
    /// states for route status. Returns whether every hostname succeeded.
    /// Shutdown stops the pass between hostnames.
    ///
    /// Provider calls are slow, so the gateway may change routes during a
    /// pass. Each hostname's plan is checked again just before its records
    /// change, and a hostname whose plan changed waits for the next pass.
    async fn reconcile_records(
        &self,
        plan: &BTreeMap<Hostname, Plan>,
        cancellation: &CancellationToken,
    ) -> bool {
        let mut states = BTreeMap::new();
        let mut complete = true;
        for (hostname, plan) in plan {
            if cancellation.is_cancelled() {
                return false;
            }
            let state = self.converge(hostname, plan).await.unwrap_or_else(|error| {
                tracing::warn!(%hostname, error=format!("{error:#}"), "route DNS records could not be updated; will retry");
                complete = false;
                RouteDns::new(
                    DnsRecordState::Pending,
                    "The DNS provider could not update the records; piqueld retries within a minute. See daemon logs for details.".to_owned(),
                )
            });
            states.insert(hostname.clone(), state);
        }
        *self.records.write().expect("DNS record state lock") = states;
        complete
    }

    /// Applies one hostname's `plan`, unless it changed since it was made.
    async fn converge(&self, hostname: &Hostname, plan: &Plan) -> Result<RouteDns> {
        let Plan::Managed {
            provider,
            zone,
            desired,
        } = plan
        else {
            return Ok(RouteDns::default());
        };
        let desired = match desired {
            Desired::Pending(reason) => {
                return Ok(RouteDns::new(DnsRecordState::Pending, (*reason).to_owned()));
            }
            Desired::Records(records) => Some(records),
            Desired::Removed => None,
        };
        if self.plan_records().await?.get(hostname) != Some(plan) {
            return Ok(RouteDns::new(
                DnsRecordState::Pending,
                "The route changed during reconciliation; piqueld retries shortly".to_owned(),
            ));
        }
        self.converge_records(hostname, *provider, zone, desired)
            .await
    }

    /// Converges one hostname's records to `desired`, or deletes them when
    /// `None`, owning the name first. Never changes a name piqueld does not
    /// own, nor one another installation also claims. Changes a provider
    /// staged but did not publish, such as after a failed OVH refresh, are
    /// published even when nothing else changed.
    async fn converge_records(
        &self,
        hostname: &Hostname,
        provider: usize,
        zone: &Zone,
        desired: Option<&BTreeSet<Record>>,
    ) -> Result<RouteDns> {
        let dns = &self.certificates.dns;
        let site = Site {
            provider: dns.provider(provider),
            http: dns.http(),
            zone,
            hostname,
        };
        let ours = Record::Txt(self.instance_id.clone());
        let (current, ownership) = site.read().await?;
        let (ours_found, foreign): (Vec<&Found>, Vec<&Found>) =
            ownership.iter().partition(|found| found.record == ours);
        // Another installation's claim makes the name shared: never exclusive.
        let owned = !ours_found.is_empty() && foreign.is_empty();
        let republish = self.store.dns_records_unpublished(hostname).await?;
        let Some(desired) = desired else {
            // Only this installation's own records: the addresses when it
            // owns the name, and its ownership records in any case.
            let delete: Vec<RecordId> = current
                .iter()
                .filter(|_| owned)
                .chain(ours_found)
                .map(|found| found.id.clone())
                .collect();
            if !delete.is_empty() || republish {
                let changes = Changes {
                    delete,
                    ..Changes::default()
                };
                self.change_records("ingress_dns_delete", &site, None, &changes)
                    .await?;
            }
            self.store.release_dns_records(hostname).await?;
            return Ok(RouteDns::default());
        };
        if !owned && (!current.is_empty() || !ownership.is_empty()) {
            let existing: Vec<String> = current
                .iter()
                .chain(&ownership)
                .map(|found| found.record.to_string())
                .collect();
            return Ok(RouteDns::new(
                DnsRecordState::DnsConflict,
                format!(
                    "Records piqueld does not own already exist ({}). piqueld never overwrites them; remove them to let it manage {hostname}",
                    existing.join(", ")
                ),
            ));
        }
        let changes = Changes::between(&current, desired);
        if !owned {
            self.change_records("ingress_dns_create", &site, Some(&ours), &changes)
                .await?;
        } else if !changes.is_empty() || republish {
            self.change_records("ingress_dns_update", &site, None, &changes)
                .await?;
        }
        Ok(RouteDns::new(DnsRecordState::Managed, None))
    }

    /// Writes `changes` in one journal action, after the ownership record
    /// `claim` when given. The hostname is tracked in the store first, with
    /// changes a provider stages marked unpublished until it publishes them.
    async fn change_records(
        &self,
        phase: &str,
        site: &Site<'_>,
        claim: Option<&Record>,
        changes: &Changes,
    ) -> Result<()> {
        let stages = site.provider.stages_changes();
        self.store.track_dns_records(site.hostname, stages).await?;
        let journal = self
            .journal_as(
                DiagnosticCode::DnsRecordsFailed,
                phase,
                site.hostname.as_str(),
            )
            .await
            .context("record the DNS change in the journal")?;
        let result = site.apply(&journal, claim, changes).await;
        journal.finish(result).await?;
        if stages {
            self.store.track_dns_records(site.hostname, false).await?;
        }
        tracing::info!(hostname=%site.hostname, phase, "changed route DNS records");
        Ok(())
    }

    /// The DNS state of `hostname` from the latest pass.
    pub(super) fn route_dns(&self, hostname: &Hostname) -> RouteDns {
        self.records
            .read()
            .expect("DNS record state lock")
            .get(hostname)
            .cloned()
            .unwrap_or_default()
    }
}

/// Where one hostname's records live.
struct Site<'a> {
    provider: &'a DnsProvider,
    http: &'a reqwest::Client,
    zone: &'a Zone,
    hostname: &'a Hostname,
}

impl Site<'_> {
    /// The ownership record's name: `_piqueld.<hostname>`.
    fn owner(&self) -> String {
        format!("_piqueld.{}", self.hostname)
    }

    /// The hostname's A/AAAA/CNAME records, and the TXT records of its
    /// ownership name.
    async fn read(&self) -> Result<(Vec<Found>, Vec<Found>)> {
        let (provider, http, zone) = (self.provider, self.http, self.zone);
        let mut current = provider.records(http, zone, self.hostname.as_str()).await?;
        current.retain(|found| !matches!(found.record, Record::Txt(_)));
        let mut ownership = provider.records(http, zone, &self.owner()).await?;
        ownership.retain(|found| matches!(found.record, Record::Txt(_)));
        Ok((current, ownership))
    }

    /// Writes the ownership record `claim`, then `changes` in order, then
    /// publishes the zone. Each request is committed to `journal` first.
    async fn apply(
        &self,
        journal: &Journaled<'_>,
        claim: Option<&Record>,
        changes: &Changes,
    ) -> Result<()> {
        let (provider, http, zone) = (self.provider, self.http, self.zone);
        let name = self.hostname.as_str();
        if let Some(claim) = claim {
            journal.request().await?;
            provider
                .upsert(http, zone, &self.owner(), None, claim)
                .await?;
        }
        for (id, record) in &changes.update {
            journal.request().await?;
            provider.upsert(http, zone, name, Some(id), record).await?;
        }
        for id in &changes.delete {
            journal.request().await?;
            provider.delete_record(http, zone, id).await?;
        }
        for record in &changes.create {
            journal.request().await?;
            provider.upsert(http, zone, name, None, record).await?;
        }
        journal.request().await?;
        provider.publish(http, zone).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
