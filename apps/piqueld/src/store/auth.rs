//! Accounts, passkeys, revocable credentials, and invitations.
//!
//! Timestamps are Unix seconds. Callers pass only hashes of credential and
//! invitation secrets, so this module never sees a usable secret. Writes queue
//! behind the shared writer, and account changes refuse to lock everyone out.
use super::{Store, StoreError, now_secs};
use piqueld_core::auth::{CredentialView, Directory, InvitationView, PasskeyView, User};
use sqlx::{Sqlite, SqliteConnection, query::Query, sqlite::SqliteArguments};
use std::fmt;

/// Browser sessions end after a day without use, even before they expire.
const SESSION_IDLE_SECS: i64 = 86_400;

/// Account changes refused because nobody could sign in afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lockout {
    /// Deleting the only account.
    LastAccount,
    /// Removing the only passkey, directly or with its account.
    LastPasskey,
}

impl Lockout {
    /// Human-readable reason, suitable for API responses.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::LastAccount => "the last account cannot be deleted",
            Self::LastPasskey => "the last passkey cannot be removed",
        }
    }
}

impl fmt::Display for Lockout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for Lockout {}

/// Credential classes. Only browser sessions have an idle timeout.
#[derive(Clone, Copy, Debug)]
pub(crate) enum CredentialKind {
    Browser,
    Cli,
    Token,
}

impl CredentialKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Browser => "browser",
            Self::Cli => "cli",
            Self::Token => "token",
        }
    }
}

/// A credential to store for an account. Only its secret's hash is persisted.
pub(crate) struct NewCredential<'a> {
    pub(crate) id: String,
    pub(crate) secret_hash: String,
    pub(crate) kind: CredentialKind,
    pub(crate) name: &'a str,
    pub(crate) expires_at: Option<i64>,
}

impl NewCredential<'_> {
    async fn insert(&self, db: &mut SqliteConnection, user_id: &str) -> Result<(), StoreError> {
        let now = now_secs();
        let kind = self.kind.as_str();
        sqlx::query!(
            "INSERT INTO auth_credentials(id,user_id,secret_hash,kind,name,created_at,last_used_at,expires_at) VALUES(?1,?2,?3,?4,?5,?6,?6,?7)",
            self.id,
            user_id,
            self.secret_hash,
            kind,
            self.name,
            now,
            self.expires_at
        )
        .execute(db)
        .await
        .map_err(StoreError::constraint)?;
        Ok(())
    }
}

/// A registered `WebAuthn` credential, serialized by the caller.
pub(crate) struct NewPasskey<'a> {
    pub(crate) id: &'a str,
    pub(crate) name: &'a str,
    pub(crate) credential: &'a str,
}

/// Who a newly registered passkey belongs to.
pub(crate) enum PasskeyOwner<'a> {
    /// An existing account adds another passkey.
    Existing(&'a str),
    /// Registration creates the account, consuming the setup secret or an
    /// invitation, and signs it in with `session`.
    New {
        user: &'a User,
        invitation_hash: &'a str,
        session: NewCredential<'a>,
    },
}

/// The account behind a live credential.
pub(crate) struct CredentialOwner {
    pub(crate) credential_id: String,
    pub(crate) user: User,
    pub(crate) last_used_at: i64,
}

