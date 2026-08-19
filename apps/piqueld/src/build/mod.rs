//! Reproducible Git/Dockerfile build pipeline.
#![allow(missing_docs)]
#![allow(deprecated)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::result_large_err
)]

use crate::config::CredentialReference;
use crate::registry::RegistryClient;
use crate::store::{BuildArtifactRepository, SqliteStore};
use async_trait::async_trait;
use bollard::{
    Docker, body_full,
    image::{BuildImageOptions, BuilderVersion, PushImageOptions},
};
use bytes::Bytes;
use futures_util::StreamExt;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::os::unix::fs::MetadataExt;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use url::Url;
use walkdir::WalkDir;

pub const DEFAULT_MAX_CONTEXT_BYTES: u64 = 256 * 1024 * 1024;
pub const DEFAULT_MAX_LOG_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("Git repository URL is unsupported")]
    RepositoryUrl,
    #[error("Git SSH transport is unsupported")]
    SshUnsupported,
    #[error("Git source could not be resolved")]
    Git,
    #[error("build context is unsafe")]
    UnsafeContext,
    #[error("build context exceeds the configured size limit")]
    ContextTooLarge,
    #[error("build timed out")]
    Timeout,
    #[error("build was cancelled")]
    Cancelled,
    #[error("BuildKit request failed")]
    BuildKit,
    #[error("registry request failed")]
    Registry,
    #[error("registry returned an invalid or unverified digest")]
    Digest,
    #[error("build secrets are not supported by this prototype")]
    BuildSecretsUnsupported,
    #[error("build storage failed")]
    Io,
}

/// Inputs whose exact canonical representation identifies a reusable build.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BuildIdentity {
    pub commit: String,
    pub context: String,
    pub dockerfile: String,
    pub build_args: BTreeMap<String, String>,
    pub target: Option<String>,
    pub platform: Option<String>,
    pub builder: String,
}

