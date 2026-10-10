//! Each application's store of manually set secrets. Every environment may
//! mount a stored secret its access list allows; values are encrypted for
//! the application and copied into each environment's own Docker secrets.
use super::{
    Envelope, NormalizedApplication, SecretOwner, SecretSource, Store, StoreError,
    deletion::SecretDeletion, now_ms,
};
use crate::store::{Actor, Attribution};
use piqueld_core::{
    ApplicationId, EnvironmentId, ValidationError, ValidationErrors,
    access::Permission,
    api::{
        EnvironmentAccess, EnvironmentView, SecretAccess, SecretMetadata, SecretProblem,
        StoredSecret,
    },
    codes,
    manifest::ApplicationTemplate,
};
use sqlx::{Sqlite, SqliteConnection, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use zeroize::Zeroizing;

impl Store {
    /// Lists an application's stored secrets and their access, without
    /// accessing the master key or decrypting values.
    /// # Errors
    /// Returns application absence or database errors.
    pub async fn stored_secrets(
        &self,
        application: &ApplicationId,
    ) -> Result<Vec<StoredSecret>, StoreError> {
        self.application(application).await?;
        Self::stored_secrets_on(
            &mut *self.pool.acquire().await.map_err(StoreError::database)?,
            application,
        )
        .await
    }

    /// Lists an application's stored secrets on an existing connection or transaction.
    pub(in crate::store) async fn stored_secrets_on(
        connection: &mut SqliteConnection,
        application: &ApplicationId,
    ) -> Result<Vec<StoredSecret>, StoreError> {
        let id = application.as_str();
        let rows = sqlx::query!("SELECT s.name,s.generation,s.updated_at_ms,s.deletion_id,s.all_environments,s.previews,v.available FROM application_secrets s JOIN application_secret_versions v USING(application_id,name,generation) WHERE s.application_id=?1 ORDER BY s.name",id)
            .fetch_all(&mut *connection).await.map_err(StoreError::database)?;
        let mut allowed = BTreeMap::<String, BTreeSet<EnvironmentId>>::new();
        for row in sqlx::query!(
            "SELECT name,environment_id FROM application_secret_access WHERE application_id=?1",
            id
        )
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::database)?
        {
            allowed
                .entry(row.name)
                .or_default()
                .insert(EnvironmentId::parse(row.environment_id).map_err(StoreError::corrupt)?);
        }
        Ok(rows
            .into_iter()
            .map(|row| StoredSecret {
                access: SecretAccess {
                    environments: if row.all_environments == 0 {
                        EnvironmentAccess::Only(allowed.remove(&row.name).unwrap_or_default())
                    } else {
                        EnvironmentAccess::All
                    },
                    previews: row.previews != 0,
                },
                metadata: SecretMetadata {
                    name: row.name,
                    generation: row.generation,
                    updated_at_ms: row.updated_at_ms,
                    deleting: row.deletion_id.is_some(),
                    unavailable: row.available == 0,
                },
            })
            .collect())
    }

    /// Reads one stored secret on an existing transaction.
    async fn stored_secret_on(
        tx: &mut Transaction<'_, Sqlite>,
        application: &ApplicationId,
        name: &str,
    ) -> Result<StoredSecret, StoreError> {
        Self::stored_secrets_on(tx, application)
            .await?
            .into_iter()
            .find(|secret| secret.metadata.name == name)
            .ok_or(StoreError::NotFound)
    }

    /// Creates or rotates a stored secret. Rotation only affects a later
    /// deployment. `expected` is the current generation (zero to create).
    /// A new secret gets `access`, or every environment and no previews; an
    /// existing one keeps its access unless `access` replaces it. Each store
    /// holds at most 100 secrets, 1000 available versions and 100 MiB.
    /// Rejected while the application or the secret is being deleted, and for
    /// names the saved manifest or an environment's last fetched one declares
    /// as generated. `actor` needs `secrets:write` on the application, checked
    /// in the transaction.
    ///
    /// # Errors
    /// Returns invalid input, refusal, conflicts, quota, key or database errors.
    pub async fn put_stored_secret(
        &self,
        actor: Actor<'_>,
        application: &ApplicationId,
        name: &str,
        expected: i64,
        value: Vec<u8>,
        access: Option<&SecretAccess>,
    ) -> Result<StoredSecret, StoreError> {
        let value = Zeroizing::new(value);
        Self::check_secret_write(name, &value, expected)?;
        let (_writer, mut tx) = self.begin_immediate().await?;
        Self::require_secrets_write_on(&mut tx, actor, application).await?;
        let app = Self::application_on(&mut tx, application.as_str())
            .await?
            .ok_or(StoreError::NotFound)?;
        if app.delete_intent {
            return Err(StoreError::Busy);
        }
        Self::check_declared_names(&app.application.spec().secrets, [name])?;
        let id = application.as_str();
        let fetched = sqlx::query_scalar!(
            r#"SELECT manifest_json AS "manifest_json!" FROM environments WHERE application_id=?1 AND manifest_json IS NOT NULL"#,
            id
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        for manifest in fetched {
            let manifest: ApplicationTemplate =
                serde_json::from_str(&manifest).map_err(StoreError::corrupt)?;
            Self::check_declared_names(&manifest.spec().secrets, [name])?;
        }
        let existing = sqlx::query!(
            "SELECT generation,deletion_id FROM application_secrets WHERE application_id=?1 AND name=?2",
            id,
            name
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let current = existing.as_ref().map_or(0, |row| row.generation);
        if existing.is_some_and(|row| row.deletion_id.is_some()) {
            return Err(StoreError::SecretDeleting);
        }
        Self::secret_version_matches(expected, current)?;
        if current == 0 {
            let count = sqlx::query_scalar!(
                "SELECT COUNT(*) FROM application_secrets WHERE application_id=?1",
                id
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(StoreError::database)?;
            if count >= 100 {
                return Err(StoreError::InvalidInput);
            }
        }
        let generation = current.checked_add(1).ok_or(StoreError::InvalidInput)?;
        let usage = sqlx::query!("SELECT COUNT(*) AS versions, COALESCE(SUM(length(ciphertext)),0) AS bytes FROM application_secret_versions WHERE application_id=?1 AND available=1",id)
            .fetch_one(&mut *tx).await.map_err(StoreError::database)?;
        Self::check_secret_quota(usage.versions, usage.bytes, value.len())?;
        let cipher = self.verified_secret_cipher(&mut tx).await?;
        let envelope = cipher
            .encrypt(
                SecretOwner::Application(application),
                name,
                generation,
                &value,
            )
            .map_err(StoreError::SecretSource)?;
        let now = now_ms();
        let default = SecretAccess::default();
        let created = access.unwrap_or(&default);
        let (all, previews) = (
            i64::from(created.environments == EnvironmentAccess::All),
            i64::from(created.previews),
        );
        sqlx::query!("INSERT INTO application_secrets(application_id,name,generation,updated_at_ms,all_environments,previews) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(application_id,name) DO UPDATE SET generation=excluded.generation,updated_at_ms=excluded.updated_at_ms",id,name,generation,now,all,previews)
            .execute(&mut *tx).await.map_err(StoreError::database)?;
        sqlx::query!("INSERT INTO application_secret_versions(application_id,name,generation,nonce,ciphertext) VALUES(?1,?2,?3,?4,?5)",id,name,generation,envelope.nonce,envelope.ciphertext)
            .execute(&mut *tx).await.map_err(StoreError::database)?;
        if let Some(access) = access {
            Self::write_access_on(&mut tx, application, name, access).await?;
        }
        // History records the logical name and version, never the value.
        let message = format!("Stored secret version {generation}");
        let by = actor.attribution();
        let operator = by.operator_uid();
        sqlx::query!("INSERT INTO events(application_id,kind,message,resource,created_at_ms,actor_user_id,actor_credential_id,actor_operator_uid) VALUES(?1,'secret_saved',?2,?3,?4,?5,?6,?7)",id,message,name,now,by.user_id,by.credential_id,operator)
            .execute(&mut *tx).await.map_err(StoreError::database)?;
        let secret = Self::stored_secret_on(&mut tx, application, name).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(secret)
    }

    /// Replaces the access list of a stored secret. A narrower list only
    /// affects later deployments; running ones keep their pinned versions.
    /// `actor` needs `secrets:write` on the application, checked in the
    /// transaction.
    ///
    /// # Errors
    /// Returns refusal, absence, unknown environments (`InvalidInput`), or
    /// database errors.
    pub async fn set_secret_access(
        &self,
        actor: Actor<'_>,
        application: &ApplicationId,
        name: &str,
        access: &SecretAccess,
    ) -> Result<StoredSecret, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        Self::require_secrets_write_on(&mut tx, actor, application).await?;
        Self::stored_secret_on(&mut tx, application, name).await?;
        let description = Self::write_access_on(&mut tx, application, name, access).await?;
        let (id, now) = (application.as_str(), now_ms());
        let message = format!("Allowed {description} to mount the secret");
        let by = actor.attribution();
        let operator = by.operator_uid();
        sqlx::query!("INSERT INTO events(application_id,kind,message,resource,created_at_ms,actor_user_id,actor_credential_id,actor_operator_uid) VALUES(?1,'secret_access_changed',?2,?3,?4,?5,?6,?7)",id,message,name,now,by.user_id,by.credential_id,operator)
            .execute(&mut *tx).await.map_err(StoreError::database)?;
        let secret = Self::stored_secret_on(&mut tx, application, name).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(secret)
    }

    /// Stores `access` for an existing secret and describes it. Listed
    /// environments must belong to the application (`InvalidInput`).
    async fn write_access_on(
        tx: &mut Transaction<'_, Sqlite>,
        application: &ApplicationId,
        name: &str,
        access: &SecretAccess,
    ) -> Result<String, StoreError> {
        let environments = Self::environments_on(tx, application.as_str()).await?;
        let id = application.as_str();
        let previews = i64::from(access.previews);
        let all = i64::from(access.environments == EnvironmentAccess::All);
        sqlx::query!("UPDATE application_secrets SET all_environments=?3,previews=?4 WHERE application_id=?1 AND name=?2",id,name,all,previews)
            .execute(&mut **tx).await.map_err(StoreError::database)?;
        sqlx::query!(
            "DELETE FROM application_secret_access WHERE application_id=?1 AND name=?2",
            id,
            name
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        if let EnvironmentAccess::Only(allowed) = &access.environments {
            for environment in allowed {
                if !environments.iter().any(|known| known.id == *environment) {
                    return Err(StoreError::InvalidInput);
                }
                let environment = environment.as_str();
                sqlx::query!("INSERT INTO application_secret_access(application_id,name,environment_id) VALUES(?1,?2,?3)",id,name,environment)
                    .execute(&mut **tx).await.map_err(StoreError::database)?;
            }
        }
        Ok(access.describe(&environments))
    }

    /// Checks that `actor` holds `secrets:write` on `application`, inside the
    /// writing transaction so a concurrent demotion wins.
    async fn require_secrets_write_on(
        tx: &mut Transaction<'_, Sqlite>,
        actor: Actor<'_>,
        application: &ApplicationId,
    ) -> Result<(), StoreError> {
        actor
            .require_on(tx, Permission::App(Self::SECRETS_WRITE), Some(application))
            .await
    }

    /// Rejects `names` that `declared` also lists: a name refers to either a
    /// generated or a stored secret, never both.
    pub(in crate::store) fn check_declared_names<'a>(
        declared: &[piqueld_core::manifest::SecretDeclaration],
        names: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), StoreError> {
        let names = names.into_iter().collect::<BTreeSet<_>>();
        let errors = declared
            .iter()
            .enumerate()
            .filter(|(_, secret)| names.contains(secret.name.as_str()))
            .map(|(index, secret)| ValidationError {
                code: codes::SECRET_NAME_CONFLICT.into(),
                path: format!("spec.secrets[{index}].name"),
                message: format!(
                    "{} is declared as a generated secret and also set in the application's secret store; delete one of them",
                    secret.name
                ),
            })
            .collect::<Vec<_>>();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ValidationErrors(errors).into())
        }
    }

    /// Rejects `declared` secrets that `application`'s store also holds; see
    /// `check_declared_names`.
    pub(in crate::store) async fn check_declared_against_store_on(
        connection: &mut SqliteConnection,
        application: &str,
        declared: &[piqueld_core::manifest::SecretDeclaration],
    ) -> Result<(), StoreError> {
        let stored = sqlx::query_scalar!(
            "SELECT name FROM application_secrets WHERE application_id=?1",
            application
        )
        .fetch_all(connection)
        .await
        .map_err(StoreError::database)?;
        Self::check_declared_names(declared, stored.iter().map(String::as_str))
    }

    /// Checks that `app`, rendered for `environment`, may use its application
    /// store: no declared secret is also stored, and every stored secret it
    /// mounts allows the environment (`SecretAccessDenied`, naming the first
    /// that doesn't). Other unusable secrets fail later, when pinned.
    pub(in crate::store) async fn check_secret_access_on(
        connection: &mut SqliteConnection,
        environment: &EnvironmentView,
        app: &NormalizedApplication,
    ) -> Result<(), StoreError> {
        let denied = Self::secret_problems_on(connection, environment, app)
            .await?
            .into_iter()
            .find_map(|problem| match problem {
                SecretProblem::AccessDenied { secret } => Some(secret),
                SecretProblem::Missing { .. }
                | SecretProblem::Unavailable { .. }
                | SecretProblem::Deleting { .. } => None,
            });
        match denied {
            Some(secret) => Err(StoreError::SecretAccessDenied {
                environment: environment.name.clone(),
                secret,
            }),
            None => Ok(()),
        }
    }

    /// Checks a preview of `app` for `environment`; see `check_secret_access_on`.
    /// # Errors
    /// Returns `SecretAccessDenied`, validation, or database errors.
    pub async fn check_secret_access(
        &self,
        environment: &EnvironmentView,
        app: &NormalizedApplication,
    ) -> Result<(), StoreError> {
        Self::check_secret_access_on(
            &mut *self.pool.acquire().await.map_err(StoreError::database)?,
            environment,
            app,
        )
        .await
    }

    /// Reserves a stored secret for deletion and returns each environment's
    /// Docker secrets to remove. A new deletion is refused (`SecretReferenced`)
    /// while any environment still uses it (see `secret_in_use_on`). Resuming
    /// an existing reservation skips that check and reuses its token. `actor`
    /// needs `secrets:write` on the application, checked in the reserving
    /// transaction.
    pub(crate) async fn begin_stored_secret_deletion(
        &self,
        actor: Actor<'_>,
        application: &ApplicationId,
        name: &str,
        expected: i64,
    ) -> Result<SecretDeletion, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        Self::require_secrets_write_on(&mut tx, actor, application).await?;
        let id = application.as_str();
        let row = sqlx::query!("SELECT generation,deletion_id FROM application_secrets WHERE application_id=?1 AND name=?2",id,name)
            .fetch_optional(&mut *tx).await.map_err(StoreError::database)?.ok_or(StoreError::NotFound)?;
        Self::secret_version_matches(expected, row.generation)?;
        let mut versions = BTreeMap::<EnvironmentId, Vec<String>>::new();
        for copy in sqlx::query!(
            "SELECT environment_id,swarm_name FROM application_secret_copies WHERE application_id=?1 AND name=?2",
            id,
            name
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?
        {
            versions
                .entry(EnvironmentId::parse(copy.environment_id).map_err(StoreError::corrupt)?)
                .or_default()
                .push(copy.swarm_name);
        }
        if row.deletion_id.is_none() {
            for environment in Self::deployables_on(&mut tx, id).await? {
                let environment = Self::environment_on(&mut tx, environment.id.as_str())
                    .await?
                    .ok_or(StoreError::NotFound)?;
                let copies = versions
                    .get(environment.id())
                    .map_or(&[][..], Vec::as_slice);
                if Self::secret_in_use_on(&mut tx, &environment, name, SecretSource::Stored, copies)
                    .await?
                {
                    return Err(StoreError::SecretReferenced);
                }
            }
        }
        let deletion_id = row
            .deletion_id
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        sqlx::query!(
            "UPDATE application_secrets SET deletion_id=?3 WHERE application_id=?1 AND name=?2",
            id,
            name,
            deletion_id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(SecretDeletion {
            id: deletion_id,
            versions,
        })
    }

    /// Deletes a stored secret, its versions, copies and pins after runtime
    /// cleanup succeeded, recording `secret_deleted` by `actor`. A no-op if the
    /// reservation token no longer matches.
    pub(crate) async fn finish_stored_secret_deletion(
        &self,
        actor: Attribution<'_>,
        application: &ApplicationId,
        name: &str,
        deletion_id: &str,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let id = application.as_str();
        // The reservation token keeps late retries away from a new secret of the same name.
        sqlx::query!("DELETE FROM deployment_stored_secret_pins WHERE name=?2 AND environment_id IN (SELECT id FROM environments WHERE application_id=?1) AND EXISTS(SELECT 1 FROM application_secrets WHERE application_id=?1 AND name=?2 AND deletion_id=?3)",id,name,deletion_id)
            .execute(&mut *tx).await.map_err(StoreError::database)?;
        let deleted = sqlx::query!("DELETE FROM application_secrets WHERE application_id=?1 AND name=?2 AND deletion_id=?3",id,name,deletion_id)
            .execute(&mut *tx).await.map_err(StoreError::database)?.rows_affected();
        if deleted > 0 {
            let now = now_ms();
            let operator = actor.operator_uid();
            sqlx::query!("INSERT INTO events(application_id,kind,message,resource,created_at_ms,actor_user_id,actor_credential_id,actor_operator_uid) VALUES(?1,'secret_deleted','Deleted secret and its runtime versions',?2,?3,?4,?5,?6)",id,name,now,actor.user_id,actor.credential_id,operator)
                .execute(&mut *tx).await.map_err(StoreError::database)?;
        }
        tx.commit().await.map_err(StoreError::database)
    }

    /// Re-encrypts values that migration 0021 moved from environments into
    /// application stores, binding each to its application. Without a usable
    /// master key they stay readable in their former context until a later
    /// start re-encrypts them.
    /// # Errors
    /// Returns database or corruption errors.
    pub(in crate::store) async fn reencrypt_moved_secrets(&self) -> Result<(), StoreError> {
        let pending = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM application_secret_versions WHERE moved_from IS NOT NULL"
        )
        .fetch_one(&self.pool)
        .await
        .map_err(StoreError::database)?;
        if pending == 0 {
            return Ok(());
        }
        let (_writer, mut tx) = self.begin_immediate().await?;
        match self.reencrypt_moved_secrets_on(&mut tx).await {
            Ok(()) => {
                tx.commit().await.map_err(StoreError::database)?;
                tracing::info!(pending, "re-encrypted secrets moved to application stores");
                Ok(())
            }
            Err(StoreError::SecretSource(error)) => {
                tracing::warn!(
                    ?error,
                    pending,
                    "secrets moved to application stores keep their environment encryption until the master key is usable"
                );
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Re-encrypts every moved value in `tx`; see `reencrypt_moved_secrets`.
    async fn reencrypt_moved_secrets_on(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
    ) -> Result<(), StoreError> {
        let cipher = self.verified_secret_cipher(tx).await?;
        let rows = sqlx::query!(r#"SELECT application_id,name,generation,nonce,ciphertext,moved_from AS "moved_from!" FROM application_secret_versions WHERE moved_from IS NOT NULL"#)
            .fetch_all(&mut **tx).await.map_err(StoreError::database)?;
        for row in rows {
            let application =
                ApplicationId::parse(row.application_id.clone()).map_err(StoreError::corrupt)?;
            let moved = EnvironmentId::parse(row.moved_from).map_err(StoreError::corrupt)?;
            let value = cipher
                .decrypt(
                    SecretOwner::Environment(&moved),
                    &row.name,
                    row.generation,
                    &Envelope {
                        nonce: row.nonce,
                        ciphertext: row.ciphertext,
                    },
                )
                .map_err(StoreError::SecretSource)?;
            let envelope = cipher
                .encrypt(
                    SecretOwner::Application(&application),
                    &row.name,
                    row.generation,
                    &value,
                )
                .map_err(StoreError::SecretSource)?;
            sqlx::query!("UPDATE application_secret_versions SET nonce=?4,ciphertext=?5,moved_from=NULL WHERE application_id=?1 AND name=?2 AND generation=?3",row.application_id,row.name,row.generation,envelope.nonce,envelope.ciphertext)
                .execute(&mut **tx).await.map_err(StoreError::database)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
