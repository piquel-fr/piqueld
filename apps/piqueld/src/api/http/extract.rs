//! Request decoding that keeps malformed input inside the API's structured
//! error contract: path parameters, query strings, bodies, and single-valued
//! headers each fail with a stable 4xx `ApiError` rather than Axum's plain
//! text rejections.
use super::{ApiError, JSON, TOML};
use axum::{
    body::Bytes,
    extract::{FromRequest, FromRequestParts, Path, Query, Request, rejection::BytesRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header, request::Parts},
};
use piqueld_core::api::ApplyApplicationRequest;
use piqueld_core::manifest::ValidatedTemplate;
use serde::de::DeserializeOwned;

/// Header carrying a mutation's idempotency key.
pub(super) const IDEMPOTENCY_KEY: &str = "idempotency-key";

/// Path extractor rejecting undecodable parameters with 400 `path_invalid`.
pub(super) struct ApiPath<T>(pub(super) T);

impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for ApiPath<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        Path::<T>::from_request_parts(parts, state)
            .await
            .map(|Path(value)| Self(value))
            .map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "path_invalid",
                    "invalid URL path parameter",
                )
            })
    }
}

/// Query string parameters, decoded by [`ApiQuery`].
pub(super) trait QueryParams: DeserializeOwned {
    /// Code of the 400 answering a query string that does not decode, e.g.
    /// `force_invalid`.
    const INVALID_CODE: &'static str;
    /// Message of that 400, e.g. `force must be true or false`.
    const INVALID_MESSAGE: &'static str;
}

/// Query extractor rejecting undecodable query strings with `T`'s
/// `INVALID_CODE` and `INVALID_MESSAGE`.
pub(super) struct ApiQuery<T>(pub(super) T);

impl<S: Send + Sync, T: QueryParams> FromRequestParts<S> for ApiQuery<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(value)| Self(value))
            .map_err(|_| {
                ApiError::new(StatusCode::BAD_REQUEST, T::INVALID_CODE, T::INVALID_MESSAGE)
            })
    }
}

/// Strict JSON body extractor: requires `Content-Type: application/json`
/// (415 otherwise), then reads the body (see [`request_body`]) and decodes
/// it with [`decode_json`].
pub(super) struct ApiJson<T>(pub(super) T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for ApiJson<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, ApiError> {
        if content_type(request.headers()) != Some(JSON) {
            return Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "content_type_unsupported",
                "Content-Type must be application/json",
            ));
        }
        let body = request_body(Bytes::from_request(request, state).await)?;
        decode_json(&body).map(Self)
    }
}

/// A validated manifest upload with its optional preconditions, in either
/// supported encoding:
/// - JSON: an `ApplyApplicationRequest` carrying the preconditions as fields.
/// - TOML (`application/toml` or `text/toml`): the raw manifest, with
///   preconditions in the single-valued `x-expected-generation` and
///   `x-expected-application-id` headers.
///
/// Any other content type fails with 415.
pub(super) struct ManifestUpload {
    pub(super) manifest: ValidatedTemplate,
    /// The inspected application revision.
    pub(super) expected_generation: Option<u64>,
    /// The inspected application identity.
    pub(super) expected_application_id: Option<String>,
}

impl<S: Send + Sync> FromRequest<S> for ManifestUpload {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, ApiError> {
        let headers = request.headers().clone();
        let body = request_body(Bytes::from_request(request, state).await)?;
        Self::decode(&headers, &body)
    }
}

impl ManifestUpload {
    /// Decodes and validates `body` in the encoding `headers` name.
    fn decode(headers: &HeaderMap, body: &[u8]) -> Result<Self, ApiError> {
        match content_type(headers) {
            Some(value) if value.eq_ignore_ascii_case(JSON) => {
                let request: ApplyApplicationRequest = decode_json(body)?;
                Ok(Self {
                    manifest: request.manifest.validate_template()?,
                    expected_generation: request.expected_generation,
                    expected_application_id: request.expected_application_id,
                })
            }
            Some(value)
                if value.eq_ignore_ascii_case(TOML) || value.eq_ignore_ascii_case("text/toml") =>
            {
                let text = std::str::from_utf8(body).map_err(|_| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "toml_malformed",
                        "request TOML is malformed",
                    )
                })?;
                let invalid =
                    |message| ApiError::new(StatusCode::BAD_REQUEST, "generation_invalid", message);
                let expected_generation = single_header(headers, "x-expected-generation", || {
                    invalid("expected generation must occur once")
                })?
                .map(|value| {
                    value
                        .to_str()
                        .ok()
                        .and_then(|value| value.parse::<u64>().ok())
                        .ok_or_else(|| invalid("expected generation must be an unsigned integer"))
                })
                .transpose()?;
                Ok(Self {
                    manifest: piqueld_core::manifest::parse_template_toml(text)?,
                    expected_generation,
                    expected_application_id: optional_header(headers, "x-expected-application-id")?,
                })
            }
            _ => Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "content_type_unsupported",
                "Content-Type must be application/json or application/toml",
            )),
        }
    }
}

