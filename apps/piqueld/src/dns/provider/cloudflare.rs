//! Cloudflare API v4 with a bearer token scoped to Zone:Read and DNS:Edit.
use super::{ApiError, Found, Provider, Record, RecordId, Zone, send};
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

/// A DNS record as listed.
#[derive(Deserialize)]
struct ListedRecord {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    content: String,
    #[serde(default)]
    proxied: bool,
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

    /// Follows every page of a list endpoint; `path` ends with its query.
    async fn list<T: serde::de::DeserializeOwned>(
        &self,
        http: &reqwest::Client,
        path: &str,
    ) -> Result<Vec<T>, ApiError> {
        let mut items = Vec::new();
        for page in 1.. {
            let response: Envelope<Vec<T>> = self
                .call(http.get(format!("{}{path}&page={page}", self.api)))
                .await?;
            items.extend(response.result);
            if response
                .result_info
                .is_none_or(|info| info.page >= info.total_pages)
            {
                break;
            }
        }
        Ok(items)
    }
}

impl Provider for Cloudflare {
    fn kind(&self) -> &'static str {
        "cloudflare"
    }

    fn proxies(&self) -> bool {
        true
    }

    /// Lists zones 50 per page. Names that are not public hostnames are skipped.
    async fn zones(&self, http: &reqwest::Client) -> Result<Vec<Zone>, ApiError> {
        let zones: Vec<Object> = self.list(http, "/zones?per_page=50").await?;
        Ok(zones
            .into_iter()
            .filter_map(|zone| {
                Some(Zone {
                    name: Hostname::parse(&zone.name).ok()?,
                    id: zone.id,
                })
            })
            .collect())
    }

    /// Lists the records named exactly `name`, 100 per page.
    async fn records(
        &self,
        http: &reqwest::Client,
        zone: &Zone,
        name: &str,
    ) -> Result<Vec<Found>, ApiError> {
        let listed: Vec<ListedRecord> = self
            .list(
                http,
                &format!(
                    "/zones/{}/dns_records?name.exact={name}&per_page=100",
                    zone.id
                ),
            )
            .await?;
        let mut found = Vec::new();
        for record in listed {
            if let Some(parsed) = Record::parse(&record.kind, &record.content, record.proxied)? {
                found.push(Found {
                    id: RecordId(record.id),
                    record: parsed,
                    proxied: record.proxied,
                });
            }
        }
        Ok(found)
    }

    /// Creates a record, or replaces record `id`. TXT content is quoted, as
    /// Cloudflare expects, and proxied records use its automatic TTL.
    async fn upsert(
        &self,
        http: &reqwest::Client,
        zone: &Zone,
        name: &str,
        id: Option<&RecordId>,
        record: &Record,
    ) -> Result<RecordId, ApiError> {
        let body = match record {
            Record::Txt(text) => {
                json!({"type":"TXT","name":name,"content":format!("\"{text}\""),"ttl":record.ttl()})
            }
            Record::Cname { proxied: true, .. } => {
                json!({"type":"CNAME","name":name,"content":record.content(),"ttl":1,"proxied":true})
            }
            _ => {
                json!({"type":record.kind(),"name":name,"content":record.content(),"ttl":record.ttl(),"proxied":false})
            }
        };
        let records = format!("{}/zones/{}/dns_records", self.api, zone.id);
        let request = match id {
            Some(id) => http.put(format!("{records}/{}", id.0)),
            None => http.post(records),
        };
        let response: Envelope<Object> = self.call(request.json(&body)).await?;
        Ok(RecordId(response.result.id))
    }

    /// Deletes a record by its ID.
    async fn delete_record(
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
    use crate::dns::provider::{DnsProvider, tests::Recorded};
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
            .upsert(
                &http,
                &zones[0],
                "_acme-challenge.piquel.fr",
                None,
                &Record::Txt("v".into()),
            )
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
    async fn records_follow_pages_and_upserts_create_or_replace() {
        const LIST: &str = "/zones/z1/dns_records?name.exact=www.piquel.fr&per_page=100";
        let recorded = Recorded::serve(vec![
            (Method::GET, "/zones/z1/dns_records?name.exact=www.piquel.fr&per_page=100&page=1", 200,
                r#"{"success":true,"errors":[],"result":[{"id":"r1","type":"A","name":"www.piquel.fr","content":"192.0.2.1","proxied":false},{"id":"r2","type":"MX","name":"www.piquel.fr","content":"mail.piquel.fr"}],"result_info":{"page":1,"per_page":100,"total_pages":2}}"#),
            (Method::GET, "/zones/z1/dns_records?name.exact=www.piquel.fr&per_page=100&page=2", 200,
                r#"{"success":true,"errors":[],"result":[{"id":"r3","type":"CNAME","name":"www.piquel.fr","content":"t.cfargotunnel.com","proxied":true},{"id":"r4","type":"TXT","name":"www.piquel.fr","content":"\"v=spf1 -all\""}],"result_info":{"page":2,"per_page":100,"total_pages":2}}"#),
            (Method::PUT, "/zones/z1/dns_records/r1", 200,
                r#"{"success":true,"errors":[],"result":{"id":"r1"}}"#),
            (Method::POST, "/zones/z1/dns_records", 200,
                r#"{"success":true,"errors":[],"result":{"id":"r5"}}"#),
        ]).await;
        let provider = provider(recorded.url.clone());
        let http = reqwest::Client::new();
        let zone = Zone {
            name: Hostname::parse("piquel.fr").unwrap(),
            id: "z1".into(),
        };
        let found = provider
            .records(&http, &zone, "www.piquel.fr")
            .await
            .unwrap();
        let tunnel = Hostname::parse("t.cfargotunnel.com").unwrap();
        assert_eq!(
            found
                .iter()
                .map(|f| (f.id.0.as_str(), f.record.clone()))
                .collect::<Vec<_>>(),
            [
                ("r1", Record::A("192.0.2.1".parse().unwrap())),
                (
                    "r3",
                    Record::Cname {
                        target: tunnel.clone(),
                        proxied: true
                    }
                ),
                ("r4", Record::Txt("v=spf1 -all".into())),
            ]
        );
        let replaced = provider
            .upsert(
                &http,
                &zone,
                "www.piquel.fr",
                Some(&found[0].id),
                &Record::Aaaa("2001:db8::1".parse().unwrap()),
            )
            .await
            .unwrap();
        assert_eq!(replaced, RecordId("r1".into()));
        let created = provider
            .upsert(
                &http,
                &zone,
                "www.piquel.fr",
                None,
                &Record::Cname {
                    target: tunnel,
                    proxied: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(created, RecordId("r5".into()));
        let requests = recorded.requests().await;
        assert!(requests[0].path.starts_with(LIST));
        assert_eq!(
            requests[2].body,
            json!({"type":"AAAA","name":"www.piquel.fr","content":"2001:db8::1","ttl":300,"proxied":false})
        );
        // Proxied records use Cloudflare's automatic TTL.
        assert_eq!(
            requests[3].body,
            json!({"type":"CNAME","name":"www.piquel.fr","content":"t.cfargotunnel.com","ttl":1,"proxied":true})
        );
    }

    #[tokio::test]
    async fn discovery_is_reused_for_an_hour_unless_forced_or_failed() {
        const ZONES: &str = "/zones?per_page=50&page=1";
        let working = Recorded::serve(vec![(
            Method::GET,
            ZONES,
            200,
            r#"{"success":true,"errors":[],"result":[{"id":"z1","name":"piquel.fr"}]}"#,
        )])
        .await;
        let failing = Recorded::serve(vec![(Method::GET, ZONES, 500, "{}")]).await;
        let mut dns = crate::dns::Dns::new(vec![provider(working.url.clone()).into()]).unwrap();
        dns.refresh().await;
        dns.refresh().await;
        assert_eq!(working.requests().await.len(), 1);
        dns.discover().await;
        assert_eq!(working.requests().await.len(), 1);
        // A failed forced discovery keeps the zones, and the next hourly
        // refresh retries at once instead of trusting the earlier success.
        dns.providers[0] = provider(failing.url.clone()).into();
        dns.discover().await;
        let status = &dns.status().await[0];
        assert!(
            !status.healthy && status.zones == ["piquel.fr"],
            "{status:?}"
        );
        dns.providers[0] = provider(working.url.clone()).into();
        dns.refresh().await;
        assert_eq!(working.requests().await.len(), 1);
        assert!(dns.status().await[0].healthy);
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
            .upsert(
                &reqwest::Client::new(),
                &zone,
                "_acme-challenge.piquel.fr",
                None,
                &Record::Txt("v".into()),
            )
            .await
            .unwrap_err();
        let message = format!("{:#}", anyhow::Error::from(error));
        assert!(
            message.starts_with("cloudflare write record in zone piquel.fr: HTTP 403"),
            "{message}"
        );
        assert!(message.contains("Authentication error"), "{message}");
        assert!(!message.contains("token"), "{message}");
    }
}
