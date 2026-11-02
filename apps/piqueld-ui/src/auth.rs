//! Sign-in routing, grant selection, and form parsing behind the dashboard's
//! authentication and account pages, independent of the browser.
use piqueld_client::{
    ApplicationId,
    access::{Grant, Grants, Permission, Scope},
    auth::{DeviceRequest, Manage, Session},
    tailnet::TailnetBinding,
};
use std::collections::BTreeSet;

/// A secret link being redeemed on the sign-in page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invite {
    /// `#invite=`: creates an account, or the first one during setup.
    Account(String),
    /// `#enroll=`: adds a passkey to an existing account.
    Enrollment(String),
    /// `#operator=`: signs the browser in as the host operator.
    Operator(String),
}

impl Invite {
    /// Reads the secret of an invitation, enrollment, or host operator sign-in
    /// link from its URL fragment (`#invite=…`).
    #[must_use]
    pub fn parse(fragment: &str) -> Option<Self> {
        if let Some(secret) = fragment.strip_prefix("#invite=") {
            Some(Self::Account(secret.to_owned()))
        } else if let Some(secret) = fragment.strip_prefix("#operator=") {
            Some(Self::Operator(secret.to_owned()))
        } else {
            Some(Self::Enrollment(
                fragment.strip_prefix("#enroll=")?.to_owned(),
            ))
        }
    }
}

/// What the `/dashboard/auth` page shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignInPage {
    /// Redeem a secret link, whoever is signed in.
    Redeem(Invite),
    /// First-run instructions, before any account exists.
    Setup,
    /// Passkey sign-in.
    SignIn,
    /// The CLI approval page while the host operator is signed in: CLI logins
    /// sign the CLI in as an account, so the operator cannot approve them.
    OperatorCannotApprove,
    /// CLI approval by `who`, reviewing `code` straight away unless empty.
    DeviceApproval {
        /// Name of the signed-in account.
        who: String,
        /// Code from the CLI's link, or empty to enter one.
        code: String,
    },
    /// Signed in as `who`, with a link to the dashboard.
    SignedIn {
        /// Name of the signed-in principal.
        who: String,
    },
}

impl SignInPage {
    /// Chooses the page from the link being redeemed, whether the first
    /// account exists, the current session, and the URL fragment, which marks
    /// the CLI approval page with `#device` or `#device=CODE`. The approval
    /// code stays in the fragment, so it survives the reload after sign-in.
    #[must_use]
    pub fn new(
        invitation: Option<Invite>,
        initialized: bool,
        session: Option<&Session>,
        fragment: &str,
    ) -> Self {
        let device = fragment
            .strip_prefix("#device")
            .and_then(|rest| match rest {
                "" => Some(String::new()),
                code => code.strip_prefix('=').map(str::to_owned),
            });
        match (invitation, session) {
            (Some(invite), _) => Self::Redeem(invite),
            (None, _) if !initialized => Self::Setup,
            (None, None) => Self::SignIn,
            (None, Some(session)) => {
                let who = session.principal.name().to_owned();
                match device {
                    Some(_) if session.principal.user().is_none() => Self::OperatorCannotApprove,
                    Some(code) => Self::DeviceApproval { who, code },
                    None => Self::SignedIn { who },
                }
            }
        }
    }
}

/// Labels for a pending CLI login on its approval page.
pub trait DeviceRequestLabels {
    /// Where the request originated, e.g. `network address 192.0.2.1`.
    fn requester_label(&self) -> String;
    /// Coarse age, e.g. `less than a minute ago` or `2 minutes ago`.
    fn age_label(&self) -> String;
}

impl DeviceRequestLabels for DeviceRequest {
    fn requester_label(&self) -> String {
        self.requester.as_deref().map_or_else(
            || "the daemon's local Unix socket".into(),
            |address| format!("network address {address}"),
        )
    }

    fn age_label(&self) -> String {
        match self.age / 60 {
            0 => "less than a minute ago".into(),
            1 => "1 minute ago".into(),
            minutes => format!("{minutes} minutes ago"),
        }
    }
}

/// Human-readable grant lists.
pub trait DescribeGrants {
    /// One line per grant, e.g. `apps:deploy on blog, shop`, naming
    /// applications with `name`.
    fn describe(&self, name: impl Fn(&ApplicationId) -> String) -> Vec<String>;
}

impl DescribeGrants for Grants {
    fn describe(&self, name: impl Fn(&ApplicationId) -> String) -> Vec<String> {
        self.to_list()
            .iter()
            .map(|grant: &Grant| match &grant.applications {
                None if grant.permission.scopable() => {
                    format!("{} on every application", grant.permission)
                }
                None => grant.permission.to_string(),
                Some(ids) => format!(
                    "{} on {}",
                    grant.permission,
                    ids.iter().map(&name).collect::<Vec<_>>().join(", ")
                ),
            })
            .collect()
    }
}

