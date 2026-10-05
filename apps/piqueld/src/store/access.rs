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
    /// The grants a scoped credential is limited to.
    Credential(&'a str),
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
    ) -> Result<Option<Authority>, StoreError> {
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
        let Some(authority) = self.load(db).await? else {
            return Ok(());
        };
        match (permission, application) {
            (Permission::App(permission), Some(id)) => authority.grants.require_app(permission, id),
            (permission, _) => authority.grants.require(permission),
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
        let Some(authority) = self.load(db).await? else {
            return Ok(());
        };
        let application = Store::environment_application_on(db, environment).await?;
        let target = application.as_ref().map_or(Target::Unknown, Target::Id);
        authority
            .grants
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
    /// Reads what the caller may do now. A credential revoked or expired
    /// since authentication is `CredentialRevoked`.
    pub(crate) async fn load(self, db: &mut SqliteConnection) -> Result<Authority, StoreError> {
        let now = now_secs();
        let idle = now - super::auth::SESSION_IDLE_SECS;
        let row = sqlx::query!(
            r#"SELECT user_id,scoped AS "scoped: bool" FROM auth_credentials WHERE id=?1 AND (expires_at IS NULL OR expires_at>?2) AND (kind!='browser' OR last_used_at>?3)"#,
            self.credential_id,
            now,
            idle
        )
        .fetch_optional(&mut *db)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::CredentialRevoked)?;
        Authority::read(db, row.user_id, self.credential_id, row.scoped).await
    }
}

/// What a credential may do: its account's grants, limited by the
/// credential's own grants when it is scoped, like an API token.
#[derive(Clone, Debug)]
pub(crate) struct Authority {
    /// Account the credential belongs to.
    pub(crate) user_id: String,
    /// Effective grants.
    pub(crate) grants: Grants,
    /// Whether the credential is limited to its own grants.
    pub(crate) scoped: bool,
}

impl Authority {
    /// Reads the effective grants of `credential_id`, belonging to `user_id`.
    pub(crate) async fn read(
        db: &mut SqliteConnection,
        user_id: String,
        credential_id: &str,
        scoped: bool,
    ) -> Result<Self, StoreError> {
        let mut grants = Holder::User(&user_id).grants(&mut *db).await?;
        if scoped {
            grants = grants.intersection(&Holder::Credential(credential_id).grants(db).await?);
        }
        Ok(Self {
            user_id,
            grants,
            scoped,
        })
    }

    /// Requires a credential with the account's full access: scoped
    /// credentials cannot create credentials, so they cannot outlive or
    /// exceed their own limits.
    pub(crate) fn require_unscoped(&self) -> Result<(), StoreError> {
        if self.scoped {
            Err(StoreError::Denied(piqueld_core::access::Denied::Scoped))
        } else {
            Ok(())
        }
    }
}

