//! Test-only provider keeping records in memory, shared by its clones so a
//! test can inspect and change what reconciliation sees.
use super::{ApiError, Found, Record, RecordId, Zone};
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
    /// Every call fails while set, as during a provider outage.
    failing: bool,
    /// Changes wait for a publish, like OVH's, while set.
    staging: bool,
    /// Publishing fails while set.
    failing_publish: bool,
    /// Every change made through the provider, such as `delete A 192.0.2.1`.
    changes: Vec<String>,
}

/// An in-memory provider serving fixed zones.
#[derive(Clone, Debug)]
pub struct Memory {
    zones: Vec<Hostname>,
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.records.iter()).finish()
    }
}

/// Clones share their records, so they are equal.
impl PartialEq for Memory {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }
}

impl Eq for Memory {}

impl Memory {
    pub(crate) fn new(zones: &[&str]) -> Self {
        Self {
            zones: zones.iter().map(|z| Hostname::parse(*z).unwrap()).collect(),
            state: Arc::default(),
        }
    }

    fn state(&self) -> Result<std::sync::MutexGuard<'_, State>, ApiError> {
        let state = self.state.lock().unwrap();
        if state.failing {
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

    /// Makes every call fail, or succeed again.
    pub(crate) fn fail(&self, failing: bool) {
        self.state.lock().unwrap().failing = failing;
    }

    /// Makes changes wait for a publish, like OVH's.
    pub(crate) fn stage(&self) {
        self.state.lock().unwrap().staging = true;
    }

    /// Makes publishing fail, or succeed again.
    pub(crate) fn fail_publish(&self, failing: bool) {
        self.state.lock().unwrap().failing_publish = failing;
    }

    pub(super) fn stages_changes(&self) -> bool {
        self.state.lock().unwrap().staging
    }

    /// Records a publish of staged changes.
    pub(super) fn publish(&self) -> Result<(), ApiError> {
        let mut state = self.state()?;
        if !state.staging {
            return Ok(());
        }
        if state.failing_publish {
            return Err(ApiError::Unsupported("publish failed"));
        }
        state.changes.push("publish".into());
        Ok(())
    }

    pub(super) fn zones(&self) -> Result<Vec<Zone>, ApiError> {
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

    pub(crate) fn records(&self, name: &str) -> Result<Vec<Found>, ApiError> {
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

    pub(crate) fn upsert(
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

    pub(super) fn delete_record(&self, id: &RecordId) -> Result<(), ApiError> {
        let mut state = self.state()?;
        if let Some((name, record)) = state.records.remove(&id.0.parse().unwrap()) {
            state.changes.push(format!("delete {name} {record}"));
        }
        Ok(())
    }
}