impl Store {
    /// Runs one statement through the shared writer queue and returns the
    /// number of affected rows.
    async fn write_one<'q>(
        &self,
        query: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Result<u64, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let rows = query
            .execute(&mut *tx)
            .await
            .map_err(StoreError::constraint)?
            .rows_affected();
        tx.commit().await.map_err(StoreError::database)?;
        Ok(rows)
    }

    /// Whether first-account setup has completed.
    pub(crate) async fn auth_initialized(&self) -> Result<bool, StoreError> {
        sqlx::query_scalar!(
            r#"SELECT initialized AS "initialized: bool" FROM auth_setup WHERE singleton=1"#
        )
        .fetch_one(&self.pool)
        .await
        .map_err(StoreError::database)
    }

    /// Replaces the setup secret while the installation is unclaimed.
    pub(crate) async fn set_setup_secret(&self, hash: &str) -> Result<(), StoreError> {
        self.write_one(sqlx::query!(
            "UPDATE auth_setup SET secret_hash=?1 WHERE singleton=1 AND initialized=0",
            hash
        ))
        .await?;
        Ok(())
    }

    /// Whether a hash matches the open setup secret or a live invitation.
    pub(crate) async fn invitation_valid(&self, hash: &str) -> Result<bool, StoreError> {
        let now = now_secs();
        sqlx::query_scalar!(
            r#"SELECT (EXISTS(SELECT 1 FROM auth_setup WHERE initialized=0 AND secret_hash=?1) OR EXISTS(SELECT 1 FROM auth_invitations WHERE secret_hash=?1 AND expires_at>?2)) AS "valid!: bool""#,
            hash,
            now
        )
        .fetch_one(&self.pool)
        .await
        .map_err(StoreError::database)
    }

    pub(crate) async fn auth_user(&self, id: &str) -> Result<Option<User>, StoreError> {
        sqlx::query_as!(
            User,
            r#"SELECT id AS "id!",username,display_name FROM auth_users WHERE id=?1"#,
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)
    }

    /// Serialized `WebAuthn` credentials registered to one account.
    pub(crate) async fn passkey_credentials(
        &self,
        user_id: &str,
    ) -> Result<Vec<String>, StoreError> {
        sqlx::query_scalar!(
            "SELECT credential FROM auth_passkeys WHERE user_id=?1",
            user_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)
    }

    /// Stores a registered passkey. For a new account this atomically consumes
    /// the invitation, creates the account, and opens its session. Returns
    /// `false` when the invitation was already used or has expired.
    pub(crate) async fn add_passkey(
        &self,
        owner: PasskeyOwner<'_>,
        passkey: NewPasskey<'_>,
    ) -> Result<bool, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let now = now_secs();
        let user_id = match &owner {
            PasskeyOwner::Existing(user_id) => user_id,
            PasskeyOwner::New {
                user,
                invitation_hash,
                session,
            } => {
                let setup = sqlx::query!(
                    "UPDATE auth_setup SET initialized=1,secret_hash=NULL WHERE initialized=0 AND secret_hash=?1",
                    invitation_hash
                )
                .execute(&mut *tx)
                .await
                .map_err(StoreError::database)?
                .rows_affected();
                if setup == 0 {
                    let consumed = sqlx::query!(
                        "DELETE FROM auth_invitations WHERE secret_hash=?1 AND expires_at>?2",
                        invitation_hash,
                        now
                    )
                    .execute(&mut *tx)
                    .await
                    .map_err(StoreError::database)?
                    .rows_affected();
                    if consumed != 1 {
                        return Ok(false);
                    }
                }
                sqlx::query!(
                    "INSERT INTO auth_users(id,username,display_name,created_at) VALUES(?1,?2,?3,?4)",
                    user.id,
                    user.username,
                    user.display_name,
                    now
                )
                .execute(&mut *tx)
                .await
                .map_err(StoreError::constraint)?;
                session.insert(&mut tx, &user.id).await?;
                user.id.as_str()
            }
        };
        sqlx::query!(
            "INSERT INTO auth_passkeys(id,user_id,name,credential,created_at) VALUES(?1,?2,?3,?4,?5)",
            passkey.id,
            user_id,
            passkey.name,
            passkey.credential,
            now
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::constraint)?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(true)
    }

    /// Verifies a passkey assertion and opens a browser session while holding
    /// the writer lock, so a concurrent deletion or replay cannot resurrect
    /// access. `verify` receives the stored credential and returns its updated
    /// form. Returns `None` when the passkey does not belong to `user_id`.
    pub(crate) async fn sign_in_with_passkey<E: From<StoreError>>(
        &self,
        passkey_id: &str,
        user_id: &str,
        verify: impl FnOnce(&str) -> Result<String, E>,
        session: &NewCredential<'_>,
    ) -> Result<Option<User>, E> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let Some(row) = sqlx::query!(
            r#"SELECT p.credential,u.id AS "id!",u.username,u.display_name FROM auth_passkeys p JOIN auth_users u ON u.id=p.user_id WHERE p.id=?1 AND u.id=?2"#,
            passkey_id,
            user_id
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?
        else {
            return Ok(None);
        };
        let credential = verify(&row.credential)?;
        sqlx::query!(
            "UPDATE auth_passkeys SET credential=?1 WHERE id=?2",
            credential,
            passkey_id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        session.insert(&mut tx, user_id).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(Some(User {
            id: row.id,
            username: row.username,
            display_name: row.display_name,
        }))
    }

    /// Finds the account for a live credential by its secret hash. This is
    /// read-only, so authenticating never waits for writers.
    pub(crate) async fn credential_owner(
        &self,
        hash: &str,
    ) -> Result<Option<CredentialOwner>, StoreError> {
        let now = now_secs();
        let idle = now - SESSION_IDLE_SECS;
        let row = sqlx::query!(
            r#"SELECT c.id AS "credential_id!",c.last_used_at,u.id AS "id!",u.username,u.display_name FROM auth_credentials c JOIN auth_users u ON u.id=c.user_id WHERE c.secret_hash=?1 AND (c.expires_at IS NULL OR c.expires_at>?2) AND (c.kind!='browser' OR c.last_used_at>?3)"#,
            hash,
            now,
            idle
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?;
        Ok(row.map(|row| CredentialOwner {
            credential_id: row.credential_id,
            user: User {
                id: row.id,
                username: row.username,
                display_name: row.display_name,
            },
            last_used_at: row.last_used_at,
        }))
    }

    /// Records use of a live credential. It queues behind other writers, so a
    /// revocation that lands first wins. Returns `false` when the credential
    /// was revoked or expired in the meantime.
    pub(crate) async fn touch_credential(&self, id: &str) -> Result<bool, StoreError> {
        let now = now_secs();
        let idle = now - SESSION_IDLE_SECS;
        let rows = self
            .write_one(sqlx::query!(
                "UPDATE auth_credentials SET last_used_at=MAX(last_used_at,?1) WHERE id=?2 AND (expires_at IS NULL OR expires_at>?1) AND (kind!='browser' OR last_used_at>?3)",
                now,
                id,
                idle
            ))
            .await?;
        Ok(rows == 1)
    }

    /// Issues `credential` to the account behind the live credential
    /// `approver_id`. Returns `None` when that approving session has ended.
    pub(crate) async fn issue_for_credential_owner(
        &self,
        approver_id: &str,
        credential: &NewCredential<'_>,
    ) -> Result<Option<User>, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let now = now_secs();
        let idle = now - SESSION_IDLE_SECS;
        let Some(user) = sqlx::query_as!(
            User,
            r#"SELECT u.id AS "id!",u.username,u.display_name FROM auth_users u JOIN auth_credentials c ON c.user_id=u.id WHERE c.id=?1 AND (c.expires_at IS NULL OR c.expires_at>?2) AND (c.kind!='browser' OR c.last_used_at>?3)"#,
            approver_id,
            now,
            idle
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?
        else {
            return Ok(None);
        };
        credential.insert(&mut tx, &user.id).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(Some(user))
    }

    /// Stores a credential for an existing account.
    pub(crate) async fn insert_credential(
        &self,
        user_id: &str,
        credential: &NewCredential<'_>,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        credential.insert(&mut tx, user_id).await?;
        tx.commit().await.map_err(StoreError::database)
    }

    pub(crate) async fn revoke_credential(&self, id: &str) -> Result<(), StoreError> {
        self.write_one(sqlx::query!("DELETE FROM auth_credentials WHERE id=?1", id))
            .await?;
        Ok(())
    }

    pub(crate) async fn revoke_credentials(&self, user_id: &str) -> Result<(), StoreError> {
        self.write_one(sqlx::query!(
            "DELETE FROM auth_credentials WHERE user_id=?1",
            user_id
        ))
        .await?;
        Ok(())
    }

    /// Every account, passkey, live credential, and live invitation, read in
    /// one snapshot. Secrets and their hashes are never included.
    pub(crate) async fn auth_directory(&self) -> Result<Directory, StoreError> {
        let now = now_secs();
        let idle = now - SESSION_IDLE_SECS;
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        let users = sqlx::query_as!(
            User,
            r#"SELECT id AS "id!",username,display_name FROM auth_users ORDER BY username"#
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let passkeys = sqlx::query_as!(
            PasskeyView,
            r#"SELECT id AS "id!",user_id,name FROM auth_passkeys ORDER BY created_at"#
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let credentials = sqlx::query_as!(
            CredentialView,
            r#"SELECT id AS "id!",user_id,kind,name,last_used_at,expires_at FROM auth_credentials WHERE (expires_at IS NULL OR expires_at>?1) AND (kind!='browser' OR last_used_at>?2) ORDER BY created_at"#,
            now,
            idle
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let invitations = sqlx::query_as!(
            InvitationView,
            r#"SELECT id AS "id!",issuer_id,expires_at FROM auth_invitations WHERE expires_at>?1 ORDER BY expires_at"#,
            now
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(Directory {
            users,
            passkeys,
            credentials,
            invitations,
        })
    }

    pub(crate) async fn update_user(
        &self,
        id: &str,
        username: &str,
        display_name: &str,
    ) -> Result<(), StoreError> {
        self.write_one(sqlx::query!(
            "UPDATE auth_users SET username=?1,display_name=?2 WHERE id=?3",
            username,
            display_name,
            id
        ))
        .await?;
        Ok(())
    }

    /// Deletes an account with its passkeys, credentials, and invitations.
    pub(crate) async fn delete_user(&self, id: &str) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let accounts = sqlx::query_scalar!(r#"SELECT COUNT(*) AS "count!: i64" FROM auth_users"#)
            .fetch_one(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        if accounts <= 1 {
            return Err(Lockout::LastAccount.into());
        }
        sqlx::query!("DELETE FROM auth_users WHERE id=?1", id)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        Self::require_a_passkey(&mut tx).await?;
        tx.commit().await.map_err(StoreError::database)
    }

    pub(crate) async fn remove_passkey(&self, id: &str) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        sqlx::query!("DELETE FROM auth_passkeys WHERE id=?1", id)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        Self::require_a_passkey(&mut tx).await?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// Refuses a pending removal that would leave no passkey. The caller holds
    /// the writer lock, so concurrent removals cannot both pass this check.
    async fn require_a_passkey(db: &mut SqliteConnection) -> Result<(), StoreError> {
        let exists =
            sqlx::query_scalar!(r#"SELECT EXISTS(SELECT 1 FROM auth_passkeys) AS "exists!: bool""#)
                .fetch_one(db)
                .await
                .map_err(StoreError::database)?;
        if exists {
            Ok(())
        } else {
            Err(Lockout::LastPasskey.into())
        }
    }

    pub(crate) async fn rename_passkey(&self, id: &str, name: &str) -> Result<(), StoreError> {
        self.write_one(sqlx::query!(
            "UPDATE auth_passkeys SET name=?1 WHERE id=?2",
            name,
            id
        ))
        .await?;
        Ok(())
    }

    pub(crate) async fn create_invitation(
        &self,
        id: &str,
        issuer_id: &str,
        secret_hash: &str,
        expires_at: i64,
    ) -> Result<(), StoreError> {
        self.write_one(sqlx::query!(
            "INSERT INTO auth_invitations(id,issuer_id,secret_hash,expires_at) VALUES(?1,?2,?3,?4)",
            id,
            issuer_id,
            secret_hash,
            expires_at
        ))
        .await?;
        Ok(())
    }

    pub(crate) async fn revoke_invitation(&self, id: &str) -> Result<(), StoreError> {
        self.write_one(sqlx::query!("DELETE FROM auth_invitations WHERE id=?1", id))
            .await?;
        Ok(())
    }
}

/// Fixtures for authentication tests that need states no API produces.
#[cfg(test)]
impl Store {
    /// Claims the installation for an account that has no passkey.
    pub(crate) async fn seed_auth_user(&self, user: &User) {
        let (_writer, mut tx) = self.begin_immediate().await.unwrap();
        let now = now_secs();
        sqlx::query!(
            "INSERT INTO auth_users(id,username,display_name,created_at) VALUES(?1,?2,?3,?4)",
            user.id,
            user.username,
            user.display_name,
            now
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query!("UPDATE auth_setup SET initialized=1,secret_hash=NULL")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    /// Makes every credential look unused since `secs` ago.
    pub(crate) async fn age_auth_credentials(&self, secs: i64) {
        let last_used = now_secs() - secs;
        self.write_one(sqlx::query!(
            "UPDATE auth_credentials SET last_used_at=?1",
            last_used
        ))
        .await
        .unwrap();
    }

    /// Empties the account table behind the daemon's back.
    pub(crate) async fn clear_auth_users(&self) {
        self.write_one(sqlx::query!("DELETE FROM auth_users"))
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Authentication reads never wait for writers; refreshes queue behind
    /// them without holding a pool connection, and a revocation that lands
    /// first wins.
    #[tokio::test]
    async fn credential_reads_skip_the_writer_queue_and_refreshes_respect_revocation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("state.db")).await.unwrap();
        let user = User {
            id: "alice".into(),
            username: "alice".into(),
            display_name: String::new(),
        };
        store.seed_auth_user(&user).await;
        let credential = NewCredential {
            id: "credential".into(),
            secret_hash: "hash".into(),
            kind: CredentialKind::Browser,
            name: "Browser",
            expires_at: Some(now_secs() + SESSION_IDLE_SECS),
        };
        store
            .insert_credential(&user.id, &credential)
            .await
            .unwrap();

        let (writer, tx) = store.begin_immediate().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), store.credential_owner("hash"))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let refresh = store.touch_credential("credential");
        tokio::pin!(refresh);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut refresh)
                .await
                .is_err()
        );
        tx.rollback().await.unwrap();
        // The queued refresh holds no pool connection while it waits.
        let mut connections = Vec::new();
        for _ in 0..8 {
            connections.push(
                tokio::time::timeout(Duration::from_secs(1), store.pool.acquire())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        // Revoke while the refresh is still queued behind the writer.
        sqlx::query!("DELETE FROM auth_credentials")
            .execute(&mut *connections[0])
            .await
            .unwrap();
        drop(connections);
        drop(writer);
        assert!(!refresh.await.unwrap());
        assert!(store.credential_owner("hash").await.unwrap().is_none());
    }
}
