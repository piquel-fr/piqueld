//! Certificates piqueld obtains itself through ACME DNS-01 and hands to Caddy,
//! for hostnames a public CA cannot reach with HTTP-01 or TLS-ALPN-01. DNS
//! credentials stay in the daemon, so a compromised gateway cannot take over
//! a domain.
//!
//! Each certificate covers one name: the wildcard of a hostname's parent when
//! that parent lies inside the provider's zone, otherwise the exact hostname.
//!
//! ```text
//! admin.piquel.fr, staging.piquel.fr         -> *.piquel.fr
//! auth.staging.piquel.fr                     -> *.staging.piquel.fr
//! piquel.fr (parent `fr` is outside the zone) -> piquel.fr
//! ```
//!
//! Issuance and renewal are daemon-scoped journal actions with
//! `ingress_certificate_*` phases. Files live under `<data_dir>/ingress`:
//!
//! ```text
//! acme/account-<directory hash>.json        ACME account key (0600)
//! certificates/<name>.pem                   private key, then chain (0600)
//! ```
//!
//! where `<name>` is the hostname, or `_wildcard.<parent>` for wildcards. One
//! file holds both, so a renewal replaces the pair atomically.

use super::{Ingress, wire::Journaled};
use crate::{
    config::AcmeConfig,
    dns::{
        Dns, ZoneError,
        provider::{Record, RecordId, Zone},
    },
};
use anyhow::{Context, Result, bail};
use instant_acme::{
    Account, AuthorizationStatus, ChallengeType, Identifier, NewAccount, NewOrder, OrderStatus,
    RetryPolicy,
};
use piqueld_core::{
    api::{CertificateStatus, DnsStatus},
    manifest::Hostname,
    observability::DiagnosticCode,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

/// A failing certificate this close to expiry raises a daemon condition.
const EXPIRY_WARNING_MS: i64 = 14 * 24 * 3600 * 1000;
/// First retry delay after a failure; it doubles up to [`MAX_BACKOFF_MS`].
const FIRST_BACKOFF_MS: i64 = 5 * 60 * 1000;
/// Longest delay between retries.
const MAX_BACKOFF_MS: i64 = 6 * 3600 * 1000;
/// Error of an order interrupted by daemon shutdown.
const SHUTDOWN: &str = "cancelled by daemon shutdown";
/// Longest wait for one ACME step, polling included: instant-acme's HTTP
/// client has no deadline of its own.
const ACME_TIMEOUT: Duration = Duration::from_mins(2);

/// The single name a certificate covers.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum CertificateName {
    /// `*.<parent>`, covering every direct child of the parent.
    Wildcard(Hostname),
    /// One exact hostname.
    Exact(Hostname),
}

impl CertificateName {
    /// The name covering `hostname`, which belongs to `zone`.
    pub(crate) fn covering(hostname: &Hostname, zone: &Hostname) -> Self {
        match hostname.parent() {
            Some(parent) if parent.is_within(zone) => Self::Wildcard(parent),
            _ => Self::Exact(hostname.clone()),
        }
    }

    /// The domain whose `_acme-challenge` record proves control.
    fn domain(&self) -> &Hostname {
        match self {
            Self::Wildcard(domain) | Self::Exact(domain) => domain,
        }
    }

    /// The DNS-01 challenge record name.
    fn challenge(&self) -> String {
        format!("_acme-challenge.{}", self.domain())
    }

    /// Storage file name. Hostnames cannot contain `_`, so the wildcard
    /// prefix never collides with an exact name.
    fn file(&self) -> String {
        match self {
            Self::Wildcard(parent) => format!("_wildcard.{parent}.pem"),
            Self::Exact(hostname) => format!("{hostname}.pem"),
        }
    }

    /// Parses a storage file name.
    fn from_file(file: &str) -> Option<Self> {
        let name = file.strip_suffix(".pem")?;
        match name.strip_prefix("_wildcard.") {
            Some(parent) => Hostname::parse(parent).ok().map(Self::Wildcard),
            None => Hostname::parse(name).ok().map(Self::Exact),
        }
    }
}

