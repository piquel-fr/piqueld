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
    /// Returns the signed-in account.
    /// # Errors
    /// Returns authentication, transport, decoding, or API failures.
    pub async fn auth_me(&self) -> Result<User, ClientError> {
        crate::client::generated_result(self.generated.auth_me().await).await
    }
    /// Starts passkey registration for an invitation or an existing account.
    /// # Errors
    /// Returns authentication, transport, decoding, or API failures.
    pub async fn auth_register_start(
        &self,
        input: &RegistrationStart,
    ) -> Result<Ceremony, ClientError> {
        crate::client::generated_result(self.generated.auth_registration_start(input).await).await
    }
    /// Completes passkey registration and signs in a newly created account.
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
    /// Lists accounts, passkeys, sessions, tokens, and pending invitations.
    /// # Errors
    /// Returns authentication, transport, decoding, or API failures.
    pub async fn auth_directory(&self) -> Result<Directory, ClientError> {
        crate::client::generated_result(self.generated.auth_directory().await).await
    }
    /// Changes any account's settings; accounts have equal capabilities.
    /// # Errors
    /// Returns authentication, transport, decoding, or API failures.
    pub async fn auth_manage(&self, input: &Manage) -> Result<Managed, ClientError> {
        crate::client::generated_result(self.generated.auth_manage(input).await).await
    }
    /// Starts a browser-assisted CLI login.
    /// # Errors
    /// Returns transport, decoding, or API failures.
    pub async fn auth_device_start(&self) -> Result<DeviceStart, ClientError> {
        crate::client::generated_result(self.generated.auth_device_start().await).await
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
