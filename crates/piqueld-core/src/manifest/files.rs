//! Read-only files mounted into services: their manifest shape, their limits,
//! and the repository contents a deployment captures for them.
//!
//! File contents are never rendered with variables: configuration formats such
//! as Prometheus' and Grafana's use `${…}` and `{{ }}` themselves.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;
use utoipa::ToSchema;

/// A read-only file mounted into a service's containers, from exactly one
/// source: `path` in the manifest's repository, or inline `content`.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(try_from = "ServiceFileFields")]
pub struct ServiceFile {
    /// Absolute, normalized container path of the file.
    pub target: String,
    /// Where the file's content comes from.
    #[serde(flatten)]
    pub source: FileSource,
    /// Octal permissions, `"0444"` by default. The file is owned by root.
    #[serde(default, skip_serializing_if = "FileMode::is_default")]
    pub mode: FileMode,
}

/// Where a file's content comes from.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum FileSource {
    /// A file in the manifest's own repository, relative to its root, read at
    /// the exact commit the manifest was fetched from. Only repository-backed
    /// applications can use it.
    Path(String),
    /// The content itself.
    Content(String),
}

/// The fields a manifest spells a [`ServiceFile`] with, before checking that
/// exactly one source is set.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceFileFields {
    target: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    mode: FileMode,
}

impl TryFrom<ServiceFileFields> for ServiceFile {
    type Error = &'static str;

    fn try_from(fields: ServiceFileFields) -> Result<Self, Self::Error> {
        let source = match (fields.path, fields.content) {
            (Some(path), None) => FileSource::Path(path),
            (None, Some(content)) => FileSource::Content(content),
            _ => return Err("a file sets exactly one of `path` or `content`"),
        };
        Ok(Self {
            target: fields.target,
            source,
            mode: fields.mode,
        })
    }
}

impl ServiceFile {
    /// Largest content of one file: Swarm's limit on one config.
    pub const MAX_BYTES: usize = 500 * 1024;
    /// Most files one service may mount.
    pub const MAX_PER_SERVICE: usize = 32;
    /// Most bytes an application's files may total: inline contents, plus
    /// each repository file once.
    pub const MAX_TOTAL_BYTES: usize = 2 * 1024 * 1024;
    /// Directories whose files piqueld mounts itself: secrets, and the health
    /// probe at [`piqueld_probe::CONTAINER_PATH`].
    pub const RESERVED_DIRECTORIES: [&str; 2] = ["/run/secrets", "/run/piqueld"];

    /// Whether the target is one of [`Self::RESERVED_DIRECTORIES`], inside
    /// one, or a directory containing one.
    #[must_use]
    pub fn reserved_target(&self) -> bool {
        Self::RESERVED_DIRECTORIES.iter().any(|directory| {
            *directory == self.target
                || inside(&self.target, directory)
                || inside(directory, &self.target)
        })
    }

    /// Whether a file at the target would shadow `other`, a path inside it.
    #[must_use]
    pub fn contains(&self, other: &str) -> bool {
        inside(other, &self.target)
    }
}

/// Whether `path` is strictly inside the directory `directory`.
fn inside(path: &str, directory: &str) -> bool {
    path.strip_prefix(directory)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Octal file permissions from `0000` to `0777`, written as text such as
/// `"0444"` or `"644"`.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, ToSchema)]
#[schema(value_type = String, pattern = "^0?[0-7]{3}$", example = "0444")]
pub struct FileMode(u32);

impl FileMode {
    /// Readable by every container user, writable by none.
    pub const READ_ONLY: Self = Self(0o444);

    /// The permission bits.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether this is the default mode, which exports omit.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl Default for FileMode {
    fn default() -> Self {
        Self::READ_ONLY
    }
}

/// A mode that is not an octal permission.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("file modes are octal permissions from \"0000\" to \"0777\", e.g. \"0444\"")]
pub struct FileModeError;

impl FromStr for FileMode {
    type Err = FileModeError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let digits = if text.len() == 4 {
            text.strip_prefix('0').ok_or(FileModeError)?
        } else {
            text
        };
        if digits.len() != 3 || !digits.bytes().all(|digit| (b'0'..=b'7').contains(&digit)) {
            return Err(FileModeError);
        }
        u32::from_str_radix(digits, 8)
            .map(Self)
            .map_err(|_| FileModeError)
    }
}

impl fmt::Display for FileMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:04o}", self.0)
    }
}

impl Serialize for FileMode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for FileMode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// The content of each repository file an application's services mount,
/// keyed by path from the repository root, as read at the commit its manifest
/// was fetched from. A deployment captures them once, with its manifest, so
/// retries, restarts, and promotions mount exactly the same bytes.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(transparent)]
pub struct RepositoryFiles(BTreeMap<String, String>);

impl RepositoryFiles {
    /// Whether no repository file was captured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The captured content of `path`.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&str> {
        self.0.get(path).map(String::as_str)
    }

    /// Total bytes of every captured file.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.0.values().map(String::len).sum()
    }
}