/// `*.example.com` or `example.com`.
impl std::fmt::Display for CertificateName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wildcard(parent) => write!(f, "*.{parent}"),
            Self::Exact(hostname) => hostname.fmt(f),
        }
    }
}

/// A stored certificate chain and its private key.
#[derive(Clone)]
struct Stored {
    /// PEM chain, leaf first.
    chain: String,
    /// PEM private key.
    key: String,
    not_before_ms: i64,
    not_after_ms: i64,
}

impl Stored {
    /// Ends the private key in a stored file; the chain follows it.
    const KEY_END: &str = "-----END PRIVATE KEY-----";

    /// The stored file: the PKCS#8 private key, then the chain.
    fn bundle(&self) -> String {
        format!("{}\n{}", self.key.trim_end(), self.chain)
    }

    /// Parses a file written by [`Self::bundle`].
    fn from_bundle(bundle: &str) -> Result<Self> {
        let (key, chain) = bundle
            .split_once(Self::KEY_END)
            .context("no PKCS#8 private key")?;
        Self::parse(
            chain.trim_start().to_owned(),
            format!("{key}{}\n", Self::KEY_END),
        )
    }

    /// Reads the leaf's validity period.
    fn parse(chain: String, key: String) -> Result<Self> {
        let (_, pem) = x509_parser::pem::parse_x509_pem(chain.as_bytes())
            .map_err(|error| anyhow::anyhow!("decode certificate PEM: {error}"))?;
        let leaf = pem
            .parse_x509()
            .map_err(|error| anyhow::anyhow!("decode certificate: {error}"))?;
        let validity = leaf.validity();
        Ok(Self {
            not_before_ms: validity.not_before.timestamp() * 1000,
            not_after_ms: validity.not_after.timestamp() * 1000,
            chain,
            key,
        })
    }

    /// Less than a third of the certificate's lifetime remains.
    fn due(&self, now_ms: i64) -> bool {
        (self.not_after_ms - now_ms) * 3 < self.not_after_ms - self.not_before_ms
    }
}

/// A certificate to issue or renew now, through `provider`'s `zone`.
struct Due {
    name: CertificateName,
    provider: usize,
    zone: Zone,
    /// A stored certificate is being replaced.
    renewal: bool,
}

/// A challenge TXT record piqueld created, through `provider`'s `zone`.
#[derive(Clone)]
pub(super) struct Challenge {
    pub(super) provider: usize,
    pub(super) zone: Zone,
    pub(super) id: RecordId,
}

/// The provider and zone owning a hostname, or why there is none.
type Owner = Result<(usize, Zone), ZoneError>;

/// What piqueld knows about one certificate name.
#[derive(Default)]
struct Entry {
    stored: Option<Stored>,
    /// Hostnames that currently need this certificate.
    hostnames: BTreeSet<Hostname>,
    /// Latest failure, cleared by a successful issuance.
    error: Option<String>,
    /// Why no provider zone owns the hostnames, recomputed every pass.
    unowned: Option<ZoneError>,
    /// Consecutive failures, which set the retry backoff.
    failures: u32,
    /// No attempt is made before this time.
    retry_at_ms: i64,
}

/// DNS providers, the ACME account and every known certificate.
pub(super) struct Certificates {
    pub(super) dns: Dns,
    acme: AcmeConfig,
    /// `<data_dir>/ingress`.
    directory: PathBuf,
    /// ACME account, loaded or registered on first use.
    account: tokio::sync::Mutex<Option<Account>>,
    entries: std::sync::RwLock<BTreeMap<CertificateName, Entry>>,
    /// Challenge records whose deletion failed, by certificate. The next
    /// attempt for a name deletes its leftover before creating another, so
    /// failed cleanups never pile up records: too many TXT values at one name
    /// make the CA reject the validation. Kept apart from `entries` so that
    /// retiring a certificate does not forget them.
    leftovers: std::sync::Mutex<BTreeMap<CertificateName, Challenge>>,
    /// How long a TXT record may take to reach every authoritative nameserver;
    /// 10 minutes, because OVH propagation is slow.
    pub(super) propagation_timeout: Duration,
    /// Root CA trusted for the ACME directory instead of the system roots.
    #[cfg(test)]
    pub(super) acme_root: Option<PathBuf>,
}

