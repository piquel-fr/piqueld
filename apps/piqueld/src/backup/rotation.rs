//! Deleting old archives that the daemon or `piqueld backup` generated.

use super::BackupError;
use std::{fs, path::Path};

/// Archive families that rotation deletes from. Only exact generated names
/// match, so archives named by hand in the same directory are never deleted.
#[derive(Clone, Copy)]
pub(super) enum Rotation {
    /// `piqueld-<ms>.tar`, written by `piqueld backup --directory`.
    Scheduled,
    /// `pre-<schema>-<ms>.tar`, written before migrations.
    PreMigration,
}

impl Rotation {
    /// Creation time encoded in `name`, if it belongs to this family.
    fn created_at(self, name: &str) -> Option<i64> {
        let stem = name.strip_suffix(".tar")?;
        let created = match self {
            Self::Scheduled => stem.strip_prefix("piqueld-")?,
            Self::PreMigration => {
                let (schema, created) = stem.strip_prefix("pre-")?.split_once('-')?;
                decimal(schema)?;
                created
            }
        };
        decimal(created)
    }

    /// Deletes all but the newest `keep` archives of this family in `directory`.
    pub(super) fn prune(self, directory: &Path, keep: usize) -> Result<(), BackupError> {
        let mut archives = Vec::new();
        for entry in
            fs::read_dir(directory).map_err(BackupError::io("read directory", directory))?
        {
            let name = entry
                .map_err(BackupError::io("read directory", directory))?
                .file_name();
            if let Some(created) = name.to_str().and_then(|name| self.created_at(name)) {
                archives.push((created, name));
            }
        }
        archives.sort_unstable_by(|left, right| right.cmp(left));
        for (_, name) in archives.into_iter().skip(keep) {
            let path = directory.join(name);
            fs::remove_file(&path).map_err(BackupError::io("delete old backup", &path))?;
        }
        Ok(())
    }
}

/// Parses an unsigned decimal with no sign or other characters.
fn decimal(text: &str) -> Option<i64> {
    if text.bytes().all(|byte| byte.is_ascii_digit()) {
        text.parse().ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_keeps_the_newest_archives_with_exact_generated_names() {
        let directory = tempfile::tempdir().unwrap();
        for name in [
            "pre-9-100.tar",
            "pre-10-300.tar",
            "pre-10-200.tar",
            "pre-10-50.tar",
            "pre-manual-1.tar",
            "piqueld-1.tar",
        ] {
            fs::write(directory.path().join(name), b"").unwrap();
        }
        Rotation::PreMigration.prune(directory.path(), 2).unwrap();
        let mut remaining: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        remaining.sort();
        assert_eq!(
            remaining,
            [
                "piqueld-1.tar",
                "pre-10-200.tar",
                "pre-10-300.tar",
                "pre-manual-1.tar"
            ]
        );
        assert_eq!(Rotation::Scheduled.created_at("piqueld-5.tar"), Some(5));
        assert_eq!(
            Rotation::Scheduled.created_at("piqueld-before-upgrade-5.tar"),
            None
        );
    }
}
