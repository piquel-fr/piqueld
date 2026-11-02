//! Validation errors, each naming the path of the field it rejects.

use super::safe_decode_path;
use crate::codes;
use serde::{Deserialize, Serialize};
use std::fmt;
use utoipa::ToSchema;

/// A field-level, safe validation error.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ValidationError {
    /// Stable machine-readable validation code.
    pub code: String,
    /// Safe manifest field path.
    pub path: String,
    /// Safe human-readable validation message.
    pub message: String,
}

impl ValidationError {
    /// An error with the stable `code` at the manifest field `path`.
    #[must_use]
    pub fn new(
        code: impl Into<String>,
        path: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            path: path.into(),
            message: message.into(),
        }
    }
}

/// All independently discoverable manifest errors, in stable path order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(transparent)]
pub struct ValidationErrors(pub Vec<ValidationError>);

impl ValidationErrors {
    /// Orders errors by path, then code.
    pub(crate) fn sorted(mut errors: Vec<ValidationError>) -> Self {
        errors.sort_by(|left, right| left.path.cmp(&right.path).then(left.code.cmp(&right.code)));
        Self(errors)
    }

    /// One decode failure. Serde messages carry the rejected field and
    /// location, and may echo a mistyped value back to its submitter.
    /// Manifests reference secrets by name only, so they are safe to return.
    pub(super) fn decode(path: &str, message: String) -> Self {
        ValidationError::new(
            codes::MANIFEST_DECODE_FAILED,
            safe_decode_path(path),
            message,
        )
        .into()
    }
}

/// A single error.
impl From<ValidationError> for ValidationErrors {
    fn from(error: ValidationError) -> Self {
        Self(vec![error])
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "application manifest has {} error(s)",
            self.0.len()
        )?;
        for error in &self.0 {
            write!(
                formatter,
                "; {} at {}: {}",
                error.code, error.path, error.message
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}
