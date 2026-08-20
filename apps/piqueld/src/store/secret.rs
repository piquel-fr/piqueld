//! Logical-secret persistence with bounded, transactional lifecycle updates.

use piqueld_client::Page;

use super::{
    ApplicationId, SqliteStore, StoreError, decode_application, decode_deployed, new_id, now_ms,
    page_limit,
};

pub(crate) struct SecretMetadataRow {
    pub(crate) name: String,
    pub(crate) value_is_set: i64,
    pub(crate) generation: i64,
    pub(crate) created_at_ms: i64,
    pub(crate) updated_at_ms: i64,
}

pub(crate) struct SecretEnvelopeRow {
    pub(crate) generation: i64,
    pub(crate) encryption_algorithm: String,
    pub(crate) encryption_key_id: String,
    pub(crate) nonce: Vec<u8>,
    pub(crate) ciphertext: Vec<u8>,
    pub(crate) content_hash: String,
    pub(crate) swarm_secret_name: String,
}

pub(crate) struct SecretGenerationRow {
    pub(crate) content_hash: String,
    pub(crate) swarm_secret_name: String,
}

pub(crate) struct SecretWrite {
    pub(crate) algorithm: String,
    pub(crate) key_id: String,
    pub(crate) nonce: Vec<u8>,
    pub(crate) ciphertext: Vec<u8>,
    pub(crate) content_hash: String,
    pub(crate) swarm_name: String,
}

pub(crate) enum SecretDeleteResult {
    Deleted,
    Referenced,
    NotFound,
}

impl SqliteStore {
    pub(crate) async fn secret_metadata(
        &self,
        name: &str,
    ) -> Result<Option<SecretMetadataRow>, StoreError> {
        sqlx::query_as!(
            SecretMetadataRow,
            r#"SELECT name AS "name!",value_is_set AS "value_is_set!",generation AS "generation!",created_at_ms AS "created_at_ms!",updated_at_ms AS "updated_at_ms!" FROM logical_secrets WHERE name=?1"#,
            name
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)
    }

