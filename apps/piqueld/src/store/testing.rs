//! Fixtures shared by the store's tests.
use super::{
    Actor, ApplicationId, Caller, CredentialKind, MIGRATIONS, NewCredential, OperationState, Store,
    StoreError,
};
use crate::api::{Mutation, MutationResponse};
use piqueld_core::{
    NormalizedApplication, access::Grants, api::SavedApplication, auth::User,
    manifest::ValidatedTemplate,
};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::path::Path;

/// Parses the application manifest named `name`, with `spec`, TOML following
/// its metadata.
pub(super) fn template(name: &str, spec: &str) -> ValidatedTemplate {
    piqueld_core::manifest::parse_template_toml(&format!(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='{name}'\n{spec}"
    ))
    .unwrap()
}

/// The `notes` application of core's `prebuilt.toml` fixture, whose `web`
/// service pulls `ghcr.io/example/notes:1.4.0`, under `id`.
pub(super) fn prebuilt(id: &str) -> NormalizedApplication {
    piqueld_core::parse_toml(include_str!(
        "../../../../crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml"
    ))
    .unwrap()
    .normalize(ApplicationId::parse(id).unwrap())
}

impl Store {
    /// Opens a store in a new temporary directory, which must outlive it.
    pub(super) async fn temporary() -> (tempfile::TempDir, Self) {
        let directory = tempfile::tempdir().unwrap();
        let store = Self::open(directory.path().join("db")).await.unwrap();
        (directory, store)
    }

    /// Opens the database at `path` migrated to schema `version`, as an
    /// older daemon left it.
    pub(super) async fn migrated(path: &Path, version: usize) -> SqlitePool {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        for (index, migration) in MIGRATIONS.iter().take(version).enumerate() {
            Self::apply_migration(&pool, index + 1, migration)
                .await
                .unwrap();
        }
        pool
    }

    /// Accepts `mutation` as the daemon, forced past revision checks.
    pub(super) async fn force(&self, mutation: Mutation) -> Result<MutationResponse, StoreError> {
        let (response, _) = self
            .accept(Actor::Daemon, mutation, None, true, None)
            .await?;
        Ok(response)
    }

    /// Creates the application `template` as the daemon, deploying it when
    /// `deploy`; it must not exist yet.
    pub(super) async fn save_template(
        &self,
        template: ValidatedTemplate,
        deploy: bool,
    ) -> SavedApplication {
        self.saved(template, deploy, (Some(0), false)).await
    }

    /// Saves `template` as the daemon, deploying it when `deploy`, over the
    /// application of that name if any, forced past revision checks.
    pub(super) async fn save_template_forced(
        &self,
        template: ValidatedTemplate,
        deploy: bool,
    ) -> SavedApplication {
        self.saved(template, deploy, (None, true)).await
    }

    /// Saves `template` with the `expected` revision and `force` of
    /// [`Store::accept`].
    async fn saved(
        &self,
        template: ValidatedTemplate,
        deploy: bool,
        (expected, force): (Option<u64>, bool),
    ) -> SavedApplication {
        let save = Mutation::save(template, None, deploy);
        let (MutationResponse::Saved(saved), _) = self
            .accept(Actor::Daemon, save, expected, force, None)
            .await
            .unwrap()
        else {
            panic!("saved response expected")
        };
        saved
    }

    /// Marks requested operation `id` running, as the controller does when
    /// it starts executing it.
    pub(super) async fn mark_running(&self, id: &str) {
        self.transition_operation(id, OperationState::Requested, OperationState::Running, None)
            .await
            .unwrap();
    }

    /// Seeds the account `name` holding `grants`, with an unscoped CLI
    /// session also named `name`, and returns the caller acting with it.
    pub(super) async fn seed_caller(&self, name: &'static str, grants: &Grants) -> Actor<'static> {
        let user = User {
            id: name.into(),
            username: name.into(),
            display_name: String::new(),
        };
        self.seed_auth_user(&user, grants).await;
        let credential = NewCredential {
            id: name.into(),
            secret_hash: name.into(),
            kind: CredentialKind::Cli,
            name,
            expires_at: None,
            grants: None,
        };
        self.insert_credential(name, &credential).await.unwrap();
        Actor::Account(Caller {
            credential_id: name,
            user_id: name,
        })
    }
}
