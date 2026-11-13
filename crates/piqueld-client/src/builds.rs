//! Build history and build output retrieval.
use crate::{BuildLogPage, BuildRecord, Client, ClientError, Page, client::EnvelopedCall};
impl Client {
    /// Reads one page of build attempts, newest first, optionally of one
    /// application's environments or of one environment, of at most `limit`
    /// builds, or the daemon's default.
    /// # Errors
    /// Returns transport, decoding, or API errors.
    pub async fn builds(
        &self,
        application: Option<&str>,
        environment: Option<&str>,
        cursor: Option<&str>,
        limit: Option<u16>,
    ) -> Result<Page<BuildRecord>, ClientError> {
        if let Some(limit) = limit {
            crate::client::check_page_limit("build", limit)?;
        }
        self.generated
            .list_builds(application, cursor, environment, limit.map(i64::from))
            .data()
            .await
    }
    /// Reads the newest filtered build output before an optional exclusive cursor.
    /// # Errors
    /// Returns transport, decoding, or API errors.
    pub async fn build_logs(
        &self,
        id: i64,
        before: Option<i64>,
        stream: Option<crate::LogStream>,
    ) -> Result<BuildLogPage, ClientError> {
        self.generated
            .build_logs(id, before, stream.as_ref())
            .data()
            .await
    }
}
