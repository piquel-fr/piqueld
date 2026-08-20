//! Transactional repository support for the versioned control-plane archive.

use super::{SqliteConnection, SqliteStore, StoreError, new_id, now_ms};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use piqueld_client::StateExportMode;
use piqueld_core::{
    InstanceId, NormalizedApplication, ResolutionSet, compile_application as compile_resolved,
    resource::{ResolvedApplication, SecretGeneration},
};

use crate::transfer::{
    ArchiveApplication, ArchiveEnvelope, ArchiveSecret, ArchiveState, ArchiveStatus,
};

impl SqliteStore {
    pub(crate) async fn snapshot_state(
        &self,
        mode: StateExportMode,
    ) -> Result<ArchiveState, StoreError> {
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        let metadata = sqlx::query!(
            r#"SELECT instance_id AS "instance_id!",created_at_ms AS "created_at_ms!" FROM instance_metadata WHERE singleton=1"#
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::Corrupt)?;
        let applications = Self::snapshot_applications(&mut tx).await?;
        let secrets = Self::snapshot_secrets(&mut tx, mode).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(ArchiveState {
            source_instance_id: metadata.instance_id,
            source_created_at_ms: metadata.created_at_ms,
            applications,
            secrets,
        })
    }

    async fn snapshot_applications(
        connection: &mut SqliteConnection,
    ) -> Result<Vec<ArchiveApplication>, StoreError> {
        let rows = sqlx::query!(
            r#"SELECT a.desired_json AS "desired_json!",a.resolved_json AS "resolved_json!",a.generation AS "generation!",a.spec_hash AS "spec_hash!",a.created_at_ms AS "created_at_ms!",a.updated_at_ms AS "updated_at_ms!",s.state AS "state!",s.observed_generation,s.updated_at_ms AS "status_updated_at_ms!" FROM applications a JOIN application_status s ON s.application_id=a.id WHERE a.delete_intent=0 AND a.deleted_at_ms IS NULL ORDER BY a.id"#
        )
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::database)?;
        rows.into_iter()
            .map(|row| {
                Ok(ArchiveApplication {
                    application: serde_json::from_str(&row.desired_json)
                        .map_err(StoreError::corrupt)?,
                    resolved: serde_json::from_str(&row.resolved_json)
                        .map_err(StoreError::corrupt)?,
                    generation: u64::try_from(row.generation).map_err(StoreError::corrupt)?,
                    spec_hash: row.spec_hash,
                    created_at_ms: row.created_at_ms,
                    updated_at_ms: row.updated_at_ms,
                    status: ArchiveStatus {
                        state: row.state,
                        observed_generation: None,
                        message: None,
                        updated_at_ms: row.status_updated_at_ms,
                    },
                })
            })
            .collect()
    }

    async fn snapshot_secrets(
        connection: &mut SqliteConnection,
        mode: StateExportMode,
    ) -> Result<Vec<ArchiveSecret>, StoreError> {
        let rows = sqlx::query!(
            r#"SELECT id AS "id!",name AS "name!",generation AS "generation!",value_is_set AS "value_is_set!",created_at_ms AS "created_at_ms!",updated_at_ms AS "updated_at_ms!",encryption_algorithm,encryption_key_id,nonce,ciphertext,content_hash,swarm_secret_name FROM logical_secrets ORDER BY name"#
        )
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::database)?;
        let mut secrets = Vec::with_capacity(rows.len());
        for row in rows {
            let generation = u64::try_from(row.generation).map_err(StoreError::corrupt)?;
            let value_is_set = row.value_is_set == 1;
            let current = if mode == StateExportMode::Encrypted && value_is_set {
                Some(ArchiveEnvelope {
                    generation,
                    algorithm: row.encryption_algorithm.ok_or(StoreError::Corrupt)?,
                    key_id: row.encryption_key_id.ok_or(StoreError::Corrupt)?,
                    nonce_base64: BASE64.encode(row.nonce.ok_or(StoreError::Corrupt)?),
                    ciphertext_base64: BASE64.encode(row.ciphertext.ok_or(StoreError::Corrupt)?),
                    content_hash: row.content_hash.ok_or(StoreError::Corrupt)?,
                    swarm_secret_name: row.swarm_secret_name.ok_or(StoreError::Corrupt)?,
                    created_at_ms: row.created_at_ms,
                    retired_at_ms: None,
                })
            } else {
                None
            };
            let mut archive_secret = ArchiveSecret {
                id: row.id,
                name: row.name,
                generation,
                value_is_set: mode == StateExportMode::Encrypted && value_is_set,
                created_at_ms: row.created_at_ms,
                updated_at_ms: row.updated_at_ms,
                current,
                generations: Vec::new(),
            };
            if mode == StateExportMode::Encrypted {
                Self::snapshot_secret_generations(connection, &mut archive_secret).await?;
            }
            secrets.push(archive_secret);
        }
        Ok(secrets)
    }

    async fn snapshot_secret_generations(
        connection: &mut SqliteConnection,
        archive_secret: &mut ArchiveSecret,
    ) -> Result<(), StoreError> {
        let rows = sqlx::query!(
            r#"SELECT generation AS "generation!",encryption_algorithm AS "encryption_algorithm!",encryption_key_id AS "encryption_key_id!",nonce AS "nonce!",ciphertext AS "ciphertext!",content_hash AS "content_hash!",swarm_secret_name AS "swarm_secret_name!",created_at_ms AS "created_at_ms!",retired_at_ms FROM secret_generations WHERE secret_id=?1 ORDER BY generation"#,
            archive_secret.id
        )
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::database)?;
        for row in rows {
            archive_secret.generations.push(ArchiveEnvelope {
                generation: u64::try_from(row.generation).map_err(StoreError::corrupt)?,
                algorithm: row.encryption_algorithm,
                key_id: row.encryption_key_id,
                nonce_base64: BASE64.encode(row.nonce),
                ciphertext_base64: BASE64.encode(row.ciphertext),
                content_hash: row.content_hash,
                swarm_secret_name: row.swarm_secret_name,
                created_at_ms: row.created_at_ms,
                retired_at_ms: row.retired_at_ms,
            });
        }
        if let Some(current) = archive_secret.current.as_ref() {
            let journal = archive_secret
                .generations
                .iter()
                .find(|generation| generation.generation == current.generation)
                .ok_or(StoreError::Corrupt)?;
            if journal != current || journal.retired_at_ms.is_some() {
                return Err(StoreError::Corrupt);
            }
        }
        Ok(())
    }

    pub(crate) async fn replace_state(
        &self,
        state: &ArchiveState,
        fail_after_delete: bool,
        operation_id: &str,
        archive_digest: &str,
    ) -> Result<(), StoreError> {
        let mut tx = self.begin_immediate().await?;
        delete_imported_state(&mut tx).await?;
        if fail_after_delete {
            return Err(StoreError::Database);
        }

        insert_imported_secrets(&mut tx, state).await?;
        insert_imported_applications(&mut tx, state).await?;

        finish_import(&mut tx, operation_id, archive_digest).await?;
        tx.commit().await.map_err(StoreError::database)
    }

    pub(crate) async fn audit_transfer_start(
        &self,
        id: &str,
        direction: &str,
        mode: &str,
        source: Option<&str>,
    ) -> Result<(), StoreError> {
        let now = now_ms();
        sqlx::query!(
            "INSERT INTO state_transfer_operations(id,direction,mode,state,source_instance_id,created_at_ms) VALUES(?1,?2,?3,'running',?4,?5)",
            id,
            direction,
            mode,
            source,
            now
        )
        .execute(&self.pool)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    pub(crate) async fn audit_transfer_finish(
        &self,
        id: &str,
        succeeded: bool,
        archive_digest: Option<&str>,
        diagnostic: Option<&str>,
    ) -> Result<(), StoreError> {
        let state = if succeeded { "succeeded" } else { "failed" };
        let finished = now_ms();
        sqlx::query!(
            "UPDATE state_transfer_operations SET state=?2,archive_digest=?3,diagnostic_code=?4,finished_at_ms=?5 WHERE id=?1",
            id,
            state,
            archive_digest,
            diagnostic,
            finished
        )
        .execute(&self.pool)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }
}

