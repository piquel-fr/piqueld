//! Installation checks for `GET /api/v1/system/checks`: each inspects the
//! live installation read-only and suggests a next step when it is not as
//! expected. Checks owned by other parts of the daemon live with them, such
//! as [`crate::ingress::Ingress::docker_check`].

use super::{ApplicationError, ApplicationService};
use crate::ingress::Ingress;
use piqueld_core::api::{CheckStatus, SystemCheck, SystemCheckId, SystemChecks, TailnetStatus};
use std::{os::unix::fs::MetadataExt, path::PathBuf, time::Duration};

/// Longest a check that talks to the network or Docker may take.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Effective settings the checks inspect, captured at startup once the
/// tailnet node may have filled `auth.public_url`.
pub(super) struct CheckSettings {
    public_url: String,
    runtime_dir: PathBuf,
}

impl From<&crate::config::DaemonConfig> for CheckSettings {
    fn from(config: &crate::config::DaemonConfig) -> Self {
        Self {
            public_url: config.public_url().to_owned(),
            runtime_dir: config.server.runtime_dir.clone(),
        }
    }
}

impl CheckSettings {
    /// Checks that the website origin is HTTPS and answers piqueld's
    /// liveness probe with a trusted certificate. Reaching it from the
    /// daemon's host does not prove browsers elsewhere can, nor the reverse.
    async fn public_url_check(&self) -> SystemCheck {
        let check =
            |status, message: String| SystemCheck::new(SystemCheckId::PublicUrl, status, message);
        let mut health = match crate::auth::Auth::validate_origin(&self.public_url) {
            Ok(url) => url,
            Err(error) => {
                return check(
                    CheckStatus::Fail,
                    format!("auth.public_url is invalid: {error}"),
                );
            }
        };
        let origin = health.origin().ascii_serialization();
        if health.scheme() != "https" {
            return check(
                CheckStatus::Warn,
                format!("{origin} is a local development origin: passkeys work only in a browser on this host"),
            )
            .next_step("Set auth.public_url to an HTTPS origin, or enable [tailscale].");
        }
        health.set_path("/health");
        let probe = reqwest::Client::builder()
            .timeout(CHECK_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map(|client| client.get(health.clone()).send());
        let response = match probe {
            Ok(request) => request.await,
            Err(error) => {
                return check(
                    CheckStatus::Fail,
                    format!("could not create an HTTPS client: {error}"),
                );
            }
        };
        match response {
            Ok(response) => {
                let status = response.status();
                if status.is_success() && Self::is_liveness(response).await {
                    check(
                        CheckStatus::Pass,
                        format!("{origin} answers from this host with a trusted certificate; browsers elsewhere also need DNS and network access to it"),
                    )
                } else {
                    check(
                        CheckStatus::Warn,
                        format!("{health} answered {status} without piqueld's liveness response"),
                    )
                    .next_step("Point auth.public_url's host at this daemon, or forward its requests to piqueld.")
                }
            }
            Err(error) if Self::untrusted_certificate(&error) => check(
                CheckStatus::Fail,
                format!("{origin} serves a certificate this host does not trust: {}", Self::causes(&error)),
            )
            .next_step("Serve a certificate from a publicly trusted CA for this hostname; browsers reject it too."),
            Err(error) => check(
                CheckStatus::Warn,
                format!("{origin} is not reachable from this host: {}", Self::causes(&error)),
            )
            .next_step("Check its DNS records and firewall. Browsers may still reach it when this host cannot, such as without NAT hairpinning."),
        }
    }

    /// Whether `response` carries piqueld's liveness body, `{"status":"ok"}`.
    /// At most 1 KiB is read, so another site's page cannot stall the check.
    async fn is_liveness(mut response: reqwest::Response) -> bool {
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) if body.len() + chunk.len() <= 1024 => {
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                // Too long, or the body broke off before its end.
                Ok(Some(_)) | Err(_) => return false,
            }
        }
        serde_json::from_slice::<serde_json::Value>(&body)
            .is_ok_and(|body| body == serde_json::json!({"status": "ok"}))
    }

    /// Whether a request failed because the server's certificate is untrusted.
    fn untrusted_certificate(error: &reqwest::Error) -> bool {
        Self::causes(error).contains("certificate")
    }

    /// The error and its causes, as one line.
    fn causes(error: &(dyn std::error::Error + 'static)) -> String {
        std::iter::successors(Some(error), |error| error.source())
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(": ")
    }

    /// Checks the runtime directory as startup does, and that the API socket
    /// in it is `0660` with the daemon's group, so group members can connect.
    async fn runtime_check(&self) -> SystemCheck {
        let check = |status, message: String| {
            SystemCheck::new(SystemCheckId::RuntimeDirectory, status, message)
        };
        let directory = self.runtime_dir.display();
        if let Err(error) = crate::directories::validate_runtime_dir(&self.runtime_dir).await {
            return check(CheckStatus::Fail, format!("{directory}: {error}")).next_step(
                "Give the runtime directory the daemon's owner and group with mode 0750, as systemd's RuntimeDirectory= does.",
            );
        }
        let socket = self.runtime_dir.join(crate::runtime_dir::SOCKET_NAME);
        let group = rustix::process::getegid().as_raw();
        match tokio::fs::symlink_metadata(&socket).await {
            Ok(metadata) if metadata.mode() & 0o7777 == 0o660 && metadata.gid() == group => check(
                CheckStatus::Pass,
                format!(
                    "{} is mode 0660 with the daemon's group (GID {group}); add users to that group to let them connect",
                    socket.display()
                ),
            ),
            Ok(metadata) => check(
                CheckStatus::Fail,
                format!(
                    "{} has mode {:04o} and GID {}, not 0660 and the daemon's GID {group}",
                    socket.display(),
                    metadata.mode() & 0o7777,
                    metadata.gid()
                ),
            )
            .next_step(
                "Restart piqueld, which recreates the socket; nothing else should change it.",
            ),
            Err(error) => check(
                CheckStatus::Fail,
                format!("could not inspect {}: {error}", socket.display()),
            )
            .next_step(
                "Restart piqueld, which recreates the socket; nothing else should remove it.",
            ),
        }
    }
}

