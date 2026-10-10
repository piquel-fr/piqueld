//! Zone ownership: a hostname belongs to the provider with the longest zone
//! containing it. A zone claimed by two providers is a conflict and not used.
use super::provider::Zone;
use piqueld_core::manifest::Hostname;
use thiserror::Error;

/// Why no provider can manage a hostname.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ZoneError {
    /// No discovered zone contains the hostname.
    #[error("no configured DNS provider zone contains {0}")]
    NoZone(Hostname),
    /// The hostname's longest matching zone is claimed by several providers.
    #[error("zone {0} is claimed by more than one DNS provider")]
    Conflict(Hostname),
}

/// The provider index and zone owning `hostname`, given each provider's zones.
pub(super) fn owner<'a>(
    providers: &[&'a [Zone]],
    hostname: &Hostname,
) -> Result<(usize, &'a Zone), ZoneError> {
    let mut matches = providers.iter().enumerate().flat_map(|(index, zones)| {
        zones
            .iter()
            .filter(|zone| hostname.is_within(&zone.name))
            .map(move |zone| (index, zone))
    });
    let first = matches
        .next()
        .ok_or_else(|| ZoneError::NoZone(hostname.clone()))?;
    let mut best = first;
    let mut claimed_twice = false;
    for candidate in matches {
        let (longer, same) = (
            candidate.1.name.as_str().len() > best.1.name.as_str().len(),
            candidate.1.name == best.1.name,
        );
        if longer {
            best = candidate;
            claimed_twice = false;
        } else if same && candidate.0 != best.0 {
            claimed_twice = true;
        }
    }
    if claimed_twice {
        return Err(ZoneError::Conflict(best.1.name.clone()));
    }
    Ok(best)
}

/// Zones of provider `index` that another provider also claims.
pub(super) fn conflicts(providers: &[&[Zone]], index: usize) -> Vec<String> {
    providers[index]
        .iter()
        .filter(|zone| {
            providers
                .iter()
                .enumerate()
                .any(|(other, zones)| other != index && zones.iter().any(|z| z.name == zone.name))
        })
        .map(|zone| zone.name.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(name: &str) -> Zone {
        Zone {
            name: Hostname::parse(name).unwrap(),
            id: name.into(),
        }
    }

    fn host(name: &str) -> Hostname {
        Hostname::parse(name).unwrap()
    }

    #[test]
    fn longest_zone_wins_and_shared_zones_conflict() {
        let first = [zone("piquel.fr"), zone("shared.com")];
        let second = [zone("dev.piquel.fr"), zone("shared.com")];
        let providers = [first.as_slice(), second.as_slice()];
        for (hostname, expected) in [
            ("piquel.fr", Ok((0, "piquel.fr"))),
            ("admin.piquel.fr", Ok((0, "piquel.fr"))),
            ("x.dev.piquel.fr", Ok((1, "dev.piquel.fr"))),
            ("dev.piquel.fr", Ok((1, "dev.piquel.fr"))),
            (
                "app.shared.com",
                Err(ZoneError::Conflict(host("shared.com"))),
            ),
            ("other.com", Err(ZoneError::NoZone(host("other.com")))),
            // Label boundaries: `notpiquel.fr` is not inside `piquel.fr`.
            ("notpiquel.fr", Err(ZoneError::NoZone(host("notpiquel.fr")))),
        ] {
            let found =
                owner(&providers, &host(hostname)).map(|(index, zone)| (index, zone.name.as_str()));
            assert_eq!(found, expected, "{hostname}");
        }
        assert_eq!(conflicts(&providers, 0), ["shared.com"]);
        assert_eq!(conflicts(&providers, 1), ["shared.com"]);
    }
}
