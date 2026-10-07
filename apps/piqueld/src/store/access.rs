//! Grant storage. Each `auth_grants` row grants one permission on one
//! application, or on every application when `application_id` is NULL, to
//! exactly one account, credential, or invitation.
use super::{Store, StoreError, now_secs};
use piqueld_core::{
    ApplicationId, EnvironmentId,
    access::{AppPermission, Grants, Permission, Scope, Target},
};
use sqlx::SqliteConnection;

/// Who a set of grants belongs to.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Holder<'a> {
    /// An account's own access.
    User(&'a str),
    /// The access an invitation gives the account it creates.
    Invitation(&'a str),
}

/// Who submits a write. Account callers are re-read and checked inside the
/// writing transaction, so a concurrent demotion or revocation always wins.
#[derive(Clone, Copy, Debug)]
pub enum Actor<'a> {
    /// The daemon itself or a trusted embedding, e.g. tests; never restricted.
    Daemon,
    /// A signed-in caller.
    Account(Caller<'a>),
}

impl Actor<'_> {
    /// Re-reads an account caller's ID and grants; `None` for the daemon.
    pub(crate) async fn load(
        self,
        db: &mut SqliteConnection,
    ) -> Result<Option<(String, Grants)>, StoreError> {
        match self {
            Self::Daemon => Ok(None),
            Self::Account(caller) => caller.load(db).await.map(Some),
        }
    }

    /// Checks `permission` against the caller's current grants, on
    /// `application` for application permissions.
    pub(crate) async fn require_on(
        self,
        db: &mut SqliteConnection,
        permission: Permission,
        application: Option<&ApplicationId>,
    ) -> Result<(), StoreError> {
        let Some((_, grants)) = self.load(db).await? else {
            return Ok(());
        };
        match (permission, application) {
            (Permission::App(permission), Some(id)) => grants.require_app(permission, id),
            (permission, _) => grants.require(permission),
        }
        .map_err(StoreError::Denied)
    }

    /// Checks an application permission on the application owning
    /// `environment`. An unknown environment is hidden unless the caller holds
    /// the permission on every application.
    pub(crate) async fn require_on_environment(
        self,
        db: &mut SqliteConnection,
        permission: AppPermission,
        environment: &EnvironmentId,
    ) -> Result<(), StoreError> {
        let Some((_, grants)) = self.load(db).await? else {
            return Ok(());
        };
        let application = Store::environment_application_on(db, environment).await?;
        let target = application.as_ref().map_or(Target::Unknown, Target::Id);
        grants
            .require_change(&[permission], target)
            .map_err(StoreError::Denied)
    }
}

/// The history a caller may read: application events within `applications`,
/// and daemon events when `daemon` is set.
#[derive(Clone, Debug)]
pub struct Visibility {
    /// Applications whose events are visible.
    pub applications: Scope,
    /// Whether daemon-scoped events are visible.
    pub daemon: bool,
}

impl Visibility {
    /// Every event, for the daemon itself.
    pub const ALL: Self = Self {
        applications: Scope::All,
        daemon: true,
    };
}

/// Binds a scope as a JSON array of application IDs for
/// `(?1 IS NULL OR application_id IN (SELECT value FROM json_each(?1)))`;
/// `None` covers every application.
pub(crate) fn scope_json(scope: &Scope) -> Option<String> {
    scope.applications().map(|ids| {
        serde_json::to_string(&ids.iter().map(ApplicationId::as_str).collect::<Vec<_>>())
            .expect("application IDs serialize")
    })
}

/// The signed-in caller of a change, by the credential it authenticated with.
/// Its account and grants are re-read inside the changing transaction, so a
/// concurrent revocation or demotion always takes effect first.
#[derive(Clone, Copy, Debug)]
pub struct Caller<'a> {
    /// Credential that authenticated the request.
    pub credential_id: &'a str,
}

impl Caller<'_> {
    /// Reads the caller's account ID and current grants. A credential revoked
    /// or expired since authentication is `CredentialRevoked`.
    pub(crate) async fn load(
        self,
        db: &mut SqliteConnection,
    ) -> Result<(String, Grants), StoreError> {
        let now = now_secs();
        let idle = now - super::auth::SESSION_IDLE_SECS;
        let user_id = sqlx::query_scalar!(
            "SELECT user_id FROM auth_credentials WHERE id=?1 AND (expires_at IS NULL OR expires_at>?2) AND (kind!='browser' OR last_used_at>?3)",
            self.credential_id,
            now,
            idle
        )
        .fetch_optional(&mut *db)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::CredentialRevoked)?;
        let grants = Holder::User(&user_id).grants(db).await?;
        Ok((user_id, grants))
    }
}

