//! Account access: `whoami`, `account list|access|invite|enroll|update|delete`,
//! `account passkey list|rename|remove`, `account invitation list|revoke`,
//! `token create|list|revoke`, and the `audit` trail.
//!
//! Grants are given with `--preset` and `--permission`, limited to the
//! applications named with `--app` (every application when omitted). Grants
//! are displayed with application names where the caller can read them.
use crate::{
    auth::Credentials,
    cli::one_of,
    error::{CliError, ErrorKind, Result},
    output::{Console, HumanWriter, Report, reports::report},
};
use clap::{Args, Subcommand};
use piqueld_client::{
    ApplicationId, Client, Page,
    access::{Grant, Grants, Permission, Preset, Scope},
    audit::{AuditEvent, AuditFilter, AuditLink, AuditOutcome, AuditVerification},
    auth::{
        Account, CredentialView, Directory, InvitationView, Manage, OperatorSessionView,
        PasskeyView, Principal, Session, User,
    },
    tailnet::TailnetBinding,
};
use serde::Serialize;
use std::{collections::BTreeMap, io};

// `account` subcommands; `///` on variants and fields is user-facing help.
#[derive(Debug, Subcommand)]
pub(crate) enum AccountCommand {
    /// List the accounts you can see, with their grants.
    ///
    /// With `accounts:manage`, that is every account, and the host operator's
    /// live browser sessions, which `token revoke` ends. Otherwise, it is only
    /// yours.
    List,
    /// Replace an account's grants.
    ///
    /// Choose the new grants with `--preset`, `--permission`, or both. Needs
    /// `accounts:manage`, every grant the account has, and every grant you give
    /// it. An API token can't change grants.
    Access {
        /// Account username or ID.
        #[arg(value_name = "ACCOUNT")]
        target: String,
        #[command(flatten)]
        grants: GrantArgs,
        /// Skip the confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },
    /// Print a link that creates a new account with the selected grants.
    ///
    /// Choose the grants with `--preset`, `--permission`, or both. The link
    /// works once within 24 hours. Needs `accounts:manage` and every grant it
    /// hands out. An API token can't create invitations.
    Invite {
        #[command(flatten)]
        grants: GrantArgs,
    },
    /// Print a link that adds a passkey to an existing account.
    ///
    /// The link works once within 24 hours, and signs in whoever uses it as
    /// that account. Anyone may create one for their own account; another
    /// account needs `accounts:manage` and every grant it has. An API token or
    /// limited login can't create enrollment links.
    Enroll {
        /// Account username or ID.
        #[arg(value_name = "ACCOUNT")]
        target: String,
    },
    /// Change an account's username or display name.
    ///
    /// Give `--username`, `--display-name`, or both; the other keeps its value.
    /// Anyone may change their own account; another account needs
    /// `accounts:manage` and every grant it has. An API token or limited
    /// login can't change its own account. Renaming an account you saved
    /// a login for also renames the login, so `--account` finds it.
    #[command(group(clap::ArgGroup::new("changes").required(true).multiple(true)))]
    Update {
        /// Account username or ID.
        #[arg(value_name = "ACCOUNT")]
        target: String,
        /// Rename the account to this unique username.
        #[arg(long, group = "changes")]
        username: Option<String>,
        /// Set the account's display name.
        ///
        /// An empty value clears it.
        #[arg(long, value_name = "NAME", group = "changes")]
        display_name: Option<String>,
    },
    /// Delete an account, with its passkeys, sessions and API tokens.
    ///
    /// Anyone may delete their own account; another account needs
    /// `accounts:manage` and every grant it has. An API token or limited
    /// login can't delete its own account. The daemon refuses when no
    /// account with `admin` and a passkey would remain.
    Delete {
        /// Account username or ID.
        #[arg(value_name = "ACCOUNT")]
        target: String,
        /// Skip the confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },
    /// List, rename and remove passkeys.
    ///
    /// Add a passkey with an `account enroll` link.
    Passkey {
        #[command(subcommand)]
        command: PasskeyCommand,
    },
    /// List and revoke pending invitation and enrollment links.
    Invitation {
        #[command(subcommand)]
        command: InvitationCommand,
    },
}

// `account passkey` subcommands; `///` on variants and fields is user-facing help.
#[derive(Debug, Subcommand)]
pub(crate) enum PasskeyCommand {
    /// List the passkeys of the accounts you can see, with their owners.
    ///
    /// With `accounts:manage`, that is every account. Otherwise, it is only
    /// yours.
    List {
        /// Only list this account's passkeys, by username or ID.
        #[arg(value_name = "ACCOUNT")]
        target: Option<String>,
    },
    /// Rename a passkey.
    ///
    /// Anyone may rename their own; another account's needs `accounts:manage`
    /// and every grant it has. An API token or limited login can't rename its
    /// own account's.
    Rename {
        /// Passkey ID, from `account passkey list`.
        id: String,
        /// New name.
        name: String,
    },
    /// Remove a passkey, so it can no longer sign in.
    ///
    /// Sessions it signed in stay valid until revoked with `token revoke`.
    /// Anyone may remove their own; another account's needs `accounts:manage`
    /// and every grant it has. An API token or limited login can't remove its
    /// own account's. The daemon refuses when no account with `admin` and a
    /// passkey would remain.
    Remove {
        /// Passkey ID, from `account passkey list`.
        id: String,
        /// Skip the confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },
}

// `account invitation` subcommands; `///` on variants and fields is user-facing help.
#[derive(Debug, Subcommand)]
pub(crate) enum InvitationCommand {
    /// List pending invitation and enrollment links, with the grants they
    /// hand out.
    ///
    /// Needs `accounts:manage`; without it, the list is empty.
    List,
    /// Revoke a pending invitation or enrollment link, so it no longer works.
    ///
    /// An invitation needs `accounts:manage` and every grant it hands out. An
    /// enrollment link needs what `account enroll` does.
    Revoke {
        /// Invitation ID, from `account invitation list`.
        id: String,
    },
}

// Grants selected on the command line: a preset, permissions, or both.
#[derive(Debug, Args)]
pub(crate) struct GrantArgs {
    /// Grant the permissions of this preset.
    #[arg(long, value_parser = one_of!(Preset::ALL, Preset::parse, GrantArgs::describe_preset))]
    preset: Option<Preset>,
    /// Grant this permission.
    ///
    /// Repeat it to grant several.
    #[arg(
        long = "permission",
        value_name = "PERMISSION",
        value_parser = one_of!(Permission::all(), Permission::parse, Permission::description)
    )]
    permissions: Vec<Permission>,
    /// Limit the application permissions to this application.
    ///
    /// Repeat it to allow several applications. Without it, the permissions
    /// apply to every application. Installation-wide permissions, such as
    /// `system:read`, are never limited.
    #[arg(long = "app", value_name = "NAME_OR_ID")]
    applications: Vec<String>,
}

impl GrantArgs {
    /// What a preset grants, for `--help`.
    fn describe_preset(preset: Preset) -> &'static str {
        match preset {
            Preset::ReadOnly => "Read applications, their logs and history",
            Preset::Deploy => "Read and deploy applications, such as for CI",
            Preset::Developer => {
                "Everything on applications but deleting them, including running commands in them"
            }
            Preset::Admin => {
                "Every permission, on the selected applications or on the installation"
            }
        }
    }