impl BuildIdentity {
    #[must_use]
    pub fn key(&self) -> String {
        let canonical = serde_json::to_vec(self).expect("build identity is serializable");
        format!("sha256:{:x}", Sha256::digest(canonical))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedGit {
    pub commit: String,
    pub checkout: PathBuf,
}

/// Credential material is obtained just-in-time and never stored in source URLs.
pub trait GitCredentialProvider: Send + Sync {
    fn token(&self, repository: &Url) -> Result<Option<zeroize::Zeroizing<String>>, BuildError>;
}

#[derive(Default)]
pub struct NoGitCredentials;
impl GitCredentialProvider for NoGitCredentials {
    fn token(&self, _: &Url) -> Result<Option<zeroize::Zeroizing<String>>, BuildError> {
        Ok(None)
    }
}

/// Protected file/systemd credential provider. The value is read only for a
/// resolution attempt and returned in a zeroizing allocation.
pub struct ProtectedGitCredentials {
    path: Option<PathBuf>,
}
impl ProtectedGitCredentials {
    pub fn from_reference(reference: Option<&CredentialReference>) -> Result<Self, BuildError> {
        let path = match reference {
            None => None,
            Some(CredentialReference::File { path }) => Some(path.clone()),
            Some(CredentialReference::SystemdCredential { name }) => {
                let root = std::env::var_os("CREDENTIALS_DIRECTORY")
                    .map(PathBuf::from)
                    .ok_or(BuildError::Git)?;
                Some(root.join(name))
            }
        };
        Ok(Self { path })
    }
}
impl GitCredentialProvider for ProtectedGitCredentials {
    fn token(&self, _: &Url) -> Result<Option<zeroize::Zeroizing<String>>, BuildError> {
        let Some(path) = &self.path else {
            return Ok(None);
        };
        if path.starts_with("/nix/store") {
            return Err(BuildError::Git);
        }
        let descriptor = rustix::fs::openat2(
            rustix::fs::CWD,
            path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
            rustix::fs::ResolveFlags::NO_SYMLINKS | rustix::fs::ResolveFlags::NO_MAGICLINKS,
        )
        .map_err(|_| BuildError::Git)?;
        let mut file = File::from(descriptor);
        let metadata = file.metadata().map_err(|_| BuildError::Git)?;
        if !metadata.is_file()
            || metadata.mode() & 0o077 != 0
            || metadata.uid() != rustix::process::geteuid().as_raw()
        {
            return Err(BuildError::Git);
        }
        let canonical = fs::canonicalize(path).map_err(|_| BuildError::Git)?;
        if canonical.starts_with("/nix/store") {
            return Err(BuildError::Git);
        }
        let mut bytes = zeroize::Zeroizing::new(Vec::with_capacity(4097));
        file.by_ref()
            .take(4097)
            .read_to_end(&mut bytes)
            .map_err(|_| BuildError::Git)?;
        while bytes.last().is_some_and(|b| matches!(b, b'\n' | b'\r')) {
            bytes.pop();
        }
        if bytes.is_empty() || bytes.len() > 4096 {
            return Err(BuildError::Git);
        }
        let token = String::from_utf8(bytes.to_vec()).map_err(|_| BuildError::Git)?;
        Ok(Some(zeroize::Zeroizing::new(token)))
    }
}

/// HTTPS resolver/checkouter backed by `gix`; each attempt receives a new empty directory.
pub struct GixGitSource<C = NoGitCredentials> {
    credentials: Arc<C>,
    checkout_root: PathBuf,
}

impl<C> GixGitSource<C> {
    #[must_use]
    pub fn new(checkout_root: PathBuf, credentials: Arc<C>) -> Self {
        Self {
            credentials,
            checkout_root,
        }
    }
}

impl<C: GitCredentialProvider + 'static> GixGitSource<C> {
    pub async fn resolve_checkout(
        &self,
        repository: &str,
        reference: &str,
        cancellation: CancellationToken,
        timeout: Duration,
    ) -> Result<ResolvedGit, BuildError> {
        let url = validated_repository_url(repository)?;
        let token = self.credentials.token(&url)?.map(Arc::new);
        let root = self.checkout_root.clone();
        let reference = reference.to_owned();
        let url = url.to_string();
        let interrupted = Arc::new(AtomicBool::new(false));
        let worker_interrupt = Arc::clone(&interrupted);
        let task = tokio::task::spawn_blocking(move || {
            fs::create_dir_all(&root).map_err(|_| BuildError::Io)?;
            let directory = tempfile::Builder::new()
                .prefix("checkout-")
                .tempdir_in(&root)
                .map_err(|_| BuildError::Io)?;
            let path = directory.keep();
            let mut prepare = gix::prepare_clone(url, &path).map_err(|_| BuildError::Git)?;
            prepare = prepare.configure_connection(move |connection| {
                if let Some(token) = token.clone() {
                    connection.set_credentials(move |action| match action {
                        gix::credentials::helper::Action::Get(context) => {
                            Ok(Some(gix::credentials::protocol::Outcome {
                                identity: gix::sec::identity::Account {
                                    username: "oauth2".into(),
                                    password: token.as_str().to_owned(),
                                    oauth_refresh_token: None,
                                },
                                next: context.into(),
                            }))
                        }
                        gix::credentials::helper::Action::Store(_)
                        | gix::credentials::helper::Action::Erase(_) => Ok(None),
                    });
                }
                Ok(())
            });
            let wanted = if reference.starts_with("refs/") {
                reference.clone()
            } else {
                format!("refs/heads/{reference}")
            };
            prepare = prepare
                .with_ref_name(Some(wanted.as_str()))
                .map_err(|_| BuildError::Git)?;
            let (mut checkout, _) = prepare
                .fetch_then_checkout(gix::progress::Discard, worker_interrupt.as_ref())
                .map_err(|_| BuildError::Git)?;
            let (repository, _) = checkout
                .main_worktree(gix::progress::Discard, worker_interrupt.as_ref())
                .map_err(|_| BuildError::Git)?;
            let commit = repository
                .head_commit()
                .map_err(|_| BuildError::Git)?
                .id()
                .to_string();
            Ok(ResolvedGit {
                commit,
                checkout: path,
            })
        });
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                interrupted.store(true, Ordering::Relaxed);
                Err(BuildError::Cancelled)
            }
            result = tokio::time::timeout(timeout, task) => match result {
                Err(_) => { interrupted.store(true, Ordering::Relaxed); Err(BuildError::Timeout) }
                Ok(Err(_)) => Err(BuildError::Git),
                Ok(Ok(result)) => result,
            }
        }
    }
}

