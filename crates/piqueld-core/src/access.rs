//! Authorization contracts shared by the daemon, dashboard, and CLI.
//!
//! An account's access is a set of grants. Each grant gives one permission,
//! either on every application or on a list of applications:
//!
//! ```text
//! admin                          every ability, including permissions added later
//! admin on blog                  every application permission on `blog`
//! apps:deploy on blog, shop      deploy these two applications
//! system:read                    installation-wide; never limited to applications
//! ```
//!
//! Every application permission also lets its holder read that application
//! (`apps:read`), so a deploy-only token can follow its own deployment. The
//! wire form is a list of [`Grant`]s; [`Grants`] is the validated set, whose
//! types make an application scope on an installation-wide permission
//! unrepresentable.
use crate::ApplicationId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use utoipa::ToSchema;

// Declares a permission enum whose wire names, descriptions, and `ALL` list all
// come from one `Variant => "wire", "description"` table.
macro_rules! permissions {
    ($(#[$meta:meta])* $name:ident { $($(#[$variant_meta:meta])* $variant:ident => $value:literal, $description:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum $name {
            $($(#[$variant_meta])* $variant),+
        }
        impl $name {
            /// Every value, in declaration order.
            pub const ALL: &[Self] = &[$(Self::$variant),+];
            /// Stable wire and storage name.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $value),+ }
            }
            /// One-line explanation shown next to the permission.
            #[must_use]
            pub const fn description(self) -> &'static str {
                match self { $(Self::$variant => $description),+ }
            }
            /// Parses a wire name, rejecting unknown permissions.
            #[must_use]
            pub fn parse(value: &str) -> Option<Self> {
                match value { $($value => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

permissions! {
    /// Permissions on applications. Grants may limit them to specific applications.
    AppPermission {
        /// Read configuration, status, deployments, operations, and secret names.
        Read => "apps:read", "Read configuration, status, deployments and secret names",
        /// Save configuration edits, apply manifests, rename, and add or
        /// rename environments.
        Write => "apps:write", "Edit configuration, apply manifests, rename and add environments",
        /// Deploy saved configuration to environments and request reconciliation.
        Deploy => "apps:deploy", "Deploy and reconcile environments",
        /// Delete the application or its environments, with their runtime resources.
        Delete => "apps:delete", "Delete applications and environments",
        /// Store and delete secret values.
        SecretsWrite => "secrets:write", "Store and delete secret values",
        /// Read runtime, build and job logs, which may contain sensitive output.
        LogsRead => "logs:read", "Read runtime, build and job logs",
        /// Run commands in running containers. This reaches everything those
        /// containers can, including secret values.
        Exec => "apps:exec", "Run commands in running containers, which can read secret values",
        /// Read application history and diagnostics.
        EventsRead => "events:read", "Read history, diagnostics and analytics",
    }
}

permissions! {
    /// Installation-wide permissions, never limited to applications.
    GlobalPermission {
        /// Create applications. The creator receives matching grants on them.
        AppsCreate => "apps:create", "Create applications",
        /// Read daemon status, configuration, resources, and daemon history.
        SystemRead => "system:read", "Read daemon configuration, resources, history and notifications",
        /// Recover the secret key and retry notification deliveries.
        SystemOperate => "system:operate", "Recover the secret key and retry notifications",
        /// Manage other accounts with equal or less access, and invitations.
        AccountsManage => "accounts:manage", "Manage accounts with equal or less access, and invitations",
    }
}

/// One permission name. `admin` grants every ability, including permissions
/// added in later releases.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Permission {
    /// Every ability on the grant's scope.
    Admin,
    /// A permission on applications.
    App(AppPermission),
    /// An installation-wide permission.
    Global(GlobalPermission),
}

impl Permission {
    /// Wire name of [`Permission::Admin`].
    const ADMIN: &str = "admin";

    /// Every permission: `admin`, application permissions, then global ones.
    pub fn all() -> impl Iterator<Item = Self> {
        std::iter::once(Self::Admin)
            .chain(AppPermission::ALL.iter().copied().map(Self::App))
            .chain(GlobalPermission::ALL.iter().copied().map(Self::Global))
    }

    /// Stable wire and storage name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admin => Self::ADMIN,
            Self::App(permission) => permission.as_str(),
            Self::Global(permission) => permission.as_str(),
        }
    }

    /// One-line explanation shown next to the permission.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Admin => "Every ability, including permissions added later",
            Self::App(permission) => permission.description(),
            Self::Global(permission) => permission.description(),
        }
    }

    /// Parses a wire name, rejecting unknown permissions.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        if value == Self::ADMIN {
            return Some(Self::Admin);
        }
        AppPermission::parse(value)
            .map(Self::App)
            .or_else(|| GlobalPermission::parse(value).map(Self::Global))
    }

    /// Whether grants of this permission may be limited to applications.
    #[must_use]
    pub const fn scopable(self) -> bool {
        !matches!(self, Self::Global(_))
    }
}