    /// Whether no grant option was given at all.
    pub(crate) fn is_empty(&self) -> bool {
        self.preset.is_none() && self.permissions.is_empty() && self.applications.is_empty()
    }

    /// Resolves `--app` names through the daemon and builds the selected grants.
    async fn grants(&self, client: &Client) -> Result<Grants> {
        let mut ids = std::collections::BTreeSet::new();
        for application in &self.applications {
            let view = crate::commands::resolve_application(client, application).await?;
            ids.insert(view.application.id().clone());
        }
        self.build(ids)
    }

    /// Builds the selected grants with `--app` values taken as application
    /// IDs, for `login`, which cannot look names up before signing in.
    pub(crate) fn grants_by_id(&self) -> Result<Grants> {
        let ids = self
            .applications
            .iter()
            .map(|id| {
                ApplicationId::parse(id.as_str()).map_err(|_| {
                    CliError::new(
                        ErrorKind::Input,
                        format!("--app {id:?} must be an application ID when signing in"),
                    )
                })
            })
            .collect::<Result<_>>()?;
        self.build(ids)
    }

    /// Builds the selected grants on `ids`, or every application when empty.
    fn build(&self, ids: std::collections::BTreeSet<ApplicationId>) -> Result<Grants> {
        if self.preset.is_none() && self.permissions.is_empty() {
            return Err(CliError::new(
                ErrorKind::Input,
                "select grants with --preset or --permission",
            ));
        }
        let scope = if ids.is_empty() {
            Scope::All
        } else {
            Scope::Only(ids)
        };
        let mut grants = self
            .preset
            .map_or_else(Grants::default, |preset| preset.grants(&scope));
        for permission in &self.permissions {
            grants.grant_within(*permission, &scope);
        }
        Ok(grants)
    }
}

