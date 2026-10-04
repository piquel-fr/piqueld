//! Per-service rollout settings and the update policy they resolve to.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Optional `rollout` block of a service. Omitted fields keep their defaults,
/// so an empty block is equivalent to no block.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Rollout {
    /// Update order; derived from the service's mounts when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<RolloutOrder>,
    /// Seconds Docker watches each replacement task for failure; 30 when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1, maximum = 3600)]
    pub monitor_seconds: Option<u32>,
}

/// Whether Docker stops a task before or after starting its replacement.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RolloutOrder {
    /// Stop the old task first: tasks never overlap, at the cost of a short downtime.
    StopFirst,
    /// Start the replacement first: no downtime, but old and new tasks briefly overlap.
    StartFirst,
}

/// Where a service's effective update order comes from.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RolloutOrderSource {
    /// Derived from the service's mounts.
    Derived,
    /// Set in the service's `rollout` block.
    Explicit,
}

/// The update policy Docker applies when it replaces a service's tasks.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RolloutPolicy {
    /// Effective update order.
    pub order: RolloutOrder,
    /// Effective monitor window in seconds.
    pub monitor_seconds: u32,
}

impl Rollout {
    /// Monitor window applied when a service declares none.
    pub const DEFAULT_MONITOR_SECONDS: u32 = 30;
    /// Longest monitor window a service may declare.
    pub const MAX_MONITOR_SECONDS: u32 = 3_600;

    /// Whether every setting keeps its default; such blocks are omitted on export.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Resolves the policy for a service whose mounts have the given read-only
    /// flags. This is the only place the default rule lives: stop-first when
    /// any mount is writable, so two tasks never share a writable data
    /// directory (e.g. PostgreSQL), and start-first otherwise.
    #[must_use]
    pub fn policy(&self, read_only: impl IntoIterator<Item = bool>) -> RolloutPolicy {
        RolloutPolicy {
            order: self.order.unwrap_or_else(|| Self::derived_order(read_only)),
            monitor_seconds: self
                .monitor_seconds
                .unwrap_or(Self::DEFAULT_MONITOR_SECONDS),
        }
    }

    /// Whether the order is set explicitly or derived from the mounts.
    #[must_use]
    pub const fn order_source(&self) -> RolloutOrderSource {
        match self.order {
            Some(_) => RolloutOrderSource::Explicit,
            None => RolloutOrderSource::Derived,
        }
    }

    /// Whether an explicit start-first order lets two tasks share a writable volume.
    #[must_use]
    pub fn overlaps_writable_volume(&self, read_only: impl IntoIterator<Item = bool>) -> bool {
        self.order == Some(RolloutOrder::StartFirst)
            && Self::derived_order(read_only) == RolloutOrder::StopFirst
    }

    fn derived_order(read_only: impl IntoIterator<Item = bool>) -> RolloutOrder {
        if read_only.into_iter().any(|read_only| !read_only) {
            RolloutOrder::StopFirst
        } else {
            RolloutOrder::StartFirst
        }
    }
}

impl RolloutOrder {
    /// The manifest spelling of this order.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StopFirst => "stop-first",
            Self::StartFirst => "start-first",
        }
    }
}

impl RolloutOrderSource {
    /// Lowercase label shown next to an effective order.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Derived => "derived",
            Self::Explicit => "explicit",
        }
    }
}

impl std::fmt::Display for RolloutOrder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Parses the manifest spelling, e.g. a CLI flag value.
impl std::str::FromStr for RolloutOrder {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        [Self::StopFirst, Self::StartFirst]
            .into_iter()
            .find(|order| order.as_str() == value)
            .ok_or("rollout order must be \"stop-first\" or \"start-first\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_policy_derives_order_from_mounts_unless_explicit() {
        let derived = Rollout::default();
        assert_eq!(
            derived.policy([]),
            RolloutPolicy {
                order: RolloutOrder::StartFirst,
                monitor_seconds: Rollout::DEFAULT_MONITOR_SECONDS,
            }
        );
        assert_eq!(derived.policy([true]).order, RolloutOrder::StartFirst);
        assert_eq!(derived.policy([true, false]).order, RolloutOrder::StopFirst);
        assert_eq!(derived.order_source(), RolloutOrderSource::Derived);

        let explicit = Rollout {
            order: Some(RolloutOrder::StartFirst),
            monitor_seconds: Some(5),
        };
        assert_eq!(
            explicit.policy([false]),
            RolloutPolicy {
                order: RolloutOrder::StartFirst,
                monitor_seconds: 5,
            }
        );
        assert_eq!(explicit.order_source(), RolloutOrderSource::Explicit);
        let stop_first = Rollout {
            order: Some(RolloutOrder::StopFirst),
            monitor_seconds: None,
        };
        assert_eq!(stop_first.policy([]).order, RolloutOrder::StopFirst);
    }

    #[test]
    fn only_explicit_start_first_with_a_writable_volume_overlaps() {
        let start_first = Rollout {
            order: Some(RolloutOrder::StartFirst),
            monitor_seconds: None,
        };
        assert!(start_first.overlaps_writable_volume([true, false]));
        assert!(!start_first.overlaps_writable_volume([true]));
        assert!(!Rollout::default().overlaps_writable_volume([false]));
    }
}