impl Certificates {
    /// Loads certificates stored under `directory/certificates`. Unreadable
    /// ones are logged and reissued when a route needs them.
    pub(super) fn new(dns: Dns, acme: AcmeConfig, directory: PathBuf) -> Self {
        let mut entries = BTreeMap::new();
        for entry in std::fs::read_dir(directory.join("certificates"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let Some(name) = entry
                .file_name()
                .to_str()
                .and_then(CertificateName::from_file)
            else {
                continue;
            };
            match std::fs::read_to_string(entry.path())
                .map_err(anyhow::Error::from)
                .and_then(|bundle| Stored::from_bundle(&bundle))
            {
                Ok(stored) => {
                    entries.insert(
                        name,
                        Entry {
                            stored: Some(stored),
                            ..Entry::default()
                        },
                    );
                }
                Err(error) => {
                    tracing::warn!(certificate=%name, error=format!("{error:#}"), "ignoring unreadable stored certificate");
                }
            }
        }
        Self {
            dns,
            acme,
            directory,
            account: tokio::sync::Mutex::new(None),
            entries: std::sync::RwLock::new(entries),
            leftovers: std::sync::Mutex::default(),
            propagation_timeout: Duration::from_mins(10),
            #[cfg(test)]
            acme_root: None,
        }
    }

    /// Certificate chains and keys for Caddy to load. Only certificates that
    /// a route needs are loaded.
    pub(super) fn loaded(&self) -> Vec<(String, String)> {
        let entries = self.entries.read().expect("certificate state lock");
        entries
            .values()
            .filter(|entry| !entry.hostnames.is_empty())
            .filter_map(|entry| entry.stored.as_ref())
            .map(|stored| (stored.chain.clone(), stored.key.clone()))
            .collect()
    }

    /// Why `hostname` has no certificate to serve: no provider zone owns it,
    /// or its issuance failed. `None` while one is stored or being issued.
    pub(super) fn problem(&self, hostname: &Hostname) -> Option<String> {
        let entries = self.entries.read().expect("certificate state lock");
        entries
            .values()
            .find(|entry| entry.hostnames.contains(hostname))
            .filter(|entry| entry.stored.is_none())
            .and_then(Entry::problem)
    }

    /// Refreshes provider zones when they are stale, then records which
    /// hostnames need which certificate name. Returns the certificates that
    /// are missing or due for renewal and past their failure backoff.
    ///
    /// A hostname no zone owns, for instance while discovery fails after a
    /// restart, keeps a stored certificate covering it and reports why.
    async fn due(&self, desired: &BTreeSet<Hostname>, now_ms: i64) -> Vec<Due> {
        self.dns.refresh().await;
        let mut wanted: BTreeMap<CertificateName, (BTreeSet<Hostname>, Owner)> = BTreeMap::new();
        for hostname in desired {
            let owner = self.dns.zone_for(hostname).await;
            let name = match &owner {
                Ok((_, zone)) => CertificateName::covering(hostname, &zone.name),
                Err(_) => self.stored_covering(hostname),
            };
            wanted
                .entry(name)
                .or_insert_with(|| (BTreeSet::new(), owner))
                .0
                .insert(hostname.clone());
        }
        let mut entries = self.entries.write().expect("certificate state lock");
        for entry in entries.values_mut() {
            entry.hostnames.clear();
            entry.unowned = None;
        }
        let mut due = Vec::new();
        for (name, (hostnames, owner)) in wanted {
            let entry = entries.entry(name.clone()).or_default();
            entry.hostnames = hostnames;
            match owner {
                Err(error) => entry.unowned = Some(error),
                Ok((provider, zone))
                    if entry
                        .stored
                        .as_ref()
                        .is_none_or(|stored| stored.due(now_ms))
                        && now_ms >= entry.retry_at_ms =>
                {
                    due.push(Due {
                        name,
                        provider,
                        zone,
                        renewal: entry.stored.is_some(),
                    });
                }
                Ok(_) => {}
            }
        }
        due
    }

    /// The stored certificate covering `hostname`: its parent's wildcard or
    /// its exact name. Without one, its exact name.
    fn stored_covering(&self, hostname: &Hostname) -> CertificateName {
        let entries = self.entries.read().expect("certificate state lock");
        hostname
            .parent()
            .map(CertificateName::Wildcard)
            .filter(|wildcard| entries.get(wildcard).is_some_and(|e| e.stored.is_some()))
            .unwrap_or_else(|| CertificateName::Exact(hostname.clone()))
    }

    /// Replaces the leftover challenge record of `name`, returning the old one.
    pub(super) fn replace_leftover(
        &self,
        name: &CertificateName,
        leftover: Option<Challenge>,
    ) -> Option<Challenge> {
        let mut leftovers = self.leftovers.lock().expect("leftover lock");
        match leftover {
            Some(leftover) => leftovers.insert(name.clone(), leftover),
            None => leftovers.remove(name),
        }
    }

    /// Where the certificate for `name` is stored.
    fn path(&self, name: &CertificateName) -> PathBuf {
        self.directory.join("certificates").join(name.file())
    }

    /// Per-certificate names, hostnames, expiry and last error.
    fn status(&self) -> Vec<CertificateStatus> {
        self.entries
            .read()
            .expect("certificate state lock")
            .iter()
            .map(|(name, entry)| CertificateStatus {
                name: name.to_string(),
                hostnames: entry.hostnames.iter().map(ToString::to_string).collect(),
                expires_at_ms: entry.stored.as_ref().map(|stored| stored.not_after_ms),
                error: entry.problem(),
            })
            .collect()
    }
}

impl Entry {
    /// What keeps this certificate from being issued: no owning zone, or the
    /// latest failure.
    fn problem(&self) -> Option<String> {
        self.unowned
            .as_ref()
            .map(ToString::to_string)
            .or_else(|| self.error.clone())
    }
}

impl Certificates {
    /// Awaits one ACME `step`, bounded by [`ACME_TIMEOUT`] and interrupted by
    /// `cancellation`. Every ACME call goes through here.
    async fn acme<T, E>(
        cancellation: &CancellationToken,
        step: &'static str,
        future: impl Future<Output = Result<T, E>>,
    ) -> Result<T>
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        cancellation
            .run_until_cancelled(tokio::time::timeout(ACME_TIMEOUT, future))
            .await
            .with_context(|| format!("{step}: {SHUTDOWN}"))?
            .with_context(|| format!("{step}: no answer within {}s", ACME_TIMEOUT.as_secs()))?
            .context(step)
    }
}