#[must_use]
pub fn redact_url(value: &str) -> String {
    let Ok(mut url) = Url::parse(value) else {
        return "[REDACTED URL]".into();
    };
    if !url.username().is_empty() || url.password().is_some() {
        let _ = url.set_username("[REDACTED]");
        let _ = url.set_password(None);
    }
    for (key, _) in url.query_pairs() {
        if matches!(
            key.to_ascii_lowercase().as_str(),
            "token" | "access_token" | "auth" | "key"
        ) {
            url.set_query(Some("[REDACTED]"));
            break;
        }
    }
    url.to_string()
}

fn validated_repository_url(value: &str) -> Result<Url, BuildError> {
    let url = Url::parse(value).map_err(|_| BuildError::RepositoryUrl)?;
    if matches!(url.scheme(), "ssh" | "git+ssh") {
        return Err(BuildError::SshUnsupported);
    }
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
        || url.fragment().is_some()
    {
        return Err(BuildError::RepositoryUrl);
    }
    Ok(url)
}

/// A validated tar context held in memory for direct Docker API upload.
pub struct BuildContext {
    pub archive: Vec<u8>,
    pub dockerfile: String,
    pub content_hash: String,
}

impl BuildContext {
    pub fn create(
        checkout: &Path,
        context: &str,
        dockerfile: &str,
        max_bytes: u64,
    ) -> Result<Self, BuildError> {
        let context_rel = safe_relative(context)?;
        let dockerfile_rel = safe_relative(dockerfile)?;
        let root = checkout
            .canonicalize()
            .map_err(|_| BuildError::UnsafeContext)?;
        let context_root = root
            .join(context_rel)
            .canonicalize()
            .map_err(|_| BuildError::UnsafeContext)?;
        if !context_root.starts_with(&root) || !context_root.is_dir() {
            return Err(BuildError::UnsafeContext);
        }
        let dockerfile_path = root
            .join(&dockerfile_rel)
            .canonicalize()
            .map_err(|_| BuildError::UnsafeContext)?;
        if !dockerfile_path.starts_with(&context_root) || !dockerfile_path.is_file() {
            return Err(BuildError::UnsafeContext);
        }
        let dockerfile_name = dockerfile_path
            .strip_prefix(&context_root)
            .map_err(|_| BuildError::UnsafeContext)?;
        let ignores = load_dockerignore(&context_root)?;
        let mut entries = Vec::new();
        let mut total = 0_u64;
        for entry in WalkDir::new(&context_root)
            .follow_links(false)
            .sort_by_file_name()
        {
            let entry = entry.map_err(|_| BuildError::UnsafeContext)?;
            let path = entry.path();
            let relative = path
                .strip_prefix(&context_root)
                .map_err(|_| BuildError::UnsafeContext)?;
            if relative.as_os_str().is_empty() {
                continue;
            }
            let metadata = fs::symlink_metadata(path).map_err(|_| BuildError::Io)?;
            if metadata.file_type().is_symlink() {
                return Err(BuildError::UnsafeContext);
            }
            if ignores.is_match(relative) && relative != dockerfile_name {
                continue;
            }
            if metadata.is_file() {
                total = total
                    .checked_add(metadata.len())
                    .ok_or(BuildError::ContextTooLarge)?;
                if total > max_bytes {
                    return Err(BuildError::ContextTooLarge);
                }
            }
            entries.push((path.to_owned(), relative.to_owned(), metadata.is_dir()));
        }
        let mut archive = Vec::new();
        {
            let mut tar = tar::Builder::new(&mut archive);
            tar.mode(tar::HeaderMode::Deterministic);
            for (path, relative, directory) in entries {
                if directory {
                    tar.append_dir(relative, path).map_err(|_| BuildError::Io)?;
                } else {
                    let mut file = File::open(path).map_err(|_| BuildError::Io)?;
                    tar.append_file(relative, &mut file)
                        .map_err(|_| BuildError::Io)?;
                }
            }
            tar.finish().map_err(|_| BuildError::Io)?;
        }
        let content_hash = format!("sha256:{:x}", Sha256::digest(&archive));
        Ok(Self {
            archive,
            dockerfile: dockerfile_name.to_string_lossy().replace('\\', "/"),
            content_hash,
        })
    }
}

