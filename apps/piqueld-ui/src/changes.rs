//! How a plan's configuration changes are shown: a scalar's values side by
//! side, and a structured value (a service's environment, a list of mounts)
//! line by line, marking the lines only one side has.

use piqueld_client::ManifestChange;
use serde_json::Value;

/// One changed manifest field, ready to render.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeDisplay {
    /// A value short enough to read in one line on each side; `None` is the
    /// side that lacks the field.
    Scalar {
        /// The value before the change.
        before: Option<String>,
        /// The value after it.
        after: Option<String>,
    },
    /// A structured value, pretty-printed, with the lines of both sides
    /// merged and marked.
    Lines(Vec<DiffLine>),
}

/// One line of a structured change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    /// The line, without its sign.
    pub text: String,
    /// Which side has it.
    pub mark: Mark,
}

/// Which side a line of a structured change belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// On both sides.
    Same,
    /// Only before the change.
    Removed,
    /// Only after it.
    Added,
}

impl Mark {
    /// The sign a diff shows for the line.
    #[must_use]
    pub const fn sign(self) -> &'static str {
        match self {
            Self::Same => " ",
            Self::Removed => "-",
            Self::Added => "+",
        }
    }
}

impl ChangeDisplay {
    /// How `change` is shown: line by line when either side is a JSON object
    /// or array, else side by side.
    #[must_use]
    pub fn of(change: &ManifestChange) -> Self {
        let structured = |value: &Option<String>| {
            value
                .as_deref()
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .filter(|value| value.is_object() || value.is_array())
                .map(|value| serde_json::to_string_pretty(&value).unwrap_or_default())
        };
        match (structured(&change.before), structured(&change.after)) {
            (None, None) => Self::Scalar {
                before: change.before.clone(),
                after: change.after.clone(),
            },
            (before, after) => Self::Lines(diff_lines(
                before
                    .or_else(|| change.before.clone())
                    .as_deref()
                    .unwrap_or_default(),
                after
                    .or_else(|| change.after.clone())
                    .as_deref()
                    .unwrap_or_default(),
            )),
        }
    }
}

/// The lines of `before` and `after` merged: common lines once, the others
/// marked by the side they belong to, in order. A longest common
/// subsequence keeps unchanged lines aligned.
fn diff_lines(before: &str, after: &str) -> Vec<DiffLine> {
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    // lengths[i][j]: the longest common subsequence of old[i..] and new[j..].
    let mut lengths = vec![vec![0_usize; new.len() + 1]; old.len() + 1];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            lengths[i][j] = if old[i] == new[j] {
                lengths[i + 1][j + 1] + 1
            } else {
                lengths[i + 1][j].max(lengths[i][j + 1])
            };
        }
    }
    let mut lines = Vec::with_capacity(old.len().max(new.len()));
    let (mut i, mut j) = (0, 0);
    let line = |text: &str, mark| DiffLine {
        text: text.to_owned(),
        mark,
    };
    while i < old.len() && j < new.len() {
        if old[i] == new[j] {
            lines.push(line(old[i], Mark::Same));
            i += 1;
            j += 1;
        } else if lengths[i + 1][j] >= lengths[i][j + 1] {
            lines.push(line(old[i], Mark::Removed));
            i += 1;
        } else {
            lines.push(line(new[j], Mark::Added));
            j += 1;
        }
    }
    lines.extend(old[i..].iter().map(|text| line(text, Mark::Removed)));
    lines.extend(new[j..].iter().map(|text| line(text, Mark::Added)));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(before: Option<&str>, after: Option<&str>) -> ManifestChange {
        ManifestChange {
            field: "services.web.environment".into(),
            before: before.map(str::to_owned),
            after: after.map(str::to_owned),
        }
    }

    #[test]
    fn scalars_show_side_by_side_and_structures_line_by_line() {
        assert_eq!(
            ChangeDisplay::of(&change(Some("1"), Some("2"))),
            ChangeDisplay::Scalar {
                before: Some("1".into()),
                after: Some("2".into()),
            }
        );
        // A string that merely looks like JSON text stays a scalar.
        assert_eq!(
            ChangeDisplay::of(&change(None, Some("\"nginx:1.27\""))),
            ChangeDisplay::Scalar {
                before: None,
                after: Some("\"nginx:1.27\"".into()),
            }
        );
        let ChangeDisplay::Lines(lines) = ChangeDisplay::of(&change(
            Some(r#"{"PORT":"80","ROLE":"web"}"#),
            Some(r#"{"PORT":"8080","ROLE":"web"}"#),
        )) else {
            panic!("objects are shown line by line");
        };
        let marked: Vec<(&str, Mark)> = lines
            .iter()
            .map(|line| (line.text.as_str(), line.mark))
            .collect();
        assert_eq!(
            marked,
            [
                ("{", Mark::Same),
                ("  \"PORT\": \"80\",", Mark::Removed),
                ("  \"PORT\": \"8080\",", Mark::Added),
                ("  \"ROLE\": \"web\"", Mark::Same),
                ("}", Mark::Same),
            ]
        );
    }

    #[test]
    fn an_added_structure_is_all_additions() {
        let ChangeDisplay::Lines(lines) = ChangeDisplay::of(&change(None, Some("[\"a\"]"))) else {
            panic!("arrays are shown line by line");
        };
        assert!(lines.iter().all(|line| line.mark == Mark::Added));
        assert_eq!(lines.len(), 3);
    }
}
