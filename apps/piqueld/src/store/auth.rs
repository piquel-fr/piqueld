//! Accounts, passkeys, revocable credentials, and invitations.
//!
//! Timestamps are Unix seconds. Callers pass only hashes of credential and
//! invitation secrets, so this module never sees a usable secret. Writes queue
//! behind the shared writer, and account changes refuse to lock everyone out.
//! Changes to an account take the [`Caller`], whose authority over the account
//! is checked against both sides' current grants inside the same transaction.
use super::access::{Caller, Holder};
use super::{Store, StoreError, now_secs};
use piqueld_core::access::{GlobalPermission, Grants, Permission};
use piqueld_core::auth::{Account, CredentialView, Directory, InvitationView, PasskeyView, User};
use sqlx::{Sqlite, SqliteConnection, query::Query, sqlite::SqliteArguments};
use std::fmt;

/// Browser sessions end after a day without use, even before they expire.
pub(super) const SESSION_IDLE_SECS: i64 = 86_400;

/// Account changes refused because nobody could administer the installation
/// afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lockout {
    /// No account would hold `admin` on every application together with a
    /// passkey to sign in with.
    LastAdmin,
}

impl Lockout {
    /// Human-readable reason, suitable for API responses.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::LastAdmin => {
                "at least one account must keep admin on every application and a passkey"
            }
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
    /// Dashboard session cookie.
    Browser,
    /// Session approved for the command-line client.
    Cli,
    /// Named API token.
    Token,
}

impl CredentialKind {
    /// Value stored in the `kind` column.
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
    /// Public identifier, used to revoke the credential.
    pub(crate) id: String,
    /// Hash of the bearer secret; the secret itself is never stored.
    pub(crate) secret_hash: String,
    /// Credential class, deciding idle-timeout behavior.
    pub(crate) kind: CredentialKind,
    /// Human-readable label shown in the account directory.
    pub(crate) name: &'a str,
    /// Absolute expiry in Unix seconds; `None` never expires.
    pub(crate) expires_at: Option<i64>,
}

impl NewCredential<'_> {
    /// Inserts the credential for `user_id`, marking it used now. A duplicate ID
    /// or hash maps to `AlreadyExists`, and a missing account to `NotFound`.
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
    /// `WebAuthn` credential ID.
    pub(crate) id: &'a str,
    /// Human-readable label.
    pub(crate) name: &'a str,
    /// Serialized passkey state, updated after each sign-in.
    pub(crate) credential: &'a str,
}

/// Who a newly registered passkey belongs to.
pub(crate) enum PasskeyOwner<'a> {
    /// A signed-in caller adds another passkey to its own account, named by
    /// ID; refused unless the caller's credential still belongs to it.
    Existing(Caller<'a>, &'a str),
    /// Registration redeems the setup secret or an invitation, and signs the
    /// account in with `session`. Account invitations create `user` with the
    /// invitation's grants; the setup secret creates it as administrator.
    /// Enrollment invitations add the passkey to their existing account
    /// instead, ignoring `user`.
    Redeem {
        /// Account to create, for setup and account invitations.
        user: &'a User,
        /// Hash of the setup secret or invitation secret being redeemed.
        invitation_hash: &'a str,
        /// Browser session opened for the account.
        session: NewCredential<'a>,
    },
}

/// What an unused setup secret or invitation will do once redeemed.
pub(crate) enum Invitation {
    /// Creates a new account: the first administrator for the setup secret,
    /// otherwise with the invitation's grants.
    Account,
    /// Adds a passkey to this existing account.
    Enrollment(User),
}

/// An invitation to store, by the hash of its secret. The caller creating it
/// becomes its issuer.
pub(crate) struct NewInvitation {
    /// Revocation identifier.
    pub(crate) id: String,
    /// Hash of the secret in the shared link.
    pub(crate) secret_hash: String,
    /// Absolute expiry in Unix seconds.
    pub(crate) expires_at: i64,
}