/// The grant editor's selection: permissions, and one scope (every
/// application, or chosen ones) shared by all application permissions.
/// Installation-wide permissions always apply everywhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantSelection {
    /// Selected permissions.
    pub permissions: BTreeSet<Permission>,
    /// Whether application permissions apply to every application.
    pub everywhere: bool,
    /// Applications they apply to otherwise.
    pub applications: BTreeSet<ApplicationId>,
    /// Whether the grants it was read from limit application permissions to
    /// different applications, which one scope cannot show: changing the
    /// selection replaces them with it.
    pub mixed: bool,
}

impl From<&Grants> for GrantSelection {
    fn from(grants: &Grants) -> Self {
        let list = grants.to_list();
        let scopes: BTreeSet<_> = list
            .iter()
            .filter(|grant| grant.permission.scopable())
            .map(|grant| grant.applications.clone())
            .collect();
        Self {
            permissions: list.iter().map(|grant| grant.permission).collect(),
            everywhere: scopes.iter().all(Option::is_none),
            applications: scopes.iter().flatten().flatten().cloned().collect(),
            mixed: scopes.len() > 1,
        }
    }
}

impl GrantSelection {
    /// The selected grants: application permissions on the selected scope,
    /// installation-wide ones everywhere.
    #[must_use]
    pub fn grants(&self) -> Grants {
        let scope = if self.everywhere {
            Scope::All
        } else {
            Scope::Only(self.applications.clone())
        };
        let mut grants = Grants::default();
        for permission in &self.permissions {
            grants.grant_within(*permission, &scope);
        }
        grants
    }
}

/// The new API token form, as typed.
#[derive(Clone, Debug, Default)]
pub struct TokenForm {
    /// Token name.
    pub name: String,
    /// Lifetime in days; blank never expires.
    pub days: String,
    /// Tailnet login or tag the token is bound to; blank for none.
    pub tailnet: String,
}

