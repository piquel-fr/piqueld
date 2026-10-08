//! Authentication contracts shared by the daemon, browser, and CLI.
use crate::access::Grants;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Public account information.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct User {
    /// Immutable `WebAuthn` user handle.
    pub id: String,
    /// Unique, editable account name.
    pub username: String,
    /// Optional human-readable name (empty when unset).
    pub display_name: String,
}
/// The signed-in account and what the current credential may do.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Session {
    /// Signed-in account.
    pub user: User,
    /// Effective grants of the credential used for this request.
    pub grants: Grants,
    /// Whether the credential is limited to its own grants, like an API
    /// token; such credentials cannot create credentials.
    pub scoped: bool,
}
/// An account and its grants.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Account {
    /// Account information.
    pub user: User,
    /// What the account may do.
    pub grants: Grants,
}
/// Public initialization state and canonical browser origin.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct AuthStatus {
    /// Whether the first account has been created.
    pub initialized: bool,
    /// Canonical website origin.
    pub public_url: String,
}
/// First-account setup link, served only over the daemon's Unix socket.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct SetupLink {
    /// Dashboard URL carrying the single-use setup secret.
    pub url: String,
}
/// One-time admin recovery link, issued only over the daemon's Unix socket
/// to root or the daemon's own user.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct RecoveryLink {
    /// Dashboard URL that registers a new account with `admin`.
    pub url: String,
    /// Unix seconds after which the link no longer works.
    pub expires_at: i64,
}
/// Passkey registration: a new account redeeming an invitation or setup
/// secret, an existing account redeeming an enrollment link, or a signed-in
/// account adding a passkey to itself.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct RegistrationStart {
    /// Invitation, enrollment, or initial setup secret; absent when a signed-in
    /// account adds a passkey to itself.
    pub invitation: Option<String>,
    /// The signed-in caller's own account, when adding a passkey to itself.
    pub user_id: Option<String>,
    /// New account name; ignored for enrollment.
    pub username: String,
    /// New account display name; ignored for enrollment.
    pub display_name: String,
    /// Human-readable passkey label.
    pub passkey_name: String,
}
/// Server-generated, browser-bound `WebAuthn` challenge.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Ceremony {
    /// Single-use ceremony identifier.
    pub id: String,
    /// `WebAuthn` creation/request options, including `publicKey`.
    pub options: serde_json::Value,
}
/// Browser's signed response to a pending ceremony.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct CeremonyFinish {
    /// Pending ceremony identifier.
    pub id: String,
    /// Serialized `WebAuthn` credential response.
    pub credential: serde_json::Value,
}
/// Named enrolled authenticator, excluding its internal credential data.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct PasskeyView {
    /// Credential identifier.
    pub id: String,
    /// Account identifier.
    pub user_id: String,
    /// Editable label.
    pub name: String,
}
/// Revocable browser session, CLI session, or automation token metadata.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct CredentialView {
    /// Grants this credential is limited to, within its account's current
    /// access; absent when it acts with the account's full access.
    pub grants: Option<Grants>,
    /// Revocation identifier, never a bearer secret.
    pub id: String,
    /// Account identifier.
    pub user_id: String,
    /// `browser`, `cli`, or `token`.
    pub kind: String,
    /// Human-readable label.
    pub name: String,
    /// Last use, as Unix seconds.
    pub last_used_at: i64,
    /// Absolute expiry, as Unix seconds; absent for non-expiring tokens.
    pub expires_at: Option<i64>,
    /// Tailnet user or tag this token is bound to, if any.
    pub tailnet: Option<crate::tailnet::TailnetBinding>,
}
/// Pending invitation metadata. Its secret is returned only at creation.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct InvitationView {
    /// Revocation identifier.
    pub id: String,
    /// Issuing account.
    pub issuer_id: String,
    /// Existing account an enrollment link adds a passkey to; absent for
    /// invitations that create an account.
    pub user_id: Option<String>,
    /// Expiry as Unix seconds.
    pub expires_at: i64,
    /// Grants the created account receives; empty for enrollment links.
    pub grants: Grants,
}
/// Account management directory. Callers with `accounts:manage` see every
/// account and invitation; everyone else sees only their own account.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Directory {
    /// Accounts with their grants.
    pub users: Vec<Account>,
    /// Enrolled passkeys.
    pub passkeys: Vec<PasskeyView>,
    /// Sessions and tokens, without secrets.
    pub credentials: Vec<CredentialView>,
    /// Unexpired invitations.
    pub invitations: Vec<InvitationView>,
}
/// Account management commands.
///
/// Every account may manage itself: its profile, passkeys, sessions, and tokens.
/// Changing another account requires `accounts:manage` and that the caller
/// holds every grant of that account; grants can only be handed out by
/// someone who holds them. Passkeys are only added by their owner, so others
/// receive an enrollment link instead.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Manage {
    /// Edit an account's profile.
    UpdateUser {
        /// Target account.
        user_id: String,
        /// Unique name.
        username: String,
        /// Display name.
        display_name: String,
    },
    /// Delete an account, unless no administrator with a passkey would remain.
    DeleteUser {
        /// Target account.
        user_id: String,
    },
    /// Remove an authenticator without revoking existing sessions.
    RemovePasskey {
        /// Passkey identifier.
        id: String,
    },
    /// Rename an authenticator.
    RenamePasskey {
        /// Passkey identifier.
        id: String,
        /// New label.
        name: String,
    },
    /// Revoke a session or API token.
    RevokeCredential {
        /// Credential identifier.
        id: String,
    },
    /// Revoke all sessions and tokens belonging to an account.
    RevokeAll {
        /// Target account.
        user_id: String,
    },
    /// Replace an account's grants.
    SetGrants {
        /// Target account.
        user_id: String,
        /// Complete new grant list.
        grants: Grants,
    },
    /// Create a transferable invitation, valid for 24 hours, whose account
    /// receives `grants`.
    CreateInvitation {
        /// Grants for the created account.
        grants: Grants,
    },
    /// Create a single-use link, valid for 24 hours, that adds a passkey to an
    /// existing account.
    CreateEnrollment {
        /// Target account.
        user_id: String,
    },
    /// Revoke a pending invitation.
    RevokeInvitation {
        /// Invitation identifier.
        id: String,
    },
    /// Create an automation token for the caller's own account; callers supply
    /// 90 days for the default. The token acts with `grants`, limited by the
    /// account's current access, and cannot create credentials itself.
    CreateToken {
        /// Grants the token is limited to; the caller must hold them.
        grants: Grants,
        /// Token label.
        name: String,
        /// Lifetime in days; absent means no expiry.
        days: Option<u32>,
        /// Tailnet user or tag the token is bound to; absent accepts it
        /// from anywhere.
        #[serde(default)]
        tailnet: Option<crate::tailnet::TailnetBinding>,
    },
}
/// Management result. Secrets are disclosed only once.
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
pub struct Managed {
    /// Newly created automation token.
    pub token: Option<String>,
    /// Newly created invitation or enrollment link.
    pub invitation_url: Option<String>,
}
/// Optional limits for a CLI device login.
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
pub struct DeviceStartRequest {
    /// Grants the issued CLI session is limited to; absent for the approving
    /// account's full access.
    #[serde(default)]
    pub grants: Option<Grants>,
}
/// Device login challenge for an interactive CLI.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct DeviceStart {
    /// Secret used only by the initiating CLI to poll.
    pub device_code: String,
    /// Human-readable code explicitly confirmed in the browser.
    pub user_code: String,
    /// Canonical browser login page.
    pub verification_uri: String,
    /// Lifetime in seconds.
    pub expires_in: u32,
    /// Minimum polling interval, in seconds.
    pub interval: u32,
    /// Network address the daemon observed for this request, shown again on
    /// the approval page. Absent for requests made over the Unix socket.
    pub requester: Option<String>,
    /// Grants the session will be limited to, as requested. Daemons older
    /// than limited logins omit it, so clients can tell they ignored a limit.
    #[serde(default)]
    pub grants: Option<Grants>,
}
/// CLI polling request.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct DevicePoll {
    /// Secret from device initiation.
    pub device_code: String,
}
/// Browser confirmation of the code shown by the initiating CLI.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct DeviceApprove {
    /// Code entered by the user.
    pub user_code: String,
}
/// Pending device login shown to the approver before they confirm it.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct DeviceRequest {
    /// Code entered by the user.
    pub user_code: String,
    /// Network address the daemon observed when the login started. Absent for
    /// requests made over the Unix socket.
    pub requester: Option<String>,
    /// Seconds since the login started.
    pub age: u32,
    /// Seconds until the request expires.
    pub expires_in: u32,
    /// Grants the session is limited to; absent for the approver's full access.
    pub grants: Option<Grants>,
}
/// Poll result; the token is returned exactly once after explicit approval.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct DeviceToken {
    /// `authorization_pending`, `slow_down`, or `complete`.
    pub status: String,
    /// Issued CLI credential.
    pub token: Option<String>,
    /// Account associated with the credential.
    pub user: Option<User>,
}
