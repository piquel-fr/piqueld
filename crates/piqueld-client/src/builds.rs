use crate::{BuildLogPage, BuildRecord, Client, ClientError, Page, client::generated_result};
impl Client {
    /// Reads one page of build attempts, newest first.
    /// # Errors
    /// Returns transport, decoding, or API errors.
    pub async fn builds(
        &self,
        application: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<Page<BuildRecord>, ClientError> {
        generated_result(self.generated.list_builds(application, cursor, None).await)
            .await
            .map(|response| response.data)
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
        generated_result(self.generated.build_logs(id, before, stream.as_ref()).await)
            .await
            .map(|response| response.data)
    }
}
