use std::path::PathBuf;

use gix::bstr::BString;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryInfo {
    owner: String,
    name: String,
}

impl RepositoryInfo {
    pub fn new(owner: &str, name: &str) -> Self {
        Self {
            owner: owner.to_string(),
            name: name.to_string(),
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
    pub fn path(&self, mut root: PathBuf) -> PathBuf {
        // TODO: in the future we should hash the full name and use that as
        // the path to avoid issues with duplicate paths
        root.push(self.name());
        root
    }
}