/// Writes a file readable only by the daemon, replacing it atomically.
pub(super) async fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let temporary = path.with_extension("tmp");
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)
        .await
        .with_context(|| format!("create {}", temporary.display()))?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    tokio::fs::rename(&temporary, path)
        .await
        .with_context(|| format!("replace {}", path.display()))
}

impl Ingress {
    /// DNS providers and certificates for system status.
    pub async fn dns_status(&self) -> DnsStatus {
        DnsStatus {
            providers: self.certificates.dns.status().await,
            certificates: self.certificates.status(),
        }
    }

    /// Checks every DNS provider's credentials and zones now instead of at
    /// the next hourly discovery, then reports the result.
    pub async fn refresh_dns(&self) -> DnsStatus {
        self.certificates.dns.discover().await;
        self.dns_status().await
    }

    /// One maintenance pass over the certificates `desired` hostnames need:
    ///
    /// 1. Issues missing certificates and renews due ones (see
    ///    [`Certificates::due`]), one at a time.
    /// 2. Deletes certificates no route needs once they have expired.
    /// 3. Raises `certificate_renewal_failed` while a needed certificate that
    ///    failed to renew expires within 14 days.
    ///
    /// `cancellation` stops zone discovery, which can wait behind forced
    /// discoveries, and interrupts the order in progress, which still deletes
    /// its TXT record; the pass stops after it.
    pub(super) async fn maintain_certificates(
        &self,
        desired: &BTreeSet<Hostname>,
        now_ms: i64,
        cancellation: &CancellationToken,
    ) {
        let certificates = &self.certificates;
        let started = std::time::Instant::now();
        // Planning changes nothing until its awaits finish, so dropping it is safe.
        let Some(due) = cancellation
            .run_until_cancelled(certificates.due(desired, now_ms))
            .await
        else {
            return;
        };
        for due in due {
            let phase = if due.renewal {
                "ingress_certificate_renew"
            } else {
                "ingress_certificate_issue"
            };
            let result = self.issue_certificate(phase, &due, cancellation).await;
            let mut entries = certificates
                .entries
                .write()
                .expect("certificate state lock");
            let entry = entries.entry(due.name.clone()).or_default();
            match result {
                Ok(stored) => {
                    tracing::info!(certificate=%due.name, phase, "obtained DNS-01 certificate");
                    *entry = Entry {
                        stored: Some(stored),
                        hostnames: std::mem::take(&mut entry.hostnames),
                        ..Entry::default()
                    };
                }
                Err(error) => {
                    tracing::warn!(certificate=%due.name, phase, error=format!("{error:#}"), "DNS-01 certificate failed; will retry with backoff");
                    entry.failures += 1;
                    // Backoff starts when the attempt failed, which can be
                    // minutes into the pass.
                    let failed_at_ms = now_ms.saturating_add(
                        i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
                    );
                    entry.retry_at_ms = failed_at_ms
                        + (FIRST_BACKOFF_MS << (entry.failures - 1).min(10)).min(MAX_BACKOFF_MS);
                    entry.error = Some(format!("{error:#}"));
                }
            }
            drop(entries);
            if cancellation.is_cancelled() {
                return;
            }
        }
        self.retire_certificates(now_ms).await;
        let expiring = certificates
            .entries
            .read()
            .expect("certificate state lock")
            .values()
            .any(|entry| {
                !entry.hostnames.is_empty()
                    && entry.problem().is_some()
                    && entry
                        .stored
                        .as_ref()
                        .is_some_and(|stored| stored.not_after_ms - now_ms < EXPIRY_WARNING_MS)
            });
        if let Err(error) = self
            .store
            .observe_condition(
                "certificate_renewal_failed",
                None,
                expiring,
                now_ms,
                180_000,
            )
            .await
        {
            tracing::error!(error=?error, "certificate expiry observation could not be recorded");
        }
    }

