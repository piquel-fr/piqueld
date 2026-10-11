//! Installation checks owned by ingress: Docker compatibility and the host
//! ports the gateway publishes.

use super::Ingress;
use piqueld_core::api::{CheckStatus, SystemCheck, SystemCheckId};
use std::collections::BTreeSet;

impl Ingress {
    /// Reports the Docker Engine's versions and, while ingress is enabled,
    /// whether they meet its requirements.
    pub(crate) async fn docker_check(&self) -> SystemCheck {
        let check =
            |status, message: String| SystemCheck::new(SystemCheckId::Docker, status, message);
        let version = match self.docker_version().await {
            Ok(version) => version,
            Err(error) => {
                return check(
                    CheckStatus::Fail,
                    format!("Docker Engine did not report its version: {error:#}"),
                )
                .next_step(
                    "Start Docker and make sure the daemon's user can reach docker.socket.",
                );
            }
        };
        let found = format!("Docker Engine {} with API {}", version.engine, version.api);
        match (self.enabled, version.supports_ingress()) {
            (false, _) => check(
                CheckStatus::Pass,
                format!("{found}; ingress is disabled, so no minimum applies"),
            ),
            (true, true) => check(CheckStatus::Pass, format!("{found} supports ingress")),
            (true, false) => check(
                CheckStatus::Fail,
                format!("{found} is too old for ingress, which needs Engine 28+ with API 1.48+"),
            )
            .next_step("Upgrade Docker Engine, or disable [ingress]."),
        }
    }

    /// Checks that ports 80 and 443 are available to the gateway: held by
    /// the running managed gateway, or without a listener on any host
    /// address. Skipped while
    /// ingress is disabled or public routes use the Cloudflare Tunnel.
    pub(crate) async fn ports_check(&self) -> SystemCheck {
        let check =
            |status, message: &str| SystemCheck::new(SystemCheckId::IngressPorts, status, message);
        if !self.enabled {
            return check(CheckStatus::Skip, "ingress is disabled");
        }
        if self.tunnel.is_some() {
            return check(
                CheckStatus::Skip,
                "public routes arrive through the Cloudflare Tunnel, which needs no host ports",
            );
        }
        match self.container().await {
            Ok(Some(container)) if container["State"]["Running"] == true => {
                return check(
                    CheckStatus::Pass,
                    "held by piqueld's managed Caddy gateway, which is running",
                );
            }
            Ok(_) => {}
            Err(error) => {
                return check(
                    CheckStatus::Fail,
                    &format!("could not inspect the managed gateway: {error:#}"),
                )
                .next_step("Check Docker; daemon logs have details.");
            }
        }
        let occupied = match Self::listening(&[80, 443]).await {
            Ok(occupied) => occupied,
            Err(error) => {
                return check(
                    CheckStatus::Warn,
                    &format!("could not list this host's listening sockets: {error}"),
                )
                .next_step("Check that nothing else listens on ports 80 and 443.");
            }
        };
        if occupied.is_empty() {
            check(
                CheckStatus::Pass,
                "nothing listens on them, so they are free for the managed gateway, which is not running yet; the ingress status explains why",
            )
        } else {
            check(
                CheckStatus::Fail,
                &format!(
                    "port {} has another listener while the managed gateway is not running",
                    occupied
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(" and ")
                ),
            )
            .next_step(
                "Stop the other web server, or route public traffic through [ingress.tunnel].",
            )
        }
    }

    /// Which of `ports` have a listening TCP socket on any host address,
    /// from `/proc/net/tcp` and `/proc/net/tcp6`. A table the kernel lacks,
    /// such as `tcp6` without IPv6, has no listeners.
    async fn listening(ports: &[u16]) -> std::io::Result<BTreeSet<u16>> {
        let mut listening = BTreeSet::new();
        for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
            match tokio::fs::read_to_string(table).await {
                Ok(table) => listening.extend(Self::listening_in(&table, ports)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(listening)
    }

    /// The `ports` listening in one `/proc/net/tcp*` table, whose rows read
    /// `sl local_address rem_address st …` with hexadecimal addresses, ports
    /// and states, `0A` being `LISTEN`:
    ///
    /// ```text
    ///   0: 00000000:0050 00000000:0000 0A …
    /// ```
    fn listening_in<'a>(table: &'a str, ports: &'a [u16]) -> impl Iterator<Item = u16> + 'a {
        table.lines().skip(1).filter_map(|row| {
            let mut fields = row.split_whitespace().skip(1);
            let local = fields.next()?;
            let state = fields.nth(1)?;
            let port = u16::from_str_radix(local.rsplit_once(':')?.1, 16).ok()?;
            (state == "0A" && ports.contains(&port)).then_some(port)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Ingress;

    #[test]
    fn listeners_on_any_address_are_found() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue\n\
            0: 0100007F:1F90 00000000:0000 0A 00000000:00000000\n\
            1: 0A00000A:01BB 00000000:0000 0A 00000000:00000000\n\
            2: 0100007F:0050 0100007F:9C40 01 00000000:00000000\n";
        assert_eq!(
            Ingress::listening_in(table, &[80, 443]).collect::<Vec<_>>(),
            [443],
            "a listener on a public address counts; an established connection does not"
        );
    }
}
