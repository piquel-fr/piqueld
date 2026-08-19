//! Authenticated logical-secret storage and metadata-only lifecycle service.

use crate::{
    config::CredentialReference,
    store::{
        MAX_PAGE_SIZE, SecretDeleteResult, SecretMetadataRow, SecretWrite, SqliteStore, StoreError,
        StoredApplication,
    },
};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
};
use piqueld_client::Page;
use piqueld_core::{
    ApplicationId, NormalizedApplication, ObservedApplication, PlanAction, PlanRequest, plan,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Read,
    os::unix::fs::MetadataExt,
    path::PathBuf,
    sync::Arc,
};
use subtle::ConstantTimeEq;
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

/// Envelope algorithm identifier persisted with every generation.
pub const ALGORITHM: &str = "xchacha20-poly1305-v1";
/// Maximum logical-secret value size.
pub const MAX_SECRET_BYTES: usize = 500 * 1024;

/// Secret bytes that cannot be formatted, cloned, or serialized.
pub struct PlaintextSecret(Zeroizing<Vec<u8>>);

impl PlaintextSecret {
    /// Validates and takes ownership of one secret value.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError::InvalidValue`] for an empty or oversized value.
    pub fn new(value: Vec<u8>) -> Result<Self, SecretError> {
        if value.is_empty() || value.len() > MAX_SECRET_BYTES {
            return Err(SecretError::InvalidValue);
        }
        Ok(Self(Zeroizing::new(value)))
    }

    pub(crate) fn expose(&self) -> &[u8] {
        &self.0
    }

    /// Returns the value length without exposing its contents.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns whether the secret contains no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Loaded master encryption key.
pub struct MasterKey {
    bytes: Zeroizing<[u8; 32]>,
    key_id: String,
}

impl MasterKey {
    /// Loads a key from a protected file or systemd credential.
    ///
    /// # Errors
    ///
    /// Returns a classified error when the key is missing, malformed, or unsafe
    /// to read.
    pub fn load(reference: &CredentialReference) -> Result<Self, SecretError> {
        let path = match reference {
            CredentialReference::File { path } => path.clone(),
            CredentialReference::SystemdCredential { name } => {
                let directory = std::env::var_os("CREDENTIALS_DIRECTORY")
                    .map(PathBuf::from)
                    .ok_or(SecretError::KeyUnavailable)?;
                if !directory.is_absolute()
                    || name.is_empty()
                    || matches!(name.as_str(), "." | "..")
                    || name.contains(['/', '\0'])
                {
                    return Err(SecretError::KeyUnavailable);
                }
                directory.join(name)
            }
        };
        if path.starts_with("/nix/store") {
            return Err(SecretError::KeyPermissions);
        }
        let descriptor = rustix::fs::openat2(
            rustix::fs::CWD,
            &path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
            rustix::fs::ResolveFlags::NO_SYMLINKS | rustix::fs::ResolveFlags::NO_MAGICLINKS,
        )
        .map_err(|_| SecretError::KeyPermissions)?;
        let mut file = File::from(descriptor);
        validate_key_file(&file)?;
        let canonical = fs::canonicalize(&path).map_err(|_| SecretError::KeyUnavailable)?;
        if canonical.starts_with("/nix/store") {
            return Err(SecretError::KeyPermissions);
        }
        let mut raw = Zeroizing::new(Vec::with_capacity(33));
        file.by_ref()
            .take(33)
            .read_to_end(&mut raw)
            .map_err(|_| SecretError::KeyUnavailable)?;
        if raw.len() != 32 {
            return Err(SecretError::KeyInvalid);
        }
        let mut bytes = Zeroizing::new([0_u8; 32]);
        bytes.copy_from_slice(&raw);
        let key_id = format!("sha256:{}", hex(&Sha256::digest(bytes.as_slice())));
        Ok(Self { bytes, key_id })
    }