impl ApplicationService {
    /// Runs every installation check concurrently, each bounded by
    /// [`CHECK_TIMEOUT`] where it talks to the network or Docker. Their
    /// order is stable.
    ///
    /// # Errors
    /// Returns [`ApplicationError::ConfigurationUnavailable`] when the
    /// service was built without daemon configuration.
    pub async fn system_checks(&self) -> Result<SystemChecks, ApplicationError> {
        let settings = &self.configuration()?.checks;
        let ingress = self.ingress.as_deref();
        let (public_url, docker, ports, secret_key, runtime) = tokio::join!(
            settings.public_url_check(),
            Self::ingress_check(SystemCheckId::Docker, ingress.map(Ingress::docker_check)),
            Self::ingress_check(
                SystemCheckId::IngressPorts,
                ingress.map(Ingress::ports_check)
            ),
            self.store.secret_key_check(),
            settings.runtime_check(),
        );
        Ok(SystemChecks {
            checks: vec![
                public_url,
                self.tailnet_check(),
                docker,
                ports,
                secret_key,
                runtime,
                Self::backups_check(),
            ],
        })
    }

    /// Reports the tailnet node's login and certificate from its latest
    /// status, and whether `auth.public_url` is its origin.
    fn tailnet_check(&self) -> SystemCheck {
        let status = self
            .tailnet
            .as_ref()
            .map(|status| status.borrow().clone())
            .unwrap_or_default();
        let check =
            |status, message: &str| SystemCheck::new(SystemCheckId::Tailnet, status, message);
        let TailnetStatus {
            enabled,
            healthy,
            public_url_matches,
            ..
        } = status;
        match (enabled, healthy, public_url_matches) {
            (false, ..) => check(CheckStatus::Skip, "[tailscale] is disabled"),
            (true, false, _) => check(CheckStatus::Fail, &status.message).next_step(
                "Log the node in and make sure HTTPS certificates are enabled for the tailnet; daemon logs have details.",
            ),
            (true, true, false) => check(CheckStatus::Warn, &status.message).next_step(
                "Unset auth.public_url to use the node's origin, or keep it if passkeys belong to another origin.",
            ),
            (true, true, true) => check(
                CheckStatus::Pass,
                &format!(
                    "{} is logged in and serves auth.public_url",
                    status.dns_name.as_deref().unwrap_or("the node")
                ),
            ),
        }
    }

    /// Backups are not implemented yet, so their freshness cannot be
    /// checked. Once they are, this reports the latest backup's age.
    fn backups_check() -> SystemCheck {
        SystemCheck::new(
            SystemCheckId::Backups,
            CheckStatus::Skip,
            "piqueld does not take backups yet, so their freshness is not checked",
        )
        .next_step("Back up the data directory, including secrets.key, with your own tooling.")
    }

    /// Runs an ingress `check`, failing check `id` if it takes longer than
    /// [`CHECK_TIMEOUT`]. Skipped without ingress, as in tests.
    async fn ingress_check(
        id: SystemCheckId,
        check: Option<impl Future<Output = SystemCheck>>,
    ) -> SystemCheck {
        let Some(check) = check else {
            return SystemCheck::new(
                id,
                CheckStatus::Skip,
                "ingress is not managed by this daemon",
            );
        };
        tokio::time::timeout(CHECK_TIMEOUT, check)
            .await
            .unwrap_or_else(|_| {
                SystemCheck::new(
                    id,
                    CheckStatus::Fail,
                    "Docker Engine did not answer in time",
                )
                .next_step("Check that Docker Engine is running and responsive.")
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn only_piqueld_liveness_responses_pass() {
        for (body, expected) in [
            (r#"{"status":"ok"}"#.to_owned(), true),
            ("<!doctype html><title>Welcome</title>".into(), false),
            (
                format!(r#"{{"status":"ok","padding":"{}"}}"#, "x".repeat(2048)),
                false,
            ),
        ] {
            let response = reqwest::Response::from(http::Response::new(body));
            assert_eq!(CheckSettings::is_liveness(response).await, expected);
        }
        // A body that breaks off after the expected JSON is not complete.
        let truncated = futures_util::stream::iter([
            Ok(br#"{"status":"ok"}"#.to_vec()),
            Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset)),
        ]);
        let response =
            reqwest::Response::from(http::Response::new(reqwest::Body::wrap_stream(truncated)));
        assert!(!CheckSettings::is_liveness(response).await);
    }

    #[tokio::test]
    async fn the_public_url_is_checked_as_a_normalized_origin() {
        let settings = CheckSettings {
            public_url: "HTTP://LOCALHOST:7845/".into(),
            runtime_dir: PathBuf::from("/run/piqueld"),
        };
        let check = settings.public_url_check().await;
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check
                .message
                .starts_with("http://localhost:7845 is a local development origin"),
            "{}",
            check.message
        );
    }
}