impl std::fmt::Display for Permission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Permission {
    type Err = GrantError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value).ok_or_else(|| GrantError::Unknown(value.to_owned()))
    }
}

impl Serialize for Permission {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Permission {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

impl utoipa::PartialSchema for Permission {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::schema::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::Type::String)
            .enum_values(Some(Self::all().map(Self::as_str)))
            .into()
    }
}

impl ToSchema for Permission {}

/// Applications a grant covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Every application, including ones created later.
    All,
    /// Only these applications; empty means none.
    Only(BTreeSet<ApplicationId>),
}

impl Scope {
    /// Covers no application.
    pub const NONE: Self = Self::Only(BTreeSet::new());

    /// Covers exactly one application.
    #[must_use]
    pub fn one(id: ApplicationId) -> Self {
        Self::Only(BTreeSet::from([id]))
    }

    /// Whether the scope covers `id`.
    #[must_use]
    pub fn contains(&self, id: &ApplicationId) -> bool {
        match self {
            Self::All => true,
            Self::Only(ids) => ids.contains(id),
        }
    }

    /// Whether the scope covers no application at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Only(ids) if ids.is_empty())
    }

    /// Whether every application in `other` is also in `self`.
    #[must_use]
    pub fn covers(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::All, _) => true,
            (Self::Only(_), Self::All) => false,
            (Self::Only(ours), Self::Only(theirs)) => theirs.is_subset(ours),
        }
    }

    /// Extends this scope with every application in `other`.
    pub fn extend(&mut self, other: &Self) {
        match (&mut *self, other) {
            (Self::All, _) => {}
            (_, Self::All) => *self = Self::All,
            (Self::Only(ours), Self::Only(theirs)) => ours.extend(theirs.iter().cloned()),
        }
    }

    /// Applications covered by both scopes.
    #[must_use]
    pub fn intersection(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::All, scope) | (scope, Self::All) => scope.clone(),
            (Self::Only(ours), Self::Only(theirs)) => {
                Self::Only(ours.intersection(theirs).cloned().collect())
            }
        }
    }

    /// Applications in `self` that `covered` does not include. Every
    /// application minus a list stays every application, since later
    /// applications remain uncovered.
    #[must_use]
    pub fn without(&self, covered: &Self) -> Self {
        match (self, covered) {
            (_, Self::All) => Self::NONE,
            (Self::All, Self::Only(_)) => Self::All,
            (Self::Only(ours), Self::Only(theirs)) => {
                Self::Only(ours.difference(theirs).cloned().collect())
            }
        }
    }

    /// The application list, or `None` for every application.
    #[must_use]
    pub fn applications(&self) -> Option<&BTreeSet<ApplicationId>> {
        match self {
            Self::All => None,
            Self::Only(ids) => Some(ids),
        }
    }
}