    #[cfg(test)]
    pub(crate) fn testing(bytes: [u8; 32]) -> Self {
        let key_id = format!("sha256:{}", hex(&Sha256::digest(bytes)));
        Self {
            bytes: Zeroizing::new(bytes),
            key_id,
        }
    }
}

/// One application/service reference shown with secret metadata.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SecretReferenceView {
    /// Stable application ID.
    pub application_id: String,
    /// User-facing application name.
    pub application_name: String,
    /// Logical service name.
    pub service: String,
    /// Whether the reference is present in the deployed snapshot.
    pub deployed: bool,
}

/// Metadata returned by secret lifecycle APIs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SecretMetadata {
    /// Logical secret name.
    pub name: String,
    /// Whether an encrypted value exists.
    pub value_is_set: bool,
    /// Monotonic content generation.
    pub generation: u64,
    /// Creation timestamp.
    pub created_at_ms: i64,
    /// Last update timestamp.
    pub updated_at_ms: i64,
    /// Current desired/deployed application references.
    pub references: Vec<SecretReferenceView>,
}

pub(crate) struct EncryptedGeneration {
    algorithm: String,
    key_id: String,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
    content_hash: String,
    generation: u64,
    swarm_name: String,
}

/// Authenticated envelope supplied by the state-transfer validator.
pub(crate) struct ImportEnvelope<'a> {
    pub(crate) generation: u64,
    pub(crate) algorithm: &'a str,
    pub(crate) key_id: &'a str,
    pub(crate) nonce: &'a [u8],
    pub(crate) ciphertext: &'a [u8],
    pub(crate) content_hash: &'a str,
    pub(crate) swarm_name: &'a str,
}

impl Drop for EncryptedGeneration {
    fn drop(&mut self) {
        self.ciphertext.zeroize();
    }
}

/// Secret lifecycle failures mapped to safe API/runtime messages.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SecretError {
    /// The configured key could not be read.
    #[error("master encryption key is unavailable")]
    KeyUnavailable,
    /// The configured key file is not safely owned or permissioned.
    #[error("master encryption key has unsafe permissions")]
    KeyPermissions,
    /// The key did not contain exactly 32 bytes.
    #[error("master encryption key is invalid")]
    KeyInvalid,
    /// The secret value is empty or exceeds the bounded size.
    #[error("secret value is invalid")]
    InvalidValue,
    /// The logical name is outside the manifest identifier alphabet.
    #[error("logical secret name is invalid")]
    InvalidName,
    /// Pagination parameters are invalid.
    #[error("secret pagination parameters are invalid")]
    InvalidPagination,
    /// The logical secret does not exist.
    #[error("logical secret was not found")]
    NotFound,
    /// A create attempted to overwrite an existing value.
    #[error("logical secret already exists")]
    AlreadyExists,
    /// The secret is still referenced by desired or deployed state.
    #[error("logical secret is referenced")]
    Referenced,
    /// An encrypted envelope failed authentication or integrity checks.
    #[error("secret value cannot be decrypted")]
    DecryptionFailed,
    /// Secret persistence failed.
    #[error("secret storage is unavailable")]
    Storage,
}

/// Authenticated secret store and lifecycle coordinator.
pub struct SecretService {
    store: Arc<SqliteStore>,
    key: Arc<MasterKey>,
}

impl SecretService {
    /// Creates a service using the supplied durable store and master key.
    #[must_use]
    pub fn new(store: Arc<SqliteStore>, key: MasterKey) -> Self {
        Self {
            store,
            key: Arc::new(key),
        }
    }

    /// Returns the non-secret identifier of the loaded master key.
    pub(crate) fn master_key_id(&self) -> &str {
        &self.key.key_id
    }

