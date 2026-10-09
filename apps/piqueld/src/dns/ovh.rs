//! OVH API v1 with signed requests.
//!
//! Every call is signed with the application secret and consumer key over the
//! method, URL, body and the API's own clock:
//!
//! ```text
//! X-Ovh-Signature: "$1$" + hex(sha1(secret + "+" + consumer + "+" + METHOD + "+" + url + "+" + body + "+" + time))
//! ```
use super::{ApiError, Found, Record, RecordId, Zone, send};
use crate::config::{Credential, CredentialError, CredentialFile};
use piqueld_core::manifest::Hostname;
use reqwest::Method;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::fmt::Write as _;

/// The OVH API region an account belongs to.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum OvhEndpoint {
    /// OVH Europe.
    OvhEu,
    /// OVH Canada.
    OvhCa,
    /// OVH US.
    OvhUs,
}

impl OvhEndpoint {
    /// API base URL of the region.
    fn url(self) -> &'static str {
        match self {
            Self::OvhEu => "https://eu.api.ovh.com/1.0",
            Self::OvhCa => "https://ca.api.ovh.com/1.0",
            Self::OvhUs => "https://api.us.ovhcloud.com/1.0",
        }
    }
}

/// `ovh-eu`, as written in daemon TOML.
impl std::fmt::Display for OvhEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::OvhEu => "ovh-eu",
            Self::OvhCa => "ovh-ca",
            Self::OvhUs => "ovh-us",
        })
    }
}

/// One OVH account, configured as:
///
/// ```toml
/// [[dns.providers]]
/// kind = "ovh"
/// endpoint = "ovh-eu"
/// application_key_file = "ovh-application-key"
/// application_secret_file = "ovh-application-secret"
/// consumer_key_file = "ovh-consumer-key"
/// ```
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(try_from = "RawOvh")]
pub struct Ovh {
    /// API region.
    pub endpoint: OvhEndpoint,
    /// Identifies the application, sent with every request.
    pub application_key: Credential,
    /// Signs requests; never sent.
    pub application_secret: Credential,
    /// Authorizes the application on the account.
    pub consumer_key: Credential,
    /// API base URL; tests point it at recorded responses.
    api: String,
}

/// TOML form of [`Ovh`], before its credential files are read.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOvh {
    endpoint: OvhEndpoint,
    application_key_file: CredentialFile,
    application_secret_file: CredentialFile,
    consumer_key_file: CredentialFile,
}

impl TryFrom<RawOvh> for Ovh {
    type Error = CredentialError;
    fn try_from(raw: RawOvh) -> Result<Self, Self::Error> {
        Ok(Self {
            endpoint: raw.endpoint,
            application_key: Credential::read("application_key", raw.application_key_file)?,
            application_secret: Credential::read(
                "application_secret",
                raw.application_secret_file,
            )?,
            consumer_key: Credential::read("consumer_key", raw.consumer_key_file)?,
            api: raw.endpoint.url().into(),
        })
    }
}

/// A record as OVH describes it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OvhRecord {
    id: u64,
    #[serde(default)]
    field_type: String,
    #[serde(default)]
    sub_domain: String,
    #[serde(default)]
    target: String,
}

impl Ovh {
    /// Signature of one request at API time `time`.
    fn signature(&self, method: &Method, url: &str, body: &str, time: i64) -> String {
        let payload = format!(
            "{}+{}+{method}+{url}+{body}+{time}",
            self.application_secret.expose(),
            self.consumer_key.expose()
        );
        openssl::sha::sha1(payload.as_bytes()).iter().fold(
            String::from("$1$"),
            |mut signature, byte| {
                let _ = write!(signature, "{byte:02x}");
                signature
            },
        )
    }