// `token` subcommands; `///` on variants and fields is user-facing help.
#[derive(Debug, Subcommand)]
pub(crate) enum TokenCommand {
    /// Create an API token for your account, and print it.
    ///
    /// The token has only the grants you select, with `--preset`,
    /// `--permission`, or both, and you must hold them. Use it with
    /// `PIQUELD_TOKEN`. Keep it safe: it is printed once and never shown
    /// again. An API token can't create tokens.
    Create {
        /// Token name, up to 200 bytes.
        name: String,
        #[command(flatten)]
        grants: GrantArgs,
        /// Expire the token after this many days, from 1.
        ///
        /// The daemon refuses more days than its `auth.max_token_days`.
        #[arg(long, default_value_t = 90, conflicts_with = "no_expiry", value_parser = clap::value_parser!(u32).range(1..))]
        days: u32,
        /// Never expire the token.
        ///
        /// The daemon refuses it when it sets `auth.max_token_days`.
        #[arg(long)]
        no_expiry: bool,
        /// Accept the token only over the tailnet, from this user's devices or
        /// devices with this tag.
        ///
        /// Give a login name, such as `alice@example.com`, or a tag, such as
        /// `tag:ci`. The daemon needs its tailnet node or a private dashboard
        /// hostname.
        #[arg(long, value_name = "USER_OR_TAG", value_parser = parse_tailnet)]
        tailnet: Option<TailnetBinding>,
    },
    /// List the sessions and API tokens of an account, with their grants and
    /// expiry.
    ///
    /// Without `--user`, lists yours.
    List {
        /// List this account's instead, by username or ID.
        ///
        /// Accounts other than yours need `accounts:manage`.
        #[arg(long, value_name = "ACCOUNT")]
        user: Option<String>,
    },
    /// Revoke a session or API token by ID, or every one of an account with
    /// `--all`.
    ///
    /// Give the ID of a credential from `token list`, or of a host operator
    /// browser session, from `account list`.
    Revoke {
        /// Credential ID.
        #[arg(required_unless_present = "all")]
        id: Option<String>,
        /// Revoke every session and API token of an account instead.
        ///
        /// Without `--user`, revokes yours, including the login this command
        /// uses. An API token or limited login can't revoke its own
        /// account's.
        #[arg(long, conflicts_with = "id")]
        all: bool,
        /// Revoke this account's with `--all`, by username or ID.
        ///
        /// Accounts other than yours need `accounts:manage` and every grant
        /// the account has.
        #[arg(long, value_name = "ACCOUNT", conflicts_with = "id")]
        user: Option<String>,
        /// Skip the confirmation prompt of `--all`.
        #[arg(long, short)]
        yes: bool,
    },
}