async fn delete_imported_state(connection: &mut SqliteConnection) -> Result<(), StoreError> {
    sqlx::query!("DELETE FROM mutation_idempotency")
        .execute(&mut *connection)
        .await
        .map_err(StoreError::database)?;
    sqlx::query!("DELETE FROM applications")
        .execute(&mut *connection)
        .await
        .map_err(StoreError::database)?;
    sqlx::query!("DELETE FROM logical_secrets")
        .execute(&mut *connection)
        .await
        .map_err(StoreError::database)?;
    Ok(())
}

async fn insert_imported_secrets(
    connection: &mut SqliteConnection,
    state: &ArchiveState,
) -> Result<(), StoreError> {
    for secret in &state.secrets {
        let envelope = secret.current.as_ref();
        let generation = i64::try_from(secret.generation).map_err(StoreError::corrupt)?;
        let algorithm = envelope.map(|value| value.algorithm.as_str());
        let key_id = envelope.map(|value| value.key_id.as_str());
        let nonce = envelope
            .map(|value| BASE64.decode(&value.nonce_base64))
            .transpose()
            .map_err(|_| StoreError::Corrupt)?;
        let ciphertext = envelope
            .map(|value| BASE64.decode(&value.ciphertext_base64))
            .transpose()
            .map_err(|_| StoreError::Corrupt)?;
        let swarm_name = envelope.map(|value| value.swarm_secret_name.as_str());
        let value_is_set = i64::from(secret.value_is_set && envelope.is_some());
        let content_hash = envelope.map(|value| value.content_hash.as_str());
        sqlx::query!(
            "INSERT INTO logical_secrets(id,name,generation,encryption_algorithm,encryption_key_id,nonce,ciphertext,swarm_secret_name,value_is_set,content_hash,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            secret.id,
            secret.name,
            generation,
            algorithm,
            key_id,
            nonce,
            ciphertext,
            swarm_name,
            value_is_set,
            content_hash,
            secret.created_at_ms,
            secret.updated_at_ms
        )
        .execute(&mut *connection)
        .await
        .map_err(StoreError::database)?;
        for envelope in &secret.generations {
            insert_imported_generation(connection, &secret.id, envelope).await?;
        }
    }
    Ok(())
}