fn safe_relative(value: &str) -> Result<PathBuf, BuildError> {
    if value.is_empty() || value.contains('\\') || value.contains('\0') {
        return Err(BuildError::UnsafeContext);
    }
    let path = Path::new(value);
    if path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(BuildError::UnsafeContext);
    }
    Ok(path.to_owned())
}

fn load_dockerignore(root: &Path) -> Result<GlobSet, BuildError> {
    let mut builder = GlobSetBuilder::new();
    let path = root.join(".dockerignore");
    if let Ok(file) = File::open(path) {
        let mut text = String::new();
        file.take(1024 * 1024)
            .read_to_string(&mut text)
            .map_err(|_| BuildError::Io)?;
        for line in text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
        {
            // Negation requires ordered matching; reject it instead of silently building the wrong context.
            if line.starts_with('!') || line.contains("..") {
                return Err(BuildError::UnsafeContext);
            }
            let pattern = line.trim_start_matches('/');
            builder.add(Glob::new(pattern).map_err(|_| BuildError::UnsafeContext)?);
            builder
                .add(Glob::new(&format!("{pattern}/**")).map_err(|_| BuildError::UnsafeContext)?);
        }
    }
    builder.build().map_err(|_| BuildError::UnsafeContext)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawBuildLog {
    pub stream: String,
}

#[async_trait]
pub trait BuildKit: Send + Sync {
    async fn build_and_push(
        &self,
        context: BuildContext,
        tag: &str,
        platform: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<Vec<RawBuildLog>, BuildError>;
}

/// Direct Docker Engine API implementation. Setting builder version to `BuildKit`
/// avoids shell pipelines and delegates isolation to the daemon's `BuildKit` worker.
#[derive(Clone)]
pub struct BollardBuildKit {
    docker: Docker,
}
impl BollardBuildKit {
    #[must_use]
    pub fn new(docker: Docker) -> Self {
        Self { docker }
    }
}

#[async_trait]
#[allow(deprecated)]
impl BuildKit for BollardBuildKit {
    async fn build_and_push(
        &self,
        context: BuildContext,
        tag: &str,
        platform: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<Vec<RawBuildLog>, BuildError> {
        let options = BuildImageOptions::<String> {
            dockerfile: context.dockerfile,
            t: tag.to_owned(),
            pull: true,
            rm: true,
            forcerm: true,
            platform: platform.unwrap_or_default().to_owned(),
            // The Docker Engine BuildKit endpoint requires a session even when
            // this build has no credentials or auxiliary providers.
            session: Some(uuid::Uuid::now_v7().to_string()),
            version: BuilderVersion::BuilderBuildKit,
            ..Default::default()
        };
        let mut logs = Vec::new();
        let mut stream =
            self.docker
                .build_image(options, None, Some(body_full(Bytes::from(context.archive))));
        loop {
            let item = tokio::select! { biased; () = cancellation.cancelled() => return Err(BuildError::Cancelled), item = stream.next() => item };
            let Some(item) = item else { break };
            let item = item.map_err(|_| BuildError::BuildKit)?;
            if item.error.is_some() || item.error_detail.is_some() {
                return Err(BuildError::BuildKit);
            }
            for message in [item.stream, item.status, item.progress]
                .into_iter()
                .flatten()
            {
                logs.push(RawBuildLog { stream: message });
            }
        }
        let (repository, image_tag) = split_tag(tag)?;
        let mut push =
            self.docker
                .push_image(repository, Some(PushImageOptions { tag: image_tag }), None);
        loop {
            let item = tokio::select! { biased; () = cancellation.cancelled() => return Err(BuildError::Cancelled), item = push.next() => item };
            let Some(item) = item else { break };
            let item = item.map_err(|_| BuildError::Registry)?;
            if item.error.is_some() || item.error_detail.is_some() {
                return Err(BuildError::Registry);
            }
            for message in [item.status, item.progress].into_iter().flatten() {
                logs.push(RawBuildLog { stream: message });
            }
        }
        Ok(logs)
    }
}

fn split_tag(value: &str) -> Result<(&str, &str), BuildError> {
    let colon = value.rfind(':').ok_or(BuildError::Registry)?;
    if value[colon + 1..].contains('/')
        || value[..colon].is_empty()
        || value[colon + 1..].is_empty()
    {
        return Err(BuildError::Registry);
    }
    Ok((&value[..colon], &value[colon + 1..]))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BuildLogEntry {
    pub sequence: u64,
    pub timestamp_ms: i64,
    pub message: String,
}

/// Ordered bounded log collector which redacts all known sensitive strings.
pub struct BuildLogCollector {
    entries: Vec<BuildLogEntry>,
    sensitive: Vec<String>,
    retained: usize,
    max_bytes: usize,
    next_sequence: u64,
}

impl BuildLogCollector {
    #[must_use]
    pub fn new(max_bytes: usize, sensitive: impl IntoIterator<Item = String>) -> Self {
        Self {
            entries: Vec::new(),
            sensitive: sensitive.into_iter().filter(|v| !v.is_empty()).collect(),
            retained: 0,
            max_bytes,
            next_sequence: 1,
        }
    }
    pub fn push(&mut self, message: &str) {
        let mut message = message
            .split_whitespace()
            .map(|word| {
                if word.contains("://") {
                    redact_url(word)
                } else {
                    word.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        for value in &self.sensitive {
            message = message.replace(value, "[REDACTED]");
        }
        let available = self.max_bytes.saturating_sub(self.retained);
        if available == 0 {
            return;
        }
        if message.len() > available {
            let boundary = message
                .char_indices()
                .map(|(index, _)| index)
                .take_while(|index| *index <= available)
                .last()
                .unwrap_or(0);
            message.truncate(boundary);
        }
        self.retained += message.len();
        self.entries.push(BuildLogEntry {
            sequence: self.next_sequence,
            timestamp_ms: now_ms(),
            message,
        });
        self.next_sequence += 1;
    }
    #[must_use]
    pub fn entries(&self) -> &[BuildLogEntry] {
        &self.entries
    }
}

/// Bounded execution helper shared by build controllers and tests.
pub struct BuildExecutor<B> {
    backend: Arc<B>,
    permits: Arc<Semaphore>,
    timeout: Duration,
}
impl<B: BuildKit> BuildExecutor<B> {
    #[must_use]
    pub fn new(backend: Arc<B>, concurrency: usize, timeout: Duration) -> Self {
        assert!(concurrency > 0);
        Self {
            backend,
            permits: Arc::new(Semaphore::new(concurrency)),
            timeout,
        }
    }
    pub async fn execute(
        &self,
        context: BuildContext,
        tag: &str,
        platform: Option<&str>,
        cancellation: CancellationToken,
    ) -> Result<Vec<RawBuildLog>, BuildError> {
        let permit = tokio::select! { biased; () = cancellation.cancelled() => return Err(BuildError::Cancelled), p = self.permits.acquire() => p.map_err(|_| BuildError::BuildKit)? };
        let _permit = permit;
        tokio::select! { biased;
            () = cancellation.cancelled() => Err(BuildError::Cancelled),
            result = tokio::time::timeout(self.timeout, self.backend.build_and_push(context, tag, platform, &cancellation)) => result.map_err(|_| BuildError::Timeout)?,
        }
    }
}

pub fn deterministic_repository(
    registry: &str,
    instance: &str,
    application: &str,
    service: &str,
) -> Result<String, BuildError> {
    if registry.is_empty()
        || [instance, application, service]
            .iter()
            .any(|v| !safe_component(v))
    {
        return Err(BuildError::Registry);
    }
    Ok(format!(
        "{}/{}/{}/{}",
        registry.trim_end_matches('/'),
        instance,
        application,
        service
    ))
}
fn safe_component(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 64
        && v.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub fn verified_digest(reference: &str, digest: &str) -> Result<String, BuildError> {
    let valid = digest
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()));
    if !valid || reference.contains('@') {
        return Err(BuildError::Digest);
    }
    Ok(format!("{reference}@{}", digest.to_ascii_lowercase()))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[derive(Clone, Debug)]
pub struct BuiltSource {
    pub commit: String,
    pub registry_reference: String,
    pub digest_reference: String,
    pub build_key: String,
    pub context_hash: String,
    pub logs: Vec<BuildLogEntry>,
}

/// Complete source-to-verified-digest controller used by API preparation.
pub struct BuildService<C, B> {
    git: GixGitSource<C>,
    executor: BuildExecutor<B>,
    registry: RegistryClient,
    registry_authority: String,
    instance: String,
    git_timeout: Duration,
    context_limit: u64,
    cache: Arc<SqliteStore>,
}

#[async_trait]
pub trait SourceBuilder: Send + Sync {
    /// Checks the configured registry without publishing or mutating an image.
    async fn registry_ready(&self) -> Result<(), BuildError> {
        Ok(())
    }

    async fn build_source(
        &self,
        application: &str,
        service: &str,
        repository: &str,
        reference: &str,
        context: &str,
        dockerfile: &str,
    ) -> Result<BuiltSource, BuildError>;
}

impl<C, B> BuildService<C, B> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        git: GixGitSource<C>,
        executor: BuildExecutor<B>,
        registry: RegistryClient,
        registry_authority: String,
        instance: String,
        git_timeout: Duration,
        context_limit: u64,
        cache: Arc<SqliteStore>,
    ) -> Self {
        Self {
            git,
            executor,
            registry,
            registry_authority,
            instance,
            git_timeout,
            context_limit,
            cache,
        }
    }
}

impl<C: GitCredentialProvider + 'static, B: BuildKit> BuildService<C, B> {
    #[allow(clippy::too_many_arguments)]
    pub async fn build(
        &self,
        application: &str,
        service: &str,
        repository: &str,
        reference: &str,
        context: &str,
        dockerfile: &str,
        cancellation: CancellationToken,
    ) -> Result<BuiltSource, BuildError> {
        self.registry.ready().await?;
        let checkout = self
            .git
            .resolve_checkout(
                repository,
                reference,
                cancellation.child_token(),
                self.git_timeout,
            )
            .await?;
        let result = async {
            let context_archive =
                BuildContext::create(&checkout.checkout, context, dockerfile, self.context_limit)?;
            let context_hash = context_archive.content_hash.clone();
            let identity = BuildIdentity {
                commit: checkout.commit.clone(),
                context: context.into(),
                dockerfile: dockerfile.into(),
                build_args: BTreeMap::new(),
                target: None,
                platform: None,
                builder: "docker-buildkit-v1".into(),
            };
            let build_key = identity.key();
            let repository_path = deterministic_repository(
                &self.registry_authority,
                &self.instance,
                application,
                service,
            )?;
            if let Ok(Some(cached)) = self.cache.verified_build_for_key(&build_key).await
                && cached.source_commit.as_deref() == Some(checkout.commit.as_str())
                && let (Some(registry_reference), Some(digest_reference)) =
                    (cached.image_reference, cached.image_digest)
            {
                let path = repository_path
                    .split_once('/')
                    .map(|(_, path)| path)
                    .ok_or(BuildError::Registry)?;
                let tag = registry_reference
                    .rsplit_once(':')
                    .map(|(_, tag)| tag)
                    .ok_or(BuildError::Registry)?;
                if self
                    .registry
                    .verified_reference(path, tag)
                    .await
                    .is_ok_and(|verified| verified == digest_reference)
                {
                    return Ok(BuiltSource {
                        commit: checkout.commit.clone(),
                        registry_reference,
                        digest_reference,
                        build_key,
                        context_hash,
                        logs: vec![BuildLogEntry {
                            sequence: 1,
                            timestamp_ms: now_ms(),
                            message: "verified build cache reused".into(),
                        }],
                    });
                }
            }
            let tag = format!("{}:{}", repository_path, &build_key[7..23]);
            let raw = self
                .executor
                .execute(context_archive, &tag, None, cancellation)
                .await?;
            let mut logs = BuildLogCollector::new(DEFAULT_MAX_LOG_BYTES, [repository.to_owned()]);
            for line in raw {
                logs.push(&line.stream);
            }
            let path = repository_path
                .split_once('/')
                .map(|(_, path)| path)
                .ok_or(BuildError::Registry)?;
            let image_tag = tag
                .rsplit_once(':')
                .map(|(_, tag)| tag)
                .ok_or(BuildError::Registry)?;
            let digest_reference = self.registry.verified_reference(path, image_tag).await?;
            Ok(BuiltSource {
                commit: checkout.commit.clone(),
                registry_reference: tag,
                digest_reference,
                build_key,
                context_hash,
                logs: logs.entries().to_vec(),
            })
        }
        .await;
        // Checkout roots contain only directories created by this controller.
        let _ = tokio::fs::remove_dir_all(&checkout.checkout).await;
        result
    }
}

#[async_trait]
impl<C: GitCredentialProvider + 'static, B: BuildKit + 'static> SourceBuilder
    for BuildService<C, B>
{
    async fn registry_ready(&self) -> Result<(), BuildError> {
        self.registry.ready().await
    }

    async fn build_source(
        &self,
        application: &str,
        service: &str,
        repository: &str,
        reference: &str,
        context: &str,
        dockerfile: &str,
    ) -> Result<BuiltSource, BuildError> {
        self.build(
            application,
            service,
            repository,
            reference,
            context,
            dockerfile,
            CancellationToken::new(),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn identity() -> BuildIdentity {
        BuildIdentity {
            commit: "a".repeat(40),
            context: ".".into(),
            dockerfile: "Dockerfile".into(),
            build_args: BTreeMap::from([("MODE".into(), "release".into())]),
            target: None,
            platform: Some("linux/amd64".into()),
            builder: "buildkit-v1".into(),
        }
    }
    #[test]
    fn build_key_is_stable_and_exact() {
        let a = identity();
        assert_eq!(a.key(), a.clone().key());
        let mut b = a;
        b.commit = "b".repeat(40);
        assert_ne!(b.key(), identity().key());
    }
    #[test]
    fn urls_reject_credentials_and_redact_tokens() {
        assert!(matches!(
            validated_repository_url("https://u:p@example.test/r"),
            Err(BuildError::RepositoryUrl)
        ));
        let redacted = redact_url("https://u:canary@example.test/r?token=canary");
        assert!(!redacted.contains("canary"));
        assert!(!redacted.contains(":canary"));
    }
    #[cfg(unix)]
    #[test]
    fn protected_git_tokens_require_private_regular_files() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("token");
        fs::write(&path, "canary-token\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let provider = ProtectedGitCredentials::from_reference(Some(&CredentialReference::File {
            path: path.clone(),
        }))
        .unwrap();
        assert_eq!(
            provider
                .token(&Url::parse("https://example.test/r").unwrap())
                .unwrap()
                .unwrap()
                .as_str(),
            "canary-token"
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            provider
                .token(&Url::parse("https://example.test/r").unwrap())
                .is_err()
        );
    }
    #[test]
    fn context_rejects_traversal_symlinks_and_bounds_size() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("Dockerfile"), "FROM scratch").unwrap();
        fs::write(d.path().join("large"), vec![0; 32]).unwrap();
        assert!(matches!(
            BuildContext::create(d.path(), ".", "Dockerfile", 8),
            Err(BuildError::ContextTooLarge)
        ));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", d.path().join("escape")).unwrap();
            assert!(matches!(
                BuildContext::create(d.path(), ".", "Dockerfile", 1024),
                Err(BuildError::UnsafeContext)
            ));
        }
        assert!(matches!(
            BuildContext::create(d.path(), "../x", "Dockerfile", 1024),
            Err(BuildError::UnsafeContext)
        ));
    }
    #[test]
    fn logs_are_ordered_bounded_and_redacted() {
        let mut logs = BuildLogCollector::new(80, ["canary-token".into()]);
        logs.push("https://u:canary-token@example.test/r?token=canary-token");
        logs.push("done");
        assert_eq!(logs.entries()[0].sequence, 1);
        assert_eq!(logs.entries()[1].sequence, 2);
        assert!(
            !serde_json::to_string(logs.entries())
                .unwrap()
                .contains("canary-token")
        );
    }

    struct Gate {
        active: AtomicUsize,
        high: AtomicUsize,
    }
    #[async_trait]
    impl BuildKit for Gate {
        async fn build_and_push(
            &self,
            _: BuildContext,
            _: &str,
            _: Option<&str>,
            _: &CancellationToken,
        ) -> Result<Vec<RawBuildLog>, BuildError> {
            let n = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.high.fetch_max(n, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(vec![])
        }
    }
    fn empty_context() -> BuildContext {
        BuildContext {
            archive: vec![],
            dockerfile: "Dockerfile".into(),
            content_hash: "sha256:0".into(),
        }
    }
    #[tokio::test]
    async fn concurrency_is_bounded() {
        let backend = Arc::new(Gate {
            active: AtomicUsize::new(0),
            high: AtomicUsize::new(0),
        });
        let executor = Arc::new(BuildExecutor::new(
            Arc::clone(&backend),
            2,
            Duration::from_secs(1),
        ));
        let mut tasks = Vec::new();
        for _ in 0..6 {
            let e = Arc::clone(&executor);
            tasks.push(tokio::spawn(async move {
                e.execute(empty_context(), "tag", None, CancellationToken::new())
                    .await
                    .unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(backend.high.load(Ordering::SeqCst), 2);
    }
    #[tokio::test]
    async fn cancellation_and_timeout_are_honest() {
        let backend = Arc::new(Gate {
            active: AtomicUsize::new(0),
            high: AtomicUsize::new(0),
        });
        let executor = BuildExecutor::new(backend, 1, Duration::from_millis(1));
        assert!(matches!(
            executor
                .execute(empty_context(), "tag", None, CancellationToken::new())
                .await,
            Err(BuildError::Timeout)
        ));
        let token = CancellationToken::new();
        token.cancel();
        assert!(matches!(
            executor.execute(empty_context(), "tag", None, token).await,
            Err(BuildError::Cancelled)
        ));
    }
    #[test]
    fn digest_and_repository_are_deterministic() {
        let repo =
            deterministic_repository("127.0.0.1:5000", "instance-a", "app-a", "web").unwrap();
        assert_eq!(repo, "127.0.0.1:5000/instance-a/app-a/web");
        assert_eq!(
            verified_digest(&repo, &format!("sha256:{}", "a".repeat(64))).unwrap(),
            format!("{repo}@sha256:{}", "a".repeat(64))
        );
    }
}