impl Holder<'_> {
    /// The `(user_id, credential_id, invitation_id)` values selecting this
    /// holder: its ID in its own column, `NULL` in the others.
    fn ids(self) -> (Option<String>, Option<String>, Option<String>) {
        match self {
            Self::User(id) => (Some(id.to_owned()), None, None),
            Self::Invitation(id) => (None, None, Some(id.to_owned())),
        }
    }

    /// Reads the holder's grants.
    pub(crate) async fn grants(self, db: &mut SqliteConnection) -> Result<Grants, StoreError> {
        let (user, credential, invitation) = self.ids();
        let rows = sqlx::query!(
            "SELECT permission,application_id FROM auth_grants WHERE user_id IS ?1 AND credential_id IS ?2 AND invitation_id IS ?3",
            user,
            credential,
            invitation
        )
        .fetch_all(db)
        .await
        .map_err(StoreError::database)?;
        let mut grants = Grants::default();
        for row in rows {
            let permission = Permission::parse(&row.permission).ok_or(StoreError::Corrupt)?;
            let scope = match row.application_id {
                None => Scope::All,
                Some(id) => Scope::one(ApplicationId::parse(id).map_err(StoreError::corrupt)?),
            };
            grants
                .grant(permission, &scope)
                .map_err(StoreError::corrupt)?;
        }
        Ok(grants)
    }

    /// Replaces the holder's grants. A grant on an unknown application fails
    /// with `NotFound`.
    pub(crate) async fn replace(
        self,
        db: &mut SqliteConnection,
        grants: &Grants,
    ) -> Result<(), StoreError> {
        let (user, credential, invitation) = self.ids();
        sqlx::query!(
            "DELETE FROM auth_grants WHERE user_id IS ?1 AND credential_id IS ?2 AND invitation_id IS ?3",
            user,
            credential,
            invitation
        )
        .execute(&mut *db)
        .await
        .map_err(StoreError::database)?;
        for grant in grants.to_list() {
            let permission = grant.permission.as_str();
            let applications = grant
                .applications
                .map_or_else(|| vec![None], |ids| ids.into_iter().map(Some).collect());
            for application in applications {
                let application = application.as_ref().map(ApplicationId::as_str);
                sqlx::query!(
                    "INSERT INTO auth_grants(user_id,credential_id,invitation_id,permission,application_id) VALUES(?1,?2,?3,?4,?5)",
                    user,
                    credential,
                    invitation,
                    permission,
                    application
                )
                .execute(&mut *db)
                .await
                .map_err(StoreError::constraint)?;
            }
        }
        Ok(())
    }
}

impl Holder<'_> {
    /// Adds `grants` to the holder's existing grants.
    pub(crate) async fn extend(
        self,
        db: &mut SqliteConnection,
        grants: &Grants,
    ) -> Result<(), StoreError> {
        let mut current = self.grants(&mut *db).await?;
        current.extend(grants);
        self.replace(db, &current).await
    }
}

impl Store {
    /// Checks `actor`'s current grants for `permission` on the application
    /// owning `environment`, for requests that write nothing themselves, like
    /// starting a command long after its connection was authorized.
    ///
    /// # Errors
    /// Returns a refusal, a revoked credential, or storage errors.
    pub async fn require_on_environment(
        &self,
        actor: Actor<'_>,
        permission: AppPermission,
        environment: &EnvironmentId,
    ) -> Result<(), StoreError> {
        let mut db = self.pool.acquire().await.map_err(StoreError::database)?;
        actor
            .require_on_environment(&mut db, permission, environment)
            .await
    }

    /// Replaces an account's grants, if `caller` may change that account and
    /// holds every new grant. Refuses to leave no administrator able to sign in.
    pub(crate) async fn set_user_grants(
        &self,
        caller: Caller<'_>,
        user_id: &str,
        grants: &Grants,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let (_, caller) = Self::check_account_on(&mut tx, caller, user_id).await?;
        caller.may_grant(grants).map_err(StoreError::Denied)?;
        Holder::User(user_id).replace(&mut tx, grants).await?;
        Self::require_an_admin(&mut tx).await?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// Checks that `caller` may change the account `user_id`, against both
    /// sides' current grants, and returns the caller's ID and grants. Unknown
    /// accounts are checked as holding nothing, then reported `NotFound`, so
    /// only callers allowed to manage accounts learn whether one exists.
    pub(crate) async fn check_account_on(
        db: &mut SqliteConnection,
        caller: Caller<'_>,
        user_id: &str,
    ) -> Result<(String, Grants), StoreError> {
        let (caller_id, caller) = caller.load(&mut *db).await?;
        let exists = sqlx::query_scalar!(
            r#"SELECT EXISTS(SELECT 1 FROM auth_users WHERE id=?1) AS "exists!: bool""#,
            user_id
        )
        .fetch_one(&mut *db)
        .await
        .map_err(StoreError::database)?;
        let target = Holder::User(user_id).grants(db).await?;
        caller
            .may_change_account(caller_id == user_id, &target)
            .map_err(StoreError::Denied)?;
        if exists {
            Ok((caller_id, caller))
        } else {
            Err(StoreError::NotFound)
        }
    }

    /// Refuses a pending change that would leave no account holding `admin` on
    /// every application together with a passkey to sign in with. The caller
    /// holds the writer lock, so concurrent changes cannot both pass.
    pub(crate) async fn require_an_admin(db: &mut SqliteConnection) -> Result<(), StoreError> {
        let exists = sqlx::query_scalar!(
            r#"SELECT EXISTS(SELECT 1 FROM auth_grants g JOIN auth_passkeys p ON p.user_id=g.user_id WHERE g.permission='admin' AND g.application_id IS NULL) AS "exists!: bool""#
        )
        .fetch_one(db)
        .await
        .map_err(StoreError::database)?;
        if exists {
            Ok(())
        } else {
            Err(super::Lockout::LastAdmin.into())
        }
    }
}
