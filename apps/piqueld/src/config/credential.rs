//! Secret-bearing settings, read from credential files given as `key_file`.
//!
//! Relative file paths resolve
//! against `$CREDENTIALS_DIRECTORY`, so systemd `LoadCredential=` works
//! unchanged, and cannot leave it. Files are read once while the configuration
//! loads, except during a [`Resolution::Structural`] check, and must be regular
//! files.

use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

/// How credential files are treated while configuration deserializes on the
/// current thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Resolution {
    /// Read every file, as the daemon does at startup.
    Read,
    /// Locate files without reading them, so `piqueld check-config` can run
    /// in a build sandbox without the credentials. A configuration parsed
    /// this way is only checked, never used.
    Structural,
}

thread_local! {
    static RESOLUTION: Cell<Resolution> = const { Cell::new(Resolution::Read) };
}

impl Resolution {
    /// Runs `parse` with credential files treated as `self` on this thread.
    pub(super) fn during<T>(self, parse: impl FnOnce() -> T) -> T {
        /// Restores the previous mode, even if `parse` panics.
        struct Restore(Resolution);
        impl Drop for Restore {
            fn drop(&mut self) {
                RESOLUTION.set(self.0);
            }
        }
        let _restore = Restore(RESOLUTION.replace(self));
        parse()
    }

    fn current() -> Self {
        RESOLUTION.get()
    }
}

/// A resolved secret. Formatting shows where it came from, never the value.
#[derive(Clone, Eq, PartialEq)]
pub struct Credential {
    /// `None` only for a file left unread by a [`Resolution::Structural`] check.
    value: Option<String>,
    file: Option<CredentialFile>,
}

/// The path of a file holding a secret, such as an API token. The daemon
/// reads it once at startup, so restart it after changing the file.
/// Surrounding whitespace, such as a trailing newline, is removed, and the
/// file must be a non-empty regular file; symlinks to one are followed.
/// A relative path resolves against `$CREDENTIALS_DIRECTORY`, where systemd's
/// `LoadCredential=` puts credentials, and may not contain `.` or `..`.
/// Errors and the settings shown by the dashboard name the file, never its
/// contents.
//
// The absolute location of a credential file: relative paths resolve while
// the configuration loads.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(try_from = "PathBuf")]
#[schema(value_type = String)]
pub struct CredentialFile(PathBuf);

impl CredentialFile {
    /// Resolves `file`, joining a relative path onto `directory`; one with
    /// `.` or `..` components is refused, so it stays inside. A structural
    /// check accepts a relative path without a directory, which only systemd
    /// provides at startup.
    fn resolve(file: PathBuf, directory: Option<PathBuf>) -> Result<Self, CredentialError> {
        if file.is_absolute() {
            return Ok(Self(file));
        }
        if !file
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        {
            return Err(CredentialError::OutsideDirectory { path: file });
        }
        match directory {
            Some(directory) => Ok(Self(directory.join(file))),
            None if Resolution::current() == Resolution::Structural => {
                Ok(Self(Path::new("$CREDENTIALS_DIRECTORY").join(file)))
            }
            None => Err(CredentialError::NoDirectory { path: file }),
        }
    }

    /// The resolved absolute path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Reads the file, refusing anything but a regular file. Opening never
    /// blocks, so a FIFO is refused instead of stalling startup. Symlinks are
    /// followed: secret managers such as sops-nix and agenix link their files
    /// into place, and systemd's credentials directory is not writable by
    /// the service.
    fn read(&self) -> std::io::Result<String> {
        use rustix::fs::{Mode, OFlags};
        let file = std::fs::File::from(rustix::fs::open(
            &self.0,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        std::io::read_to_string(file)
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
    /// Reads the credential file of the `key_file` setting. Surrounding whitespace, such as a trailing newline, is removed. A
    /// structural check leaves the file unread.
    ///
    /// # Errors
    ///
    /// The file must be a readable, non-empty regular file.
    pub(crate) fn read(key: &'static str, file: CredentialFile) -> Result<Self, CredentialError> {
        if Resolution::current() == Resolution::Structural {
            return Ok(Self {
                value: None,
                file: Some(file),
            });
        }
        let value = file
            .read()
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
            value: Some(value),
            file: Some(file),
        })
    }

    /// The secret itself. Never log or display it. Empty for a file left
    /// unread by a structural check, whose configuration is never used.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.value.as_deref().unwrap_or_default()
    }

    /// The secret, or `None` when a structural check left its file unread.
    /// Validation of secret contents uses this to skip unread files.
    pub(super) fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }
}

impl From<String> for Credential {
    fn from(value: String) -> Self {
        Self {
            value: Some(value),
            file: None,
        }
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
    /// A relative path was given outside a systemd credential context.
    #[error("relative credential file {} requires $CREDENTIALS_DIRECTORY", path.display())]
    NoDirectory {
        /// Configured relative path.
        path: PathBuf,
    },
    /// A relative path had `.` or `..` components, which could leave
    /// `$CREDENTIALS_DIRECTORY`.
    #[error(
        "relative credential file {} must name a file inside $CREDENTIALS_DIRECTORY",
        path.display()
    )]
    OutsideDirectory {
        /// Configured relative path.
        path: PathBuf,
    },
    /// The file could not be read.
    #[error("could not read `{key}_file` {}: {error}", path.display())]
    Read {
        /// Setting name, without `_file`.
        key: &'static str,
        /// Resolved path.
        path: PathBuf,
        /// Underlying filesystem failure.
        error: std::io::Error,
    },
    /// The file contained only whitespace.
    #[error("`{key}_file` {} is empty", path.display())]
    Empty {
        /// Setting name, without `_file`.
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
    fn files_must_be_regular_and_inside_the_credentials_directory() {
        let directory = tempfile::tempdir().unwrap();
        assert!(matches!(
            CredentialFile::resolve("../webhook".into(), Some(directory.path().to_path_buf())),
            Err(CredentialError::OutsideDirectory { .. })
        ));
        let fifo = directory.path().join("fifo");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .unwrap();
        let error = Credential::read("url", CredentialFile::resolve(fifo, None).unwrap());
        assert!(
            error
                .unwrap_err()
                .to_string()
                .contains("not a regular file")
        );
    }

    #[test]
    fn unreadable_files_are_refused_without_their_contents() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), " \n").unwrap();
        for path in [file.path().to_path_buf(), file.path().join("missing")] {
            let file = CredentialFile::resolve(path, None).unwrap();
            assert!(Credential::read("headers", file).is_err());
        }
        let inline = Credential::from("secret");
        assert_eq!(format!("{inline} {inline:?}"), "inline Credential(inline)");
    }
}