async fn insert_imported_generation(
    connection: &mut SqliteConnection,
    secret_id: &str,
    envelope: &ArchiveEnvelope,
) -> Result<(), StoreError> {
    let generation = i64::try_from(envelope.generation).map_err(StoreError::corrupt)?;
    let nonce = BASE64
        .decode(&envelope.nonce_base64)
        .map_err(|_| StoreError::Corrupt)?;
    let ciphertext = BASE64
        .decode(&envelope.ciphertext_base64)
        .map_err(|_| StoreError::Corrupt)?;
    sqlx::query!(
        "INSERT INTO secret_generations(secret_id,generation,encryption_algorithm,encryption_key_id,nonce,ciphertext,content_hash,swarm_secret_name,created_at_ms,retired_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        secret_id,
        generation,
        envelope.algorithm,
        envelope.key_id,
        nonce,
        ciphertext,
        envelope.content_hash,
        envelope.swarm_secret_name,
        envelope.created_at_ms,
        envelope.retired_at_ms
    )
    .execute(&mut *connection)
    .await
    .map_err(StoreError::database)?;
    Ok(())
}

async fn insert_imported_applications(
    connection: &mut SqliteConnection,
    state: &ArchiveState,
) -> Result<(), StoreError> {
    for application in &state.applications {
        let id = application.application.id.as_str();
        let desired = application
            .application
            .canonical_json()
            .map_err(StoreError::corrupt)?;
        let resolved = serde_json::to_string(&application.resolved).map_err(StoreError::corrupt)?;
        let generation = i64::try_from(application.generation).map_err(StoreError::corrupt)?;
        sqlx::query!(
            "INSERT INTO applications(id,name,generation,desired_json,resolved_json,spec_hash,delete_intent,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,0,?7,?8)",
            id,
            application.application.metadata.name,
            generation,
            desired,
            resolved,
            application.spec_hash,
            application.created_at_ms,
            application.updated_at_ms
        )
        .execute(&mut *connection)
        .await
        .map_err(StoreError::database)?;
        insert_imported_status(connection, id, application, generation).await?;
    }
    Ok(())
}

async fn insert_imported_status(
    connection: &mut SqliteConnection,
    id: &str,
    application: &ArchiveApplication,
    generation: i64,
) -> Result<(), StoreError> {
    let status_time = now_ms().max(application.status.updated_at_ms);
    sqlx::query!(
        "INSERT INTO application_status(application_id,state,observed_generation,message,updated_at_ms) VALUES(?1,'pending',NULL,?2,?3)",
        id,
        "imported desired state is awaiting reconciliation",
        status_time
    )
    .execute(&mut *connection)
    .await
    .map_err(StoreError::database)?;
    let reconcile_id = new_id("operation");
    sqlx::query!(
        "INSERT INTO operations(id,application_id,generation,kind,state,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,'reconcile','pending',?4,?4)",
        reconcile_id,
        id,
        generation,
        status_time
    )
    .execute(&mut *connection)
    .await
    .map_err(StoreError::database)?;
    Ok(())
}

async fn finish_import(
    connection: &mut SqliteConnection,
    operation_id: &str,
    archive_digest: &str,
) -> Result<(), StoreError> {
    let finished = now_ms();
    let updated = sqlx::query!(
        "UPDATE state_transfer_operations SET state='succeeded',archive_digest=?2,diagnostic_code=NULL,finished_at_ms=?3 WHERE id=?1 AND state='running'",
        operation_id,
        archive_digest,
        finished
    )
    .execute(&mut *connection)
    .await
    .map_err(StoreError::database)?
    .rows_affected();
    if updated == 1 {
        Ok(())
    } else {
        Err(StoreError::Database)
    }
}

/// Application-side archive record validation helper used by tests.
pub(crate) fn recompile_imported_application(
    application: &NormalizedApplication,
    resolved: &ResolvedApplication,
    target: &InstanceId,
) -> Result<ResolvedApplication, StoreError> {
    let sources = resolved
        .services
        .iter()
        .map(|service| (service.logical_name.clone(), service.source.clone()))
        .collect();
    let secrets = resolved
        .secrets
        .iter()
        .map(|secret| {
            (
                secret.logical_name.clone(),
                SecretGeneration {
                    logical_name: secret.logical_name.clone(),
                    generation: secret.generation.clone(),
                    swarm_name: secret.name.clone(),
                },
            )
        })
        .collect();
    compile_resolved(
        application,
        target.clone(),
        &ResolutionSet { sources, secrets },
    )
    .map_err(|_| StoreError::Corrupt)
}
