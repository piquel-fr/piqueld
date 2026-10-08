//! Passkey and account management API. Bearer credentials use the same request
//! pipeline as application operations; browser callers use same-origin cookies.
use crate::{Client, ClientError};
pub use piqueld_core::auth::*;
impl Client {
    /// Returns initialization state and the canonical website origin.
    /// # Errors
    /// Returns transport, decoding, or API failures.
    pub async fn auth_status(&self) -> Result<AuthStatus, ClientError> {
        crate::client::generated_result(self.generated.auth_status().await).await
    }
    /// Returns the first-account setup link. Only the daemon's Unix socket serves it.
    /// # Errors
    /// Returns setup-completed, transport, decoding, or API failures.
    pub async fn auth_setup_link(&self) -> Result<SetupLink, ClientError> {
        crate::client::generated_result(self.generated.auth_setup_link().await).await
    }
    /// Issues a one-time admin recovery link. Only the daemon's Unix socket
    /// serves it, and only to root or the daemon's own user.
    /// # Errors
    /// Returns setup-pending, transport, decoding, or API failures.
    pub async fn auth_recover_admin(&self) -> Result<RecoveryLink, ClientError> {
        crate::client::generated_result(self.generated.auth_recover_admin().await).await
    }
    /// Returns the signed-in account and what the current credential may do.
    /// # Errors
    /// Returns authentication, transport, decoding, or API failures.
    pub async fn auth_me(&self) -> Result<Session, ClientError> {
        crate::client::generated_result(self.generated.auth_me().await).await
    }
    /// Starts passkey registration for an invitation, an enrollment link, or
    /// the signed-in account itself.
    /// # Errors
    /// Returns authentication, transport, decoding, or API failures.
    pub async fn auth_register_start(
        &self,
        input: &RegistrationStart,
    ) -> Result<Ceremony, ClientError> {
        crate::client::generated_result(self.generated.auth_registration_start(input).await).await
    }
    /// Completes passkey registration, signing in accounts that redeemed an
    /// invitation, setup, or enrollment secret.
    /// # Errors
    /// Returns authentication, transport, decoding, or API failures.
    pub async fn auth_register_finish(&self, input: &CeremonyFinish) -> Result<User, ClientError> {
        crate::client::generated_result(self.generated.auth_registration_finish(input).await).await
    }
    /// Starts username-less passkey login.
    /// # Errors
    /// Returns transport, decoding, or API failures.
    pub async fn auth_login_start(&self) -> Result<Ceremony, ClientError> {
        crate::client::generated_result(self.generated.auth_login_start().await).await
    }
    /// Completes passkey login and sets the browser session cookie.
    /// # Errors
    /// Returns authentication, transport, decoding, or API failures.
    pub async fn auth_login_finish(&self, input: &CeremonyFinish) -> Result<User, ClientError> {
        crate::client::generated_result(self.generated.auth_login_finish(input).await).await
    }
    /// Revokes the credential used for this request.
    /// # Errors
    /// Returns authentication, transport, decoding, or API failures.
    pub async fn auth_logout(&self) -> Result<Managed, ClientError> {
        crate::client::generated_result(self.generated.auth_logout().await).await
    }
    /// Lists the accounts the caller may see with their grants, passkeys,
    /// sessions, tokens, and pending invitations.
    /// # Errors
    /// Returns authentication, transport, decoding, or API failures.
    pub async fn auth_directory(&self) -> Result<Directory, ClientError> {
        crate::client::generated_result(self.generated.auth_directory().await).await
    }
    /// Applies one account change. Changing other accounts requires
    /// `accounts:manage` and every grant they hold.
    /// # Errors
    /// Returns authentication, transport, decoding, or API failures.
    pub async fn auth_manage(&self, input: &Manage) -> Result<Managed, ClientError> {
        crate::client::generated_result(self.generated.auth_manage(input).await).await
    }
    /// Starts a browser-assisted CLI login, limited to `grants` when set.
    /// # Errors
    /// Returns transport, decoding, or API failures.
    pub async fn auth_device_start(
        &self,
        grants: Option<piqueld_core::access::Grants>,
    ) -> Result<DeviceStart, ClientError> {
        crate::client::generated_result(
            self.generated
                .auth_device_start(&DeviceStartRequest { grants })
                .await,
        )
        .await
    }
    /// Polls for the one-time result of a device login.
    /// # Errors
    /// Returns expiry, transport, decoding, or API failures.
    pub async fn auth_device_poll(&self, device_code: &str) -> Result<DeviceToken, ClientError> {
        crate::client::generated_result(
            self.generated
                .auth_device_poll(&DevicePoll {
                    device_code: device_code.into(),
                })
                .await,
        )
        .await
    }
    /// Describes a pending device login so the approver can check its origin.
    /// # Errors
    /// Returns invalid-code, authentication, transport, decoding, or API failures.
    pub async fn auth_device_inspect(&self, user_code: &str) -> Result<DeviceRequest, ClientError> {
        crate::client::generated_result(
            self.generated
                .auth_device_inspect(&DeviceApprove {
                    user_code: user_code.into(),
                })
                .await,
        )
        .await
    }
    /// Explicitly approves the device code entered in the browser.
    /// # Errors
    /// Returns invalid-code, authentication, transport, decoding, or API failures.
    pub async fn auth_device_approve(&self, user_code: &str) -> Result<Managed, ClientError> {
        crate::client::generated_result(
            self.generated
                .auth_device_approve(&DeviceApprove {
                    user_code: user_code.into(),
                })
                .await,
        )
        .await
    }
}