impl TokenCommand {
    /// Runs one `token` subcommand.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        match self {
            Self::Create {
                name,
                grants,
                days,
                no_expiry,
                tailnet,
            } => {
                // Daemons older than scoped tokens would ignore `grants` and
                // issue full access. Their sessions lack `scoped`, so this
                // fails to decode before anything is created.
                match client.auth_me().await {
                    Ok(_) => {}
                    Err(piqueld_client::ClientError::Decode { .. }) => {
                        return Err(CliError::new(
                            ErrorKind::General,
                            "the daemon predates limited tokens, so none was created",
                        ));
                    }
                    Err(error) => return Err(error.into()),
                }
                let grants = grants.grants(client).await?;
                let managed = client
                    .auth_manage(&Manage::CreateToken {
                        grants,
                        name: name.clone(),
                        days: (!no_expiry).then_some(*days),
                        tailnet: tailnet.clone(),
                    })
                    .await?;
                let token = managed.token.ok_or_else(|| {
                    CliError::new(ErrorKind::General, "the daemon returned no token")
                        .invalid_response()
                })?;
                console.emit(&TokenReport { token })
            }
            Self::List { user } => {
                let directory = client.auth_directory().await?;
                let owner = owner(client, &directory, user.as_deref()).await?;
                let names = Names::load(client).await;
                let credentials = directory
                    .credentials
                    .into_iter()
                    .filter(|credential| credential.user_id == owner.id)
                    .collect::<Vec<_>>();
                console.emit(&CredentialsReport { credentials, names })
            }
            Self::Revoke { id: Some(id), .. } => {
                client
                    .auth_manage(&Manage::RevokeCredential { id: id.clone() })
                    .await?;
                console.info(format_args!("revoked {id}"))
            }
            Self::Revoke { user, yes, .. } => {
                let directory = client.auth_directory().await?;
                let owner = owner(client, &directory, user.as_deref()).await?;
                console
                    .confirm(
                        *yes,
                        &format!("Revoke every session and token of {:?}?", owner.username),
                    )
                    .await?;
                client
                    .auth_manage(&Manage::RevokeAll {
                        user_id: owner.id.clone(),
                    })
                    .await?;
                console.info(format_args!(
                    "revoked every session and token of {}",
                    owner.username
                ))
            }
        }
    }
}

impl AccountCommand {
    /// Runs one `account` subcommand.
    pub(crate) async fn run(
        &self,
        cli: &crate::cli::Cli,
        client: &Client,
        console: &mut Console,
    ) -> Result<()> {
        match self {
            Self::List => {
                let directory = client.auth_directory().await?;
                let names = Names::load(client).await;
                console.emit(&AccountsReport {
                    list: AccountList {
                        accounts: &directory.users,
                        operator_sessions: &directory.operator_sessions,
                    },
                    names: &names,
                })
            }
            Self::Access {
                target,
                grants,
                yes,
            } => {
                let target = find(&client.auth_directory().await?, target)?;
                let grants = grants.grants(client).await?;
                console
                    .confirm(
                        *yes,
                        &format!("Replace every grant of {:?}?", target.user.username),
                    )
                    .await?;
                client
                    .auth_manage(&Manage::SetGrants {
                        user_id: target.user.id.clone(),
                        grants: grants.clone(),
                    })
                    .await?;
                let account = Account {
                    user: target.user,
                    grants,
                };
                AccountsReport::emit_one(client, console, account).await
            }
            Self::Invite { grants } => {
                let grants = grants.grants(client).await?;
                let managed = client
                    .auth_manage(&Manage::CreateInvitation { grants })
                    .await?;
                console.emit(&LinkReport::new(managed.invitation_url)?)
            }
            Self::Enroll { target } => {
                let target = find(&client.auth_directory().await?, target)?;
                let managed = client
                    .auth_manage(&Manage::CreateEnrollment {
                        user_id: target.user.id,
                    })
                    .await?;
                console.emit(&LinkReport::new(managed.invitation_url)?)
            }
            Self::Update {
                target,
                username,
                display_name,
            } => {
                let mut account = find(&client.auth_directory().await?, target)?;
                if let Some(username) = username {
                    account.user.username.clone_from(username);
                }
                if let Some(display_name) = display_name {
                    account.user.display_name.clone_from(display_name);
                }
                client
                    .auth_manage(&Manage::UpdateUser {
                        user_id: account.user.id.clone(),
                        username: account.user.username.clone(),
                        display_name: account.user.display_name.clone(),
                    })
                    .await?;
                if username.is_some() {
                    Credentials::rename(cli, &account.user.id, &account.user.username)?;
                }
                AccountsReport::emit_one(client, console, account).await
            }
            Self::Delete { target, yes } => {
                let target = find(&client.auth_directory().await?, target)?.user;
                console
                    .confirm(*yes, &format!("Delete account {:?}?", target.username))
                    .await?;
                client
                    .auth_manage(&Manage::DeleteUser { user_id: target.id })
                    .await?;
                console.info(format_args!("deleted {}", target.username))
            }
            Self::Passkey { command } => command.run(client, console).await,
            Self::Invitation { command } => command.run(client, console).await,
        }
    }
}