impl TokenForm {
    /// The command creating this token with `grants`.
    ///
    /// # Errors
    ///
    /// Describes the first invalid field: the lifetime, then the tailnet
    /// binding.
    pub fn command(self, grants: Grants) -> Result<Manage, &'static str> {
        let days = if self.days.is_empty() {
            None
        } else {
            Some(
                self.days
                    .parse::<u32>()
                    .map_err(|_| "Enter a positive number of days, or leave blank")?,
            )
        };
        let tailnet = self.tailnet.trim();
        let tailnet = if tailnet.is_empty() {
            None
        } else {
            Some(TailnetBinding::parse(tailnet)?)
        };
        Ok(Manage::CreateToken {
            grants,
            name: self.name,
            days,
            tailnet,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use piqueld_client::{
        access::{AppPermission, GlobalPermission},
        auth::{HostOperator, Principal, User},
    };

    fn app(id: &str) -> ApplicationId {
        ApplicationId::parse(id).unwrap()
    }

    fn session(principal: Principal) -> Session {
        Session {
            principal,
            grants: Grants::default(),
            scoped: false,
        }
    }

    fn alice() -> Session {
        session(Principal::User(User {
            id: "alice-id".into(),
            username: "alice".into(),
            display_name: String::new(),
        }))
    }

    fn grants(list: &[(Permission, Scope)]) -> Grants {
        let mut grants = Grants::default();
        for (permission, scope) in list {
            grants.grant(*permission, scope).unwrap();
        }
        grants
    }

    const READ: Permission = Permission::App(AppPermission::Read);
    const DEPLOY: Permission = Permission::App(AppPermission::Deploy);
    const MANAGE: Permission = Permission::Global(GlobalPermission::AccountsManage);

    #[test]
    fn invite_links_are_recognized_only_by_their_exact_prefix() {
        assert_eq!(
            Invite::parse("#operator=s3cret"),
            Some(Invite::Operator("s3cret".into()))
        );
        assert_eq!(
            Invite::parse("#enroll=abc"),
            Some(Invite::Enrollment("abc".into()))
        );
        for fragment in ["", "#invite", "#device=ABCD-EFGH", "#enrollment=abc"] {
            assert_eq!(Invite::parse(fragment), None, "{fragment}");
        }
    }

    #[test]
    fn a_secret_link_wins_over_setup_and_an_existing_session() {
        let invite = Invite::Operator("secret".into());
        let page = SignInPage::new(Some(invite.clone()), false, Some(&alice()), "");
        assert_eq!(page, SignInPage::Redeem(invite));
        assert_eq!(SignInPage::new(None, false, None, ""), SignInPage::Setup);
    }

    #[test]
    fn the_device_fragment_opens_approval_once_signed_in_as_an_account() {
        let alice = alice();
        let page = |fragment| SignInPage::new(None, true, Some(&alice), fragment);
        let approval = |code: &str| SignInPage::DeviceApproval {
            who: "alice".into(),
            code: code.into(),
        };
        assert_eq!(page("#device"), approval(""));
        assert_eq!(page("#device="), approval(""));
        assert_eq!(page("#device=ABCD-EFGH"), approval("ABCD-EFGH"));
        let signed_in = SignInPage::SignedIn {
            who: "alice".into(),
        };
        assert_eq!(page("#devices"), signed_in);
        assert_eq!(page(""), signed_in);
        // Signed out, the page asks for a passkey first.
        assert_eq!(
            SignInPage::new(None, true, None, "#device=ABCD-EFGH"),
            SignInPage::SignIn
        );
    }

    #[test]
    fn the_host_operator_cannot_approve_cli_logins() {
        let operator = session(Principal::Operator(HostOperator { uid: 0 }));
        assert_eq!(
            SignInPage::new(None, true, Some(&operator), "#device=ABCD-EFGH"),
            SignInPage::OperatorCannotApprove
        );
        assert_eq!(
            SignInPage::new(None, true, Some(&operator), ""),
            SignInPage::SignedIn {
                who: "Host operator".into()
            }
        );
    }

    #[test]
    fn device_request_age_rounds_down_to_whole_minutes() {
        let request = |age| DeviceRequest {
            user_code: "ABCD-EFGH".into(),
            requester: None,
            age,
            expires_in: 600,
            grants: None,
        };
        for (age, label) in [
            (59, "less than a minute ago"),
            (60, "1 minute ago"),
            (119, "1 minute ago"),
            (120, "2 minutes ago"),
        ] {
            assert_eq!(request(age).age_label(), label, "{age}");
        }
    }

    #[test]
    fn grant_descriptions_name_scoped_applications_only() {
        let grants = grants(&[
            (READ, Scope::All),
            (
                DEPLOY,
                Scope::Only([app("app-blog"), app("app-shop")].into()),
            ),
            (MANAGE, Scope::All),
        ]);
        let lines = grants.describe(|id| id.to_string().to_uppercase());
        assert_eq!(
            lines,
            [
                "apps:read on every application",
                "apps:deploy on APP-BLOG, APP-SHOP",
                "accounts:manage",
            ]
        );
    }

    #[test]
    fn a_single_scope_selection_round_trips() {
        let only_blog = grants(&[
            (READ, Scope::Only([app("app-blog")].into())),
            (DEPLOY, Scope::Only([app("app-blog")].into())),
            (MANAGE, Scope::All),
        ]);
        let selection = GrantSelection::from(&only_blog);
        assert!(!selection.everywhere && !selection.mixed);
        assert_eq!(selection.applications, [app("app-blog")].into());
        assert_eq!(selection.grants(), only_blog);
        // Installation-wide permissions alone count as every application.
        assert!(GrantSelection::from(&grants(&[(MANAGE, Scope::All)])).everywhere);
    }

    #[test]
    fn mixed_scopes_are_flagged_and_replaced_by_their_union() {
        let mixed = grants(&[
            (READ, Scope::Only([app("app-blog")].into())),
            (DEPLOY, Scope::Only([app("app-shop")].into())),
        ]);
        let selection = GrantSelection::from(&mixed);
        assert!(selection.mixed && !selection.everywhere);
        let both = Scope::Only([app("app-blog"), app("app-shop")].into());
        assert_eq!(
            selection.grants(),
            grants(&[(READ, both.clone()), (DEPLOY, both)])
        );
    }

    #[test]
    fn no_chosen_application_keeps_only_installation_wide_permissions() {
        let selection = GrantSelection {
            permissions: [READ, MANAGE].into(),
            everywhere: false,
            applications: BTreeSet::new(),
            mixed: false,
        };
        assert_eq!(selection.grants(), grants(&[(MANAGE, Scope::All)]));
    }

    #[test]
    fn token_form_validates_lifetime_before_tailnet() {
        let form = |days: &str, tailnet: &str| {
            TokenForm {
                name: "ci".into(),
                days: days.into(),
                tailnet: tailnet.into(),
            }
            .command(Grants::default())
        };
        let Ok(Manage::CreateToken { days, tailnet, .. }) = form("", "  tag:ci ") else {
            panic!("blank lifetime and a padded tag are valid");
        };
        assert_eq!(
            (days, tailnet),
            (None, Some(TailnetBinding::parse("tag:ci").unwrap()))
        );
        let days_error = "Enter a positive number of days, or leave blank";
        assert_eq!(form("-1", "not a binding").unwrap_err(), days_error);
        assert_eq!(form("1.5", "").unwrap_err(), days_error);
        assert!(form("30", "not a binding").is_err());
    }
}
