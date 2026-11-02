//! Build history: refreshing a list that older pages extended, and how a
//! build's arguments read.
use piqueld_client::BuildRecord;
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fmt::Display;

/// A build list, newest (highest ID) first.
pub trait BuildList {
    /// Applies a refreshed first `page`: builds it lists replace their
    /// listed versions and new ones are added, while builds of older pages
    /// stay listed.
    fn merge_newest(&mut self, page: Vec<BuildRecord>);
}

impl BuildList for Vec<BuildRecord> {
    fn merge_newest(&mut self, page: Vec<BuildRecord>) {
        for build in page {
            match self.iter_mut().find(|listed| listed.id == build.id) {
                Some(listed) => *listed = build,
                None => self.push(build),
            }
        }
        self.sort_by_key(|build| Reverse(build.id));
    }
}

/// Renders Docker build arguments as space-separated `KEY="VALUE"` pairs.
/// Values are quoted and escaped so spaces cannot read as extra arguments.
#[must_use]
pub fn build_arguments<V: Display>(args: &BTreeMap<String, V>) -> String {
    args.iter()
        .map(|(key, value)| format!("{key}={:?}", value.to_string()))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use piqueld_client::BuildState;
    use serde_json::json;

    /// Build `id` in `state`.
    fn build(id: i64, state: &str) -> BuildRecord {
        serde_json::from_value(json!({
            "id": id,
            "environment_id": "app-shop0001",
            "operation_id": format!("op-{id}"),
            "service": "web",
            "source": { "type": "image", "image": "nginx:alpine" },
            "state": state,
            "started_at_ms": id,
            "finished_at_ms": null,
            "commit": null,
            "image_id": null,
            "log_bytes": 0,
            "log_truncated": false,
            "log_expired": false,
        }))
        .expect("build")
    }

    #[test]
    fn a_refreshed_first_page_keeps_older_pages() {
        // The first page (9, 8) and an older one (5, 4).
        let mut builds = vec![
            build(9, "running"),
            build(8, "succeeded"),
            build(5, "failed"),
            build(4, "succeeded"),
        ];
        builds.merge_newest(vec![build(10, "running"), build(9, "succeeded")]);
        assert_eq!(
            builds
                .iter()
                .map(|build| (build.id, build.state))
                .collect::<Vec<_>>(),
            [
                (10, BuildState::Running),
                (9, BuildState::Succeeded),
                (8, BuildState::Succeeded),
                (5, BuildState::Failed),
                (4, BuildState::Succeeded),
            ]
        );
    }

    #[test]
    fn build_arguments_quote_values_with_spaces_and_quotes() {
        let args = BTreeMap::from([
            ("NAME".to_owned(), "two words"),
            ("GREETING".to_owned(), r#"say "hi""#),
        ]);
        assert_eq!(
            build_arguments(&args),
            r#"GREETING="say \"hi\"" NAME="two words""#
        );
    }
}
