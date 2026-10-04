//! Secret-bearing settings, given inline or read from a credential file.
//!
//! Every such setting `key` also accepts `key_file`. Relative file paths resolve
//! against `$CREDENTIALS_DIRECTORY`, so systemd `LoadCredential=` works
//! unchanged. Files are read once while the configuration loads.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// A resolved secret. Formatting shows where it came from, never the value.
#[derive(Clone, Eq, PartialEq)]
pub struct Credential {
    value: String,
    file: Option<CredentialFile>,
}

/// The absolute location of a credential file. Relative paths resolve against
/// `$CREDENTIALS_DIRECTORY` while the configuration loads.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(try_from = "PathBuf")]
pub struct CredentialFile(PathBuf);

impl CredentialFile {
    /// Resolves `file`, joining a relative path onto `directory`.
    fn resolve(file: PathBuf, directory: Option<PathBuf>) -> Result<Self, CredentialError> {
        if file.is_absolute() {
            return Ok(Self(file));
        }
        match directory {
            Some(directory) => Ok(Self(directory.join(file))),
            None => Err(CredentialError::NoDirectory { path: file }),
        }
    }

    /// The resolved absolute path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl TryFrom<PathBuf> for CredentialFile {
    type Error = CredentialError;
    fn try_from(file: PathBuf) -> Result<Self, Self::Error> {
        Self::resolve(
            file,
            std::env::var_os("CREDENTIALS_DIRECTORY").map(PathBuf::from),
        )
    }
}

/// Describes the source, e.g. `from /run/credentials/piqueld.service/discord-webhook`.
impl std::fmt::Display for CredentialFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "from {}", self.0.display())
    }
}

impl Credential {
    /// Resolves the `key`/`key_file` pair of one configuration entry.
    ///
    /// # Errors
    ///
    /// Exactly one variant must be set, and a file must be readable and non-empty.
    pub(super) fn resolve(
        key: &'static str,
        inline: Option<String>,
        file: Option<CredentialFile>,
    ) -> Result<Self, CredentialError> {
        match (inline, file) {
            (Some(value), None) => Ok(Self::from(value)),
            (None, Some(file)) => Self::read(key, file),
            (Some(_), Some(_)) => Err(CredentialError::Conflict { key }),
            (None, None) => Err(CredentialError::Missing { key }),
        }
    }

    /// Reads a credential file. Surrounding whitespace, such as a trailing
    /// newline, is removed.
    fn read(key: &'static str, file: CredentialFile) -> Result<Self, CredentialError> {
        let value = std::fs::read_to_string(file.path())
            .map_err(|error| CredentialError::Read {
                key,
                path: file.0.clone(),
                error,
            })?
            .trim()
            .to_owned();
        if value.is_empty() {
            return Err(CredentialError::Empty { key, path: file.0 });
        }
        Ok(Self {
            value,
            file: Some(file),
        })
    }

    /// The secret itself. Never log or display it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.value
    }
}

impl From<String> for Credential {
    fn from(value: String) -> Self {
        Self { value, file: None }
    }
}

impl From<&str> for Credential {
    fn from(value: &str) -> Self {
        value.to_owned().into()
    }
}

/// Describes the source: `inline`, or the file as for [`CredentialFile`].
impl std::fmt::Display for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.file {
            Some(file) => file.fmt(f),
            None => f.write_str("inline"),
        }
    }
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Credential({self})")
    }
}

/// A secret-bearing setting could not be resolved. Messages never contain the value.
///
/// Serde only keeps the rendered message, so read failures include their cause.
#[derive(Debug, Error)]
pub enum CredentialError {
    /// Both variants were set.
    #[error("set only one of `{key}` and `{key}_file`")]
    Conflict {
        /// Inline setting name.
        key: &'static str,
    },
    /// Neither variant was set.
    #[error("`{key}` or `{key}_file` is required")]
    Missing {
        /// Inline setting name.
        key: &'static str,
    },
    /// A relative path was given outside a systemd credential context.
    #[error("relative credential file {} requires $CREDENTIALS_DIRECTORY", path.display())]
    NoDirectory {
        /// Configured relative path.
        path: PathBuf,
    },
    /// The file could not be read.
    #[error("could not read `{key}_file` {}: {error}", path.display())]
    Read {
        /// Inline setting name.
        key: &'static str,
        /// Resolved path.
        path: PathBuf,
        /// Underlying filesystem failure.
        error: std::io::Error,
    },
    /// The file contained only whitespace.
    #[error("`{key}_file` {} is empty", path.display())]
    Empty {
        /// Inline setting name.
        key: &'static str,
        /// Resolved path.
        path: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_files_resolve_against_the_credentials_directory() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("webhook"), "https://example.com/x\n").unwrap();
        let file = CredentialFile::resolve("webhook".into(), Some(directory.path().to_path_buf()))
            .unwrap();
        let credential = Credential::read("url", file).unwrap();
        assert_eq!(credential.expose(), "https://example.com/x");
        assert_eq!(
            credential.to_string(),
            format!("from {}", directory.path().join("webhook").display())
        );
        assert!(matches!(
            CredentialFile::resolve("webhook".into(), None),
            Err(CredentialError::NoDirectory { .. })
        ));
    }

    #[test]
    fn exactly_one_non_empty_variant_is_required() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), " \n").unwrap();
        for (inline, path) in [
            (Some("secret".to_owned()), Some(file.path().to_path_buf())),
            (None, None),
            (None, Some(file.path().to_path_buf())),
            (None, Some(file.path().join("missing"))),
        ] {
            let path = path.map(|path| CredentialFile::resolve(path, None).unwrap());
            let error = Credential::resolve("url", inline, path).unwrap_err();
            assert!(!error.to_string().contains("secret"), "{error}");
        }
        let inline = Credential::resolve("url", Some("secret".into()), None).unwrap();
        assert_eq!(format!("{inline} {inline:?}"), "inline Credential(inline)");
    }
}