    /// Sends a signed request, using the API's clock so local skew cannot
    /// invalidate signatures.
    async fn call<T: DeserializeOwned>(
        &self,
        http: &reqwest::Client,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T, ApiError> {
        let time: i64 = send(http.get(format!("{}/auth/time", self.api))).await?;
        let url = format!("{}{path}", self.api);
        let body = body.map(Value::to_string).unwrap_or_default();
        let signature = self.signature(&method, &url, &body, time);
        send(
            http.request(method, url)
                .header("X-Ovh-Application", self.application_key.expose())
                .header("X-Ovh-Consumer", self.consumer_key.expose())
                .header("X-Ovh-Timestamp", time.to_string())
                .header("X-Ovh-Signature", signature)
                .header("Content-Type", "application/json")
                .body(body),
        )
        .await
    }

    /// Applies pending record changes to the zone's nameservers.
    pub(super) async fn refresh(
        &self,
        http: &reqwest::Client,
        zone: &Zone,
    ) -> Result<(), ApiError> {
        self.call::<Value>(
            http,
            Method::POST,
            &format!("/domain/zone/{}/refresh", zone.id),
            None,
        )
        .await?;
        Ok(())
    }

    /// Lists zone names. Names that are not public hostnames are skipped.
    pub(super) async fn zones(&self, http: &reqwest::Client) -> Result<Vec<Zone>, ApiError> {
        let names: Vec<String> = self.call(http, Method::GET, "/domain/zone", None).await?;
        Ok(names
            .into_iter()
            .filter_map(|name| {
                Some(Zone {
                    name: Hostname::parse(&name).ok()?,
                    id: name,
                })
            })
            .collect())
    }

    /// Lists the records named exactly `name`: their IDs, then each record.
    /// OVH names records relative to their zone; the apex is the empty name.
    pub(super) async fn records(
        &self,
        http: &reqwest::Client,
        zone: &Zone,
        name: &str,
    ) -> Result<Vec<Found>, ApiError> {
        let subdomain = zone.relative(name);
        let ids: Vec<u64> = self
            .call(
                http,
                Method::GET,
                &format!("/domain/zone/{}/record?subDomain={subdomain}", zone.id),
                None,
            )
            .await?;
        let mut found = Vec::new();
        for id in ids {
            let record: OvhRecord = self
                .call(
                    http,
                    Method::GET,
                    &format!("/domain/zone/{}/record/{id}", zone.id),
                    None,
                )
                .await?;
            // An empty `subDomain` filter may match every record of the zone.
            if record.sub_domain != subdomain {
                continue;
            }
            if let Some(parsed) = Record::parse(&record.field_type, &record.target, false)? {
                found.push(Found {
                    id: RecordId(record.id.to_string()),
                    record: parsed,
                });
            }
        }
        Ok(found)
    }

    /// Creates a record, or replaces record `id` of the same type, published
    /// by [`Self::refresh`]. CNAME targets get a trailing dot, so OVH does not
    /// read them as relative to the zone. OVH cannot proxy records.
    pub(super) async fn upsert(
        &self,
        http: &reqwest::Client,
        zone: &Zone,
        name: &str,
        id: Option<&RecordId>,
        record: &Record,
    ) -> Result<RecordId, ApiError> {
        let target = match record {
            Record::Cname { proxied: true, .. } => {
                return Err(ApiError::Unsupported(
                    "OVH cannot proxy records; a proxied CNAME needs a Cloudflare zone",
                ));
            }
            Record::Cname { target, .. } => format!("{target}."),
            record => record.content(),
        };
        let subdomain = zone.relative(name);
        let records = format!("/domain/zone/{}/record", zone.id);
        if let Some(id) = id {
            self.call::<Value>(
                http,
                Method::PUT,
                &format!("{records}/{}", id.0),
                Some(&json!({"subDomain":subdomain,"target":target,"ttl":record.ttl()})),
            )
            .await?;
            return Ok(id.clone());
        }
        let created: OvhRecord = self
            .call(
                http,
                Method::POST,
                &records,
                Some(&json!({"fieldType":record.kind(),"subDomain":subdomain,"target":target,"ttl":record.ttl()})),
            )
            .await?;
        Ok(RecordId(created.id.to_string()))
    }

    /// Deletes a record by its ID, published by [`Self::refresh`].
    pub(super) async fn delete_record(
        &self,
        http: &reqwest::Client,
        zone: &Zone,
        id: &RecordId,
    ) -> Result<(), ApiError> {
        self.call::<Value>(
            http,
            Method::DELETE,
            &format!("/domain/zone/{}/record/{}", zone.id, id.0),
            None,
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dns::{DnsProvider, tests::Recorded};

    fn ovh(api: String) -> Ovh {
        Ovh {
            endpoint: OvhEndpoint::OvhEu,
            application_key: "ak".into(),
            application_secret: "as".into(),
            consumer_key: "ck".into(),
            api,
        }
    }

    #[test]
    fn signature_covers_method_url_body_and_time() {
        // Computed independently: printf 'as+ck+GET+https://eu.api.ovh.com/1.0/domain/zone++1700000000' | sha1sum
        assert_eq!(
            ovh(OvhEndpoint::OvhEu.url().into()).signature(
                &Method::GET,
                "https://eu.api.ovh.com/1.0/domain/zone",
                "",
                1_700_000_000
            ),
            "$1$a83d7f6455abba1184731d8daa59ad88cc891a0a"
        );
    }

    #[tokio::test]
    async fn records_are_listed_by_name_and_cnames_are_absolute() {
        let recorded = Recorded::serve(vec![
            (Method::GET, "/auth/time", 200, "1700000000"),
            (Method::GET, "/domain/zone/piquel.fr/record?subDomain=www", 200, "[1,2,3]"),
            (Method::GET, "/domain/zone/piquel.fr/record/1", 200,
                r#"{"id":1,"zone":"piquel.fr","fieldType":"CNAME","subDomain":"www","target":"web.example.com.","ttl":300}"#),
            (Method::GET, "/domain/zone/piquel.fr/record/2", 200,
                r#"{"id":2,"zone":"piquel.fr","fieldType":"TXT","subDomain":"www","target":"\"owner\"","ttl":60}"#),
            // Another name, as an empty filter returns for the apex.
            (Method::GET, "/domain/zone/piquel.fr/record/3", 200,
                r#"{"id":3,"zone":"piquel.fr","fieldType":"A","subDomain":"api","target":"192.0.2.1","ttl":300}"#),
            (Method::PUT, "/domain/zone/piquel.fr/record/1", 200, "null"),
            (Method::POST, "/domain/zone/piquel.fr/record", 200,
                r#"{"id":4,"zone":"piquel.fr","fieldType":"A","subDomain":"www","target":"192.0.2.9","ttl":300}"#),
        ]).await;
        let provider = DnsProvider::Ovh(ovh(recorded.url.clone()));
        let http = reqwest::Client::new();
        let zone = Zone {
            name: Hostname::parse("piquel.fr").unwrap(),
            id: "piquel.fr".into(),
        };
        let found = provider
            .records(&http, &zone, "www.piquel.fr")
            .await
            .unwrap();
        let target = Hostname::parse("web2.example.com").unwrap();
        assert_eq!(
            found.iter().map(|f| f.record.clone()).collect::<Vec<_>>(),
            [
                Record::Cname {
                    target: Hostname::parse("web.example.com").unwrap(),
                    proxied: false
                },
                Record::Txt("owner".into()),
            ]
        );
        let cname = Record::Cname {
            target: target.clone(),
            proxied: false,
        };
        provider
            .upsert(&http, &zone, "www.piquel.fr", Some(&found[0].id), &cname)
            .await
            .unwrap();
        let id = provider
            .upsert(
                &http,
                &zone,
                "www.piquel.fr",
                None,
                &Record::A("192.0.2.9".parse().unwrap()),
            )
            .await
            .unwrap();
        assert_eq!(id, RecordId("4".into()));
        // OVH cannot proxy, so nothing is sent.
        let error = provider
            .upsert(
                &http,
                &zone,
                "www.piquel.fr",
                None,
                &Record::Cname {
                    target,
                    proxied: true,
                },
            )
            .await
            .unwrap_err();
        assert!(format!("{:#}", anyhow::Error::from(error)).contains("OVH cannot proxy"));
        let signed: Vec<_> = recorded
            .requests()
            .await
            .into_iter()
            .filter(|r| r.path != "/auth/time")
            .collect();
        assert_eq!(signed.len(), 6);
        assert_eq!(
            signed[4].body,
            json!({"subDomain":"www","target":"web2.example.com.","ttl":300})
        );
        assert_eq!(
            signed[5].body,
            json!({"fieldType":"A","subDomain":"www","target":"192.0.2.9","ttl":300})
        );
    }

    #[tokio::test]
    async fn records_are_relative_to_their_zone_and_published_by_refresh() {
        let recorded = Recorded::serve(vec![
            (Method::GET, "/auth/time", 200, "1700000000"),
            (Method::GET, "/domain/zone", 200, r#"["piquel.fr","bad_zone"]"#),
            (Method::POST, "/domain/zone/piquel.fr/record", 200,
                r#"{"id":5170142,"zone":"piquel.fr","fieldType":"TXT","subDomain":"_acme-challenge.staging","target":"v","ttl":60}"#),
            (Method::POST, "/domain/zone/piquel.fr/refresh", 200, ""),
            (Method::DELETE, "/domain/zone/piquel.fr/record/5170142", 200, "null"),
            (Method::DELETE, "/domain/zone/piquel.fr/record/1", 404,
                r#"{"message":"The requested object (id = 1) does not exist"}"#),
        ]).await;
        let provider = DnsProvider::Ovh(ovh(recorded.url.clone()));
        let http = reqwest::Client::new();
        let zones = provider.zones(&http).await.unwrap();
        assert_eq!(zones.len(), 1);
        let id = provider
            .upsert(
                &http,
                &zones[0],
                "_acme-challenge.staging.piquel.fr",
                None,
                &Record::Txt("v".into()),
            )
            .await
            .unwrap();
        assert_eq!(id, RecordId("5170142".into()));
        provider.publish(&http, &zones[0]).await.unwrap();
        provider.delete_record(&http, &zones[0], &id).await.unwrap();
        provider.publish(&http, &zones[0]).await.unwrap();
        // A record that is already gone counts as deleted.
        provider
            .delete_record(&http, &zones[0], &RecordId("1".into()))
            .await
            .unwrap();
        let requests = recorded.requests().await;
        let signed: Vec<_> = requests.iter().filter(|r| r.path != "/auth/time").collect();
        assert_eq!(
            signed.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            [
                "/domain/zone",
                "/domain/zone/piquel.fr/record",
                "/domain/zone/piquel.fr/refresh",
                "/domain/zone/piquel.fr/record/5170142",
                "/domain/zone/piquel.fr/refresh",
                "/domain/zone/piquel.fr/record/1",
            ]
        );
        for request in &signed {
            assert_eq!(request.header("x-ovh-application"), Some("ak"));
            assert_eq!(request.header("x-ovh-consumer"), Some("ck"));
            assert_eq!(request.header("x-ovh-timestamp"), Some("1700000000"));
            let url = format!("{}{}", recorded.url, request.path);
            let body = if request.body.is_null() {
                String::new()
            } else {
                request.body.to_string()
            };
            let expected =
                ovh(recorded.url.clone()).signature(&request.method, &url, &body, 1_700_000_000);
            assert_eq!(request.header("x-ovh-signature"), Some(expected.as_str()));
        }
        assert_eq!(
            signed[1].body,
            json!({"fieldType":"TXT","subDomain":"_acme-challenge.staging","target":"v","ttl":60})
        );
    }
}