impl FromIterator<(String, String)> for RepositoryFiles {
    fn from_iter<I: IntoIterator<Item = (String, String)>>(files: I) -> Self {
        Self(files.into_iter().collect())
    }
}

/// Why a deployment cannot capture the repository files its services mount.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum FileCaptureError {
    /// The path names nothing at the fetched commit.
    #[error("repository file {0} does not exist at the fetched commit")]
    Missing(String),
    /// The file is empty, which Swarm configs cannot be.
    #[error("repository file {0} is empty")]
    Empty(String),
    /// The path names a directory, or leaves the repository through a symlink.
    #[error("repository file {0} is not a regular file inside the repository")]
    NotAFile(String),
    /// The file exceeds [`ServiceFile::MAX_BYTES`].
    #[error("repository file {0} is larger than {max} bytes", max = ServiceFile::MAX_BYTES)]
    TooLarge(String),
    /// The file is not UTF-8 text.
    #[error("repository file {0} is not UTF-8 text")]
    NotText(String),
    /// Inline and repository files together exceed [`ServiceFile::MAX_TOTAL_BYTES`].
    #[error("the application's files total more than {max} bytes", max = ServiceFile::MAX_TOTAL_BYTES)]
    TotalTooLarge,
}

impl super::ApplicationSpec {
    /// The repository paths services mount files from, each once.
    #[must_use]
    pub fn repository_file_paths(&self) -> BTreeSet<&str> {
        self.services
            .iter()
            .flat_map(|service| &service.files)
            .filter_map(|file| match &file.source {
                FileSource::Path(path) => Some(path.as_str()),
                FileSource::Content(_) => None,
            })
            .collect()
    }

    /// Total bytes of inline file contents, which count towards
    /// [`ServiceFile::MAX_TOTAL_BYTES`] with the repository files.
    #[must_use]
    pub fn inline_file_bytes(&self) -> usize {
        self.services
            .iter()
            .flat_map(|service| &service.files)
            .map(|file| match &file.source {
                FileSource::Content(content) => content.len(),
                FileSource::Path(_) => 0,
            })
            .sum()
    }

    /// Checks `files`, captured for this specification, against the size limits.
    ///
    /// # Errors
    ///
    /// Returns the first empty file, the first over
    /// [`ServiceFile::MAX_BYTES`], or [`FileCaptureError::TotalTooLarge`].
    pub(super) fn check_captured(&self, files: &RepositoryFiles) -> Result<(), FileCaptureError> {
        if let Some((path, _)) = files.0.iter().find(|(_, content)| content.is_empty()) {
            return Err(FileCaptureError::Empty(path.clone()));
        }
        if let Some((path, _)) = files
            .0
            .iter()
            .find(|(_, content)| content.len() > ServiceFile::MAX_BYTES)
        {
            return Err(FileCaptureError::TooLarge(path.clone()));
        }
        if self.inline_file_bytes() + files.bytes() > ServiceFile::MAX_TOTAL_BYTES {
            return Err(FileCaptureError::TotalTooLarge);
        }
        Ok(())
    }
}

/// The lowercase hexadecimal SHA-256 digest of `content`, which names the
/// Swarm config a file is mounted from.
#[must_use]
pub fn content_digest(content: &str) -> String {
    format!("{:x}", Sha256::digest(content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_has_exactly_one_source_and_an_octal_mode() {
        let parse = |toml: &str| toml::from_str::<ServiceFile>(toml);
        assert_eq!(
            parse("target = \"/etc/motd\"\ncontent = \"${HOME} {{ x }}\"").unwrap(),
            ServiceFile {
                target: "/etc/motd".into(),
                source: FileSource::Content("${HOME} {{ x }}".into()),
                mode: FileMode::READ_ONLY,
            }
        );
        let file = parse("target = \"/etc/a\"\npath = \"a.yaml\"\nmode = \"0640\"").unwrap();
        assert_eq!(
            (file.source.clone(), file.mode.bits()),
            (FileSource::Path("a.yaml".into()), 0o640)
        );
        assert_eq!(
            toml::to_string(&file).unwrap(),
            "target = \"/etc/a\"\npath = \"a.yaml\"\nmode = \"0640\"\n"
        );
        for invalid in [
            "target = \"/a\"",
            "target = \"/a\"\npath = \"a\"\ncontent = \"b\"",
            "target = \"/a\"\ncontent = \"b\"\nowner = \"root\"",
            "target = \"/a\"\ncontent = \"b\"\nmode = \"0800\"",
            "target = \"/a\"\ncontent = \"b\"\nmode = \"1777\"",
            "target = \"/a\"\ncontent = \"b\"\nmode = \"44\"",
        ] {
            assert!(parse(invalid).is_err(), "{invalid}");
        }
        assert_eq!("644".parse::<FileMode>().unwrap().to_string(), "0644");
    }
}