impl PasskeyCommand {
    /// Runs one `account passkey` subcommand.
    async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        match self {
            Self::List { target } => {
                let mut directory = client.auth_directory().await?;
                if let Some(target) = target {
                    let id = find(&directory, target)?.user.id;
                    directory.passkeys.retain(|passkey| passkey.user_id == id);
                }
                console.emit(&PasskeysReport {
                    passkeys: directory.passkeys,
                    users: directory.users,
                })
            }
            Self::Rename { id, name } => {
                client
                    .auth_manage(&Manage::RenamePasskey {
                        id: id.clone(),
                        name: name.clone(),
                    })
                    .await?;
                console.info(format_args!("renamed passkey {id} to {name}"))
            }
            Self::Remove { id, yes } => {
                console
                    .confirm(*yes, &format!("Remove passkey {id}?"))
                    .await?;
                client
                    .auth_manage(&Manage::RemovePasskey { id: id.clone() })
                    .await?;
                console.info(format_args!("removed passkey {id}"))
            }
        }
    }
}

impl InvitationCommand {
    /// Runs one `account invitation` subcommand.
    async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        match self {
            Self::List => {
                let directory = client.auth_directory().await?;
                console.emit(&InvitationsReport {
                    invitations: directory.invitations,
                    users: directory.users,
                    names: Names::load(client).await,
                })
            }
            Self::Revoke { id } => {
                client
                    .auth_manage(&Manage::RevokeInvitation { id: id.clone() })
                    .await?;
                console.info(format_args!("revoked invitation {id}"))
            }
        }
    }
}

/// The account `--user` names, or the caller's own; the host operator has
/// none, so it must name one.
async fn owner(client: &Client, directory: &Directory, user: Option<&str>) -> Result<User> {
    if let Some(user) = user {
        return Ok(find(directory, user)?.user);
    }
    client
        .auth_me()
        .await?
        .principal
        .user()
        .cloned()
        .ok_or_else(|| {
            CliError::new(
                ErrorKind::Input,
                "the host operator has no account; name one with --user",
            )
        })
}

/// Username of a visible account, or its ID when hidden.
fn username<'a>(users: &'a [Account], id: &'a str) -> &'a str {
    users
        .iter()
        .find(|account| account.user.id == id)
        .map_or(id, |account| account.user.username.as_str())
}

/// Finds a visible account by ID, then by username (unique regardless of
/// case). IDs come first because a username may equal another account's ID.
fn find(directory: &Directory, account: &str) -> Result<Account> {
    let users = &directory.users;
    users
        .iter()
        .find(|candidate| candidate.user.id == account)
        .or_else(|| {
            users
                .iter()
                .find(|candidate| candidate.user.username.eq_ignore_ascii_case(account))
        })
        .cloned()
        .ok_or_else(|| {
            CliError::new(
                ErrorKind::Input,
                format!("account {account:?} was not found"),
            )
        })
}

/// Application names by ID, for displaying grants. Empty when the caller
/// cannot list applications; IDs are shown instead.
pub(crate) struct Names(BTreeMap<String, String>);

impl Names {
    /// Lists the applications the caller can read.
    pub(crate) async fn load(client: &Client) -> Self {
        let mut names = BTreeMap::new();
        if let Ok(page) = client.applications().await {
            for application in page.items {
                names.insert(application.id.to_string(), application.name.clone());
            }
        }
        Self(names)
    }

