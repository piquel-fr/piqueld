//! Versioned, bounded, and transactional control-plane state transfer.
//!
//! Archives contain configuration and database metadata only. They never carry
//! plaintext secrets, master keys, volumes, registry blobs, logs, worktrees, or
//! external ingress state.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use piqueld_client::{
    ImportDependencyReport, MAX_STATE_ARCHIVE_BYTES, StateExportMode, StateImportResult,
};
use piqueld_core::{InstanceId, NormalizedApplication, parse_toml};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Cursor, Read},
    sync::{Arc, RwLock},
};
use tar::EntryType;
use thiserror::Error;
use uuid::Uuid;

use crate::{
    secrets::{ImportEnvelope, SecretService},
    store::{SqliteStore, StoreError, recompile_imported_application, validate_resolved},
};

/// Binary media type for state archive v1.
pub const ARCHIVE_CONTENT_TYPE: &str = "application/vnd.piqueld.state-v1+tar";
/// Maximum complete archive size accepted by the daemon.
pub const MAX_ARCHIVE_BYTES: usize = MAX_STATE_ARCHIVE_BYTES;
/// Maximum size of one regular archive entry.
pub const MAX_ENTRY_BYTES: u64 = 4 * 1024 * 1024;
/// Maximum number of archive entries.
pub const MAX_ENTRIES: usize = 2_048;
const FORMAT: &str = "piqueld-state-archive";
const VERSION: u32 = 1;

#[derive(Debug, Error)]
/// Sanitized state-transfer failures.
pub enum TransferError {
    /// Archive entries or framing are malformed.
    #[error("archive is malformed")]
    Malformed,
    /// An archive exceeds a bounded safety limit.
    #[error("archive exceeds a safety limit")]
    Limit,
    /// An entry digest does not match the archive manifest.
    #[error("archive checksum verification failed")]
    Checksum,
    /// The archive format or database schema is unsupported.
    #[error("archive schema is unsupported")]
    Schema,
    /// The archive failed domain or authenticated-envelope validation.
    #[error("archive state failed domain validation")]
    Validation,
    /// The state-transfer repository operation failed.
    #[error("state transfer storage failed")]
    Storage,
    /// The destructive replacement confirmation is invalid.
    #[error("state import confirmation is invalid")]
    Confirmation,
    /// Another import already owns the maintenance gate.
    #[error("state maintenance is already active")]
    Maintenance,
}

