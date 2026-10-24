//! Per-service rollout settings and the update policy they resolve to.

use super::variables::Typed;
use crate::names::string_enum;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// How Docker replaces a service's containers during a deployment, one at a
/// time. An empty block keeps the defaults.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Rollout {
    /// `stop-first` stops the old container before starting its replacement,
    /// with a short downtime. `start-first` starts the replacement first, so
    /// both briefly run side by side. When unset, `stop-first` if the service
    /// mounts a volume writable or receives telemetry, `start-first`
    /// otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<Typed<RolloutOrder>>,
    /// Seconds Docker watches each new container for failure before it
    /// replaces the next one, from 1 to 3,600. 30 when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitor_seconds: Option<Typed<u32>>,
}

impl Rollout {
    /// Whether no setting is given; such blocks are omitted on export.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Validated rollout settings of a service.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ValidatedRollout {
    /// Update order; derived from the service's mounts when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<RolloutOrder>,
    /// Seconds Docker watches each replacement task for failure; 30 when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1, maximum = 3600)]
    pub monitor_seconds: Option<u32>,
}

impl From<ValidatedRollout> for Rollout {
    fn from(value: ValidatedRollout) -> Self {
        Self {
            order: value.order.map(Typed::Literal),
            monitor_seconds: value.monitor_seconds.map(Typed::Literal),
        }
    }
}

string_enum! {
    #[serde]
    /// Whether Docker stops the old container before or after starting its
    /// replacement.
    pub enum RolloutOrder {
        /// Stop the old task first: tasks never overlap, at the cost of a short downtime.
        StopFirst => "stop-first",
        /// Start the replacement first: no downtime, but old and new tasks briefly overlap.
        StartFirst => "start-first",
    }
}

string_enum! {
    #[serde]
    /// Where a service's effective update order comes from.
    pub enum RolloutOrderSource {
        /// Derived from the service's mounts.
        Derived => "derived",
        /// Set in the service's `rollout` block.
        Explicit => "explicit",
    }
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

impl ValidatedRollout {
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

/// Parses the manifest spelling, e.g. a CLI flag value.
impl std::str::FromStr for RolloutOrder {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value).ok_or("rollout order must be \"stop-first\" or \"start-first\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_policy_derives_order_from_mounts_unless_explicit() {
        let derived = ValidatedRollout::default();
        assert_eq!(
            derived.policy([]),
            RolloutPolicy {
                order: RolloutOrder::StartFirst,
                monitor_seconds: ValidatedRollout::DEFAULT_MONITOR_SECONDS,
            }
        );
        assert_eq!(derived.policy([true]).order, RolloutOrder::StartFirst);
        assert_eq!(derived.policy([true, false]).order, RolloutOrder::StopFirst);
        assert_eq!(derived.order_source(), RolloutOrderSource::Derived);

        let explicit = ValidatedRollout {
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
        let stop_first = ValidatedRollout {
            order: Some(RolloutOrder::StopFirst),
            monitor_seconds: None,
        };
        assert_eq!(stop_first.policy([]).order, RolloutOrder::StopFirst);
    }

    #[test]
    fn only_explicit_start_first_with_a_writable_volume_overlaps() {
        let start_first = ValidatedRollout {
            order: Some(RolloutOrder::StartFirst),
            monitor_seconds: None,
        };
        assert!(start_first.overlaps_writable_volume([true, false]));
        assert!(!start_first.overlaps_writable_volume([true]));
        assert!(!ValidatedRollout::default().overlaps_writable_volume([false]));
    }
}
