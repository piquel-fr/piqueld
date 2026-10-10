//! Test-only provider writing TXT records into Pebble's `challtestsrv`, which
//! answers DNS for the test CA. Record IDs are names: `/clear-txt` removes
//! every value of a name.
use super::{ApiError, Found, Provider, Record, RecordId, Zone, send};
use piqueld_core::manifest::Hostname;
use serde::Deserialize;
use serde_json::json;

/// A `challtestsrv` management API serving fixed zones.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct Challtestsrv {
    /// Management API base URL, such as `http://127.0.0.1:8055`.
    pub(crate) management: String,
    /// Zones it answers for.
    pub(crate) zones: Vec<Hostname>,
}

impl Challtestsrv {
    /// Calls a management endpoint, which answers with an empty body.
    async fn manage(
        &self,
        http: &reqwest::Client,
        endpoint: &str,
        body: &serde_json::Value,
    ) -> Result<(), ApiError> {
        send::<serde_json::Value>(
            http.post(format!("{}/{endpoint}", self.management))
                .json(body),
        )
        .await?;
        Ok(())
    }

    /// Makes DNS queries for `name` fail with SERVFAIL, or answer again.
    pub(crate) async fn servfail(&self, http: &reqwest::Client, name: &str, enabled: bool) {
        let endpoint = if enabled {
            "set-servfail"
        } else {
            "clear-servfail"
        };
        self.manage(http, endpoint, &json!({"host":format!("{name}.")}))
            .await
            .unwrap();
    }
}

/// Only TXT records can be created or deleted, by name.
impl Provider for Challtestsrv {
    fn kind(&self) -> &'static str {
        "challtestsrv"
    }

    fn zones(&self, _http: &reqwest::Client) -> impl Future<Output = Result<Vec<Zone>, ApiError>> {
        std::future::ready(Ok(self
            .zones
            .iter()
            .map(|name| Zone {
                name: name.clone(),
                id: name.to_string(),
            })
            .collect()))
    }

    fn records(
        &self,
        _http: &reqwest::Client,
        _zone: &Zone,
        _name: &str,
    ) -> impl Future<Output = Result<Vec<Found>, ApiError>> {
        std::future::ready(Err(ApiError::Unsupported(
            "challtestsrv cannot list records",
        )))
    }

    async fn upsert(
        &self,
        http: &reqwest::Client,
        _zone: &Zone,
        name: &str,
        id: Option<&RecordId>,
        record: &Record,
    ) -> Result<RecordId, ApiError> {
        let (None, Record::Txt(value)) = (id, record) else {
            return Err(ApiError::Unsupported(
                "challtestsrv only creates TXT records",
            ));
        };
        self.manage(
            http,
            "set-txt",
            &json!({"host":format!("{name}."),"value":value}),
        )
        .await?;
        Ok(RecordId(name.into()))
    }

    async fn delete_record(
        &self,
        http: &reqwest::Client,
        _zone: &Zone,
        id: &RecordId,
    ) -> Result<(), ApiError> {
        self.manage(http, "clear-txt", &json!({"host":format!("{}.", id.0)}))
            .await
    }
}