    pub(crate) async fn secret_metadata_page(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<SecretMetadataRow>, StoreError> {
        let fetch_limit = page_limit(limit)? + 1;
        let after = cursor
            .map(|cursor| {
                cursor
                    .strip_prefix("v1:")
                    .filter(|name| valid_name(name))
                    .ok_or(StoreError::InvalidInput)
                    .map(str::to_owned)
            })
            .transpose()?;
        let rows = if let Some(after) = after.as_deref() {
            sqlx::query_as!(
                SecretMetadataRow,
                r#"SELECT name AS "name!",value_is_set AS "value_is_set!",generation AS "generation!",created_at_ms AS "created_at_ms!",updated_at_ms AS "updated_at_ms!" FROM logical_secrets WHERE name > ?1 ORDER BY name LIMIT ?2"#,
                after,
                fetch_limit
            )
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as!(
                SecretMetadataRow,
                r#"SELECT name AS "name!",value_is_set AS "value_is_set!",generation AS "generation!",created_at_ms AS "created_at_ms!",updated_at_ms AS "updated_at_ms!" FROM logical_secrets ORDER BY name LIMIT ?1"#,
                fetch_limit
            )
            .fetch_all(&self.pool)
            .await
        }
        .map_err(StoreError::database)?;
        let mut items = rows;
        let has_more = items.len() > limit;
        items.truncate(limit);
        let next_cursor = has_more.then(|| {
            format!(
                "v1:{}",
                items
                    .last()
                    .expect("a non-empty bounded page has a cursor")
                    .name
            )
        });
        Ok(Page { items, next_cursor })
    }

    pub(crate) async fn write_secret<E, F>(
        &self,
        name: &str,
        create: bool,
        encrypt: F,
    ) -> Result<(), E>
    where
        E: From<StoreError>,
        F: FnOnce(u64) -> Result<SecretWrite, E>,
    {
        let mut tx = self.begin_immediate().await.map_err(E::from)?;
        let existing = sqlx::query!(
            r#"SELECT id AS "id!",generation AS "generation!",value_is_set AS "value_is_set!",created_at_ms AS "created_at_ms!" FROM logical_secrets WHERE name=?1"#,
            name
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| E::from(StoreError::database(error)))?;
        if create && existing.as_ref().is_some_and(|row| row.value_is_set == 1) {
            return Err(E::from(StoreError::AlreadyExists));
        }
        if !create && existing.is_none() {
            return Err(E::from(StoreError::NotFound));
        }
        let new_row = existing.is_none();
        let (id, generation, created_at_ms) = match existing {
            None => (new_id("secret"), 1_u64, 0_i64),
            Some(row) => {
                let current =
                    u64::try_from(row.generation).map_err(|_| E::from(StoreError::Corrupt))?;
                let generation = if row.value_is_set == 1 {
                    current
                        .checked_add(1)
                        .ok_or_else(|| E::from(StoreError::Corrupt))?
                } else {
                    current
                };
                (row.id, generation, row.created_at_ms)
            }
        };
        let encrypted = encrypt(generation)?;
        let generation_db =
            i64::try_from(generation).map_err(|_| E::from(StoreError::InvalidInput))?;
        let now = now_ms().max(created_at_ms);
        if new_row {
            sqlx::query!(
                "INSERT INTO logical_secrets(id,name,generation,encryption_algorithm,encryption_key_id,nonce,ciphertext,swarm_secret_name,value_is_set,content_hash,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,1,?9,?10,?10)",
                id,
                name,
                generation_db,
                encrypted.algorithm,
                encrypted.key_id,
                encrypted.nonce,
                encrypted.ciphertext,
                encrypted.swarm_name,
                encrypted.content_hash,
                now
            )
            .execute(&mut *tx)
            .await
            .map_err(|error| E::from(StoreError::database(error)))?;
        } else {
            sqlx::query!(
                "UPDATE secret_generations SET retired_at_ms=?2 WHERE secret_id=?1 AND retired_at_ms IS NULL",
                id,
                now
            )
            .execute(&mut *tx)
            .await
            .map_err(|error| E::from(StoreError::database(error)))?;
            let changed = sqlx::query!(
                "UPDATE logical_secrets SET generation=?2,encryption_algorithm=?3,encryption_key_id=?4,nonce=?5,ciphertext=?6,swarm_secret_name=?7,value_is_set=1,content_hash=?8,updated_at_ms=?9 WHERE id=?1",
                id,
                generation_db,
                encrypted.algorithm,
                encrypted.key_id,
                encrypted.nonce,
                encrypted.ciphertext,
                encrypted.swarm_name,
                encrypted.content_hash,
                now
            )
            .execute(&mut *tx)
            .await
            .map_err(|error| E::from(StoreError::database(error)))?
            .rows_affected();
            if changed != 1 {
                return Err(E::from(StoreError::NotFound));
            }
        }
        sqlx::query!(
            "INSERT INTO secret_generations(secret_id,generation,encryption_algorithm,encryption_key_id,nonce,ciphertext,content_hash,swarm_secret_name,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            id,
            generation_db,
            encrypted.algorithm,
            encrypted.key_id,
            encrypted.nonce,
            encrypted.ciphertext,
            encrypted.content_hash,
            encrypted.swarm_name,
            now
        )
        .execute(&mut *tx)
        .await
        .map_err(|error| E::from(StoreError::database(error)))?;
        tx.commit()
            .await
            .map_err(|error| E::from(StoreError::database(error)))
    }

    pub(crate) async fn secret_envelope(
        &self,
        name: &str,
    ) -> Result<Option<SecretEnvelopeRow>, StoreError> {
        sqlx::query_as!(
            SecretEnvelopeRow,
            r#"SELECT generation AS "generation!",encryption_algorithm AS "encryption_algorithm!",encryption_key_id AS "encryption_key_id!",nonce AS "nonce!",ciphertext AS "ciphertext!",content_hash AS "content_hash!",swarm_secret_name AS "swarm_secret_name!" FROM logical_secrets WHERE name=?1 AND value_is_set=1"#,
            name
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)
    }

    pub(crate) async fn secret_generation(
        &self,
        name: &str,
    ) -> Result<Option<SecretGenerationRow>, StoreError> {
        sqlx::query_as!(
            SecretGenerationRow,
            r#"SELECT content_hash AS "content_hash!",swarm_secret_name AS "swarm_secret_name!" FROM logical_secrets WHERE name=?1 AND value_is_set=1"#,
            name
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)
    }

    pub(crate) async fn secret_envelope_digest(
        &self,
        name: &str,
        content_hash: &str,
    ) -> Result<Vec<SecretEnvelopeRow>, StoreError> {
        sqlx::query_as!(
            SecretEnvelopeRow,
            r#"SELECT g.generation AS "generation!",g.encryption_algorithm AS "encryption_algorithm!",g.encryption_key_id AS "encryption_key_id!",g.nonce AS "nonce!",g.ciphertext AS "ciphertext!",g.content_hash AS "content_hash!",g.swarm_secret_name AS "swarm_secret_name!" FROM secret_generations g JOIN logical_secrets s ON s.id=g.secret_id WHERE s.name=?1 AND g.content_hash=?2"#,
            name,
            content_hash
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)
    }

    pub(crate) async fn prune_retired_secret_generations(&self) -> Result<(), StoreError> {
        let mut tx = self.begin_immediate().await?;
        let retired = sqlx::query!(
            r#"SELECT g.secret_id AS "secret_id!",g.generation AS "generation!",s.name AS "logical_name!",g.content_hash AS "content_hash!",g.swarm_secret_name AS "swarm_secret_name!" FROM secret_generations g JOIN logical_secrets s ON s.id=g.secret_id WHERE g.retired_at_ms IS NOT NULL ORDER BY g.secret_id,g.generation"#
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let applications = sqlx::query!(
            r#"SELECT id AS "id!",resolved_json AS "resolved_json!",deployed_json FROM applications WHERE deleted_at_ms IS NULL"#
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        for generation in retired {
            let referenced = applications.iter().any(|application| {
                [
                    Some(application.resolved_json.as_str()),
                    application.deployed_json.as_deref(),
                ]
                .into_iter()
                .flatten()
                .filter_map(|json| {
                    serde_json::from_str::<piqueld_core::resource::ResolvedApplication>(json).ok()
                })
                .any(|resolved| {
                    resolved.secrets.iter().any(|secret| {
                        secret.logical_name == generation.logical_name
                            && secret.generation.as_str() == generation.content_hash
                            && ApplicationId::parse(application.id.clone())
                                .ok()
                                .is_some_and(|id| {
                                    crate::secrets::application_swarm_name(
                                        &generation.logical_name,
                                        &generation.swarm_secret_name,
                                        &id,
                                    ) == secret.name
                                })
                    })
                })
            });
            if !referenced {
                sqlx::query!(
                    "DELETE FROM secret_generations WHERE secret_id=?1 AND generation=?2 AND retired_at_ms IS NOT NULL",
                    generation.secret_id,
                    generation.generation
                )
                .execute(&mut *tx)
                .await
                .map_err(StoreError::database)?;
            }
        }
        tx.commit().await.map_err(StoreError::database)
    }

    pub(crate) async fn delete_secret_safely(
        &self,
        name: &str,
    ) -> Result<SecretDeleteResult, StoreError> {
        let mut tx = self.begin_immediate().await?;
        let applications = sqlx::query!(
            r#"SELECT id AS "id!",desired_json AS "desired_json!",deployed_json FROM applications WHERE deleted_at_ms IS NULL"#
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        for row in applications {
            let desired =
                decode_application(&row.desired_json, &application_hash(&row.desired_json)?)?;
            let desired_reference = desired
                .spec
                .services
                .iter()
                .any(|service| service.secrets.iter().any(|secret| secret.source == name));
            let application_id = ApplicationId::parse(row.id).map_err(StoreError::corrupt)?;
            let deployed_reference = row
                .deployed_json
                .as_deref()
                .map(|json| {
                    decode_deployed(json, &application_id, &self.instance_id).map(|resolved| {
                        resolved.services.iter().any(|service| {
                            service
                                .secrets
                                .iter()
                                .any(|secret| secret.logical_name == name)
                        })
                    })
                })
                .transpose()?
                .unwrap_or(false);
            if desired_reference || deployed_reference {
                return Ok(SecretDeleteResult::Referenced);
            }
        }
        let changed = sqlx::query!("DELETE FROM logical_secrets WHERE name=?1", name)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?
            .rows_affected();
        if changed == 1 {
            tx.commit().await.map_err(StoreError::database)?;
            Ok(SecretDeleteResult::Deleted)
        } else {
            Ok(SecretDeleteResult::NotFound)
        }
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

fn application_hash(json: &str) -> Result<String, StoreError> {
    let value: piqueld_core::NormalizedApplication =
        serde_json::from_str(json).map_err(StoreError::corrupt)?;
    Ok(value.spec_hash())
}
