//! A build machine: another machine whose `BuildKit` daemon builds this
//! installation's images and pushes them to a registry, from which this
//! host's Docker Engine pulls them. The daemon's configuration names it, and
//! its Docker boundary builds on it.
use crate::{RepositoryDigest, Sha256Digest};
use serde::{Deserialize, Serialize};

/// A build machine: its `BuildKit` daemon, and where that pushes the images
/// this host then pulls.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteBuild {
    /// The `BuildKit` daemon that runs the builds.
    pub builder: BuilderEndpoint,
    /// The image repository the builds are pushed to.
    pub repository: BuildRepository,
}

/// Where a build machine's `BuildKit` daemon listens: `tcp://host:port`,
/// without credentials or a path, which `BuildKit` would not use. Displays
/// as configured.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "String")]
pub struct BuilderEndpoint(String);

impl TryFrom<String> for BuilderEndpoint {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let address = value
            .strip_prefix("tcp://")
            .map(|rest| rest.strip_suffix('/').unwrap_or(rest));
        // An IPv6 host is bracketed, so its port follows the last colon.
        let valid = address
            .and_then(|address| address.rsplit_once(':'))
            .is_some_and(|(host, port)| {
                !host.is_empty()
                    && !host.contains(['/', '@', '?', '#', ' '])
                    && (!host.contains(':') || host.starts_with('[') && host.ends_with(']'))
                    && port.parse::<u16>().is_ok()
            });
        if valid {
            Ok(Self(value))
        } else {
            Err("build.builder must be tcp://host:port")
        }
    }
}

impl BuilderEndpoint {
    /// The daemon's `host:port`, to connect to.
    #[must_use]
    pub fn address(&self) -> &str {
        let address = self.0.strip_prefix("tcp://").unwrap_or(&self.0);
        address.strip_suffix('/').unwrap_or(address)
    }
}

impl std::fmt::Display for BuilderEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The image repository a build machine pushes to, on a named registry:
/// `registry-host[:port]/path`. Displays as configured.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "String")]
pub struct BuildRepository(String);

impl TryFrom<String> for BuildRepository {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        // Only a repository on an explicit registry, without a tag or
        // digest, is its own canonical form: `piqueld` would be Docker Hub's.
        if crate::image_repository(&value).as_deref() == Some(value.as_str()) {
            Ok(Self(value))
        } else {
            Err("build.repository must be registry-host[:port]/path, in lower case, without a tag")
        }
    }
}

impl BuildRepository {
    /// The image this repository holds under `digest`.
    ///
    /// # Panics
    /// Never: a validated repository and a digest form a repository digest.
    #[must_use]
    pub fn image(&self, digest: &Sha256Digest) -> RepositoryDigest {
        RepositoryDigest::parse(format!("{}@{digest}", self.0))
            .expect("a validated repository and a digest form a repository digest")
    }
}

impl std::fmt::Display for BuildRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_builder_is_a_tcp_host_and_port() {
        for (endpoint, address) in [
            (
                "tcp://build.example.ts.net:1234",
                "build.example.ts.net:1234",
            ),
            ("tcp://100.64.0.1:1234/", "100.64.0.1:1234"),
            ("tcp://[fd7a::1]:1234", "[fd7a::1]:1234"),
        ] {
            let parsed = BuilderEndpoint::try_from(endpoint.to_owned()).unwrap();
            assert_eq!(parsed.address(), address);
        }
        for invalid in [
            // No port, another scheme, credentials, a path, and a bare IPv6 host.
            "tcp://build.example.ts.net",
            "ssh://build.example.ts.net:22",
            "tcp://user@build.example:1234",
            "tcp://build.example:1234/buildkit",
            "tcp://fd7a::1:1234",
            "tcp://:1234",
        ] {
            assert!(
                BuilderEndpoint::try_from(invalid.to_owned()).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn a_build_repository_is_on_a_named_registry_without_a_tag() {
        let repository =
            BuildRepository::try_from("build.example.ts.net:5000/piqueld".to_owned()).unwrap();
        let digest = Sha256Digest::parse(format!("sha256:{}", "a".repeat(64))).unwrap();
        assert_eq!(
            repository.image(&digest).as_str(),
            format!("build.example.ts.net:5000/piqueld@{digest}")
        );
        // Docker Hub's, a tag, and upper case.
        for invalid in [
            "piqueld",
            "registry.example/piqueld:v1",
            "registry.example/Piqueld",
        ] {
            assert!(
                BuildRepository::try_from(invalid.to_owned()).is_err(),
                "{invalid}"
            );
        }
    }
}