    /// Renders one grant, e.g. `apps:deploy on blog, shop`.
    fn describe(&self, grant: &Grant) -> String {
        match &grant.applications {
            None if grant.permission.scopable() => {
                format!("{} on every application", grant.permission)
            }
            None => grant.permission.to_string(),
            Some(ids) => {
                let names: Vec<_> = ids
                    .iter()
                    .map(|id| self.0.get(id.as_str()).map_or(id.as_str(), String::as_str))
                    .collect();
                format!("{} on {}", grant.permission, names.join(", "))
            }
        }
    }

    /// Writes one indented line per grant, or `no access`.
    fn render(&self, grants: &Grants, out: &mut HumanWriter<'_>) -> io::Result<()> {
        let list = grants.to_list();
        if list.is_empty() {
            return out.line("  no access");
        }
        for grant in &list {
            out.line(format_args!("  {}", self.describe(grant)))?;
        }
        Ok(())
    }
}

/// `whoami` result: the account and the current credential's grants.
pub(crate) struct SessionReport {
    pub(crate) session: Session,
    pub(crate) names: Names,
}
report!(SessionReport as Session = session, self, out, {
    let limited = if self.session.scoped {
        ", limited credential"
    } else {
        ""
    };
    match &self.session.principal {
        Principal::User(user) => {
            out.line(format_args!("{} ({}{limited})", user.username, user.id))?;
        }
        Principal::Operator(operator) => out.line(operator)?,
    }
    self.names.render(&self.session.grants, out)
});

/// Accounts with their grants, and live host operator sessions, which are
/// not accounts (revoke one with `token revoke`).
#[derive(Serialize)]
struct AccountList<'a> {
    accounts: &'a [Account],
    operator_sessions: &'a [OperatorSessionView],
}
/// An [`AccountList`], with application names for human output.
struct AccountsReport<'a> {
    list: AccountList<'a>,
    names: &'a Names,
}
impl AccountsReport<'_> {
    /// Prints one account after changing it, as `account list` would.
    async fn emit_one(client: &Client, console: &mut Console, account: Account) -> Result<()> {
        let names = Names::load(client).await;
        console.emit(&AccountsReport {
            list: AccountList {
                accounts: &[account],
                operator_sessions: &[],
            },
            names: &names,
        })
    }
}
// Its JSON borrows, which `report!` cannot name.
impl<'a> Report for AccountsReport<'a> {
    type Json = AccountList<'a>;
    fn json(&self) -> &AccountList<'a> {
        &self.list
    }
    fn render_human(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
        for account in self.list.accounts {
            out.line(format_args!(
                "{} ({})",
                account.user.username, account.user.id
            ))?;
            self.names.render(&account.grants, out)?;
        }
        for session in self.list.operator_sessions {
            out.line(format_args!(
                "{} browser session {} (admin, expires {})",
                session.operator, session.id, session.expires_at
            ))?;
        }
        Ok(())
    }
}

// `audit` options: whose requests to show, newest first, or `audit verify`.
#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub(crate) struct AuditArgs {
    #[command(subcommand)]
    command: Option<AuditCommand>,
    /// Only show the requests of this account, by username or ID, even a
    /// deleted one.
    ///
    /// Accounts other than yours need `audit:read`. Unlike the global
    /// `--account`, it doesn't choose the login you read with.
    #[arg(long, value_name = "ACCOUNT")]
    user: Option<String>,
    /// Only show the requests made with this credential ID, from `token list`.
    #[arg(long)]
    credential: Option<String>,
    /// Only show the requests with this outcome.
    #[arg(long, value_parser = one_of!(AuditOutcome::ALL, AuditOutcome::parse, AuditArgs::describe_outcome))]
    outcome: Option<AuditOutcome>,
    #[command(flatten)]
    page: crate::support::PageArgs,
}

fn parse_tailnet(value: &str) -> std::result::Result<TailnetBinding, String> {
    TailnetBinding::parse(value).map_err(str::to_owned)
}

// `audit` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum AuditCommand {
    /// Check that no one altered the audit trail.
    ///
    /// It checks the trail's hash chain, and exits with code 3 when a record
    /// was edited, inserted or removed. It prints the newest link: keep it
    /// elsewhere to also detect that the newest records were removed. Needs
    /// `audit:read`.
    Verify,
}

