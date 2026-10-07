//! Waits for a published TXT record on a zone's authoritative nameservers,
//! queried directly over TCP without caching, so a CA resolving the name
//! afterwards finds it. OVH can take several minutes to propagate changes.
use super::Dns;
use anyhow::{Context, Result, anyhow, ensure};
use hickory_resolver::{
    Resolver, TokioResolver,
    config::{ConnectionConfig, NameServerConfig, ResolverConfig, ResolverOpts},
    net::runtime::TokioRuntimeProvider,
    proto::rr::RData,
};
use piqueld_core::manifest::Hostname;
use std::{net::SocketAddr, time::Duration};

impl Dns {
    /// Polls every 5s until every authoritative nameserver of `zone` serves
    /// `value` in a TXT record at `name`, failing after `timeout`.
    ///
    /// # Errors
    /// Fails when the record is not visible everywhere in time.
    pub async fn wait_visible(
        &self,
        zone: &Hostname,
        name: &str,
        value: &str,
        timeout: Duration,
    ) -> Result<()> {
        tokio::time::timeout(timeout, async {
            loop {
                match self.visible(zone, name, value).await {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(error) => {
                        tracing::debug!(%zone, name, error = format!("{error:#}"), "TXT propagation check failed");
                    }
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        })
        .await
        .with_context(|| {
            format!(
                "TXT record {name} was not visible on every authoritative nameserver of {zone} within {}s",
                timeout.as_secs()
            )
        })
    }

    /// Whether every authoritative nameserver of `zone` already serves `value`.
    /// A nameserver answers through the first of its addresses that responds,
    /// so an unreachable IPv6 address does not hide a working IPv4 one.
    async fn visible(&self, zone: &Hostname, name: &str, value: &str) -> Result<bool> {
        'nameservers: for addresses in self.nameservers(zone).await? {
            let mut failure = None;
            for address in addresses {
                match Self::serves(address, name, value).await {
                    Ok(true) => continue 'nameservers,
                    Ok(false) => return Ok(false),
                    Err(error) => failure = Some(error),
                }
            }
            return Err(failure.unwrap_or_else(|| anyhow!("a nameserver of {zone} has no address")));
        }
        Ok(true)
    }

    /// Whether the nameserver at `address` serves `value` in a TXT record at
    /// `name`, asked over TCP without recursion or caching.
    async fn serves(address: SocketAddr, name: &str, value: &str) -> Result<bool> {
        let mut connection = ConnectionConfig::tcp();
        connection.port = address.port();
        let mut options = ResolverOpts::default();
        options.cache_size = 0;
        options.recursion_desired = false;
        let resolver = Resolver::builder_with_config(
            ResolverConfig::from_parts(
                None,
                Vec::new(),
                vec![NameServerConfig::new(address.ip(), true, vec![connection])],
            ),
            TokioRuntimeProvider::default(),
        )
        .with_options(options)
        .build()?;
        match resolver.txt_lookup(format!("{name}.")).await {
            Ok(lookup) => Ok(lookup.answers().iter().any(|record| {
                matches!(&record.data, RData::TXT(txt)
                    if txt.txt_data.iter().any(|part| &**part == value.as_bytes()))
            })),
            Err(error) if error.is_no_records_found() => Ok(false),
            Err(error) => Err(error).with_context(|| format!("query {address}")),
        }
    }

    /// Addresses of each authoritative nameserver of `zone`, from its NS
    /// records.
    async fn nameservers(&self, zone: &Hostname) -> Result<Vec<Vec<SocketAddr>>> {
        #[cfg(test)]
        if let Some(nameservers) = &self.nameservers {
            return Ok(nameservers.clone());
        }
        let system = TokioResolver::builder_tokio()?.build()?;
        let records = system
            .ns_lookup(format!("{zone}."))
            .await
            .with_context(|| format!("look up NS records of {zone}"))?;
        let mut nameservers = Vec::new();
        for record in records.answers() {
            if let RData::NS(nameserver) = &record.data {
                let ips = system
                    .lookup_ip(nameserver.0.clone())
                    .await
                    .with_context(|| format!("resolve nameserver {}", nameserver.0))?;
                nameservers.push(ips.iter().map(|ip| SocketAddr::new(ip, 53)).collect());
            }
        }
        ensure!(
            !nameservers.is_empty(),
            "{zone} has no authoritative nameservers"
        );
        Ok(nameservers)
    }
}