/// One grant on the wire: a permission on every application, or only on the
/// listed ones.
///
/// ```json
/// {"permission": "apps:deploy", "applications": ["app-0123abcd"]}
/// {"permission": "system:read"}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Grant {
    /// Granted permission.
    pub permission: Permission,
    /// Stable IDs of the applications this grant is limited to; absent for
    /// every application. Installation-wide permissions never have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applications: Option<BTreeSet<ApplicationId>>,
}

/// A grant list that cannot become a [`Grants`] set.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GrantError {
    /// The permission name is not known to this version.
    #[error("unknown permission `{0}`")]
    Unknown(String),
    /// An installation-wide permission was limited to applications.
    #[error("`{0}` applies to the whole installation and cannot be limited to applications")]
    Unscopable(Permission),
    /// A grant listed no application.
    #[error("`{0}` must list at least one application, or none to cover every application")]
    EmptyScope(Permission),
}

/// Why a request was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Denied {
    /// The caller cannot see the application; reported as if it did not exist.
    #[error("application not found")]
    Hidden,
    /// The caller can see the target but lacks this permission.
    #[error("requires {0}")]
    Missing(Permission),
    /// The target account, or the access being handed out, includes grants
    /// the caller does not hold.
    #[error("the account or grants include access you do not hold")]
    Exceeds,
    /// Credentials with limited access, like API tokens, cannot create
    /// credentials (tokens, passkeys, enrollment links, or CLI logins), hand
    /// out access, or change their own account.
    #[error("credentials with limited access cannot create credentials or change their account")]
    Scoped,
}