impl From<StoreError> for TransferError {
    fn from(_: StoreError) -> Self {
        Self::Storage
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ArchiveManifest {
    format: String,
    version: u32,
    mode: StateExportMode,
    source_instance_id: String,
    database_schema_version: u64,
    entry_count: usize,
    entries: BTreeMap<String, EntryDigest>,
    exclusions: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EntryDigest {
    bytes: u64,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArchiveState {
    pub(crate) source_instance_id: String,
    pub(crate) source_created_at_ms: i64,
    pub(crate) applications: Vec<ArchiveApplication>,
    pub(crate) secrets: Vec<ArchiveSecret>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArchiveApplication {
    pub(crate) application: NormalizedApplication,
    pub(crate) resolved: piqueld_core::resource::ResolvedApplication,
    pub(crate) generation: u64,
    pub(crate) spec_hash: String,
    pub(crate) created_at_ms: i64,
    pub(crate) updated_at_ms: i64,
    pub(crate) status: ArchiveStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArchiveStatus {
    pub(crate) state: String,
    pub(crate) observed_generation: Option<u64>,
    pub(crate) message: Option<String>,
    pub(crate) updated_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArchiveSecret {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) generation: u64,
    pub(crate) value_is_set: bool,
    pub(crate) created_at_ms: i64,
    pub(crate) updated_at_ms: i64,
    pub(crate) current: Option<ArchiveEnvelope>,
    pub(crate) generations: Vec<ArchiveEnvelope>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArchiveEnvelope {
    pub(crate) generation: u64,
    pub(crate) algorithm: String,
    pub(crate) key_id: String,
    pub(crate) nonce_base64: String,
    pub(crate) ciphertext_base64: String,
    pub(crate) content_hash: String,
    pub(crate) swarm_secret_name: String,
    pub(crate) created_at_ms: i64,
    pub(crate) retired_at_ms: Option<i64>,
}

#[derive(Clone)]
/// Audited state archive exporter and transactional importer.
pub struct StateTransferService {
    store: Arc<SqliteStore>,
    secret_service: Arc<RwLock<Option<Arc<SecretService>>>>,
    #[cfg(test)]
    fail_after_delete: Arc<std::sync::atomic::AtomicBool>,
}

impl StateTransferService {
    #[must_use]
    /// Creates a transfer service over one control-plane database.
    pub fn new(store: Arc<SqliteStore>) -> Self {
        Self {
            store,
            secret_service: Arc::new(RwLock::new(None)),
            #[cfg(test)]
            fail_after_delete: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    pub(crate) fn set_secret_service(&self, service: Arc<SecretService>) {
        *self
            .secret_service
            .write()
            .expect("secret service lock is not poisoned") = Some(service);
    }

    /// Exports a deterministic archive from one consistent database snapshot.
    ///
    /// # Errors
    /// Returns a sanitized transfer or storage error when the snapshot cannot
    /// be serialized or the bounded archive cannot be built.
    pub async fn export(&self, mode: StateExportMode) -> Result<Vec<u8>, TransferError> {
        let operation = operation_id();
        self.audit_start(&operation, "export", mode_str(mode), None)
            .await?;
        let maintenance = self.store.maintenance_gate();
        let _lease = maintenance.read().await;
        let result = self.export_inner(mode).await;
        let archive_digest = result.as_ref().ok().map(digest);
        let _ = self
            .audit_finish(&operation, result.is_ok(), archive_digest.as_deref(), None)
            .await;
        result
    }

    async fn export_inner(&self, mode: StateExportMode) -> Result<Vec<u8>, TransferError> {
        let state = self.store.snapshot_state(mode).await?;
        let mut entries = BTreeMap::<String, Vec<u8>>::new();
        entries.insert(
            "state.json".into(),
            serde_json::to_vec(&state).map_err(|_| TransferError::Storage)?,
        );
        for application in &state.applications {
            entries.insert(
                format!("applications/{}.toml", application.application.id),
                application
                    .application
                    .export_toml()
                    .map_err(|_| TransferError::Storage)?
                    .into_bytes(),
            );
        }
        validate_state(
            &state,
            mode,
            &entries,
            self.store.instance_id(),
            self.secret_service
                .read()
                .expect("secret service lock is not poisoned")
                .as_deref(),
        )?;
        let manifest = ArchiveManifest {
            format: FORMAT.into(),
            version: VERSION,
            mode,
            source_instance_id: state.source_instance_id.clone(),
            database_schema_version: crate::store::SCHEMA_VERSION,
            entry_count: entries.len(),
            entries: entries
                .iter()
                .map(|(name, bytes)| {
                    (
                        name.clone(),
                        EntryDigest {
                            bytes: bytes.len() as u64,
                            sha256: digest(bytes),
                        },
                    )
                })
                .collect(),
            exclusions: exclusions(),
        };
        entries.insert(
            "manifest.json".into(),
            serde_json::to_vec(&manifest).map_err(|_| TransferError::Storage)?,
        );
        build_tar(entries)
    }

    /// Parses and validates an archive before obtaining the write gate.
    ///
    /// # Errors
    /// Returns a sanitized validation, schema, checksum, or storage error.
    pub fn validate(&self, bytes: &[u8]) -> Result<ValidatedArchive, TransferError> {
        let secret_service = self
            .secret_service
            .read()
            .map_err(|_| TransferError::Storage)?
            .clone();
        validate_archive(bytes, self.store.instance_id(), secret_service.as_deref())
    }

    /// Validates and records an auditable import attempt.
    ///
    /// # Errors
    /// Returns a sanitized validation or storage error.
    pub async fn stage_import(&self, bytes: &[u8]) -> Result<ValidatedArchive, TransferError> {
        let operation = operation_id();
        let archive_digest = digest(bytes);
        match self.validate(bytes) {
            Ok(mut archive) => {
                self.audit_start(
                    &operation,
                    "import",
                    mode_str(archive.mode),
                    Some(&archive.source_instance_id),
                )
                .await?;
                archive.audit_operation = Some(operation);
                Ok(archive)
            }
            Err(error) => {
                self.audit_start(&operation, "import", "unknown", None)
                    .await?;
                let _ = self
                    .audit_finish(
                        &operation,
                        false,
                        Some(&archive_digest),
                        Some("archive_validation_failed"),
                    )
                    .await;
                Err(error)
            }
        }
    }

    /// Replaces all control-plane configuration in one maintenance transaction.
    ///
    /// # Errors
    /// Returns a sanitized storage or maintenance error.
    pub async fn replace(
        &self,
        archive: ValidatedArchive,
    ) -> Result<StateImportResult, TransferError> {
        let operation = archive.audit_operation.clone().unwrap_or_else(operation_id);
        if archive.audit_operation.is_none() {
            self.audit_start(
                &operation,
                "import",
                mode_str(archive.mode),
                Some(&archive.source_instance_id),
            )
            .await?;
        }
        let maintenance = self.store.maintenance_gate();
        let _maintenance = maintenance.write().await;
        #[cfg(test)]
        let fail_after_delete = self.fail_after_delete();
        #[cfg(not(test))]
        let fail_after_delete = false;
        let result = self
            .store
            .replace_state(
                &archive.state,
                fail_after_delete,
                &operation,
                &archive.archive_digest,
            )
            .await;
        if let Err(error) = result {
            let _ = self
                .audit_finish(&operation, false, None, Some("import_failed"))
                .await;
            return Err(error.into());
        }
        Ok(StateImportResult {
            operation_id: operation,
            archive_digest: archive.archive_digest,
            applications_imported: archive.state.applications.len(),
            secrets_imported: archive.state.secrets.len(),
            dependencies: archive.dependencies,
        })
    }

    #[cfg(test)]
    fn fail_after_delete(&self) -> bool {
        self.fail_after_delete
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn audit_start(
        &self,
        id: &str,
        direction: &str,
        mode: &str,
        source: Option<&str>,
    ) -> Result<(), TransferError> {
        self.store
            .audit_transfer_start(id, direction, mode, source)
            .await?;
        Ok(())
    }

    async fn audit_finish(
        &self,
        id: &str,
        succeeded: bool,
        archive_digest: Option<&str>,
        diagnostic: Option<&str>,
    ) -> Result<(), TransferError> {
        self.store
            .audit_transfer_finish(id, succeeded, archive_digest, diagnostic)
            .await?;
        Ok(())
    }
}

/// Fully staged, validated input. Its fields cannot be bypassed by API callers.
pub struct ValidatedArchive {
    state: ArchiveState,
    mode: StateExportMode,
    source_instance_id: String,
    archive_digest: String,
    dependencies: ImportDependencyReport,
    audit_operation: Option<String>,
}

fn read_archive_files(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, TransferError> {
    let mut archive = tar::Archive::new(Cursor::new(bytes));
    let mut files = BTreeMap::<String, Vec<u8>>::new();
    let entries = archive
        .entries()
        .map_err(|_| TransferError::Malformed)?
        .raw(true);
    let mut casefolded_paths = BTreeSet::new();
    let mut end_of_entries = 0_u64;
    for item in entries {
        if files.len() >= MAX_ENTRIES {
            return Err(TransferError::Limit);
        }
        let mut entry = item.map_err(|_| TransferError::Malformed)?;
        validate_header(entry.header())?;
        if !entry.header().entry_type().is_file() {
            return Err(TransferError::Malformed);
        }
        let path = entry
            .path()
            .map_err(|_| TransferError::Malformed)?
            .to_str()
            .ok_or(TransferError::Malformed)?
            .to_owned();
        validate_path(&path)?;
        if !casefolded_paths.insert(path.to_ascii_lowercase()) {
            return Err(TransferError::Malformed);
        }
        if entry.size() > MAX_ENTRY_BYTES {
            return Err(TransferError::Limit);
        }
        let mut data =
            Vec::with_capacity(usize::try_from(entry.size()).map_err(|_| TransferError::Limit)?);
        entry
            .by_ref()
            .take(MAX_ENTRY_BYTES + 1)
            .read_to_end(&mut data)
            .map_err(|_| TransferError::Malformed)?;
        if data.len() as u64 != entry.size() || files.insert(path, data).is_some() {
            return Err(TransferError::Malformed);
        }
        end_of_entries = entry
            .raw_file_position()
            .checked_add(entry.size())
            .and_then(|value| value.checked_add(511))
            .map(|value| value / 512 * 512)
            .ok_or(TransferError::Limit)?;
    }
    let end = usize::try_from(end_of_entries).map_err(|_| TransferError::Limit)?;
    if bytes.len() < end.saturating_add(1_024)
        || bytes
            .get(end..)
            .is_none_or(|tail| tail.iter().any(|byte| *byte != 0))
    {
        return Err(TransferError::Malformed);
    }
    Ok(files)
}

fn validate_manifest(
    manifest: &ArchiveManifest,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<(), TransferError> {
    if manifest.format != FORMAT
        || manifest.version != VERSION
        || manifest.database_schema_version != crate::store::SCHEMA_VERSION
        || manifest.exclusions != exclusions()
    {
        return Err(TransferError::Schema);
    }
    if manifest.entry_count != files.len() || manifest.entries.len() != files.len() {
        return Err(TransferError::Checksum);
    }
    for (name, expected) in &manifest.entries {
        let value = files.get(name).ok_or(TransferError::Checksum)?;
        if value.len() as u64 != expected.bytes || digest(value) != expected.sha256 {
            return Err(TransferError::Checksum);
        }
    }
    if files
        .keys()
        .any(|name| !manifest.entries.contains_key(name))
    {
        return Err(TransferError::Checksum);
    }
    Ok(())
}

fn validate_state_paths(
    state: &ArchiveState,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<(), TransferError> {
    let expected_paths = std::iter::once("state.json".to_owned())
        .chain(
            state
                .applications
                .iter()
                .map(|application| format!("applications/{}.toml", application.application.id)),
        )
        .collect::<BTreeSet<_>>();
    if files.keys().cloned().collect::<BTreeSet<_>>() == expected_paths {
        Ok(())
    } else {
        Err(TransferError::Malformed)
    }
}

fn validate_archive(
    bytes: &[u8],
    target_instance: &str,
    secret_service: Option<&SecretService>,
) -> Result<ValidatedArchive, TransferError> {
    if bytes.is_empty() || bytes.len() > MAX_ARCHIVE_BYTES {
        return Err(TransferError::Limit);
    }
    let mut files = read_archive_files(bytes)?;
    let manifest_bytes = files
        .remove("manifest.json")
        .ok_or(TransferError::Malformed)?;
    let manifest: ArchiveManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|_| TransferError::Malformed)?;
    validate_manifest(&manifest, &files)?;
    let state: ArchiveState =
        serde_json::from_slice(files.get("state.json").ok_or(TransferError::Malformed)?)
            .map_err(|_| TransferError::Malformed)?;
    validate_state_paths(&state, &files)?;
    if state.source_instance_id != manifest.source_instance_id {
        return Err(TransferError::Validation);
    }
    validate_state(
        &state,
        manifest.mode,
        &files,
        target_instance,
        secret_service,
    )?;
    let target =
        InstanceId::parse(target_instance.to_owned()).map_err(|_| TransferError::Validation)?;
    let source_instance_id = state.source_instance_id.clone();
    let mut state = state;
    for application in &mut state.applications {
        application.resolved = recompile_imported_application(
            &application.application,
            &application.resolved,
            &target,
        )
        .map_err(|_| TransferError::Validation)?;
        application.status.observed_generation = None;
        application.status.message = Some(if source_instance_id == target_instance {
            "imported desired state is awaiting reconciliation".into()
        } else {
            "imported from another instance; runtime ownership was rebuilt".into()
        });
    }
    let dependencies = dependency_report(&state, target_instance, secret_service);
    Ok(ValidatedArchive {
        state,
        mode: manifest.mode,
        source_instance_id: manifest.source_instance_id,
        archive_digest: digest(bytes),
        dependencies,
        audit_operation: None,
    })
}

fn validate_state(
    state: &ArchiveState,
    mode: StateExportMode,
    files: &BTreeMap<String, Vec<u8>>,
    target_instance: &str,
    secret_service: Option<&SecretService>,
) -> Result<(), TransferError> {
    InstanceId::parse(state.source_instance_id.clone()).map_err(|_| TransferError::Validation)?;
    InstanceId::parse(target_instance.to_owned()).map_err(|_| TransferError::Validation)?;
    if state.source_created_at_ms <= 0 {
        return Err(TransferError::Validation);
    }
    let mut ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    for application in &state.applications {
        validate_application(
            application,
            files,
            &state.source_instance_id,
            &mut ids,
            &mut names,
        )?;
    }

    let mut secret_ids = BTreeSet::new();
    let mut secret_names = BTreeSet::new();
    for secret in &state.secrets {
        validate_secret(
            secret,
            mode,
            secret_service,
            &mut secret_ids,
            &mut secret_names,
        )?;
    }
    Ok(())
}

fn validate_application(
    application: &ArchiveApplication,
    files: &BTreeMap<String, Vec<u8>>,
    source_instance_id: &str,
    ids: &mut BTreeSet<piqueld_core::ApplicationId>,
    names: &mut BTreeSet<String>,
) -> Result<(), TransferError> {
    if !ids.insert(application.application.id.clone())
        || !names.insert(application.application.metadata.name.clone())
        || application.generation == 0
        || application.created_at_ms <= 0
        || application.updated_at_ms < application.created_at_ms
        || application.spec_hash != application.application.spec_hash()
        || application.status.updated_at_ms <= 0
        || !valid_status(&application.status.state)
        || application.resolved.id != application.application.id
        || application.resolved.spec_hash != application.spec_hash
    {
        return Err(TransferError::Validation);
    }
    validate_resolved(
        &application.application,
        &application.resolved,
        source_instance_id,
    )
    .map_err(|_| TransferError::Validation)?;
    let toml = application
        .application
        .export_toml()
        .map_err(|_| TransferError::Validation)?;
    let round_trip = parse_toml(&toml)
        .map_err(|_| TransferError::Validation)?
        .normalize(application.application.id.clone());
    if round_trip != application.application
        || files.get(&format!("applications/{}.toml", application.application.id))
            != Some(&toml.into_bytes())
    {
        Err(TransferError::Validation)
    } else {
        Ok(())
    }
}

fn validate_secret(
    secret: &ArchiveSecret,
    mode: StateExportMode,
    secret_service: Option<&SecretService>,
    ids: &mut BTreeSet<String>,
    names: &mut BTreeSet<String>,
) -> Result<(), TransferError> {
    if !ids.insert(secret.id.clone())
        || !valid_secret_id(&secret.id)
        || !names.insert(secret.name.clone())
        || !valid_secret_name(&secret.name)
        || secret.generation == 0
        || secret.created_at_ms <= 0
        || secret.updated_at_ms < secret.created_at_ms
    {
        return Err(TransferError::Validation);
    }
    if mode == StateExportMode::Portable {
        return if secret.value_is_set || secret.current.is_some() || !secret.generations.is_empty()
        {
            Err(TransferError::Validation)
        } else {
            Ok(())
        };
    }
    if !secret.value_is_set {
        return if secret.current.is_some() || !secret.generations.is_empty() {
            Err(TransferError::Validation)
        } else {
            Ok(())
        };
    }
    let current = secret.current.as_ref().ok_or(TransferError::Validation)?;
    let journal = secret
        .generations
        .iter()
        .find(|generation| generation.generation == current.generation)
        .ok_or(TransferError::Validation)?;
    if journal != current
        || current.retired_at_ms.is_some()
        || current.generation != secret.generation
    {
        return Err(TransferError::Validation);
    }
    let mut generations = BTreeSet::new();
    for envelope in &secret.generations {
        validate_secret_envelope(envelope, secret_service, &secret.name, &mut generations)?;
    }
    Ok(())
}

fn validate_secret_envelope(
    envelope: &ArchiveEnvelope,
    secret_service: Option<&SecretService>,
    logical_name: &str,
    generations: &mut BTreeSet<u64>,
) -> Result<(), TransferError> {
    if !generations.insert(envelope.generation)
        || envelope.generation == 0
        || envelope.created_at_ms <= 0
        || envelope
            .retired_at_ms
            .is_some_and(|value| value < envelope.created_at_ms)
    {
        return Err(TransferError::Validation);
    }
    let nonce = BASE64
        .decode(&envelope.nonce_base64)
        .map_err(|_| TransferError::Validation)?;
    let ciphertext = BASE64
        .decode(&envelope.ciphertext_base64)
        .map_err(|_| TransferError::Validation)?;
    if nonce.len() != 24
        || ciphertext.len() < 16
        || !valid_digest(&envelope.content_hash)
        || !valid_swarm_secret_name(&envelope.swarm_secret_name)
        || !valid_text(&envelope.algorithm, 64)
        || !valid_text(&envelope.key_id, 128)
    {
        return Err(TransferError::Validation);
    }
    let service = secret_service.ok_or(TransferError::Validation)?;
    service
        .validate_import_envelope(
            &ImportEnvelope {
                generation: envelope.generation,
                algorithm: &envelope.algorithm,
                key_id: &envelope.key_id,
                nonce: &nonce,
                ciphertext: &ciphertext,
                content_hash: &envelope.content_hash,
                swarm_name: &envelope.swarm_secret_name,
            },
            logical_name,
        )
        .map_err(|_| TransferError::Validation)
}

fn dependency_report(
    state: &ArchiveState,
    target_instance: &str,
    secret_service: Option<&SecretService>,
) -> ImportDependencyReport {
    let ownership_compatible = state.source_instance_id == target_instance;
    let missing_secret_values = state
        .secrets
        .iter()
        .filter(|secret| !secret.value_is_set)
        .map(|secret| secret.name.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let incompatible_secret_keys = state
        .secrets
        .iter()
        .flat_map(|secret| {
            secret
                .generations
                .iter()
                .map(|generation| generation.key_id.clone())
        })
        .filter(|key| secret_service.is_none_or(|service| service.master_key_id() != key))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut images = BTreeSet::new();
    let mut git = BTreeSet::new();
    let mut runtime_secrets = BTreeSet::new();
    let mut volumes = BTreeSet::new();
    for application in &state.applications {
        for service in &application.resolved.services {
            match &service.source {
                piqueld_core::resource::ResolvedSource::Image {
                    digest_reference, ..
                } => {
                    images.insert(digest_reference.clone());
                }
                piqueld_core::resource::ResolvedSource::Git {
                    repository,
                    requested_reference,
                    digest_reference,
                    ..
                } => {
                    images.insert(digest_reference.clone());
                    git.insert(format!("{repository}#{requested_reference}"));
                }
            }
        }
        runtime_secrets.extend(
            application
                .resolved
                .secrets
                .iter()
                .map(|secret| secret.name.clone()),
        );
        volumes.extend(
            application
                .application
                .spec
                .volumes
                .iter()
                .map(|volume| volume.name.clone()),
        );
    }
    ImportDependencyReport {
        source_instance_id: state.source_instance_id.clone(),
        target_instance_id: target_instance.into(),
        ownership_compatible,
        missing_secret_values,
        incompatible_secret_keys,
        image_references_to_verify: images.into_iter().collect(),
        git_sources_to_resolve: git.into_iter().collect(),
        runtime_secrets_to_recreate: runtime_secrets.into_iter().collect(),
        retained_volumes_to_verify: volumes.into_iter().collect(),
        notes: vec![
            if ownership_compatible {
                "same-instance restore preserves ownership compatibility; runtime objects are only reconciled after import".into()
            } else {
                "new-instance restore rebuilds ownership labels and never adopts old runtime objects".into()
            },
            "archive is configuration state, not a full disaster-recovery backup".into(),
        ],
    }
}

fn build_tar(entries: BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>, TransferError> {
    if entries.len() > MAX_ENTRIES {
        return Err(TransferError::Limit);
    }
    let estimated = entries
        .iter()
        .try_fold(1_024_usize, |total, (name, data)| {
            validate_path(name)?;
            if data.len() as u64 > MAX_ENTRY_BYTES {
                return Err(TransferError::Limit);
            }
            let padded = data
                .len()
                .checked_add(511)
                .map(|bytes| bytes / 512 * 512)
                .ok_or(TransferError::Limit)?;
            total
                .checked_add(512)
                .and_then(|bytes| bytes.checked_add(padded))
                .ok_or(TransferError::Limit)
        })?;
    if estimated > MAX_ARCHIVE_BYTES {
        return Err(TransferError::Limit);
    }
    let mut output = Vec::with_capacity(estimated);
    {
        let mut builder = tar::Builder::new(&mut output);
        for (name, data) in entries {
            let mut header = tar::Header::new_ustar();
            header.set_size(data.len() as u64);
            header.set_mode(0o600);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            header.set_entry_type(EntryType::Regular);
            header.set_cksum();
            builder
                .append_data(&mut header, name, Cursor::new(data))
                .map_err(|_| TransferError::Storage)?;
        }
        builder.finish().map_err(|_| TransferError::Storage)?;
    }
    if output.len() > MAX_ARCHIVE_BYTES {
        return Err(TransferError::Limit);
    }
    Ok(output)
}

fn validate_path(path: &str) -> Result<(), TransferError> {
    if path.is_empty()
        || path.len() > 240
        || path.starts_with('/')
        || path.contains('\\')
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        Err(TransferError::Malformed)
    } else {
        Ok(())
    }
}

fn validate_header(header: &tar::Header) -> Result<(), TransferError> {
    let raw = header.as_bytes();
    let nul_padded = |field: &[u8]| {
        field
            .iter()
            .position(|byte| *byte == 0)
            .is_none_or(|end| field[end..].iter().all(|byte| *byte == 0))
    };
    if raw.get(257..263) != Some(b"ustar\0")
        || raw.get(263..265) != Some(b"00")
        || !nul_padded(&raw[0..100])
        || !nul_padded(&raw[345..500])
        || raw[157..257].iter().any(|byte| *byte != 0)
    {
        return Err(TransferError::Malformed);
    }
    Ok(())
}

fn exclusions() -> Vec<String> {
    vec![
        "secret plaintext and master key".into(),
        "volumes and registry blobs".into(),
        "logs and Git worktrees".into(),
        "Cloudflare and external runtime state".into(),
    ]
}

fn valid_status(value: &str) -> bool {
    matches!(
        value,
        "pending" | "deploying" | "ready" | "degraded" | "deleting" | "failed"
    )
}

fn valid_text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

fn valid_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_secret_id(value: &str) -> bool {
    value.len() == 39
        && value.starts_with("secret-")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_secret_name(value: &str) -> bool {
    (1..=63).contains(&value.len())
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !value.ends_with('-')
}

fn valid_swarm_secret_name(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

fn digest(value: impl AsRef<[u8]>) -> String {
    format!("sha256:{:x}", Sha256::digest(value.as_ref()))
}

fn operation_id() -> String {
    format!("transfer-{}", Uuid::now_v7().simple())
}

fn mode_str(mode: StateExportMode) -> &'static str {
    match mode {
        StateExportMode::Portable => "portable",
        StateExportMode::Encrypted => "encrypted",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        secrets::{MasterKey, PlaintextSecret},
        store::ApplicationState,
    };
    use piqueld_core::{ApplicationId, parse_toml};

    #[test]
    fn archive_paths_and_tar_output_are_deterministic() {
        assert!(validate_path("../state.json").is_err());
        assert!(validate_path("/state.json").is_err());
        let first = build_tar(BTreeMap::from([("state.json".into(), b"{}".to_vec())])).unwrap();
        let second = build_tar(BTreeMap::from([("state.json".into(), b"{}".to_vec())])).unwrap();
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn portable_export_and_import_preserve_manifest_state() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteStore::open(directory.path().join("state.db"))
                .await
                .unwrap(),
        );
        let application = parse_toml(
            r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "roundtrip"
[[spec.services]]
name = "web"
[spec.services.source]
type = "image"
image = "registry.example/web:1"
"#,
        )
        .unwrap()
        .normalize(ApplicationId::parse("app-roundtrip").unwrap());
        let instance = InstanceId::parse(store.instance_id().to_owned()).unwrap();
        let resolved = recompile_imported_application(
            &application,
            &compile_for_test(&application, &instance),
            &instance,
        )
        .unwrap();
        store.create(&application, &resolved, &[]).await.unwrap();
        let transfer = StateTransferService::new(Arc::clone(&store));
        let archive = transfer.export(StateExportMode::Portable).await.unwrap();
        let staged = transfer.validate(&archive).unwrap();
        let result = transfer.replace(staged).await.unwrap();
        assert_eq!(result.applications_imported, 1);
        assert_eq!(store.list(None, 50).await.unwrap().items.len(), 1);
        assert_eq!(
            store.status(&application.id).await.unwrap().state,
            ApplicationState::Pending
        );
    }

    #[tokio::test]
    async fn encrypted_archive_requires_the_matching_key() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteStore::open(directory.path().join("state.db"))
                .await
                .unwrap(),
        );
        let secret = Arc::new(crate::secrets::SecretService::new(
            Arc::clone(&store),
            MasterKey::testing([7; 32]),
        ));
        secret
            .create("token", PlaintextSecret::new(b"canary".to_vec()).unwrap())
            .await
            .unwrap();
        let transfer = StateTransferService::new(Arc::clone(&store));
        transfer.set_secret_service(Arc::clone(&secret));
        let archive = transfer.export(StateExportMode::Encrypted).await.unwrap();
        let wrong = StateTransferService::new(Arc::clone(&store));
        wrong.set_secret_service(Arc::new(crate::secrets::SecretService::new(
            Arc::clone(&store),
            MasterKey::testing([8; 32]),
        )));
        assert!(matches!(
            wrong.validate(&archive),
            Err(TransferError::Validation)
        ));
    }

    #[tokio::test]
    async fn failed_replacement_transaction_keeps_previous_rows() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteStore::open(directory.path().join("state.db"))
                .await
                .unwrap(),
        );
        let application = parse_toml(
            r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "roundtrip"
[[spec.services]]
name = "web"
[spec.services.source]
type = "image"
image = "registry.example/web:1"
"#,
        )
        .unwrap()
        .normalize(ApplicationId::parse("app-roundtrip").unwrap());
        let instance = InstanceId::parse(store.instance_id().to_owned()).unwrap();
        let resolved = compile_for_test(&application, &instance);
        store.create(&application, &resolved, &[]).await.unwrap();
        let transfer = StateTransferService::new(Arc::clone(&store));
        let archive = transfer.export(StateExportMode::Portable).await.unwrap();
        let staged = transfer.validate(&archive).unwrap();
        transfer
            .fail_after_delete
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(transfer.replace(staged).await.is_err());
        assert_eq!(store.list(None, 50).await.unwrap().items.len(), 1);
    }

    fn compile_for_test(
        application: &NormalizedApplication,
        instance: &InstanceId,
    ) -> piqueld_core::resource::ResolvedApplication {
        let sources = application
            .spec
            .services
            .iter()
            .map(|service| {
                (
                    service.name.clone(),
                    piqueld_core::resource::ResolvedSource::Image {
                        requested: "registry.example/web:1".into(),
                        digest_reference: "registry.example/web@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                    },
                )
            })
            .collect();
        piqueld_core::compile_application(
            application,
            instance.clone(),
            &piqueld_core::resource::ResolutionSet {
                sources,
                secrets: BTreeMap::new(),
            },
        )
        .unwrap()
    }
}
