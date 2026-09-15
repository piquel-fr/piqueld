use crate::{BuildLogPage, BuildRecord, Client, ClientError, Page};
use http::Method;
impl Client {
    /// Reads one page of build attempts, newest first.
    /// # Errors
    /// Returns transport, decoding, or API errors.
    pub async fn builds(
        &self,
        application: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<Page<BuildRecord>, ClientError> {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        if let Some(app) = application {
            query.append_pair("application_id", app);
        }
        if let Some(cursor) = cursor {
            query.append_pair("cursor", cursor);
        }
        self.send::<_, ()>(
            Method::GET,
            &format!("{}/builds?{}", crate::API_PREFIX, query.finish()),
            None,
            &[],
        )
        .await
    }
    /// Reads at most 64 KiB of build output from a byte offset.
    /// # Errors
    /// Returns transport, decoding, or API errors.
    pub async fn build_logs(&self, id: i64, offset: i64) -> Result<BuildLogPage, ClientError> {
        self.send::<_, ()>(
            Method::GET,
            &format!("{}/builds/{id}/logs?offset={offset}", crate::API_PREFIX),
            None,
            &[],
        )
        .await
    }
}