/// The application a change applies to.
#[derive(Clone, Copy, Debug)]
pub enum Target<'a> {
    /// An application addressed by ID.
    Id(&'a ApplicationId),
    /// An existing application named by the caller, e.g. in a manifest.
    Named(&'a ApplicationId),
    /// An application about to be created.
    New,
    /// An application or environment that does not exist (any more), or a
    /// malformed ID.
    Unknown,
}

/// A validated set of grants in canonical form, so equal access compares
/// equal:
///
/// - grants for the same permission merge into one;
/// - nothing `admin` already covers is listed again;
/// - `apps:read` lists every application readable through other
///   application permissions, since they imply it.
///
/// Use [`Grants::intersection`] to limit one set by another, e.g. a token by
/// its owner's current access.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<Grant>", into = "Vec<Grant>")]
pub struct Grants {
    /// Scope of `admin`; `None` when not granted.
    admin: Option<Scope>,
    /// Scope of each held application permission, never empty.
    apps: BTreeMap<AppPermission, Scope>,
    /// Held installation-wide permissions.
    global: BTreeSet<GlobalPermission>,
}

impl Grants {
    /// Every ability on the installation: `admin` on every application.
    #[must_use]
    pub fn admin() -> Self {
        Self {
            admin: Some(Scope::All),
            ..Self::default()
        }
    }

    /// Adds `permission` on `scope`. Installation-wide permissions accept only
    /// [`Scope::All`]; an empty scope adds nothing.
    ///
    /// # Errors
    /// Returns [`GrantError::Unscopable`] for a scoped global permission.
    pub fn grant(&mut self, permission: Permission, scope: &Scope) -> Result<(), GrantError> {
        self.insert(permission, scope)?;
        self.normalize();
        Ok(())
    }

    /// Adds `permission` on `scope` when it may be limited to applications,
    /// and everywhere otherwise, e.g. for `--app blog --permission
    /// accounts:manage`. An empty scope adds no application permission.
    pub fn grant_within(&mut self, permission: Permission, scope: &Scope) {
        self.insert_within(permission, scope);
        self.normalize();
    }

    /// [`Grants::grant`] without restoring the canonical form, so a list of
    /// grants is normalized once rather than after every entry.
    fn insert(&mut self, permission: Permission, scope: &Scope) -> Result<(), GrantError> {
        if scope.is_empty() {
            return Ok(());
        }
        if !permission.scopable() && *scope != Scope::All {
            return Err(GrantError::Unscopable(permission));
        }
        self.insert_within(permission, scope);
        Ok(())
    }

    /// [`Grants::grant_within`] without restoring the canonical form.
    fn insert_within(&mut self, permission: Permission, scope: &Scope) {
        match permission {
            Permission::Global(permission) => {
                self.global.insert(permission);
            }
            _ if scope.is_empty() => {}
            Permission::Admin => self.admin.get_or_insert(Scope::NONE).extend(scope),
            Permission::App(permission) => {
                self.apps
                    .entry(permission)
                    .or_insert(Scope::NONE)
                    .extend(scope);
            }
        }
    }

    /// Restores the canonical form described on [`Grants`].
    fn normalize(&mut self) {
        let admin = self.admin_scope();
        if admin == Scope::All {
            self.apps.clear();
            self.global.clear();
            return;
        }
        let mut readable = Scope::NONE;
        for scope in self.apps.values_mut() {
            *scope = scope.without(&admin);
            readable.extend(scope);
        }
        self.apps.insert(AppPermission::Read, readable);
        self.apps.retain(|_, scope| !scope.is_empty());
        if admin.is_empty() {
            self.admin = None;
        }
    }

    /// Adds every grant of `other`.
    pub fn extend(&mut self, other: &Self) {
        if let Some(scope) = &other.admin {
            self.admin.get_or_insert(Scope::NONE).extend(scope);
        }
        for (permission, scope) in &other.apps {
            self.apps
                .entry(*permission)
                .or_insert(Scope::NONE)
                .extend(scope);
        }
        self.global.extend(other.global.iter().copied());
        self.normalize();
    }

    /// Whether these grants hold nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.admin.is_none() && self.apps.is_empty() && self.global.is_empty()
    }

    /// Whether these grants include `admin` on every application.
    #[must_use]
    pub fn is_superuser(&self) -> bool {
        self.admin == Some(Scope::All)
    }

    /// Applications on which `admin` is held.
    #[must_use]
    pub fn admin_scope(&self) -> Scope {
        self.admin.clone().unwrap_or(Scope::NONE)
    }

    /// Applications on which `permission` is held, directly, through `admin`,
    /// or (for `apps:read`) through any other application permission.
    #[must_use]
    pub fn app_scope(&self, permission: AppPermission) -> Scope {
        let mut scope = self.admin_scope();
        for (held, held_scope) in &self.apps {
            if *held == permission || permission == AppPermission::Read {
                scope.extend(held_scope);
            }
        }
        scope
    }

    /// Whether an installation-wide permission is held.
    #[must_use]
    pub fn has_global(&self, permission: GlobalPermission) -> bool {
        self.is_superuser() || self.global.contains(&permission)
    }

    /// Checks `permission` on one application. Applications the caller cannot
    /// read at all are [`Denied::Hidden`], so their existence does not leak.
    ///
    /// # Errors
    /// Returns why the caller may not act on `id`.
    pub fn require_app(&self, permission: AppPermission, id: &ApplicationId) -> Result<(), Denied> {
        if !self.app_scope(AppPermission::Read).contains(id) {
            return Err(Denied::Hidden);
        }
        if !self.app_scope(permission).contains(id) {
            return Err(Denied::Missing(Permission::App(permission)));
        }
        Ok(())
    }

    /// Checks that `permission` is held somewhere. Application permissions pass
    /// when held on at least one application; callers then narrow the result
    /// to [`Grants::app_scope`].
    ///
    /// # Errors
    /// Returns [`Denied::Missing`] when the permission is not held at all.
    pub fn require(&self, permission: Permission) -> Result<(), Denied> {
        let held = match permission {
            Permission::Admin => self.is_superuser(),
            Permission::App(permission) => !self.app_scope(permission).is_empty(),
            Permission::Global(permission) => self.has_global(permission),
        };
        if held {
            Ok(())
        } else {
            Err(Denied::Missing(permission))
        }
    }

    /// Approves a holder of `self` changing an account that holds `target`:
    /// always its `own` account, otherwise only with `accounts:manage` and
    /// every grant `target` holds.
    ///
    /// # Errors
    /// Returns why the change is refused.
    pub fn may_change_account(&self, own: bool, target: &Self) -> Result<(), Denied> {
        if own {
            return Ok(());
        }
        self.require(Permission::Global(GlobalPermission::AccountsManage))?;
        self.may_grant(target)
    }

    /// Approves a holder of `self` handing out `grants`, which it must hold.
    ///
    /// # Errors
    /// Returns [`Denied::Exceeds`] for grants `self` does not cover.
    pub fn may_grant(&self, grants: &Self) -> Result<(), Denied> {
        if self.covers(grants) {
            Ok(())
        } else {
            Err(Denied::Exceeds)
        }
    }

    /// Checks `required` application permissions for a change to `target`.
    ///
    /// Creating an application ([`Target::New`]) requires `apps:create` in
    /// place of `apps:write`, and every other permission on some application,
    /// which the creator then receives on it. A [`Target::Named`] application
    /// the caller cannot read is refused as if it were being created, so
    /// callers without `apps:create` cannot tell taken names apart.
    ///
    /// # Errors
    /// Returns why the change is refused.
    pub fn require_change(
        &self,
        required: &[AppPermission],
        target: Target<'_>,
    ) -> Result<(), Denied> {
        let create = || {
            required.iter().try_for_each(|permission| match permission {
                AppPermission::Write => {
                    self.require(Permission::Global(GlobalPermission::AppsCreate))
                }
                permission => self.require(Permission::App(*permission)),
            })
        };
        match target {
            Target::New => create(),
            Target::Id(id) => required
                .iter()
                .try_for_each(|permission| self.require_app(*permission, id)),
            // As for an application that does not exist: only callers who
            // could act on every application learn that.
            Target::Unknown => required.iter().try_for_each(|permission| {
                if self.app_scope(*permission) == Scope::All {
                    Ok(())
                } else {
                    Err(Denied::Hidden)
                }
            }),
            Target::Named(id) => {
                if !self.app_scope(AppPermission::Read).contains(id) {
                    return create()
                        .and(Err(Denied::Missing(Permission::App(AppPermission::Write))));
                }
                self.require_change(required, Target::Id(id))
            }
        }
    }

    /// Whether `self` holds everything `other` grants, so a holder of `self`
    /// may hand `other` out. `admin` is only covered by `admin`, because it
    /// includes permissions added later.
    #[must_use]
    pub fn covers(&self, other: &Self) -> bool {
        other
            .admin
            .as_ref()
            .is_none_or(|scope| self.admin_scope().covers(scope))
            && other
                .apps
                .iter()
                .all(|(permission, scope)| self.app_scope(*permission).covers(scope))
            && other
                .global
                .iter()
                .all(|permission| self.has_global(*permission))
    }

    /// Access held by both sets, e.g. a token limited by its owner.
    #[must_use]
    pub fn intersection(&self, other: &Self) -> Self {
        let mut result = Self {
            admin: Some(self.admin_scope().intersection(&other.admin_scope())),
            apps: AppPermission::ALL
                .iter()
                .map(|permission| {
                    let ours = self.app_scope(*permission);
                    (
                        *permission,
                        ours.intersection(&other.app_scope(*permission)),
                    )
                })
                .collect(),
            global: GlobalPermission::ALL
                .iter()
                .copied()
                .filter(|permission| self.has_global(*permission) && other.has_global(*permission))
                .collect(),
        };
        result.normalize();
        result
    }

    /// Grants an application creator receives on the new application `id`:
    /// every application permission (and `admin`) the creator holds on some,
    /// but not all, applications. Permissions already held everywhere need
    /// nothing new.
    #[must_use]
    pub fn for_created_application(&self, id: &ApplicationId) -> Self {
        let mut result = Self::default();
        let scope = Scope::one(id.clone());
        if self
            .admin
            .as_ref()
            .is_some_and(|admin| *admin != Scope::All)
        {
            result.admin = Some(scope.clone());
        }
        for (permission, held) in &self.apps {
            if *held != Scope::All {
                result.apps.insert(*permission, scope.clone());
            }
        }
        result.normalize();
        result
    }

    /// The canonical grant list: `admin`, then application permissions, then
    /// installation-wide permissions.
    #[must_use]
    pub fn to_list(&self) -> Vec<Grant> {
        let scoped = |permission, scope: &Scope| Grant {
            permission,
            applications: scope.applications().cloned(),
        };
        self.admin
            .iter()
            .map(|scope| scoped(Permission::Admin, scope))
            .chain(
                self.apps
                    .iter()
                    .map(|(permission, scope)| scoped(Permission::App(*permission), scope)),
            )
            .chain(self.global.iter().map(|permission| Grant {
                permission: Permission::Global(*permission),
                applications: None,
            }))
            .collect()
    }
}