    /// Authenticates an imported encrypted envelope without retaining plaintext.
    pub(crate) fn validate_import_envelope(
        &self,
        imported: &ImportEnvelope<'_>,
        logical_name: &str,
    ) -> Result<(), SecretError> {
        let envelope = EncryptedGeneration {
            algorithm: imported.algorithm.to_owned(),
            key_id: imported.key_id.to_owned(),
            nonce: imported.nonce.to_vec(),
            ciphertext: imported.ciphertext.to_vec(),
            content_hash: imported.content_hash.to_owned(),
            generation: imported.generation,
            swarm_name: imported.swarm_name.to_owned(),
        };
        let _plaintext = decrypt(&self.key, logical_name, &envelope)?;
        Ok(())
    }

    /// Lists metadata without returning secret values.
    pub(crate) async fn list_page(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<SecretMetadata>, SecretError> {
        let page = self
            .store
            .secret_metadata_page(cursor, limit)
            .await
            .map_err(pagination)?;
        let names = page
            .items
            .iter()
            .map(|row| row.name.clone())
            .collect::<Vec<_>>();
        let mut references = self.references_for_names(&names).await?;
        let items = page
            .items
            .into_iter()
            .map(|row| {
                let name = row.name.clone();
                metadata(row, references.remove(&name).unwrap_or_default())
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Page {
            items,
            next_cursor: page.next_cursor,
        })
    }

    /// Reads metadata for one logical secret.
    ///
    /// # Errors
    ///
    /// Returns a classified secret or storage error.
    pub async fn get(&self, name: &str) -> Result<SecretMetadata, SecretError> {
        validate_name(name)?;
        let row = self
            .store
            .secret_metadata(name)
            .await
            .map_err(storage)?
            .ok_or(SecretError::NotFound)?;
        let mut references = self.references_for_names(&[name.to_owned()]).await?;
        metadata(row, references.remove(name).unwrap_or_default())
    }

    /// Creates a logical secret.
    ///
    /// # Errors
    ///
    /// Returns a classified validation, persistence, or encryption error.
    pub async fn create(
        &self,
        name: &str,
        plaintext: PlaintextSecret,
    ) -> Result<SecretMetadata, SecretError> {
        validate_name(name)?;
        self.write(name, plaintext, true).await
    }

    /// Rotates a logical secret to a new authenticated generation.
    ///
    /// # Errors
    ///
    /// Returns a classified validation, persistence, or encryption error.
    pub async fn replace(
        &self,
        name: &str,
        plaintext: PlaintextSecret,
    ) -> Result<SecretMetadata, SecretError> {
        validate_name(name)?;
        self.write(name, plaintext, false).await
    }

    async fn write(
        &self,
        name: &str,
        plaintext: PlaintextSecret,
        create: bool,
    ) -> Result<SecretMetadata, SecretError> {
        self.store
            .write_secret(name, create, |generation| {
                let swarm_name = random_swarm_name(name);
                let encrypted = encrypt(&self.key, name, generation, &swarm_name, &plaintext)?;
                Ok::<SecretWrite, SecretError>(SecretWrite {
                    algorithm: encrypted.algorithm.clone(),
                    key_id: encrypted.key_id.clone(),
                    nonce: encrypted.nonce.clone(),
                    ciphertext: encrypted.ciphertext.clone(),
                    content_hash: encrypted.content_hash.clone(),
                    swarm_name: encrypted.swarm_name.clone(),
                })
            })
            .await?;
        self.get(name).await
    }

    /// Updates persisted desired resolutions for every application referencing a rotated secret.
    pub(crate) async fn schedule_rotation(&self, name: &str) -> Result<(), SecretError> {
        validate_name(name)?;
        let mut cursor = None;
        loop {
            let page = self
                .store
                .list(cursor.as_deref(), MAX_PAGE_SIZE)
                .await
                .map_err(SecretError::from)?;
            for current in page.items {
                if !current.delete_intent
                    && referenced_secret_names(&current.application)
                        .iter()
                        .any(|reference| reference == name)
                {
                    self.synchronize_application(&current).await?;
                }
            }
            let Some(next_cursor) = page.next_cursor else {
                break;
            };
            cursor = Some(next_cursor);
        }
        Ok(())
    }

    /// Rewrites one application's runtime resolution to current secret generations.
    pub(crate) async fn synchronize_application(
        &self,
        current: &StoredApplication,
    ) -> Result<bool, SecretError> {
        if current.delete_intent {
            return Ok(false);
        }
        let names = referenced_secret_names(&current.application);
        let generations = self
            .current_generations(names, &current.application.id)
            .await?;
        let mut resolved = current.resolved.clone();
        let mut changed = false;
        for generation in generations {
            for secret in &mut resolved.secrets {
                if secret.logical_name == generation.logical_name {
                    changed |= secret.generation != generation.generation
                        || secret.name != generation.swarm_name;
                    secret.generation.clone_from(&generation.generation);
                    secret.name.clone_from(&generation.swarm_name);
                }
            }
            for service in &mut resolved.services {
                for secret in &mut service.secrets {
                    if secret.logical_name == generation.logical_name {
                        changed |= secret.swarm_name != generation.swarm_name;
                        secret.swarm_name.clone_from(&generation.swarm_name);
                    }
                }
            }
        }
        if !changed {
            return Ok(false);
        }
        let steps = plan(
            &PlanRequest::Reconcile {
                desired: resolved.clone(),
            },
            &ObservedApplication::default(),
        )
        .actions
        .iter()
        .map(PlanAction::operation_step)
        .collect::<Vec<_>>();
        self.store
            .replace(&current.application, &resolved, current.generation, &steps)
            .await
            .map_err(SecretError::from)?;
        Ok(true)
    }

    /// Removes retired encrypted generations no longer present in desired/deployed snapshots.
    pub(crate) async fn prune_retired_generations(&self) -> Result<(), StoreError> {
        self.store.prune_retired_secret_generations().await
    }

    /// Decrypts the current generation for runtime delivery.
    ///
    /// # Errors
    ///
    /// Returns a classified storage or authentication error.
    pub async fn decrypt_current(
        &self,
        name: &str,
    ) -> Result<(String, u64, PlaintextSecret), SecretError> {
        validate_name(name)?;
        let row = self
            .store
            .secret_envelope(name)
            .await
            .map_err(storage)?
            .ok_or(SecretError::NotFound)?;
        let generation = u64::try_from(row.generation).map_err(|_| SecretError::Storage)?;
        let envelope = EncryptedGeneration {
            generation,
            algorithm: row.encryption_algorithm,
            key_id: row.encryption_key_id,
            nonce: row.nonce,
            ciphertext: row.ciphertext,
            content_hash: row.content_hash,
            swarm_name: row.swarm_secret_name,
        };
        let plaintext = decrypt(&self.key, name, &envelope)?;
        Ok((envelope.swarm_name.clone(), generation, plaintext))
    }

    /// Decrypts precisely the generation and application-scoped name persisted in desired state.
    pub(crate) async fn decrypt_generation(
        &self,
        name: &str,
        digest: &piqueld_core::resource::Sha256Digest,
        expected_swarm_name: &str,
        application: &ApplicationId,
    ) -> Result<(String, PlaintextSecret), SecretError> {
        validate_name(name)?;
        let row = self
            .store
            .secret_envelope_digest(name, digest.as_str())
            .await
            .map_err(storage)?
            .into_iter()
            .find(|row| {
                application_swarm_name(name, &row.swarm_secret_name, application)
                    == expected_swarm_name
            })
            .ok_or(SecretError::NotFound)?;
        let generation = u64::try_from(row.generation).map_err(|_| SecretError::Storage)?;
        let envelope = EncryptedGeneration {
            generation,
            algorithm: row.encryption_algorithm,
            key_id: row.encryption_key_id,
            nonce: row.nonce,
            ciphertext: row.ciphertext,
            content_hash: row.content_hash,
            swarm_name: row.swarm_secret_name,
        };
        Ok((
            expected_swarm_name.to_owned(),
            decrypt(&self.key, name, &envelope)?,
        ))
    }

    /// Returns current generation metadata for one application.
    ///
    /// # Errors
    ///
    /// Returns a classified storage or missing-secret error.
    pub async fn current_generations(
        &self,
        names: impl IntoIterator<Item = String>,
        application: &ApplicationId,
    ) -> Result<Vec<piqueld_core::resource::SecretGeneration>, SecretError> {
        let mut values = Vec::new();
        for name in BTreeSet::from_iter(names) {
            let row = self
                .store
                .secret_generation(&name)
                .await
                .map_err(storage)?
                .ok_or(SecretError::NotFound)?;
            values.push(piqueld_core::resource::SecretGeneration {
                logical_name: name.clone(),
                generation: piqueld_core::resource::Sha256Digest::parse(row.content_hash)
                    .map_err(|_| SecretError::Storage)?,
                swarm_name: application_swarm_name(&name, &row.swarm_secret_name, application),
            });
        }
        Ok(values)
    }

    /// Deletes a logical secret only when neither desired nor deployed state references it.
    ///
    /// # Errors
    ///
    /// Returns a classified validation, reference, persistence, or missing-secret error.
    pub async fn delete(&self, name: &str) -> Result<(), SecretError> {
        validate_name(name)?;
        match self
            .store
            .delete_secret_safely(name)
            .await
            .map_err(storage)?
        {
            SecretDeleteResult::Deleted => Ok(()),
            SecretDeleteResult::Referenced => Err(SecretError::Referenced),
            SecretDeleteResult::NotFound => Err(SecretError::NotFound),
        }
    }

    async fn references_for_names(
        &self,
        names: &[String],
    ) -> Result<BTreeMap<String, Vec<SecretReferenceView>>, SecretError> {
        let requested = names.iter().map(String::as_str).collect::<BTreeSet<_>>();
        let mut references = names
            .iter()
            .cloned()
            .map(|name| (name, Vec::new()))
            .collect::<BTreeMap<_, _>>();
        if requested.is_empty() {
            return Ok(references);
        }
        let mut cursor = None;
        loop {
            let page = self
                .store
                .list(cursor.as_deref(), MAX_PAGE_SIZE)
                .await
                .map_err(storage)?;
            for app in &page.items {
                append_references(&mut references, &requested, app);
            }
            let Some(next_cursor) = page.next_cursor else {
                break;
            };
            cursor = Some(next_cursor);
        }
        for values in references.values_mut() {
            values.sort_by(|left, right| {
                left.application_id
                    .cmp(&right.application_id)
                    .then(left.service.cmp(&right.service))
            });
        }
        Ok(references)
    }
}

fn append_references(
    references: &mut BTreeMap<String, Vec<SecretReferenceView>>,
    requested: &BTreeSet<&str>,
    app: &StoredApplication,
) {
    let application_id = app.application.id.to_string();
    let application_name = app.application.metadata.name.clone();
    let deployed_services = app.deployed.as_ref().map(|resolved| &resolved.services);
    for service in &app.application.spec.services {
        for name in service
            .secrets
            .iter()
            .map(|secret| secret.source.as_str())
            .filter(|name| requested.contains(name))
            .collect::<BTreeSet<_>>()
        {
            let deployed = deployed_services.is_some_and(|services| {
                services.iter().any(|deployed| {
                    deployed.logical_name == service.name
                        && deployed
                            .secrets
                            .iter()
                            .any(|secret| secret.logical_name == name)
                })
            });
            references
                .get_mut(name)
                .expect("requested secret reference has a metadata entry")
                .push(SecretReferenceView {
                    application_id: application_id.clone(),
                    application_name: application_name.clone(),
                    service: service.name.clone(),
                    deployed,
                });
        }
    }
}

fn validate_key_file(file: &File) -> Result<(), SecretError> {
    let metadata = file.metadata().map_err(|_| SecretError::KeyUnavailable)?;
    let effective_uid = rustix::process::geteuid().as_raw();
    if !metadata.is_file()
        || metadata.mode() & 0o077 != 0
        || (metadata.uid() != 0 && metadata.uid() != effective_uid)
    {
        return Err(SecretError::KeyPermissions);
    }
    Ok(())
}

fn encrypt(
    key: &MasterKey,
    name: &str,
    generation: u64,
    swarm_name: &str,
    plaintext: &PlaintextSecret,
) -> Result<EncryptedGeneration, SecretError> {
    let cipher = XChaCha20Poly1305::new_from_slice(key.bytes.as_slice())
        .map_err(|_| SecretError::KeyInvalid)?;
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let aad = associated_data(name, generation, swarm_name, &key.key_id);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext.expose(),
                aad: &aad,
            },
        )
        .map_err(|_| SecretError::Storage)?;
    Ok(EncryptedGeneration {
        algorithm: ALGORITHM.into(),
        key_id: key.key_id.clone(),
        nonce: nonce.to_vec(),
        ciphertext,
        content_hash: format!("sha256:{}", hex(&Sha256::digest(plaintext.expose()))),
        generation,
        swarm_name: swarm_name.into(),
    })
}