/// Returns the media type from `Content-Type`, without parameters.
///
/// ```text
/// "application/json; charset=utf-8" -> "application/json"
/// ```
pub(super) fn content_type(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or(value).trim())
}

/// Unwraps a buffered body, reporting the size limit as 413
/// `request_body_too_large`, a body that stopped arriving (see
/// `BODY_IDLE_TIMEOUT`) as 408 `request_body_timeout`, and other read
/// failures as 400.
pub(super) fn request_body(body: Result<Bytes, BytesRejection>) -> Result<Bytes, ApiError> {
    body.map_err(|rejection| {
        if let Some(timed_out) = body_timeout(&rejection) {
            timed_out
        } else if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_body_too_large",
                "request body exceeds the maximum allowed size",
            )
        } else {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "request_body_unreadable",
                "request body could not be read",
            )
        }
    })
}

/// 408 `request_body_timeout` when reading a body failed because it stopped
/// arriving (see `BODY_IDLE_TIMEOUT`); `None` for any other failure.
pub(super) fn body_timeout(rejection: &BytesRejection) -> Option<ApiError> {
    std::iter::successors(
        Some(rejection as &(dyn std::error::Error + 'static)),
        |error| error.source(),
    )
    .any(<dyn std::error::Error>::is::<tower_http::timeout::TimeoutError>)
    .then(|| {
        ApiError::new(
            StatusCode::REQUEST_TIMEOUT,
            "request_body_timeout",
            "request body stopped arriving before it was complete",
        )
    })
}

/// Strictly decodes a JSON body, rejecting trailing data. The failing field
/// path is only logged at debug level (sanitized); clients get a generic
/// `json_malformed` error.
pub(super) fn decode_json<T: DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    let malformed = || {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "json_malformed",
            "request JSON is malformed or contains unknown fields",
        )
    };
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let value = serde_path_to_error::deserialize(&mut deserializer).map_err(|error| {
        let path = piqueld_core::manifest::safe_decode_path(&error.path().to_string());
        tracing::debug!(%path, "request JSON was rejected");
        malformed()
    })?;
    deserializer.end().map_err(|_| malformed())?;
    Ok(value)
}

/// Reads an optional single-valued header; repeated or non-ASCII values are
/// rejected with `header_invalid`.
pub(super) fn optional_header(headers: &HeaderMap, name: &str) -> Result<Option<String>, ApiError> {
    let invalid = |message| ApiError::new(StatusCode::BAD_REQUEST, "header_invalid", message);
    single_header(headers, name, || {
        invalid("mutation headers must occur once")
    })?
    .map(|value| {
        value
            .to_str()
            .map(str::to_owned)
            .map_err(|_| invalid("invalid mutation header"))
    })
    .transpose()
}

/// The value of header `name`, which may occur at most once; `repeated`
/// builds the error when it occurs more often.
pub(super) fn single_header<'a>(
    headers: &'a HeaderMap,
    name: &str,
    repeated: impl FnOnce() -> ApiError,
) -> Result<Option<&'a HeaderValue>, ApiError> {
    let mut values = headers.get_all(name).iter();
    let value = values.next();
    if values.next().is_some() {
        return Err(repeated());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A body that stops arriving times out as 408 `request_body_timeout`,
    /// not as an unreadable body.
    #[tokio::test]
    async fn stalled_bodies_time_out() {
        let stalled = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();
        let body = tower_http::timeout::TimeoutBody::new(
            Duration::from_millis(10),
            axum::body::Body::from_stream(stalled),
        );
        let request = Request::new(axum::body::Body::new(body));
        let error = request_body(Bytes::from_request(request, &()).await).unwrap_err();
        assert_eq!(error.status, StatusCode::REQUEST_TIMEOUT);
        assert_eq!(error.body().code, "request_body_timeout");
    }
}