impl TryFrom<Vec<Grant>> for Grants {
    type Error = GrantError;
    fn try_from(list: Vec<Grant>) -> Result<Self, Self::Error> {
        let mut grants = Self::default();
        for grant in list {
            let scope = match grant.applications {
                None => Scope::All,
                Some(ids) if ids.is_empty() => {
                    return Err(GrantError::EmptyScope(grant.permission));
                }
                Some(ids) => Scope::Only(ids),
            };
            grants.insert(grant.permission, &scope)?;
        }
        grants.normalize();
        Ok(grants)
    }
}

impl From<Grants> for Vec<Grant> {
    fn from(grants: Grants) -> Self {
        grants.to_list()
    }
}

impl utoipa::PartialSchema for Grants {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::schema::ArrayBuilder::new()
            .items(utoipa::openapi::Ref::from_schema_name(Grant::name()))
            .into()
    }
}

impl ToSchema for Grants {
    fn schemas(
        schemas: &mut Vec<(
            String,
            utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
        )>,
    ) {
        schemas.push((
            Grant::name().into_owned(),
            <Grant as utoipa::PartialSchema>::schema(),
        ));
        <Grant as ToSchema>::schemas(schemas);
    }
}

/// Common grant combinations offered by account, invitation, and token forms.
/// Presets only fill in grants; what is stored is the explicit grant list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    /// Read applications, their logs and history.
    ReadOnly,
    /// Read and deploy applications, e.g. for CI.
    Deploy,
    /// Everything on applications except deleting them.
    Developer,
    /// `admin` on the scope; on every application, full control of the installation.
    Admin,
}

