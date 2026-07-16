use http::Method;
use http_body_util::BodyExt;

use crate::{Client, ClientError, decode_api_error};

impl Client {
    pub async fn openapi(&self) -> Result<serde_json::Value, ClientError> {
        self.with_request_timeout(async {
            let response = self
                .raw_request::<()>(Method::GET, "/api/v1/openapi.json", None, &[])
                .await?;
            if !response.status().is_success() {
                return Err(decode_api_error(response).await);
            }
            serde_json::from_slice(
                &response
                    .into_body()
                    .collect()
                    .await
                    .map_err(|_| ClientError::Transport)?
                    .to_bytes(),
            )
            .map_err(|_| ClientError::Decode)
        })
        .await
    }
}
