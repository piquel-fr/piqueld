//! Account access: `whoami` and `account list|access|invite|enroll`.
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
    Client,
    access::{Grant, Grants, Permission, Preset, Scope},
    auth::{Account, Directory, Manage, Session},
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
    /// Resolves `--app` names and builds the selected grants.
    async fn grants(&self, client: &Client) -> Result<Grants> {
        if self.preset.is_none() && self.permissions.is_empty() {
            return Err(CliError::new(
                ErrorKind::Input,
                "select grants with --preset or --permission",
            ));
        }
        let scope = if self.applications.is_empty() {
            Scope::All
        } else {
            let mut ids = std::collections::BTreeSet::new();
            for application in &self.applications {
                let view = crate::commands::resolve_application(client, application).await?;
                ids.insert(view.application.id().clone());
            }
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
        out.line(format_args!("{} ({})", user.username, user.id))?;
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
}
