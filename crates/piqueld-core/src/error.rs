//! Stable public error primitives.

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use thiserror::Error;

/// A stable, machine-readable error identifier.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ErrorCode(Cow<'static, str>);

impl ErrorCode {
    /// Creates an error code from a static identifier.
    #[must_use]
    pub const fn new(code: &'static str) -> Self {
        Self(Cow::Borrowed(code))
    }

    /// Returns the wire-format identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A public error with a stable code and deliberately non-sensitive message.
#[derive(Clone, Debug, Deserialize, Error, PartialEq, Serialize)]
#[error("{code}: {message}")]
pub struct PublicError {
    /// Stable machine-readable code.
    pub code: ErrorCode,
    /// Human-readable, non-sensitive summary.
    pub message: String,
}

impl PublicError {
    /// Creates a public error.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_has_stable_display_form() {
        let code = ErrorCode::new("configuration_invalid");
        assert_eq!(code.as_str(), "configuration_invalid");
        assert_eq!(code.to_string(), "configuration_invalid");
    }
}
