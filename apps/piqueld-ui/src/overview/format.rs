//! Relative times, such as "4 min ago".

/// How long before now a moment is, in milliseconds; negative for a moment
/// still to come.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Age(i64);

impl Age {
    /// The age of `moment` at `now`, both in Unix milliseconds.
    #[must_use]
    pub const fn at(moment: i64, now: i64) -> Self {
        Self(now.saturating_sub(moment))
    }

    /// Compact label such as "4 min ago" or "in 3 d", or `None` from a week
    /// on, where a date reads better. Minutes round to the nearest one, so
    /// the label turns to hours rather than reading "60 min".
    #[must_use]
    pub fn label(self) -> Option<String> {
        let future = self.0 < 0;
        let seconds = self.0.unsigned_abs() / 1000;
        let text = match seconds {
            0..=44 => {
                return Some(if future { "in a moment" } else { "just now" }.into());
            }
            45..=3_569 => format!("{} min", (seconds + 30) / 60),
            3_570..=86_399 => format!("{} h", (seconds / 3_600).max(1)),
            86_400..=604_799 => format!("{} d", seconds / 86_400),
            _ => return None,
        };
        Some(if future {
            format!("in {text}")
        } else {
            format!("{text} ago")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The label of a moment `seconds` before now.
    fn ago(seconds: i64) -> Option<String> {
        Age::at(0, seconds * 1000).label()
    }

    #[test]
    fn units_switch_at_their_boundaries_and_minutes_round() {
        for (seconds, label) in [
            (44, "just now"),
            (45, "1 min ago"),
            (89, "1 min ago"),
            (90, "2 min ago"),
            (3_569, "59 min ago"),
            (3_570, "1 h ago"),
            (86_399, "23 h ago"),
            (86_400, "1 d ago"),
            (604_799, "6 d ago"),
        ] {
            assert_eq!(ago(seconds).as_deref(), Some(label), "{seconds} s");
        }
        assert_eq!(ago(604_800), None);
    }

    #[test]
    fn moments_to_come_read_forward() {
        assert_eq!(ago(-44).as_deref(), Some("in a moment"));
        assert_eq!(ago(-90).as_deref(), Some("in 2 min"));
        assert_eq!(ago(-2 * 86_400).as_deref(), Some("in 2 d"));
        assert_eq!(ago(-604_800), None);
    }
}
