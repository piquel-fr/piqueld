use crate::generated;
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
        generated::ListBuilds {
            application_id: application.map(str::to_owned),
            cursor: cursor.map(str::to_owned),
            ..Default::default()
        }
        .send(self)
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
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        if let Some(before) = before {
            query.append_pair("before", &before.to_string());
        }
        if let Some(stream) = stream {
            query.append_pair("stream", stream.as_str());
        }
        self.send::<_, ()>(
            Method::GET,
            &format!("{}/builds/{id}/logs?{}", crate::API_PREFIX, query.finish()),
            None,
            &[],
        )
        .await
    }
}