    /// Forgets certificates no route needs: unissued ones at once, stored ones
    /// once their expired file is deleted in a journal action. A failed
    /// deletion is retried on the next pass.
    async fn retire_certificates(&self, now_ms: i64) {
        let expired: Vec<_> = {
            let mut entries = self
                .certificates
                .entries
                .write()
                .expect("certificate state lock");
            entries.retain(|_, entry| !entry.hostnames.is_empty() || entry.stored.is_some());
            entries
                .iter()
                .filter(|(_, entry)| {
                    entry.hostnames.is_empty()
                        && entry
                            .stored
                            .as_ref()
                            .is_some_and(|stored| stored.not_after_ms <= now_ms)
                })
                .map(|(name, _)| name.clone())
                .collect()
        };
        for name in expired {
            let result = async {
                let journal = self
                    .journal_as(
                        DiagnosticCode::CertificateRenewalFailed,
                        "ingress_certificate_delete",
                        &name.to_string(),
                    )
                    .await?;
                let result = async {
                    journal.request().await?;
                    match tokio::fs::remove_file(self.certificates.path(&name)).await {
                        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                            Err(error).context("delete expired certificate")
                        }
                        _ => Ok(()),
                    }
                }
                .await;
                journal.finish(result).await
            }
            .await;
            match result {
                Ok(()) => {
                    self.certificates
                        .entries
                        .write()
                        .expect("certificate state lock")
                        .remove(&name);
                }
                Err(error) => {
                    tracing::warn!(certificate=%name, error=format!("{error:#}"), "expired certificate could not be deleted; will retry");
                }
            }
        }
    }

    /// Orders and stores one certificate in its own journal action.
    async fn issue_certificate(
        &self,
        phase: &str,
        due: &Due,
        cancellation: &CancellationToken,
    ) -> Result<Stored> {
        let name = &due.name;
        let journal = self
            .journal_as(
                DiagnosticCode::CertificateRenewalFailed,
                phase,
                &name.to_string(),
            )
            .await?;
        let result = async {
            let stored = self.order_certificate(&journal, due, cancellation).await?;
            crate::prepare_data_dir(&self.certificates.directory.join("certificates")).await?;
            write_private(&self.certificates.path(name), stored.bundle().as_bytes()).await?;
            Ok(stored)
        }
        .await;
        journal.finish(result).await
    }

    /// Runs the ACME order, deleting its TXT record afterwards whether the
    /// order succeeded, failed or was interrupted by shutdown. A record whose
    /// deletion fails is kept as the name's leftover, which the next attempt
    /// deletes before ordering.
    async fn order_certificate(
        &self,
        journal: &Journaled<'_>,
        due: &Due,
        cancellation: &CancellationToken,
    ) -> Result<Stored> {
        let certificates = &self.certificates;
        if let Some(leftover) = certificates.replace_leftover(&due.name, None)
            && let Err(error) = self.delete_challenge(journal, &leftover).await
        {
            certificates.replace_leftover(&due.name, Some(leftover));
            return Err(error.context("delete the challenge record left by an earlier attempt"));
        }
        let mut record = None;
        let result = self
            .complete_order(journal, due, cancellation, &mut record)
            .await;
        if let Some(id) = record {
            let challenge = Challenge {
                provider: due.provider,
                zone: due.zone.clone(),
                id,
            };
            if let Err(error) = self.delete_challenge(journal, &challenge).await {
                tracing::warn!(certificate=%due.name, error=format!("{error:#}"), "ACME challenge record could not be deleted; the next attempt retries");
                certificates.replace_leftover(&due.name, Some(challenge));
            }
        }
        result
    }

    /// Deletes and unpublishes a challenge record in the current journal
    /// action. Retrying after a failed publish converges, since a record that
    /// is already gone counts as deleted.
    async fn delete_challenge(&self, journal: &Journaled<'_>, challenge: &Challenge) -> Result<()> {
        journal.request().await?;
        let dns = &self.certificates.dns;
        let provider = dns.provider(challenge.provider);
        provider
            .delete_record(dns.http(), &challenge.zone, &challenge.id)
            .await?;
        provider.publish(dns.http(), &challenge.zone).await?;
        Ok(())
    }

    /// Orders the certificate, answers its DNS-01 challenge, then finalizes the
    /// order and downloads the chain. `record` receives the TXT record as soon
    /// as it exists.
    ///
    /// ACME steps and the propagation wait stop on `cancellation`. DNS provider
    /// calls are bounded by their HTTP timeout and always complete, so a
    /// created record always reaches `record`.
    async fn complete_order(
        &self,
        journal: &Journaled<'_>,
        Due {
            name,
            provider,
            zone,
            ..
        }: &Due,
        cancellation: &CancellationToken,
        record: &mut Option<RecordId>,
    ) -> Result<Stored> {
        let account = self.acme_account(journal, cancellation).await?;
        let identifiers = [Identifier::Dns(name.to_string())];
        journal.request().await?;
        let mut order = Certificates::acme(
            cancellation,
            "create ACME order",
            account.new_order(&NewOrder::new(&identifiers)),
        )
        .await?;
        let dns = &self.certificates.dns;
        {
            let mut authorizations = order.authorizations();
            let mut authorization =
                Certificates::acme(cancellation, "read ACME authorization", async {
                    authorizations.next().await.transpose()
                })
                .await?
                .context("ACME order has no authorization")?;
            match authorization.status {
                // A recent validation of this name is reused.
                AuthorizationStatus::Valid => {}
                AuthorizationStatus::Pending => {
                    let mut challenge = authorization
                        .challenge(ChallengeType::Dns01)
                        .context("the CA offered no DNS-01 challenge")?;
                    let value = challenge.key_authorization().dns_value();
                    let challenge_name = name.challenge();
                    journal.request().await?;
                    let provider = dns.provider(*provider);
                    *record = Some(
                        provider
                            .upsert(
                                dns.http(),
                                zone,
                                &challenge_name,
                                None,
                                &Record::Txt(value.clone()),
                            )
                            .await?,
                    );
                    provider.publish(dns.http(), zone).await?;
                    cancellation
                        .run_until_cancelled(dns.wait_visible(
                            &zone.name,
                            &challenge_name,
                            &value,
                            self.certificates.propagation_timeout,
                        ))
                        .await
                        .context(SHUTDOWN)??;
                    journal.request().await?;
                    Certificates::acme(
                        cancellation,
                        "answer DNS-01 challenge",
                        challenge.set_ready(),
                    )
                    .await?;
                }
                status => bail!("ACME authorization is {status:?}"),
            }
        }
        let retries = RetryPolicy::new().timeout(ACME_TIMEOUT);
        let status = Certificates::acme(
            cancellation,
            "wait for ACME validation",
            order.poll_ready(&retries),
        )
        .await?;
        if status != OrderStatus::Ready {
            match &order.state().error {
                Some(problem) => bail!("ACME order is {status:?}: {problem}"),
                None => bail!("ACME order is {status:?}"),
            }
        }
        journal.request().await?;
        let key = Certificates::acme(cancellation, "finalize ACME order", order.finalize()).await?;
        let chain = Certificates::acme(
            cancellation,
            "download ACME certificate",
            order.poll_certificate(&retries),
        )
        .await?;
        Stored::parse(chain, key)
    }

    /// The ACME account for the configured directory, registered on first use.
    /// Each directory has its own account file, so changing CAs registers anew.
    async fn acme_account(
        &self,
        journal: &Journaled<'_>,
        cancellation: &CancellationToken,
    ) -> Result<Account> {
        use sha2::{Digest, Sha256};
        let certificates = &self.certificates;
        let mut cached = certificates.account.lock().await;
        if let Some(account) = &*cached {
            return Ok(account.clone());
        }
        let directory = &certificates.acme.directory;
        let hash = format!("{:x}", Sha256::digest(directory.as_bytes()));
        let path = certificates
            .directory
            .join("acme")
            .join(format!("account-{}.json", &hash[..16]));
        // Tests trust the test CA's root instead of the system roots.
        #[cfg(test)]
        let builder = match &certificates.acme_root {
            Some(root) => Account::builder_with_root(root)?,
            None => Account::builder()?,
        };
        #[cfg(not(test))]
        let builder = Account::builder()?;
        let account = if tokio::fs::try_exists(&path).await? {
            let credentials = serde_json::from_slice(&tokio::fs::read(&path).await?)
                .context("decode stored ACME account")?;
            Certificates::acme(
                cancellation,
                "load ACME account",
                builder.from_credentials(credentials),
            )
            .await?
        } else {
            let contact = certificates
                .acme
                .email
                .as_ref()
                .map(|email| format!("mailto:{email}"));
            journal.request().await?;
            let contact: Vec<_> = contact.as_deref().into_iter().collect();
            let (account, credentials) = Certificates::acme(
                cancellation,
                "register ACME account",
                builder.create(
                    &NewAccount {
                        contact: &contact,
                        terms_of_service_agreed: true,
                        only_return_existing: false,
                    },
                    directory.clone(),
                    None,
                ),
            )
            .await?;
            crate::prepare_data_dir(&certificates.directory.join("acme")).await?;
            write_private(&path, &serde_json::to_vec(&credentials)?).await?;
            account
        };
        *cached = Some(account.clone());
        Ok(account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str) -> Hostname {
        Hostname::parse(name).unwrap()
    }

    #[test]
    fn hostnames_share_their_parent_wildcard_inside_the_zone() {
        for (hostname, zone, name) in [
            ("admin.piquel.fr", "piquel.fr", "*.piquel.fr"),
            ("staging.piquel.fr", "piquel.fr", "*.piquel.fr"),
            ("auth.staging.piquel.fr", "piquel.fr", "*.staging.piquel.fr"),
            ("pr-12.dev.piquel.fr", "piquel.fr", "*.dev.piquel.fr"),
            ("piquel.fr", "piquel.fr", "piquel.fr"),
            // A delegated child zone cannot answer for its parent's wildcard.
            ("dev.piquel.fr", "dev.piquel.fr", "dev.piquel.fr"),
            ("a.dev.piquel.fr", "dev.piquel.fr", "*.dev.piquel.fr"),
        ] {
            let covering = CertificateName::covering(&host(hostname), &host(zone));
            assert_eq!(covering.to_string(), name, "{hostname} in {zone}");
            assert_eq!(
                CertificateName::from_file(&covering.file()),
                Some(covering.clone())
            );
        }
        assert_eq!(
            CertificateName::Wildcard(host("piquel.fr")).challenge(),
            "_acme-challenge.piquel.fr"
        );
    }

    #[test]
    fn renewal_is_due_with_a_third_of_the_lifetime_left() {
        let day = 24 * 3600 * 1000;
        let stored = Stored {
            chain: String::new(),
            key: String::new(),
            not_before_ms: 0,
            not_after_ms: 90 * day,
        };
        assert!(!stored.due(59 * day));
        assert!(stored.due(61 * day));
    }

    #[tokio::test]
    async fn a_stored_wildcard_keeps_serving_while_no_zone_is_known() {
        // As after a restart whose zone discovery failed: no provider zones.
        let directory = tempfile::tempdir().unwrap();
        let certificates = Certificates::new(
            Dns::new(Vec::new()).unwrap(),
            AcmeConfig::default(),
            directory.path().into(),
        );
        let stored = Stored {
            chain: "chain".into(),
            key: "key".into(),
            not_before_ms: 0,
            not_after_ms: i64::MAX,
        };
        certificates.entries.write().unwrap().insert(
            CertificateName::Wildcard(host("piquel.fr")),
            Entry {
                stored: Some(stored),
                ..Entry::default()
            },
        );
        let desired = [host("admin.piquel.fr"), host("other.fr")].into();
        assert!(certificates.due(&desired, 0).await.is_empty());
        assert_eq!(
            certificates.loaded(),
            [("chain".to_owned(), "key".to_owned())]
        );
        // Only the hostname without a stored certificate fails its route.
        assert_eq!(certificates.problem(&host("admin.piquel.fr")), None);
        assert_eq!(
            certificates.problem(&host("other.fr")).as_deref(),
            Some("no configured DNS provider zone contains other.fr")
        );
        let problems: Vec<_> = certificates
            .status()
            .into_iter()
            .map(|status| (status.name, status.error.unwrap()))
            .collect();
        assert_eq!(
            problems,
            [
                (
                    "*.piquel.fr".to_owned(),
                    "no configured DNS provider zone contains admin.piquel.fr".to_owned()
                ),
                (
                    "other.fr".to_owned(),
                    "no configured DNS provider zone contains other.fr".to_owned()
                ),
            ]
        );
    }
}