impl Holder<'_> {
    /// The `(user_id, credential_id, invitation_id)` values selecting this
    /// holder: its ID in its own column, `NULL` in the others.
    fn ids(self) -> (Option<String>, Option<String>, Option<String>) {
        match self {
            Self::User(id) => (Some(id.to_owned()), None, None),
            Self::Credential(id) => (None, Some(id.to_owned()), None),
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
    /// holds every new grant. Access handed to another account outlives the
    /// credential handing it out, so scoped callers may only change their own
    /// account, which can only reduce it. Refuses to leave no administrator
    /// able to sign in.
    pub(crate) async fn set_user_grants(
        &self,
        caller: Caller<'_>,
        user_id: &str,
        grants: &Grants,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let caller = Self::check_account_on(&mut tx, caller, user_id).await?;
        if caller.user_id != user_id {
            caller.require_unscoped()?;
        }
        caller
            .grants
            .may_grant(grants)
            .map_err(StoreError::Denied)?;
        Holder::User(user_id).replace(&mut tx, grants).await?;
        Self::require_an_admin(&mut tx).await?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// Checks that `caller` may change the account `user_id`, against both
    /// sides' current grants, and returns the caller's authority. Unknown accounts
    /// are checked as holding nothing, then reported `NotFound`, so only
    /// callers allowed to manage accounts learn whether one exists.
    pub(crate) async fn check_account_on(
        db: &mut SqliteConnection,
        caller: Caller<'_>,
        user_id: &str,
    ) -> Result<Authority, StoreError> {
        let caller = caller.load(&mut *db).await?;
        let exists = sqlx::query_scalar!(
            r#"SELECT EXISTS(SELECT 1 FROM auth_users WHERE id=?1) AS "exists!: bool""#,
            user_id
        )
        .fetch_one(&mut *db)
        .await
        .map_err(StoreError::database)?;
        let target = Holder::User(user_id).grants(db).await?;
        caller
            .grants
            .may_change_account(caller.user_id == user_id, &target)
            .map_err(StoreError::Denied)?;
        if exists {
            Ok(caller)
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

#[cfg(test)]
mod tests {
    use super::super::{CredentialKind, NewCredential, OperationState};
    use super::*;
    use crate::api::{Actor, Mutation, MutationResponse};
    use piqueld_core::access::{AppPermission, Preset};

    /// Saves an empty application named `name`, returning its ID.
    async fn application(store: &Store, name: &str) -> ApplicationId {
        let manifest = piqueld_core::manifest::parse_template_toml(&format!(
            "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='{name}'\n[spec]"
        ))
        .unwrap();
        let mutation = Mutation::save(manifest, None, false);
        let (MutationResponse::Saved(saved), _) = store
            .accept(Actor::Daemon, mutation, Some(0), false, None)
            .await
            .unwrap()
        else {
            panic!("saved application");
        };
        ApplicationId::parse(saved.application_id).unwrap()
    }

    /// Creating an application gives its creator's account matching grants
    /// there, unless it was created through a scoped credential.
    #[tokio::test]
    async fn only_unscoped_creators_receive_creator_grants() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("db")).await.unwrap();
        let blog = application(&store, "blog").await;
        let user = piqueld_core::auth::User {
            id: "alice".into(),
            username: "alice".into(),
            display_name: String::new(),
        };
        let mut grants = Preset::Developer.grants(&Scope::one(blog.clone()));
        grants
            .grant(
                Permission::Global(piqueld_core::access::GlobalPermission::AppsCreate),
                &Scope::All,
            )
            .unwrap();
        store.seed_auth_user(&user, &grants).await;
        for (id, scoped) in [("token", Some(&grants)), ("session", None)] {
            let credential = NewCredential {
                id: id.into(),
                secret_hash: id.into(),
                kind: CredentialKind::Cli,
                name: id,
                expires_at: None,
                grants: scoped,
            };
            store
                .insert_credential(&user.id, &credential)
                .await
                .unwrap();
        }
        let create = |credential_id, name: &str| {
            let manifest = piqueld_core::manifest::parse_template_toml(&format!(
                "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='{name}'\n[spec]"
            ))
            .unwrap();
            let actor = Actor::Account(Caller { credential_id });
            let store = &store;
            async move {
                let mutation = Mutation::save(manifest, None, false);
                let (MutationResponse::Saved(saved), _) = store
                    .accept(actor, mutation, Some(0), false, None)
                    .await
                    .unwrap()
                else {
                    panic!("saved application");
                };
                ApplicationId::parse(saved.application_id).unwrap()
            }
        };
        let readable = |store: &Store| {
            let store = store.clone();
            async move {
                let mut db = store.pool.acquire().await.unwrap();
                Holder::User("alice")
                    .grants(&mut db)
                    .await
                    .unwrap()
                    .app_scope(AppPermission::Write)
            }
        };
        let by_token = create("token", "from-token").await;
        assert!(!readable(&store).await.contains(&by_token));
        let by_session = create("session", "from-session").await;
        assert!(readable(&store).await.contains(&by_session));
    }

    /// Deleting an application removes grants on it, and revokes scoped
    /// credentials that held grants only there instead of leaving them with
    /// no access.
    #[tokio::test]
    async fn deleting_an_application_revokes_tokens_left_without_grants() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("db")).await.unwrap();
        let blog = application(&store, "blog").await;
        let shop = application(&store, "shop").await;
        let user = piqueld_core::auth::User {
            id: "alice".into(),
            username: "alice".into(),
            display_name: String::new(),
        };
        store.seed_auth_user(&user, &Grants::admin()).await;
        let deploy_blog = Preset::Deploy.grants(&Scope::one(blog.clone()));
        let mut both = deploy_blog.clone();
        both.extend(&Preset::ReadOnly.grants(&Scope::one(shop.clone())));
        for (id, grants) in [
            ("blog-only", Some(&deploy_blog)),
            ("both", Some(&both)),
            ("session", None),
        ] {
            let credential = NewCredential {
                id: id.into(),
                secret_hash: id.into(),
                kind: CredentialKind::Token,
                name: id,
                expires_at: None,
                grants,
            };
            store
                .insert_credential(&user.id, &credential)
                .await
                .unwrap();
        }
        let delete = Mutation::DeleteApplication {
            id: blog.clone(),
            environments: Vec::new(),
        };
        let (MutationResponse::Deleted(deleted), _) = store
            .accept(Actor::Daemon, delete, None, true, None)
            .await
            .unwrap()
        else {
            panic!("deletion");
        };
        let operation = &deleted.operations[0];
        store
            .transition_operation(
                &operation.operation_id,
                OperationState::Requested,
                OperationState::Running,
                None,
            )
            .await
            .unwrap();
        let operation = store.operation(&operation.operation_id).await.unwrap();
        store.finish_delete_operation(&operation).await.unwrap();
        assert!(store.credential_owner("blog-only").await.unwrap().is_none());
        assert!(store.credential_owner("session").await.unwrap().is_some());
        let both = store.credential_owner("both").await.unwrap().unwrap();
        assert_eq!(both.grants.app_scope(AppPermission::Read), Scope::one(shop));
        assert!(both.grants.app_scope(AppPermission::Deploy).is_empty());
    }
}