impl AuditArgs {
    /// What an outcome means, for `--help`.
    fn describe_outcome(outcome: AuditOutcome) -> &'static str {
        match outcome {
            AuditOutcome::Allowed => "Authorized, and succeeded",
            AuditOutcome::Denied => {
                "Refused: missing credentials or permission, or a target hidden from the caller"
            }
            AuditOutcome::Failed => "Authorized, but failed",
        }
    }

    /// Prints one page of the audit trail, or verifies its chain.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        if let Some(AuditCommand::Verify) = self.command {
            let verification = client.verify_audit().await?;
            console.emit(&verification)?;
            return match verification.broken_at {
                Some(id) => Err(CliError::new(
                    ErrorKind::Conflict,
                    format!("the audit trail was altered at or before record {id}"),
                )),
                None => Ok(()),
            };
        }
        // Deleted accounts keep their trail but leave the directory, and
        // auditors without accounts:manage only see themselves there, so an
        // account it lacks is matched by ID when the value is one (IDs are
        // UUIDs), and otherwise by the username its requests recorded.
        let (user_id, username) = match &self.user {
            Some(account) => match find(&client.auth_directory().await?, account) {
                Ok(found) => (Some(found.user.id), None),
                Err(_) if uuid::Uuid::try_parse(account).is_ok() => (Some(account.clone()), None),
                Err(_) => (None, Some(account.clone())),
            },
            None => (None, None),
        };
        let filter = AuditFilter {
            user_id,
            username,
            credential_id: self.credential.clone(),
            outcome: self.outcome,
        };
        let page = client
            .audit_events(&filter, self.page.cursor.as_deref(), self.page.limit)
            .await?;
        console.emit(&page)
    }
}

// Result of `audit verify`.
report!(AuditVerification, self, out, {
    let state = if self.broken_at.is_some() {
        "broken"
    } else {
        "intact"
    };
    out.label("Chain", state)?;
    out.label("Records checked", self.checked)?;
    if let Some(id) = self.broken_at {
        out.label("First mismatch", id)?;
    }
    // Operators keep both elsewhere to detect a rewritten or moved chain.
    let link = |link: &Option<AuditLink>| {
        link.as_ref().map_or_else(
            || "none".to_owned(),
            |link| format!("{} {}", link.id, link.link),
        )
    };
    out.label("Pruned through", link(&self.anchor))?;
    let head = if self.broken_at.is_some() {
        "Last verified"
    } else {
        "Newest"
    };
    out.label(head, link(&self.head))
});

// A page of audited requests, one line each.
report!(Page<AuditEvent>, self, out, {
    for event in &self.items {
        let who = event.who();
        let credential = event
            .credential_kind
            .as_deref()
            .map_or_else(String::new, |kind| format!(" via {kind}"));
        let missing = event
            .permission
            .as_deref()
            .map_or_else(String::new, |permission| {
                format!(" (requires {permission})")
            });
        let target = event
            .target()
            .map_or_else(String::new, |target| format!(" on {target}"));
        let tailnet = event
            .tailnet
            .as_deref()
            .map_or_else(String::new, |who| format!(" ({who})"));
        out.line(format_args!(
            "{}  {}  {} {}{target}  {who}{credential}  {}{tailnet}{missing}",
            event.created_at_ms,
            event.outcome.as_str(),
            event.status,
            event.action,
            event.peer.as_deref().unwrap_or("unix socket"),
        ))?;
    }
    out.next_cursor(self.next_cursor.as_deref())
});

/// A newly created token; human output is the bare secret so it can be piped.
#[derive(Serialize)]
struct TokenReport {
    token: String,
}
report!(TokenReport, self, out, { out.line(&self.token) });

