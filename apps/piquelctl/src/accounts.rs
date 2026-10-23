//! Account access: `whoami`, `account list|access|invite|enroll|update|delete`,
//! `account passkey list|rename|remove`, `account invitation list|revoke`,
//! `token create|list|revoke`, and the `audit` trail.
//!
//! Grants are given with `--preset` and `--permission`, limited to the
//! applications named with `--app` (every application when omitted). Grants
//! are displayed with application names where the caller can read them.
use crate::{
    auth::Credentials,
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
    /// List the accounts you may see with their grants.
    List,
    /// Replace an account's grants (requires accounts:manage and every grant involved).
    Access {
        /// Account username or ID.
        #[arg(value_name = "ACCOUNT")]
        target: String,
        #[command(flatten)]
        grants: GrantArgs,
        /// Skip interactive confirmation.
        #[arg(long, short)]
        yes: bool,
    },
    /// Create a 24-hour invitation link for a new account with these grants.
    Invite {
        #[command(flatten)]
        grants: GrantArgs,
    },
    /// Create a 24-hour link that adds a passkey to an existing account.
    Enroll {
        /// Account username or ID.
        #[arg(value_name = "ACCOUNT")]
        target: String,
    },
    /// Change an account's username or display name.
    #[command(group(clap::ArgGroup::new("changes").required(true).multiple(true)))]
    Update {
        /// Account username or ID.
        #[arg(value_name = "ACCOUNT")]
        target: String,
        /// New unique username.
        #[arg(long, group = "changes")]
        username: Option<String>,
        /// New display name; an empty value clears it.
        #[arg(long, group = "changes")]
        display_name: Option<String>,
    },
    /// Delete an account with its passkeys, sessions, and tokens (refused
    /// when no administrator with a passkey would remain).
    Delete {
        /// Account username or ID.
        #[arg(value_name = "ACCOUNT")]
        target: String,
        /// Skip interactive confirmation.
        #[arg(long, short)]
        yes: bool,
    },
    /// List, rename, and remove passkeys; add one with `enroll`.
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
    /// List the passkeys of every account you may see, or of one account.
    List {
        /// Account username or ID.
        #[arg(value_name = "ACCOUNT")]
        target: Option<String>,
    },
    /// Rename a passkey.
    Rename {
        /// Passkey ID from `account passkey list`.
        id: String,
        /// New label.
        name: String,
    },
    /// Remove a passkey; sessions it signed in remain until revoked.
    Remove {
        /// Passkey ID from `account passkey list`.
        id: String,
        /// Skip interactive confirmation.
        #[arg(long, short)]
        yes: bool,
    },
}

// `account invitation` subcommands; `///` on variants and fields is user-facing help.
#[derive(Debug, Subcommand)]
pub(crate) enum InvitationCommand {
    /// List pending invitation and enrollment links (requires accounts:manage).
    List,
    /// Revoke a pending invitation or enrollment link.
    Revoke {
        /// Invitation ID from `account invitation list`.
        id: String,
    },
}

/// Grants selected on the command line: a preset, permissions, or both.
#[derive(Debug, Args)]
pub(crate) struct GrantArgs {
    /// Start from a preset: read-only, deploy, developer, or admin.
    #[arg(long, value_parser = parse_preset)]
    preset: Option<Preset>,
    /// Grant a permission, e.g. apps:deploy or system:read (repeatable).
    #[arg(long = "permission", value_name = "PERMISSION", value_parser = parse_permission)]
    permissions: Vec<Permission>,
    /// Limit application permissions to this application name or ID (repeatable).
    #[arg(long = "app", value_name = "NAME_OR_ID")]
    applications: Vec<String>,
}

fn parse_preset(value: &str) -> std::result::Result<Preset, String> {
    Preset::parse(value).ok_or_else(|| {
        let names: Vec<_> = Preset::ALL.iter().map(|preset| preset.as_str()).collect();
        format!("expected one of {}", names.join(", "))
    })
}

fn parse_permission(value: &str) -> std::result::Result<Permission, String> {
    value.parse().map_err(|error| format!("{error}"))
}

impl GrantArgs {
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
    /// Create an API token for your account, limited to the selected grants.
    Create {
        /// Token name.
        name: String,
        #[command(flatten)]
        grants: GrantArgs,
        /// Lifetime in days.
        #[arg(long, default_value_t = 90, conflicts_with = "no_expiry", value_parser = clap::value_parser!(u32).range(1..))]
        days: u32,
        /// Never expire (refused when the daemon limits token lifetimes).
        #[arg(long)]
        no_expiry: bool,
        /// Accept the token only through the daemon's tailnet node, from a
        /// device of this user (alice@example.com) or with this tag (tag:ci).
        #[arg(long, value_parser = parse_tailnet)]
        tailnet: Option<TailnetBinding>,
    },
    /// List the sessions and tokens of your account, or of another with --user.
    List {
        /// Account username or ID (requires accounts:manage for others).
        #[arg(long)]
        user: Option<String>,
    },
    /// Revoke a session or token by ID, or every session and token of an
    /// account with --all.
    Revoke {
        /// Credential ID from `token list`.
        #[arg(required_unless_present = "all")]
        id: Option<String>,
        /// Revoke every session and token of your account, including the one
        /// this command uses, or of the account named with --user.
        #[arg(long, conflicts_with = "id")]
        all: bool,
        /// Account username or ID whose credentials --all revokes.
        #[arg(long, conflicts_with = "id")]
        user: Option<String>,
        /// Skip interactive confirmation of --all.
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

/// `audit` options: whose requests to show, newest first, or `audit verify`.
#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub(crate) struct AuditArgs {
    #[command(subcommand)]
    command: Option<AuditCommand>,
    /// Only this account's requests, by username or ID (including deleted
    /// accounts'); requires audit:read for other accounts. Defaults to
    /// every visible account. Distinct from the global `--account`, which
    /// selects the saved login to read with.
    #[arg(long)]
    user: Option<String>,
    /// Only requests made with this credential ID (see `token list`).
    #[arg(long)]
    credential: Option<String>,
    /// Only requests with this outcome: allowed, denied, or failed.
    #[arg(long, value_parser = parse_outcome)]
    outcome: Option<AuditOutcome>,
    /// Continue after a cursor returned by the previous page.
    #[arg(long)]
    cursor: Option<String>,
    /// Maximum number of requests in the page.
    #[arg(long, default_value_t = piqueld_client::DEFAULT_PAGE_SIZE, value_parser = clap::value_parser!(u16).range(1..=i64::from(piqueld_client::MAX_PAGE_SIZE)))]
    limit: u16,
}

fn parse_tailnet(value: &str) -> std::result::Result<TailnetBinding, String> {
    TailnetBinding::parse(value).map_err(str::to_owned)
}

fn parse_outcome(value: &str) -> std::result::Result<AuditOutcome, String> {
    AuditOutcome::parse(value).ok_or_else(|| "expected allowed, denied, or failed".into())
}

/// `audit` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum AuditCommand {
    /// Check the audit trail's hash chain (requires audit:read). Fails when a
    /// record was edited, inserted, or removed; prints the newest link, which
    /// you can keep elsewhere to also detect removal of the newest records.
    Verify,
}

impl AuditArgs {
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
            .audit_events(&filter, self.cursor.as_deref(), self.limit)
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