/// The account behind a live credential.
pub(crate) struct CredentialOwner {
    /// Matched credential, used to record its use or revoke it.
    pub(crate) credential_id: String,
    /// Account that owns the credential.
    pub(crate) user: User,
    /// The account's current grants.
    pub(crate) grants: Grants,
    /// Last recorded use in Unix seconds, letting callers skip frequent refreshes.
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

    /// Describes the open setup secret or live invitation matching a hash, or
    /// `None` when there is none.
    pub(crate) async fn invitation(&self, hash: &str) -> Result<Option<Invitation>, StoreError> {
        let now = now_secs();
        let setup = sqlx::query_scalar!(
            r#"SELECT EXISTS(SELECT 1 FROM auth_setup WHERE initialized=0 AND secret_hash=?1) AS "open!: bool""#,
            hash
        )
        .fetch_one(&self.pool)
        .await
        .map_err(StoreError::database)?;
        if setup {
            return Ok(Some(Invitation::Account));
        }
        let Some(row) = sqlx::query!(
            r#"SELECT i.user_id,u.username AS "username?",u.display_name AS "display_name?" FROM auth_invitations i LEFT JOIN auth_users u ON u.id=i.user_id WHERE i.secret_hash=?1 AND i.expires_at>?2"#,
            hash,
            now
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        else {
            return Ok(None);
        };
        Ok(Some(match (row.user_id, row.username, row.display_name) {
            (Some(id), Some(username), Some(display_name)) => Invitation::Enrollment(User {
                id,
                username,
                display_name,
            }),
            (None, ..) => Invitation::Account,
            _ => return Err(StoreError::Corrupt),
        }))
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

    /// Stores a registered passkey. Redemption atomically consumes the setup
    /// secret or invitation, creates or selects the account, and opens its
    /// session. Returns `false` when the secret was already used or has
    /// expired, when an enrollment invitation targets another account, or when
    /// a signed-in caller adds a passkey to an account that is not its own.
    pub(crate) async fn add_passkey(
        &self,
        owner: PasskeyOwner<'_>,
        passkey: NewPasskey<'_>,
    ) -> Result<bool, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let now = now_secs();
        let user_id = match &owner {
            PasskeyOwner::Existing(caller, user_id) => {
                if caller.load(&mut tx).await?.0 != *user_id {
                    return Ok(false);
                }
                *user_id
            }
            PasskeyOwner::Redeem {
                user,
                invitation_hash,
                session,
            } => {
                if !Self::redeem_on(&mut tx, user, invitation_hash, now).await? {
                    return Ok(false);
                }
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

    /// Consumes the setup secret or a live invitation for `user`:
    ///
    /// 1. The setup secret closes initial setup and creates `user` with `admin`
    ///    on every application.
    /// 2. An account invitation creates `user` with the invitation's grants.
    /// 3. An enrollment invitation must target `user`, which already exists.
    ///
    /// Invitations carry their issuer's authority, so they are only redeemed
    /// while the issuer could still create them: grants may have changed on
    /// either side since. Returns `false` when nothing matching is left to
    /// redeem.
    async fn redeem_on(
        db: &mut SqliteConnection,
        user: &User,
        invitation_hash: &str,
        now: i64,
    ) -> Result<bool, StoreError> {
        let setup = sqlx::query!(
            "UPDATE auth_setup SET initialized=1,secret_hash=NULL WHERE initialized=0 AND secret_hash=?1",
            invitation_hash
        )
        .execute(&mut *db)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if setup == 1 {
            Self::insert_user_on(db, user, now).await?;
            Holder::User(&user.id).replace(db, &Grants::admin()).await?;
            return Ok(true);
        }
        let Some(invitation) = sqlx::query!(
            r#"SELECT id AS "id!",issuer_id,user_id FROM auth_invitations WHERE secret_hash=?1 AND expires_at>?2"#,
            invitation_hash,
            now
        )
        .fetch_optional(&mut *db)
        .await
        .map_err(StoreError::database)?
        else {
            return Ok(false);
        };
        let authority = Holder::User(&invitation.issuer_id).grants(&mut *db).await?;
        if let Some(target) = invitation.user_id {
            let grants = Holder::User(&target).grants(&mut *db).await?;
            let own = invitation.issuer_id == target;
            if target != user.id || authority.may_change_account(own, &grants).is_err() {
                return Ok(false);
            }
        } else {
            let grants = Holder::Invitation(&invitation.id).grants(&mut *db).await?;
            if Self::may_invite(&authority, &grants).is_err() {
                return Ok(false);
            }
            Self::insert_user_on(db, user, now).await?;
            Holder::User(&user.id).replace(db, &grants).await?;
        }
        sqlx::query!("DELETE FROM auth_invitations WHERE id=?1", invitation.id)
            .execute(&mut *db)
            .await
            .map_err(StoreError::database)?;
        Ok(true)
    }

    /// Creates an account; a taken username maps to `AlreadyExists`.
    async fn insert_user_on(
        db: &mut SqliteConnection,
        user: &User,
        now: i64,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO auth_users(id,username,display_name,created_at) VALUES(?1,?2,?3,?4)",
            user.id,
            user.username,
            user.display_name,
            now
        )
        .execute(db)
        .await
        .map_err(StoreError::constraint)?;
        Ok(())
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

    /// Finds the account for a live credential by its secret hash, with the
    /// account's grants read from the same snapshot. This is read-only, so
    /// authenticating never waits for writers.
    pub(crate) async fn credential_owner(
        &self,
        hash: &str,
    ) -> Result<Option<CredentialOwner>, StoreError> {
        let now = now_secs();
        let idle = now - SESSION_IDLE_SECS;
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        let row = sqlx::query!(
            r#"SELECT c.id AS "credential_id!",c.last_used_at,u.id AS "id!",u.username,u.display_name FROM auth_credentials c JOIN auth_users u ON u.id=c.user_id WHERE c.secret_hash=?1 AND (c.expires_at IS NULL OR c.expires_at>?2) AND (c.kind!='browser' OR c.last_used_at>?3)"#,
            hash,
            now,
            idle
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let grants = Holder::User(&row.id).grants(&mut tx).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(Some(CredentialOwner {
            credential_id: row.credential_id,
            user: User {
                id: row.id,
                username: row.username,
                display_name: row.display_name,
            },
            grants,
            last_used_at: row.last_used_at,
        }))
    }

    /// Records use of a live credential. It queues behind other writers and
    /// judges liveness once it holds the writer lock, so a revocation or expiry
    /// that lands while it waits wins. Returns `false` when the credential was
    /// revoked or expired in the meantime.
    pub(crate) async fn touch_credential(&self, id: &str) -> Result<bool, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let now = now_secs();
        let idle = now - SESSION_IDLE_SECS;
        let rows = sqlx::query!(
            "UPDATE auth_credentials SET last_used_at=MAX(last_used_at,?1) WHERE id=?2 AND (expires_at IS NULL OR expires_at>?1) AND (kind!='browser' OR last_used_at>?3)",
            now,
            id,
            idle
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        tx.commit().await.map_err(StoreError::database)?;
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

    /// Deletes one credential if `caller` may change its owner; unknown IDs are
    /// ignored.
    pub(crate) async fn revoke_credential(
        &self,
        caller: Caller<'_>,
        id: &str,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let owner = sqlx::query_scalar!("SELECT user_id FROM auth_credentials WHERE id=?1", id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        if let Some(owner) = owner {
            Self::check_account_on(&mut tx, caller, &owner).await?;
            sqlx::query!("DELETE FROM auth_credentials WHERE id=?1", id)
                .execute(&mut *tx)
                .await
                .map_err(StoreError::database)?;
        }
        tx.commit().await.map_err(StoreError::database)
    }

    /// Deletes every credential of an account, signing it out everywhere.
    pub(crate) async fn revoke_credentials(
        &self,
        caller: Caller<'_>,
        user_id: &str,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        Self::check_account_on(&mut tx, caller, user_id).await?;
        sqlx::query!("DELETE FROM auth_credentials WHERE user_id=?1", user_id)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// Every account with its grants, passkey, live credential, and live
    /// invitation, read in one snapshot. Secrets and their hashes are never
    /// included.
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
        let mut accounts = Vec::with_capacity(users.len());
        for user in users {
            let grants = Holder::User(&user.id).grants(&mut tx).await?;
            accounts.push(Account { user, grants });
        }
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
        let rows = sqlx::query!(
            r#"SELECT id AS "id!",issuer_id,user_id,expires_at FROM auth_invitations WHERE expires_at>?1 ORDER BY expires_at"#,
            now
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let mut invitations = Vec::with_capacity(rows.len());
        for row in rows {
            let grants = Holder::Invitation(&row.id).grants(&mut tx).await?;
            invitations.push(InvitationView {
                id: row.id,
                issuer_id: row.issuer_id,
                user_id: row.user_id,
                expires_at: row.expires_at,
                grants,
            });
        }
        tx.commit().await.map_err(StoreError::database)?;
        Ok(Directory {
            users: accounts,
            passkeys,
            credentials,
            invitations,
        })
    }

    /// Changes an account's username and display name. A taken username maps to
    /// `AlreadyExists`.
    pub(crate) async fn update_user(
        &self,
        caller: Caller<'_>,
        id: &str,
        username: &str,
        display_name: &str,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        Self::check_account_on(&mut tx, caller, id).await?;
        sqlx::query!(
            "UPDATE auth_users SET username=?1,display_name=?2 WHERE id=?3",
            username,
            display_name,
            id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::constraint)?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// Deletes an account with its passkeys, credentials, grants, and
    /// invitations. Refuses to leave no administrator able to sign in.
    pub(crate) async fn delete_user(&self, caller: Caller<'_>, id: &str) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        Self::check_account_on(&mut tx, caller, id).await?;
        sqlx::query!("DELETE FROM auth_users WHERE id=?1", id)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        Self::require_an_admin(&mut tx).await?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// Checks that `caller` may change the account owning a passkey. Unknown
    /// passkeys are `NotFound`.
    async fn check_passkey_owner_on(
        db: &mut SqliteConnection,
        caller: Caller<'_>,
        id: &str,
    ) -> Result<(), StoreError> {
        let owner = sqlx::query_scalar!("SELECT user_id FROM auth_passkeys WHERE id=?1", id)
            .fetch_optional(&mut *db)
            .await
            .map_err(StoreError::database)?
            .ok_or(StoreError::NotFound)?;
        Self::check_account_on(db, caller, &owner).await?;
        Ok(())
    }

    /// Deletes one passkey, refusing to leave no administrator able to sign in.
    pub(crate) async fn remove_passkey(
        &self,
        caller: Caller<'_>,
        id: &str,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        Self::check_passkey_owner_on(&mut tx, caller, id).await?;
        sqlx::query!("DELETE FROM auth_passkeys WHERE id=?1", id)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        Self::require_an_admin(&mut tx).await?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// Changes a passkey's label.
    pub(crate) async fn rename_passkey(
        &self,
        caller: Caller<'_>,
        id: &str,
        name: &str,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        Self::check_passkey_owner_on(&mut tx, caller, id).await?;
        sqlx::query!("UPDATE auth_passkeys SET name=?1 WHERE id=?2", name, id)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// Stores an account invitation that gives the created account `grants`,
    /// if `caller` may manage accounts and holds every grant. `add_passkey`
    /// consumes it.
    pub(crate) async fn create_invitation(
        &self,
        caller: Caller<'_>,
        invitation: &NewInvitation,
        grants: &Grants,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let (issuer, authority) = caller.load(&mut tx).await?;
        Self::may_invite(&authority, grants)?;
        Self::insert_invitation_on(&mut tx, invitation, &issuer, None).await?;
        Holder::Invitation(&invitation.id)
            .replace(&mut tx, grants)
            .await?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// Requires `accounts:manage` and every grant an invitation hands out.
    fn may_invite(authority: &Grants, grants: &Grants) -> Result<(), StoreError> {
        authority
            .require(Permission::Global(GlobalPermission::AccountsManage))
            .and_then(|()| authority.may_grant(grants))
            .map_err(StoreError::Denied)
    }

    /// Stores an enrollment invitation that adds a passkey to `user_id`, if
    /// `caller` may change that account.
    pub(crate) async fn create_enrollment(
        &self,
        caller: Caller<'_>,
        invitation: &NewInvitation,
        user_id: &str,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        Self::check_account_on(&mut tx, caller, user_id).await?;
        let (issuer, _) = caller.load(&mut tx).await?;
        Self::insert_invitation_on(&mut tx, invitation, &issuer, Some(user_id)).await?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// Inserts an invitation row issued by `issuer`, targeting `user_id` for
    /// enrollment.
    async fn insert_invitation_on(
        db: &mut SqliteConnection,
        invitation: &NewInvitation,
        issuer: &str,
        user_id: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO auth_invitations(id,issuer_id,secret_hash,expires_at,user_id) VALUES(?1,?2,?3,?4,?5)",
            invitation.id,
            issuer,
            invitation.secret_hash,
            invitation.expires_at,
            user_id
        )
        .execute(db)
        .await
        .map_err(StoreError::constraint)?;
        Ok(())
    }

    /// Deletes an unused invitation if `caller` could have created it: it may
    /// change the enrollment target, or hand out the invitation's grants.
    pub(crate) async fn revoke_invitation(
        &self,
        caller: Caller<'_>,
        id: &str,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let target = sqlx::query_scalar!("SELECT user_id FROM auth_invitations WHERE id=?1", id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        match target {
            None => return Ok(()),
            Some(Some(user_id)) => {
                Self::check_account_on(&mut tx, caller, &user_id).await?;
            }
            Some(None) => {
                let (_, authority) = caller.load(&mut tx).await?;
                let grants = Holder::Invitation(id).grants(&mut tx).await?;
                Self::may_invite(&authority, &grants)?;
            }
        }
        sqlx::query!("DELETE FROM auth_invitations WHERE id=?1", id)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// Stores an API token for the caller's own account.
    pub(crate) async fn create_token(
        &self,
        caller: Caller<'_>,
        credential: &NewCredential<'_>,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let (user_id, _) = caller.load(&mut tx).await?;
        credential.insert(&mut tx, &user_id).await?;
        tx.commit().await.map_err(StoreError::database)
    }
}

/// Fixtures for authentication tests that need states no API produces.
#[cfg(test)]
impl Store {
    /// Claims the installation for an account with `grants` and no passkey.
    pub(crate) async fn seed_auth_user(&self, user: &User, grants: &Grants) {
        let (_writer, mut tx) = self.begin_immediate().await.unwrap();
        Self::insert_user_on(&mut tx, user, now_secs())
            .await
            .unwrap();
        Holder::User(&user.id)
            .replace(&mut tx, grants)
            .await
            .unwrap();
        sqlx::query!("UPDATE auth_setup SET initialized=1,secret_hash=NULL")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
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
        store.seed_auth_user(&user, &Grants::admin()).await;
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

    /// A refresh judges idle expiry when it gets the writer lock, so a session
    /// that expires while the refresh waits is not revived.
    #[tokio::test]
    async fn refreshes_waiting_for_writers_respect_idle_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("state.db")).await.unwrap();
        let user = User {
            id: "alice".into(),
            username: "alice".into(),
            display_name: String::new(),
        };
        store.seed_auth_user(&user, &Grants::admin()).await;
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
        // One second of idle time remains; the refresh waits two.
        store.age_auth_credentials(SESSION_IDLE_SECS - 1).await;
        let (writer, tx) = store.begin_immediate().await.unwrap();
        let refresh = tokio::spawn({
            let store = store.clone();
            async move { store.touch_credential("credential").await }
        });
        tokio::time::sleep(Duration::from_secs(2)).await;
        tx.rollback().await.unwrap();
        drop(writer);
        assert!(!refresh.await.unwrap().unwrap());
    }
}
