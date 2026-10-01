use std::fmt;

/// A TOML parse failure reduced to its message and location.
///
/// `toml::de::Error` renders the offending source line verbatim. Configuration
/// and profile files may hold credentials, so every TOML reader converts its
/// errors into this type, which never retains source text. The message can
/// still quote a mistyped value; callers that must not echo values replace it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TomlDiagnostic {
    /// Parser or deserializer message, on one line.
    pub message: String,
    /// Where the failure occurred, when the parser reports it.
    pub location: Option<TomlLocation>,
}

/// One-based position within a TOML document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TomlLocation {
    /// Line number.
    pub line: usize,
    /// Character column within the line.
    pub column: usize,
}

impl TomlDiagnostic {
    /// Converts a `toml` error raised while parsing `source`.
    #[must_use]
    pub fn new(source: &str, error: &toml::de::Error) -> Self {
        let location = error
            .span()
            .and_then(|span| source.get(..span.start))
            .map(|before| TomlLocation {
                line: before.matches('\n').count() + 1,
                column: before
                    .rsplit('\n')
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .count()
                    + 1,
            });
        Self {
            // Syntax messages span lines; keep each diagnostic on one line.
            message: error.message().lines().collect::<Vec<_>>().join("; "),
            location,
        }
    }
}

impl fmt::Display for TomlDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        match self.location {
            Some(location) => write!(f, " at {location}"),
            None => Ok(()),
        }
    }
}

impl std::error::Error for TomlDiagnostic {}

impl fmt::Display for TomlLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {} column {}", self.line, self.column)
    }
}
