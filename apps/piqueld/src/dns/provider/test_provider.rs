//! Test-only provider keeping records in memory, shared by its clones so a
//! test can inspect and change what reconciliation sees.
use super::{ApiError, Found, Provider, Record, RecordId, Zone};
use piqueld_core::manifest::Hostname;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

/// Records by ID, as fully qualified name and data.
#[derive(Default)]
struct State {
    records: BTreeMap<u64, (String, Record)>,
    next: u64,
    /// The real provider whose behaviour it mimics.
    mimics: Mimics,
    /// Which calls fail, if any.
    outage: Option<Outage>,
    /// Every change made through the provider, such as `delete A 192.0.2.1`.
    changes: Vec<String>,
}

/// The real provider a [`TestProvider`] behaves like.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Mimics {
    /// Changes apply at once, and CNAME records can be proxied.
    #[default]
    Cloudflare,
    /// Changes wait for a publish, and proxied records are refused.
    Ovh,
}

/// Calls that fail, as during a provider outage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Outage {
    /// Every call.
    All,
    /// Only publishing.
    Publish,
}

/// An in-memory provider serving fixed zones.
#[derive(Clone, Debug)]
pub struct TestProvider {
    zones: Vec<Hostname>,
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.records.iter()).finish()
    }
}

/// Clones share their records, so they are equal.
impl PartialEq for TestProvider {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }
}

impl Eq for TestProvider {}

impl TestProvider {
    pub(crate) fn new(zones: &[&str]) -> Self {
        Self {
            zones: zones.iter().map(|z| Hostname::parse(*z).unwrap()).collect(),
            state: Arc::default(),
        }
    }

    fn state(&self) -> Result<std::sync::MutexGuard<'_, State>, ApiError> {
        let state = self.state.lock().unwrap();
        if state.outage == Some(Outage::All) {
            return Err(ApiError::Unsupported("provider unavailable"));
        }
        Ok(state)
    }

    /// Adds a record as an operator would, outside piqueld.
    pub(crate) fn insert(&self, name: &str, record: Record) {
        let mut state = self.state.lock().unwrap();
        state.next += 1;
        let id = state.next;
        state.records.insert(id, (name.into(), record));
    }

    /// Every record, as `name kind content` lines in ID order.
    pub(crate) fn dump(&self) -> Vec<String> {
        let state = self.state.lock().unwrap();
        state
            .records
            .values()
            .map(|(name, record)| format!("{name} {record}"))
            .collect()
    }

    /// Changes made since the last call, in order.
    pub(crate) fn changes(&self) -> Vec<String> {
        std::mem::take(&mut self.state.lock().unwrap().changes)
    }

    /// Makes `outage` calls fail, or none.
    pub(crate) fn fail(&self, outage: Option<Outage>) {
        self.state.lock().unwrap().outage = outage;
    }

    /// Behaves like `mimics`.
    pub(crate) fn mimic(&self, mimics: Mimics) {
        self.state.lock().unwrap().mimics = mimics;
    }

    /// The records named `name`, as the provider lists them.
    pub(crate) fn found(&self, name: &str) -> Result<Vec<Found>, ApiError> {
        Ok(self
            .state()?
            .records
            .iter()
            .filter(|(_, (record_name, _))| record_name == name)
            .map(|(id, (_, record))| Found {
                id: RecordId(id.to_string()),
                record: record.clone(),
                proxied: matches!(record, Record::Cname { proxied: true, .. }),
            })
            .collect())
    }

    /// Creates a record, or replaces record `id`, as an API call does.
    pub(crate) fn set(
        &self,
        name: &str,
        id: Option<&RecordId>,
        record: &Record,
    ) -> Result<RecordId, ApiError> {
        let mut state = self.state()?;
        let id = if let Some(id) = id {
            id.0.parse().unwrap()
        } else {
            state.next += 1;
            state.next
        };
        state.changes.push(format!("write {name} {record}"));
        state.records.insert(id, (name.into(), record.clone()));
        Ok(RecordId(id.to_string()))
    }

    fn zone_list(&self) -> Result<Vec<Zone>, ApiError> {
        let _state = self.state()?;
        Ok(self
            .zones
            .iter()
            .map(|name| Zone {
                name: name.clone(),
                id: name.to_string(),
            })
            .collect())
    }

    fn remove(&self, id: &RecordId) -> Result<(), ApiError> {
        let mut state = self.state()?;
        if let Some((name, record)) = state.records.remove(&id.0.parse().unwrap()) {
            state.changes.push(format!("delete {name} {record}"));
        }
        Ok(())
    }

    /// Records a publish of staged changes.
    fn published(&self) -> Result<(), ApiError> {
        let mut state = self.state()?;
        if state.mimics != Mimics::Ovh {
            return Ok(());
        }
        if state.outage == Some(Outage::Publish) {
            return Err(ApiError::Unsupported("publish failed"));
        }
        state.changes.push("publish".into());
        Ok(())
    }
}

/// Every call completes at once.
impl Provider for TestProvider {
    fn kind(&self) -> &'static str {
        "test"
    }

    fn zones(&self, _http: &reqwest::Client) -> impl Future<Output = Result<Vec<Zone>, ApiError>> {
        std::future::ready(self.zone_list())
    }

    fn records(
        &self,
        _http: &reqwest::Client,
        _zone: &Zone,
        name: &str,
    ) -> impl Future<Output = Result<Vec<Found>, ApiError>> {
        std::future::ready(self.found(name))
    }

    fn upsert(
        &self,
        _http: &reqwest::Client,
        _zone: &Zone,
        name: &str,
        id: Option<&RecordId>,
        record: &Record,
    ) -> impl Future<Output = Result<RecordId, ApiError>> {
        std::future::ready(self.set(name, id, record))
    }

    fn delete_record(
        &self,
        _http: &reqwest::Client,
        _zone: &Zone,
        id: &RecordId,
    ) -> impl Future<Output = Result<(), ApiError>> {
        std::future::ready(self.remove(id))
    }

    fn proxies(&self) -> bool {
        self.state.lock().unwrap().mimics == Mimics::Cloudflare
    }

    fn stages_changes(&self) -> bool {
        self.state.lock().unwrap().mimics == Mimics::Ovh
    }

    fn publish(
        &self,
        _http: &reqwest::Client,
        _zone: &Zone,
    ) -> impl Future<Output = Result<(), ApiError>> {
        std::future::ready(self.published())
    }
}