fn decrypt(
    key: &MasterKey,
    name: &str,
    envelope: &EncryptedGeneration,
) -> Result<PlaintextSecret, SecretError> {
    if envelope.algorithm != ALGORITHM
        || envelope
            .key_id
            .as_bytes()
            .ct_eq(key.key_id.as_bytes())
            .unwrap_u8()
            != 1
        || envelope.nonce.len() != 24
    {
        return Err(SecretError::DecryptionFailed);
    }
    let cipher = XChaCha20Poly1305::new_from_slice(key.bytes.as_slice())
        .map_err(|_| SecretError::KeyInvalid)?;
    let nonce = XNonce::from_slice(&envelope.nonce);
    let aad = associated_data(
        name,
        envelope.generation,
        &envelope.swarm_name,
        &envelope.key_id,
    );
    let mut plaintext = cipher
        .decrypt(
            nonce,
            Payload {
                msg: &envelope.ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| SecretError::DecryptionFailed)?;
    let actual = format!("sha256:{}", hex(&Sha256::digest(&plaintext)));
    if actual
        .as_bytes()
        .ct_eq(envelope.content_hash.as_bytes())
        .unwrap_u8()
        != 1
    {
        plaintext.zeroize();
        return Err(SecretError::DecryptionFailed);
    }
    PlaintextSecret::new(plaintext)
}

fn associated_data(name: &str, generation: u64, swarm_name: &str, key_id: &str) -> Vec<u8> {
    format!("piqueld-secret-envelope/v1\0{name}\0{generation}\0{swarm_name}\0{key_id}\0{ALGORITHM}")
        .into_bytes()
}

fn random_swarm_name(name: &str) -> String {
    let random = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    format!("piqueld-secret-{name}-{}", hex(&random[..16]))
}

/// Derives an application-scoped immutable Swarm name from a secret generation.
pub(crate) fn application_swarm_name(
    name: &str,
    base: &str,
    application: &ApplicationId,
) -> String {
    let random = base
        .rsplit_once('-')
        .map_or("generation", |(_, suffix)| suffix)
        .chars()
        .take(22)
        .collect::<String>();
    let readable = name.chars().take(15).collect::<String>();
    let application_hash = hex(&Sha256::digest(application.as_str().as_bytes())[..5]);
    format!("piqueld-secret-{readable}-{random}-{application_hash}")
}

fn validate_name(name: &str) -> Result<(), SecretError> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(SecretError::InvalidName)
    }
}

