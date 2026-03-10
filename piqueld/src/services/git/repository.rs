use std::path::PathBuf;

use gix::bstr::BString;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Deserialize)]
// TODO: add ref as well. We should be able to manage a repository
// with different refs
pub struct RepositoryInfo {
    owner: String,
    name: String,
    path: PathBuf,
}

impl RepositoryInfo {
    pub fn new(owner: String, name: String, mut root: PathBuf) -> Self {
        // TODO: in the future we should hash the full name & ref and use that
        // as the path to avoid issues with duplicate paths
        root.push(&name);

        Self {
            path: root.to_path_buf(),
            owner,
            name,
        }
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn owner(&self) -> &str {
        &self.owner
    }
    /// Returns {owner}/{name}
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner(), self.name())
    }
    pub fn make_url(&self) -> Result<gix::Url, gix::url::parse::Error> {
        gix::Url::from_parts(
            gix::url::Scheme::Ssh,
            Some("git".into()),
            None,
            Some("github.com".into()),
            None,
            BString::from(self.full_name()),
            false,
        )
    }
    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}