/// The caller's sessions and tokens with their limits.
struct CredentialsReport {
    credentials: Vec<CredentialView>,
    names: Names,
}
report!(
    CredentialsReport as [CredentialView] = credentials,
    self,
    out,
    {
        for credential in &self.credentials {
            let expires = credential.expires_at.map_or_else(
                || "never expires".into(),
                |at| format!("expires at Unix {at}"),
            );
            out.line(format_args!(
                "{} {} {} ({expires})",
                credential.id, credential.kind, credential.name
            ))?;
            match &credential.grants {
                Some(grants) => self.names.render(grants, out)?,
                None => out.line("  the account's full access")?,
            }
            if let Some(tailnet) = &credential.tailnet {
                out.line(format_args!("  only from tailnet {tailnet}"))?;
            }
        }
        Ok(())
    }
);

/// Passkeys, with their owners' usernames for human output.
struct PasskeysReport {
    passkeys: Vec<PasskeyView>,
    users: Vec<Account>,
}
report!(PasskeysReport as [PasskeyView] = passkeys, self, out, {
    for passkey in &self.passkeys {
        out.line(format_args!(
            "{} {} ({})",
            passkey.id,
            passkey.name,
            username(&self.users, &passkey.user_id)
        ))?;
    }
    Ok(())
});

/// Pending invitation and enrollment links with the grants they hand out.
struct InvitationsReport {
    invitations: Vec<InvitationView>,
    users: Vec<Account>,
    names: Names,
}
report!(
    InvitationsReport as [InvitationView] = invitations,
    self,
    out,
    {
        for invitation in &self.invitations {
            let by = username(&self.users, &invitation.issuer_id);
            let expires = invitation.expires_at;
            if let Some(user) = &invitation.user_id {
                out.line(format_args!(
                    "{} enrollment for {} by {by} (expires at Unix {expires})",
                    invitation.id,
                    username(&self.users, user)
                ))?;
            } else {
                out.line(format_args!(
                    "{} invitation by {by} (expires at Unix {expires})",
                    invitation.id
                ))?;
                self.names.render(&invitation.grants, out)?;
            }
        }
        Ok(())
    }
);

/// A newly created invitation or enrollment link; human output is the bare URL.
#[derive(Serialize)]
struct LinkReport {
    url: String,
}
impl LinkReport {
    fn new(url: Option<String>) -> Result<Self> {
        let url = url.ok_or_else(|| {
            CliError::new(ErrorKind::General, "the daemon returned no link").invalid_response()
        })?;
        Ok(Self { url })
    }
}
report!(LinkReport, self, out, { out.line(&self.url) });

#[cfg(test)]
mod tests {
    use super::*;
    use piqueld_client::auth::User;

    #[test]
    fn account_ids_win_over_usernames() {
        let account = |id: &str, username: &str| Account {
            user: User {
                id: id.into(),
                username: username.into(),
                display_name: String::new(),
            },
            grants: Grants::default(),
        };
        let directory = Directory {
            users: vec![account("user-2", "user-1"), account("user-1", "Bob")],
            passkeys: Vec::new(),
            credentials: Vec::new(),
            invitations: Vec::new(),
            operator_sessions: Vec::new(),
        };
        assert_eq!(find(&directory, "user-1").unwrap().user.username, "Bob");
        assert_eq!(find(&directory, "bob").unwrap().user.id, "user-1");
    }

    /// `--app` alone selects nothing, so it is refused instead of silently
    /// asking for the account's full access.
    #[test]
    fn applications_without_permissions_are_refused() {
        let args = GrantArgs {
            preset: None,
            permissions: Vec::new(),
            applications: vec!["app-blog0000".into()],
        };
        assert!(!args.is_empty());
        assert!(args.grants_by_id().is_err());
    }

    /// The audit filter and the saved login it reads with are chosen
    /// independently, so an auditor can read an account it has no login for.
    #[test]
    fn audit_filter_is_separate_from_the_login_account() {
        use clap::Parser as _;
        let arguments = [
            "piquelctl",
            "--account",
            "auditor",
            "audit",
            "--user",
            "bob",
        ];
        let cli = crate::cli::Cli::try_parse_from(arguments).unwrap();
        let crate::cli::Command::Audit(audit) = cli.command else {
            panic!("audit command");
        };
        assert_eq!(cli.auth.account.as_deref(), Some("auditor"));
        assert_eq!(audit.user.as_deref(), Some("bob"));
    }
}