fn valid_name(value: &str) -> bool {
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

fn metadata(
    row: SecretMetadataRow,
    references: Vec<SecretReferenceView>,
) -> Result<SecretMetadata, SecretError> {
    Ok(SecretMetadata {
        name: row.name,
        value_is_set: row.value_is_set == 1,
        generation: u64::try_from(row.generation).map_err(|_| SecretError::Storage)?,
        created_at_ms: row.created_at_ms,
        updated_at_ms: row.updated_at_ms,
        references,
    })
}

fn referenced_secret_names(application: &NormalizedApplication) -> Vec<String> {
    application
        .spec
        .services
        .iter()
        .flat_map(|service| service.secrets.iter().map(|secret| secret.source.clone()))
        .collect()
}

fn pagination(error: StoreError) -> SecretError {
    match error {
        StoreError::InvalidInput | StoreError::InvalidInputSource(_) => {
            SecretError::InvalidPagination
        }
        error => storage(error),
    }
}

fn storage(_: StoreError) -> SecretError {
    SecretError::Storage
}

impl From<StoreError> for SecretError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::AlreadyExists => Self::AlreadyExists,
            StoreError::NotFound => Self::NotFound,
            _ => Self::Storage,
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{
        ALGORITHM, MasterKey, PlaintextSecret, SecretError, SecretService, application_swarm_name,
        decrypt, encrypt,
    };
    use crate::store::SqliteStore;
    use piqueld_core::{ApplicationId, Sha256Digest};
    use std::sync::Arc;

    #[test]
    fn authenticated_envelopes_reject_tampering() {
        let key = MasterKey::testing([7; 32]);
        let plaintext = PlaintextSecret::new(b"first-value".to_vec()).expect("bounded value");
        let mut envelope = encrypt(&key, "database-password", 1, "swarm-secret", &plaintext)
            .expect("encryption succeeds");
        assert_eq!(envelope.algorithm, ALGORITHM);
        assert_ne!(envelope.ciphertext, plaintext.expose());

        envelope.ciphertext[0] ^= 1;
        assert!(matches!(
            decrypt(&key, "database-password", &envelope),
            Err(SecretError::DecryptionFailed)
        ));
    }

    #[tokio::test]
    async fn rotation_keeps_the_previous_generation_recoverable_until_pruned() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = Arc::new(
            SqliteStore::open(directory.path().join("control-plane.db"))
                .await
                .expect("fresh database opens"),
        );
        let service = SecretService::new(Arc::clone(&store), MasterKey::testing([9; 32]));
        let application = ApplicationId::parse("app-secret-01").expect("valid application ID");

        let first = service
            .create(
                "database-password",
                PlaintextSecret::new(b"first-value".to_vec()).expect("bounded value"),
            )
            .await
            .expect("secret is created");
        assert_eq!(first.generation, 1);
        let first_row = store
            .secret_envelope("database-password")
            .await
            .expect("secret row is readable")
            .expect("current generation exists");

        let second = service
            .replace(
                "database-password",
                PlaintextSecret::new(b"second-value".to_vec()).expect("bounded value"),
            )
            .await
            .expect("secret rotates");
        assert_eq!(second.generation, 2);

        let old_digest = Sha256Digest::parse(first_row.content_hash).expect("valid digest");
        let old_swarm_name = application_swarm_name(
            "database-password",
            &first_row.swarm_secret_name,
            &application,
        );
        let (_, old_value) = service
            .decrypt_generation(
                "database-password",
                &old_digest,
                &old_swarm_name,
                &application,
            )
            .await
            .expect("old generation remains recoverable");
        assert_eq!(old_value.expose(), b"first-value");

        let (_, generation, current_value) = service
            .decrypt_current("database-password")
            .await
            .expect("current generation decrypts");
        assert_eq!(generation, 2);
        assert_eq!(current_value.expose(), b"second-value");

        service
            .delete("database-password")
            .await
            .expect("unreferenced secret deletes transactionally");
        assert_eq!(
            service.get("database-password").await,
            Err(SecretError::NotFound)
        );
    }
}
