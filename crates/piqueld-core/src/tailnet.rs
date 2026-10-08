//! Tailnet identities: who a connection through the daemon's tailnet node came
//! from, and the identity an API token can be bound to.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The tailnet user or tag an API token is bound to: a login name such as
/// `alice@example.com`, or a tag such as `tag:ci`. A bound token is accepted
/// only through the daemon's tailnet node, from a node signed in as that user
/// or carrying that tag.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(try_from = "String", into = "String")]
#[schema(value_type = String, example = "tag:ci")]
pub struct TailnetBinding(String);

impl TailnetBinding {
    /// Parses a login name (containing `@`) or a `tag:` followed by letters,
    /// digits, and dashes.
    ///
    /// # Errors
    /// Describes why `value` is neither.
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        let valid = match value.strip_prefix("tag:") {
            Some(tag) => {
                !tag.is_empty() && tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            }
            None => {
                value.contains('@')
                    && value.len() <= 256
                    && !value.chars().any(|c| c.is_whitespace() || c.is_control())
            }
        };
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err("a tailnet binding is a login name like alice@example.com or a tag like tag:ci")
        }
    }

    /// Whether a connection from `peer` satisfies this binding.
    #[must_use]
    pub fn matches(&self, peer: &TailnetPeer) -> bool {
        if self.0.starts_with("tag:") {
            peer.tags.contains(&self.0)
        } else {
            peer.login.as_ref() == Some(&self.0)
        }
    }

    /// The binding as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TailnetBinding {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<TailnetBinding> for String {
    fn from(binding: TailnetBinding) -> Self {
        binding.0
    }
}

impl std::fmt::Display for TailnetBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who a connection through the daemon's tailnet node came from, as its
/// `tailscaled` reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TailnetPeer {
    /// Login name of the node's user; absent for tagged nodes, which belong
    /// to no user.
    pub login: Option<String>,
    /// The node's name.
    pub node: String,
    /// The node's tags, such as `tag:ci`.
    pub tags: Vec<String>,
}

impl TailnetPeer {
    /// One-line description for the audit trail, e.g.
    /// `alice@example.com on laptop` or `tag:ci on runner-1`.
    #[must_use]
    pub fn describe(&self) -> String {
        let who = self.login.clone().unwrap_or_else(|| self.tags.join(","));
        format!("{who} on {}", self.node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bindings_match_users_or_tags() {
        let laptop = TailnetPeer {
            login: Some("alice@example.com".into()),
            node: "laptop".into(),
            tags: Vec::new(),
        };
        let runner = TailnetPeer {
            login: None,
            node: "runner".into(),
            tags: vec!["tag:ci".into(), "tag:prod".into()],
        };
        let alice = TailnetBinding::parse("alice@example.com").unwrap();
        let ci = TailnetBinding::parse("tag:ci").unwrap();
        assert!(alice.matches(&laptop) && !alice.matches(&runner));
        assert!(ci.matches(&runner) && !ci.matches(&laptop));
        assert_eq!(runner.describe(), "tag:ci,tag:prod on runner");
        for invalid in ["alice", "tag:", "tag:c i", "a b@example.com", ""] {
            assert!(TailnetBinding::parse(invalid).is_err(), "{invalid}");
        }
    }
}
