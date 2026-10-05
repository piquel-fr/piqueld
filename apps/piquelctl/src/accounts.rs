//! Account access: `whoami`, `account list|access|invite|enroll`,
//! `token create|list|revoke`, and the `audit` trail.
//!
//! Grants are given with `--preset` and `--permission`, limited to the
//! applications named with `--app` (every application when omitted). Grants
//! are displayed with application names where the caller can read them.
use crate::{
    cli::Cli,
    error::{CliError, ErrorKind, Result},
    output::{Console, HumanWriter, Report},
    support::confirm,
};
use clap::{Args, Subcommand};
use piqueld_client::{
    ApplicationId, Client, Page,
    access::{Grant, Grants, Permission, Preset, Scope},
    audit::{AuditEvent, AuditFilter, AuditOutcome, AuditVerification},
    auth::{Account, CredentialView, Directory, Manage, Session},
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
    },
    /// List your sessions and tokens.
    List,
    /// Revoke one of your sessions or tokens by ID.
    Revoke {
        /// Credential ID from `token list`.
        id: String,
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
                    })
                    .await?;
                let token = managed.token.ok_or_else(|| {
                    CliError::new(ErrorKind::General, "the daemon returned no token")
                        .invalid_response()
                })?;
                console.emit(&TokenReport { token })
            }
            Self::List => {
                let me = client.auth_me().await?;
                let directory = client.auth_directory().await?;
                let names = Names::load(client).await;
                let credentials = directory
                    .credentials
                    .into_iter()
                    .filter(|credential| credential.user_id == me.user.id)
                    .collect::<Vec<_>>();
                console.emit(&CredentialsReport { credentials, names })
            }
            Self::Revoke { id } => {
                client
                    .auth_manage(&Manage::RevokeCredential { id: id.clone() })
                    .await?;
                console.info(format_args!("revoked {id}"))
            }
        }
    }
}

impl AccountCommand {
    /// Runs one `account` subcommand.
    pub(crate) async fn run(
        &self,
        cli: &Cli,
        client: &Client,
        console: &mut Console,
    ) -> Result<()> {
        match self {
            Self::List => {
                let directory = client.auth_directory().await?;
                let names = Names::load(client).await;
                console.emit(&AccountsReport {
                    accounts: &directory.users,
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
                confirm(
                    console,
                    cli.noninteractive,
                    *yes,
                    &format!("Replace every grant of {:?}? [y/N] ", target.user.username),
                )
                .await?;
                client
                    .auth_manage(&Manage::SetGrants {
                        user_id: target.user.id.clone(),
                        grants: grants.clone(),
                    })
                    .await?;
                let names = Names::load(client).await;
                console.emit(&AccountsReport {
                    accounts: &[Account {
                        user: target.user,
                        grants,
                    }],
                    names: &names,
                })
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
        }
    }
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
impl Report for SessionReport {
    type Json = Session;
    fn json(&self) -> &Session {
        &self.session
    }
    fn render_human(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
        let user = &self.session.user;
        let limited = if self.session.scoped {
            ", limited credential"
        } else {
            ""
        };
        out.line(format_args!("{} ({}{limited})", user.username, user.id))?;
        self.names.render(&self.session.grants, out)
    }
}

/// Accounts with their grants.
struct AccountsReport<'a> {
    accounts: &'a [Account],
    names: &'a Names,
}
impl Report for AccountsReport<'_> {
    type Json = [Account];
    fn json(&self) -> &[Account] {
        self.accounts
    }
    fn render_human(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
        for account in self.accounts {
            out.line(format_args!(
                "{} ({})",
                account.user.username, account.user.id
            ))?;
            self.names.render(&account.grants, out)?;
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
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u16).range(1..=100))]
    limit: u16,
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
            let broken = verification.broken_at;
            console.emit(&VerificationReport(verification))?;
            return match broken {
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
        console.emit(&AuditReport(page))
    }
}

/// Result of `audit verify`.
struct VerificationReport(AuditVerification);
impl Report for VerificationReport {
    type Json = AuditVerification;
    fn json(&self) -> &AuditVerification {
        &self.0
    }
    fn render_human(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
        let result = &self.0;
        let state = if result.broken_at.is_some() {
            "broken"
        } else {
            "intact"
        };
        out.label("Chain", state)?;
        out.label("Linked records checked", result.checked)?;
        if result.unlinked > 0 {
            out.label("Records from before the chain", result.unlinked)?;
        }
        if let Some(id) = result.broken_at {
            out.label("First altered record", id)?;
        }
        out.label("Newest link", result.head.as_deref().unwrap_or("none"))
    }
}

/// A page of audited requests, one line each.
struct AuditReport(Page<AuditEvent>);
impl Report for AuditReport {
    type Json = Page<AuditEvent>;
    fn json(&self) -> &Page<AuditEvent> {
        &self.0
    }
    fn render_human(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
        for event in &self.0.items {
            let who = event
                .username
                .as_deref()
                .or(event.user_id.as_deref())
                .unwrap_or("anonymous");
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
            out.line(format_args!(
                "{}  {}  {} {}{target}  {who}{credential}  {}{missing}",
                event.created_at_ms,
                event.outcome.as_str(),
                event.status,
                event.action,
                event.peer.as_deref().unwrap_or("unix socket"),
            ))?;
        }
        if let Some(cursor) = &self.0.next_cursor {
            out.label("Next cursor", cursor)?;
        }
        Ok(())
    }
}

/// A newly created token; human output is the bare secret so it can be piped.
#[derive(Serialize)]
struct TokenReport {
    token: String,
}
impl Report for TokenReport {
    type Json = Self;
    fn json(&self) -> &Self {
        self
    }
    fn render_human(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
        out.line(&self.token)
    }
}

/// The caller's sessions and tokens with their limits.
struct CredentialsReport {
    credentials: Vec<CredentialView>,
    names: Names,
}
impl Report for CredentialsReport {
    type Json = [CredentialView];
    fn json(&self) -> &[CredentialView] {
        &self.credentials
    }
    fn render_human(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
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
        }
        Ok(())
    }
}

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
impl Report for LinkReport {
    type Json = Self;
    fn json(&self) -> &Self {
        self
    }
    fn render_human(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
        out.line(&self.url)
    }
}

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