impl Preset {
    /// Every preset, from least to most access.
    pub const ALL: &[Self] = &[Self::ReadOnly, Self::Deploy, Self::Developer, Self::Admin];

    /// Stable name used by forms and `--preset`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Deploy => "deploy",
            Self::Developer => "developer",
            Self::Admin => "admin",
        }
    }

    /// Parses a preset name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|preset| preset.as_str() == value)
    }

    /// The preset's permissions. On every application, read-only and
    /// developer access also include reading daemon status. Developers may
    /// run commands in containers: deploying their own configuration already
    /// reaches everything a command could.
    pub fn permissions(self, everywhere: bool) -> impl Iterator<Item = Permission> {
        use AppPermission::{Deploy, EventsRead, Exec, LogsRead, Read, SecretsWrite, Write};
        let apps: &[AppPermission] = match self {
            Self::ReadOnly => &[Read, LogsRead, EventsRead],
            Self::Deploy => &[Read, Deploy],
            Self::Developer => &[
                Read,
                Write,
                Deploy,
                SecretsWrite,
                LogsRead,
                EventsRead,
                Exec,
            ],
            Self::Admin => &[],
        };
        let admin = (self == Self::Admin).then_some(Permission::Admin);
        let system = (everywhere && matches!(self, Self::ReadOnly | Self::Developer))
            .then_some(Permission::Global(GlobalPermission::SystemRead));
        admin
            .into_iter()
            .chain(apps.iter().copied().map(Permission::App))
            .chain(system)
    }

    /// The preset's [`permissions`](Self::permissions) on `scope`.
    #[must_use]
    pub fn grants(self, scope: &Scope) -> Grants {
        let mut grants = Grants::default();
        for permission in self.permissions(*scope == Scope::All) {
            grants.grant_within(permission, scope);
        }
        grants
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> ApplicationId {
        ApplicationId::parse(value).unwrap()
    }

    fn only(values: &[&str]) -> Scope {
        Scope::Only(values.iter().map(|value| id(value)).collect())
    }

    fn grants(list: &[(&str, Option<&[&str]>)]) -> Grants {
        list.iter()
            .map(|(permission, applications)| Grant {
                permission: permission.parse().unwrap(),
                applications: applications.map(|ids| ids.iter().map(|value| id(value)).collect()),
            })
            .collect::<Vec<_>>()
            .try_into()
            .unwrap()
    }

    /// Hidden applications do not leak; visible ones name the missing permission.
    #[test]
    fn application_checks_hide_unreadable_applications() {
        let blog = id("app-blog0000");
        let shop = id("app-shop0000");
        let grants = grants(&[("apps:deploy", Some(&["app-blog0000"]))]);
        assert_eq!(grants.require_app(AppPermission::Read, &blog), Ok(()));
        assert_eq!(grants.require_app(AppPermission::Deploy, &blog), Ok(()));
        assert_eq!(
            grants.require_app(AppPermission::Delete, &blog),
            Err(Denied::Missing(Permission::App(AppPermission::Delete)))
        );
        assert_eq!(
            grants.require_app(AppPermission::Read, &shop),
            Err(Denied::Hidden)
        );
        assert_eq!(
            grants.require(Permission::App(AppPermission::LogsRead)),
            Err(Denied::Missing(Permission::App(AppPermission::LogsRead)))
        );
        // An unknown target is hidden unless the permission covers everything.
        let deploy = [AppPermission::Deploy];
        assert_eq!(
            grants.require_change(&deploy, Target::Unknown),
            Err(Denied::Hidden)
        );
        assert_eq!(
            Grants::admin().require_change(&deploy, Target::Unknown),
            Ok(())
        );
    }

    /// `admin` on applications covers application permissions there, but no
    /// installation-wide permission; on every application it covers everything.
    #[test]
    fn admin_scope_bounds_its_abilities() {
        let blog = id("app-blog0000");
        let scoped = grants(&[("admin", Some(&["app-blog0000"]))]);
        assert_eq!(scoped.require_app(AppPermission::Delete, &blog), Ok(()));
        assert!(!scoped.has_global(GlobalPermission::AccountsManage));
        assert!(Grants::admin().has_global(GlobalPermission::AccountsManage));
        assert_eq!(Grants::admin().app_scope(AppPermission::Delete), Scope::All);
    }

    /// Handing out access requires holding it; `admin` needs `admin`.
    #[test]
    fn covers_prevents_escalation() {
        let developer = Preset::Developer.grants(&Scope::All);
        assert!(developer.covers(&Preset::Deploy.grants(&only(&["app-blog0000"]))));
        assert!(!developer.covers(&Preset::Admin.grants(&only(&["app-blog0000"]))));
        assert!(!developer.covers(&grants(&[("accounts:manage", None)])));
        let scoped = Preset::Developer.grants(&only(&["app-blog0000"]));
        assert!(!scoped.covers(&Preset::Deploy.grants(&Scope::All)));
        assert!(Grants::admin().covers(&developer));
    }

    /// A token limited by its owner holds only what both hold.
    #[test]
    fn intersection_keeps_shared_access() {
        let owner = grants(&[("admin", Some(&["app-blog0000"])), ("system:read", None)]);
        let token = grants(&[
            ("apps:deploy", None),
            ("system:read", None),
            ("accounts:manage", None),
        ]);
        let effective = owner.intersection(&token);
        assert_eq!(
            effective,
            grants(&[
                ("apps:deploy", Some(&["app-blog0000"])),
                ("system:read", None)
            ])
        );
        assert_eq!(
            Grants::admin().intersection(&Grants::admin()),
            Grants::admin()
        );
    }

    /// Creators get grants on their new application only for permissions they
    /// hold on some, but not all, applications.
    #[test]
    fn creators_receive_matching_grants() {
        let new = id("app-new00000");
        let creator = grants(&[
            ("apps:write", Some(&["app-blog0000"])),
            ("apps:deploy", None),
            ("apps:create", None),
        ]);
        assert_eq!(
            creator.for_created_application(&new),
            grants(&[("apps:write", Some(&["app-new00000"]))])
        );
        assert!(Grants::admin().for_created_application(&new).is_empty());
    }

    /// Equal access has one representation however it was written.
    #[test]
    fn equal_access_compares_equal() {
        let blog = Some(&["app-blog0000"][..]);
        assert_eq!(
            grants(&[("apps:deploy", blog)]),
            grants(&[("apps:read", blog), ("apps:deploy", blog)])
        );
        assert_eq!(
            grants(&[("admin", blog), ("apps:delete", blog), ("logs:read", None)]),
            grants(&[("admin", blog), ("logs:read", None)])
        );
        assert_eq!(
            grants(&[("admin", None), ("system:read", None)]),
            Grants::admin()
        );
        assert_eq!(
            Preset::Developer
                .grants(&Scope::All)
                .intersection(&grants(&[("apps:deploy", blog)])),
            grants(&[("apps:deploy", blog)])
        );
    }

    /// The wire list round-trips canonically and rejects invalid scopes.
    #[test]
    fn wire_list_is_validated_and_canonical() {
        let merged = grants(&[
            ("apps:read", Some(&["app-blog0000"])),
            ("apps:read", Some(&["app-shop0000"])),
        ]);
        assert_eq!(
            serde_json::to_value(&merged).unwrap(),
            serde_json::json!([{"permission": "apps:read", "applications": ["app-blog0000", "app-shop0000"]}])
        );
        for invalid in [
            serde_json::json!([{"permission": "system:read", "applications": ["app-blog0000"]}]),
            serde_json::json!([{"permission": "apps:read", "applications": []}]),
            serde_json::json!([{"permission": "apps:fly"}]),
        ] {
            assert!(serde_json::from_value::<Grants>(invalid).is_err());
        }
    }

    /// A selection limited to applications keeps installation-wide
    /// permissions everywhere, and scoped presets omit daemon status.
    #[test]
    fn selections_scope_only_application_permissions() {
        let blog = only(&["app-blog0000"]);
        let mut selected = Preset::Developer.grants(&blog);
        selected.grant_within("accounts:manage".parse().unwrap(), &blog);
        assert!(selected.has_global(GlobalPermission::AccountsManage));
        assert!(!selected.has_global(GlobalPermission::SystemRead));
        assert_eq!(selected.app_scope(AppPermission::Exec), blog);
        assert!(
            Preset::ReadOnly
                .grants(&Scope::All)
                .has_global(GlobalPermission::SystemRead)
        );
        assert!(Preset::Admin.grants(&Scope::NONE).is_empty());
    }
}
