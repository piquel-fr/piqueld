//! Authentication contracts shared by the daemon, browser, and CLI.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Public account information; accounts have identical capabilities.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct User {
    /// Immutable `WebAuthn` user handle.
    pub id: String,
    /// Unique, editable account name.
    pub username: String,
    /// Optional human-readable name (empty when unset).
    pub display_name: String,
}
/// Public initialization state and canonical browser origin.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct AuthStatus {
    /// Whether the first account has been created.
    pub initialized: bool,
    /// Canonical website origin.
    pub public_url: String,
}
/// New account information and its single-use invitation or setup secret.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct RegistrationStart {
    /// Invitation or initial setup secret; absent when adding to an existing account.
    pub invitation: Option<String>,
    /// Existing target account; any authenticated user may enroll its passkeys.
    pub user_id: Option<String>,
    /// New account name.
    pub username: String,
    /// New account display name.
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
}
/// Pending invitation metadata. Its secret is returned only at creation.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct InvitationView {
    /// Revocation identifier.
    pub id: String,
    /// Issuing account.
    pub issuer_id: String,
    /// Expiry as Unix seconds.
    pub expires_at: i64,
}
/// Account management directory, available in full to every authenticated user.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Directory {
    /// Accounts.
    pub users: Vec<User>,
    /// Enrolled passkeys.
    pub passkeys: Vec<PasskeyView>,
    /// Sessions and tokens, without secrets.
    pub credentials: Vec<CredentialView>,
    /// Unexpired invitations.
    pub invitations: Vec<InvitationView>,
}
/// Account management commands. There are deliberately no ownership restrictions.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Manage {
    /// Edit any account's profile.
    UpdateUser {
        /// Target account.
        user_id: String,
        /// Unique name.
        username: String,
        /// Display name.
        display_name: String,
    },
    /// Delete any account except the last remaining one.
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
    /// Create a transferable invitation, valid for 24 hours.
    CreateInvitation,
    /// Revoke a pending invitation.
    RevokeInvitation {
        /// Invitation identifier.
        id: String,
    },
    /// Create an automation token; callers supply 90 days for the default.
    CreateToken {
        /// Target account.
        user_id: String,
        /// Token label.
        name: String,
        /// Lifetime in days; absent means no expiry.
        days: Option<u32>,
    },
}
/// Management result. Secrets are disclosed only once.
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
pub struct Managed {
    /// Newly created automation token.
    pub token: Option<String>,
    /// Newly created invitation link.
    pub invitation_url: Option<String>,
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
