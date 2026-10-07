//! Cloudflare API v4 with a bearer token scoped to Zone:Read and DNS:Edit.
use super::{ApiError, RecordId, Zone, send};
use crate::config::{Credential, CredentialError, CredentialFile};
use piqueld_core::manifest::Hostname;
use serde::Deserialize;
use serde_json::json;

/// API base URL.
const API: &str = "https://api.cloudflare.com/client/v4";

/// One Cloudflare account, configured as:
///
/// ```toml
/// [[dns.providers]]
/// kind = "cloudflare"
/// api_token_file = "cloudflare-dns-token"
/// ```
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(try_from = "RawCloudflare")]
pub struct Cloudflare {
    api_token: Credential,
    /// API base URL; tests point it at recorded responses.
    api: String,
}

/// TOML form of [`Cloudflare`], before its token file is read.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCloudflare {
    api_token_file: CredentialFile,
}

impl TryFrom<RawCloudflare> for Cloudflare {
    type Error = CredentialError;
    fn try_from(raw: RawCloudflare) -> Result<Self, Self::Error> {
        Ok(Self {
            api_token: Credential::read("api_token", raw.api_token_file)?,
            api: API.into(),
        })
    }
}

/// Response envelope shared by every endpoint.
#[derive(Deserialize)]
struct Envelope<T> {
    result: T,
    result_info: Option<ResultInfo>,
}

/// Pagination of list endpoints.
#[derive(Deserialize)]
struct ResultInfo {
    page: u32,
    total_pages: u32,
}

/// An object identified by `id`, such as a zone or a record.
#[derive(Deserialize)]
struct Object {
    id: String,
    #[serde(default)]
    name: String,
}

impl Cloudflare {
    /// The token file, as shown by read-only settings.
    #[must_use]
    pub fn api_token(&self) -> &Credential {
        &self.api_token
    }

    /// Sends an authenticated request and unwraps the response envelope.
    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<Envelope<T>, ApiError> {
        send(request.bearer_auth(self.api_token.expose())).await
    }

    /// Lists zones 50 per page. Names that are not public hostnames are skipped.
    pub(super) async fn zones(&self, http: &reqwest::Client) -> Result<Vec<Zone>, ApiError> {
        let mut zones = Vec::new();
        for page in 1.. {
            let response: Envelope<Vec<Object>> = self
                .call(http.get(format!("{}/zones?per_page=50&page={page}", self.api)))
                .await?;
            zones.extend(response.result.into_iter().filter_map(|zone| {
                Some(Zone {
                    name: Hostname::parse(&zone.name).ok()?,
                    id: zone.id,
                })
            }));
            if response
                .result_info
                .is_none_or(|info| info.page >= info.total_pages)
            {
                break;
            }
        }
        Ok(zones)
    }

    /// Creates a TXT record with a 60s TTL. Content is quoted, as Cloudflare
    /// expects for TXT records.
    pub(super) async fn create_txt(
        &self,
        http: &reqwest::Client,
        zone: &Zone,
        name: &str,
        value: &str,
    ) -> Result<RecordId, ApiError> {
        let response: Envelope<Object> = self
            .call(
                http.post(format!("{}/zones/{}/dns_records", self.api, zone.id))
                    .json(&json!({"type":"TXT","name":name,"content":format!("\"{value}\""),"ttl":60})),
            )
            .await?;
        Ok(RecordId(response.result.id))
    }

    /// Deletes a record by its ID.
    pub(super) async fn delete_record(
        &self,
        http: &reqwest::Client,
        zone: &Zone,
        id: &RecordId,
    ) -> Result<(), ApiError> {
        self.call::<serde_json::Value>(http.delete(format!(
            "{}/zones/{}/dns_records/{}",
            self.api, zone.id, id.0
        )))
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dns::{DnsProvider, tests::Recorded};
    use axum::http::Method;

    fn provider(api: String) -> DnsProvider {
        DnsProvider::Cloudflare(Cloudflare {
            api_token: "token".into(),
            api,
        })
    }

    #[tokio::test]
    async fn zones_follow_pages_and_records_round_trip() {
        let recorded = Recorded::serve(vec![
            (Method::GET, "/zones?per_page=50&page=1", 200,
                r#"{"success":true,"errors":[],"result":[{"id":"z1","name":"piquel.fr"},{"id":"z2","name":"local"}],"result_info":{"page":1,"per_page":50,"total_pages":2}}"#),
            (Method::GET, "/zones?per_page=50&page=2", 200,
                r#"{"success":true,"errors":[],"result":[{"id":"z3","name":"example.com"}],"result_info":{"page":2,"per_page":50,"total_pages":2}}"#),
            (Method::POST, "/zones/z1/dns_records", 200,
                r#"{"success":true,"errors":[],"result":{"id":"r1","type":"TXT","name":"_acme-challenge.piquel.fr","content":"\"v\""}}"#),
            (Method::DELETE, "/zones/z1/dns_records/r1", 200,
                r#"{"success":true,"errors":[],"result":{"id":"r1"}}"#),
        ]).await;
        let provider = provider(recorded.url.clone());
        let http = reqwest::Client::new();
        let zones = provider.zones(&http).await.unwrap();
        assert_eq!(
            zones
                .iter()
                .map(|z| (z.name.as_str(), z.id.as_str()))
                .collect::<Vec<_>>(),
            [("piquel.fr", "z1"), ("example.com", "z3")]
        );
        let id = provider
            .create_txt(&http, &zones[0], "_acme-challenge.piquel.fr", "v")
            .await
            .unwrap();
        assert_eq!(id, RecordId("r1".into()));
        provider.delete_record(&http, &zones[0], &id).await.unwrap();
        let requests = recorded.requests().await;
        assert!(
            requests
                .iter()
                .all(|r| r.authorization.as_deref() == Some("Bearer token"))
        );
        assert_eq!(
            requests[2].body,
            json!({"type":"TXT","name":"_acme-challenge.piquel.fr","content":"\"v\"","ttl":60})
        );
    }

    #[tokio::test]
    async fn api_errors_name_the_provider_and_zone_but_not_the_token() {
        let recorded = Recorded::serve(vec![(Method::POST, "/zones/z1/dns_records", 403,
            r#"{"success":false,"errors":[{"code":10000,"message":"Authentication error"}],"result":null}"#)]).await;
        let zone = Zone {
            name: Hostname::parse("piquel.fr").unwrap(),
            id: "z1".into(),
        };
        let error = provider(recorded.url.clone())
            .create_txt(
                &reqwest::Client::new(),
                &zone,
                "_acme-challenge.piquel.fr",
                "v",
            )
            .await
            .unwrap_err();
        let message = format!("{:#}", anyhow::Error::from(error));
        assert!(
            message.starts_with("cloudflare create TXT record in zone piquel.fr: HTTP 403"),
            "{message}"
        );
        assert!(message.contains("Authentication error"), "{message}");
        assert!(!message.contains("token"), "{message}");
    }
}
