//! The sync worker: deploys environments and previews whose branch moved.
//!
//! Every [`TICK`], it lists the branches of each repository that is due, with
//! one `git ls-remote` per repository however many applications, environments
//! and previews follow it, and submits a [`Mutation::Sync`] for each one whose
//! head moved past the head of its last deployment. A repository is due when its
//! poll interval elapsed, when a verified webhook hinted that it moved, while a
//! deployment kept sync from acting on a head, and once after the daemon
//! starts. A listing that fails never deploys anything: the repository is
//! retried with exponential backoff, as it is when a submission fails.
use super::{ApplicationService, ListedHead, Mutation, MutationResponse, SyncOutcome};
use crate::git::Heads;
use crate::store::{Actor, SyncApplication};
use futures_util::{StreamExt, stream};
use piqueld_core::{ApplicationId, GitBranch, sync::RepositorySync, sync::SystemActor};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// How often the worker looks for due repositories, unless a hint wakes it.
const TICK: Duration = Duration::from_secs(10);
/// Least time between two listings of one application's repository, so a
/// burst of webhooks, or a long deployment, costs one listing per period.
const RECHECK: Duration = Duration::from_secs(30);
/// Delay after a first failed listing, doubled for each further failure.
const BACKOFF: Duration = Duration::from_secs(60);
/// Longest delay between attempts while a repository keeps failing.
const MAX_BACKOFF: Duration = Duration::from_secs(3600);
/// Repositories listed at once.
const LISTINGS: usize = 4;

/// Lists a repository's branch heads: `git ls-remote` in the daemon, a
/// fake in tests.
pub(crate) trait Lister: Sync {
    /// Lists every branch of `url` with its head commit.
    fn list(&self, url: &str) -> impl Future<Output = anyhow::Result<Heads>> + Send;
}

/// Lists branches with `git ls-remote`.
pub(crate) struct Git;

impl Lister for Git {
    fn list(&self, url: &str) -> impl Future<Output = anyhow::Result<Heads>> + Send {
        Heads::list(url)
    }
}

/// Applications whose repository a verified webhook said moved, waiting for
/// the worker, which they wake.
#[derive(Default)]
pub(crate) struct Hints {
    applications: std::sync::Mutex<BTreeSet<ApplicationId>>,
    wake: tokio::sync::Notify,
}

impl Hints {
    /// Asks the worker to list `application`'s repository soon.
    pub(crate) fn hint(&self, application: ApplicationId) {
        self.applications
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(application);
        self.wake.notify_one();
    }

    /// Takes every waiting hint.
    fn take(&self) -> BTreeSet<ApplicationId> {
        std::mem::take(
            &mut *self
                .applications
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

/// When the worker next lists each repository, by URL, and why. A
/// repository several applications read is listed once for all of them.
#[derive(Default)]
pub(crate) struct Schedule(HashMap<String, Due>);

/// One repository's place in the [`Schedule`].
#[derive(Default)]
struct Due {
    /// When it was last listed successfully.
    checked: Option<Instant>,
    /// Why it must be listed regardless of poll intervals: a webhook, a
    /// deployment that kept sync from acting on a head, or a failure.
    pending: Option<SystemActor>,
    /// Consecutive failures, listing it or submitting what it showed.
    failures: u32,
    /// Not listed before then: `RECHECK` after a listing, or the backoff
    /// after a failure.
    not_before: Option<Instant>,
}

impl Due {
    /// Why the repository must be listed at `now`, if it must, given how the
    /// applications reading it sync. It is polled at the shortest of their
    /// intervals, and listed once after the daemon starts even with only
    /// webhooks, since hints and follow-ups from before then were lost.
    fn trigger(
        &self,
        syncs: impl Iterator<Item = RepositorySync>,
        now: Instant,
    ) -> Option<SystemActor> {
        if self.not_before.is_some_and(|not_before| now < not_before) {
            return None;
        }
        let interval = syncs
            .filter_map(|sync| match sync {
                RepositorySync::Poll { interval_seconds } => {
                    Some(Duration::from_secs(interval_seconds.into()))
                }
                RepositorySync::Off | RepositorySync::Webhook => None,
            })
            .min();
        let polled = self.checked.is_none_or(|checked| {
            interval.is_some_and(|interval| now.duration_since(checked) >= interval)
        });
        self.pending.or(polled.then_some(SystemActor::SyncPoll))
    }

    /// Delays the next attempt after another failure, keeping `trigger`.
    fn failed(&mut self, trigger: SystemActor, now: Instant) {
        self.pending = Some(trigger);
        self.failures = self.failures.saturating_add(1);
        let backoff = BACKOFF
            .saturating_mul(1 << self.failures.saturating_sub(1).min(16))
            .min(MAX_BACKOFF);
        self.not_before = Some(now + backoff);
    }
}

impl ApplicationService {
    /// Runs the sync worker until `cancellation`, which also interrupts a
    /// pass in progress.
    pub(super) async fn sync(&self, cancellation: CancellationToken) {
        let mut schedule = Schedule::default();
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return,
                () = self.sync_due(&Git, &mut schedule, Instant::now()) => {}
            }
            tokio::select! {
                () = cancellation.cancelled() => return,
                () = self.hints.wake.notified() => {}
                () = tokio::time::sleep(TICK) => {}
            }
        }
    }

    /// Lists every repository due at `now`, at most [`LISTINGS`] at once,
    /// then syncs every application reading each. A failed listing, or a
    /// failure submitting what it showed, records the error on the
    /// applications and backs off.
    pub(crate) async fn sync_due(
        &self,
        lister: &impl Lister,
        schedule: &mut Schedule,
        now: Instant,
    ) {
        let applications = match self.store.sync_applications().await {
            Ok(applications) => applications,
            Err(error) => {
                tracing::warn!(?error, "sync could not read applications");
                return;
            }
        };
        let hinted = self.hints.take();
        let mut repositories = BTreeMap::<String, Vec<SyncApplication>>::new();
        for application in applications {
            repositories
                .entry(application.connection.repository.url.clone())
                .or_default()
                .push(application);
        }
        schedule.0.retain(|url, _| repositories.contains_key(url));
        let due = repositories.into_iter().filter_map(|(url, applications)| {
            let state = schedule.0.entry(url.clone()).or_default();
            if applications
                .iter()
                .any(|application| hinted.contains(&application.id))
            {
                state.pending = Some(SystemActor::SyncWebhook);
            }
            let syncs = applications
                .iter()
                .map(|application| application.connection.sync);
            let trigger = state.trigger(syncs, now)?;
            Some((url, applications, trigger))
        });
        let due: Vec<_> = due.collect();
        // Deadlines count from when each repository's turn ended, not from
        // the pass's start, which may be long before with many repositories.
        let started = Instant::now();
        let mut listings = stream::iter(due)
            .map(|(url, applications, trigger)| async move {
                (lister.list(&url).await, url, applications, trigger)
            })
            .buffer_unordered(LISTINGS);
        while let Some((heads, url, applications, trigger)) = listings.next().await {
            let (mut busy, mut failed) = (false, false);
            for application in &applications {
                let result = match &heads {
                    Ok(heads) => self.sync_application(application, heads, trigger).await,
                    Err(error) => Err(format!("{error:#}")),
                };
                let error = result.map(|retry| busy |= retry).err();
                if let Some(error) = &error {
                    tracing::warn!(application = %application.id, %error, "sync failed");
                    failed = true;
                }
                if let Err(error) = self
                    .store
                    .record_sync_check(&application.id, error.as_deref())
                    .await
                {
                    tracing::warn!(?error, "sync could not record its check");
                }
            }
            let (state, done) = (schedule.0.entry(url).or_default(), now + started.elapsed());
            if heads.is_ok() {
                state.checked = Some(done);
            }
            if failed {
                state.failed(trigger, done);
            } else {
                state.failures = 0;
                state.pending = busy.then_some(trigger);
                state.not_before = Some(done + RECHECK);
            }
        }
    }

    /// Submits a [`Mutation::Sync`] by `trigger` for the head in `heads` of
    /// everything following `application`, which deploys it once the branch
    /// moved past the head of its last deployment. A gone branch deploys
    /// nothing. Returns whether a
    /// deployment kept sync from acting on a head, so it tries again, or the
    /// first submission that failed, after trying the others.
    async fn sync_application(
        &self,
        application: &SyncApplication,
        heads: &Heads,
        trigger: SystemActor,
    ) -> Result<bool, String> {
        let (mut busy, mut failure) = (false, None);
        for following in &application.following {
            let Some(branch) = following.source.branch() else {
                continue;
            };
            let Some(head) = heads.head(branch.branch()) else {
                continue;
            };
            let Ok(branch) = GitBranch::parse(branch.branch()) else {
                continue;
            };
            let mutation = Mutation::Sync(ListedHead {
                id: following.id.clone(),
                repository: heads.url().to_owned(),
                branch,
                head: head.to_owned(),
                since: following.synced.clone(),
            });
            match self
                .accept(Actor::System(trigger), mutation, None, false, None)
                .await
            {
                Ok(MutationResponse::Synced(SyncOutcome::Busy)) => busy = true,
                Ok(MutationResponse::Synced(SyncOutcome::Deployed(operation))) => {
                    tracing::info!(environment = %following.id, operation = %operation.operation_id, %head, "sync deployed a moved branch");
                }
                Ok(_) => {}
                Err(error) => {
                    failure.get_or_insert_with(|| {
                        format!("could not deploy {}: {error}", following.name)
                    });
                }
            }
        }
        failure.map_or(Ok(busy), Err)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    pub(crate) use super::Schedule;
    use super::*;
    use crate::api::PreviewMutation;
    use piqueld_core::{
        EnvironmentId, EnvironmentName, TrackedBranch, manifest::parse_template_toml,
    };
    use std::sync::{Arc, Mutex};

    pub(crate) const OLD: &str = "1111111111111111111111111111111111111111";
    pub(crate) const NEW: &str = "2222222222222222222222222222222222222222";
    const URL: &str = "https://example.com/notes.git";

    /// A service over a fresh store, with `notes` syncing as `sync`.
    pub(crate) struct Fixture {
        pub(crate) service: ApplicationService,
        pub(crate) application: ApplicationId,
        _temp: tempfile::TempDir,
        _docker: tokio::net::UnixListener,
    }

    impl Fixture {
        pub(crate) async fn new(sync: &str) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let store = Arc::new(
                crate::store::Store::open(temp.path().join("db"))
                    .await
                    .unwrap(),
            );
            // Only the Docker socket's existence is checked; no request is made.
            let socket = temp.path().join("docker.sock");
            let docker = tokio::net::UnixListener::bind(&socket).unwrap();
            let runtime = crate::reconcile::Controller::new(
                Arc::new(crate::docker::BollardDocker::connect(&socket).unwrap()),
                Arc::clone(&store),
            )
            .runtime(Arc::new(tokio::sync::Notify::new()));
            let service = ApplicationService::new(store, runtime);
            Self {
                application: Self::save(&service, "notes", sync).await,
                service,
                _temp: temp,
                _docker: docker,
            }
        }

        /// Saves application `name` reading `main` of the shared repository
        /// and syncing as `sync`, returning its ID.
        pub(crate) async fn save(
            service: &ApplicationService,
            name: &str,
            sync: &str,
        ) -> ApplicationId {
            let template = parse_template_toml(&format!(
                "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='{name}'\n[spec.manifest]\npath='app.toml'\nsync={sync}\n[spec.manifest.repository]\nurl='{URL}'\nbranch='main'\n"
            ))
            .unwrap()
            .normalize(ApplicationId::parse("app-pending-01").unwrap());
            let save = Mutation::Save {
                application: Box::new(template),
                expected_application_id: None,
                deploy: false,
            };
            let MutationResponse::Saved(saved) = service
                .accept(Actor::Daemon, save, Some(0), false, None)
                .await
                .unwrap()
            else {
                panic!("saved")
            };
            ApplicationId::parse(saved.application_id).unwrap()
        }

        /// The application's first environment, `production` following
        /// `main`, opted in and deployed at `OLD`.
        pub(crate) async fn production(&self) -> EnvironmentId {
            let store = &self.service.store;
            let production = store.environments(&self.application).await.unwrap()[0]
                .id
                .clone();
            self.follow(&production).await;
            production
        }

        /// Opts environment `id` in and deploys it at `OLD`.
        pub(crate) async fn follow(&self, id: &EnvironmentId) {
            let opt_in = Mutation::SetSync {
                id: id.clone(),
                enabled: true,
            };
            self.service
                .accept(Actor::Daemon, opt_in, None, false, None)
                .await
                .unwrap();
            self.service.store.deployed_at(id, OLD).await;
        }

        /// Adds an environment following `branch`, opted in and deployed at `OLD`.
        pub(crate) async fn environment(&self, name: &str, branch: &str) -> EnvironmentId {
            let create = Mutation::CreateEnvironment {
                application: self.application.clone(),
                name: EnvironmentName::parse(name).unwrap(),
                branch: Some(TrackedBranch::new(branch.into(), None).unwrap()),
            };
            let MutationResponse::Environment(environment) = self
                .service
                .accept(Actor::Daemon, create, None, true, None)
                .await
                .unwrap()
            else {
                panic!("environment")
            };
            self.follow(&environment.id).await;
            environment.id
        }

        /// Adds a preview of `branch`, deployed at `OLD`.
        async fn preview(&self, branch: &str) -> EnvironmentId {
            let create = Mutation::Preview(PreviewMutation::Create {
                application: self.application.clone(),
                branch: GitBranch::parse(branch).unwrap(),
                slot: None,
            });
            let MutationResponse::Preview(created) = self
                .service
                .accept(Actor::Daemon, create, None, false, None)
                .await
                .unwrap()
            else {
                panic!("preview")
            };
            self.service
                .store
                .deployed_at(&created.preview.id, OLD)
                .await;
            created.preview.id
        }

        /// The commits sync deployed to `id`, newest first, with their actor.
        pub(crate) async fn synced(&self, id: &EnvironmentId) -> Vec<(String, String)> {
            sqlx::query_as(
                "SELECT json_extract(i.application_json,'$.spec.manifest.repository.commit'),o.actor_system FROM operations o JOIN deployment_inputs i ON i.operation_id=o.id WHERE o.environment_id=?1 AND o.actor_system IS NOT NULL ORDER BY o.created_at_ms DESC,o.id DESC",
            )
            .bind(id.as_str())
            .fetch_all(self.service.store.pool())
            .await
            .unwrap()
        }
    }

    /// Answers listings with `heads`, or fails, counting them per URL.
    #[derive(Default)]
    pub(crate) struct Fake {
        pub(crate) heads: Mutex<Option<Vec<(&'static str, &'static str)>>>,
        pub(crate) listed: Mutex<Vec<String>>,
    }

    impl Fake {
        pub(crate) fn with(heads: Vec<(&'static str, &'static str)>) -> Self {
            Self {
                heads: Mutex::new(Some(heads)),
                listed: Mutex::default(),
            }
        }

        pub(crate) fn listings(&self) -> usize {
            self.listed.lock().unwrap().len()
        }
    }

    impl Lister for Fake {
        fn list(&self, url: &str) -> impl Future<Output = anyhow::Result<Heads>> + Send {
            self.listed.lock().unwrap().push(url.to_owned());
            let heads = self.heads.lock().unwrap().clone();
            let url = url.to_owned();
            async move {
                let heads = heads
                    .ok_or_else(|| anyhow::anyhow!("git ls-remote failed (exit status: 128)"))?;
                Ok(Heads::new(
                    &url,
                    heads
                        .into_iter()
                        .map(|(branch, head)| (branch.into(), head.into())),
                ))
            }
        }
    }

    #[tokio::test]
    async fn polling_lists_a_repository_once_for_every_application_and_deploys_only_what_moved() {
        let fixture = Fixture::new("{mode='poll',interval_seconds=60}").await;
        let production = fixture.production().await;
        // Another application reading the same repository, polling slower.
        let wiki = Fixture::save(
            &fixture.service,
            "wiki",
            "{mode='poll',interval_seconds=600}",
        )
        .await;
        let wiki = fixture.service.store.environments(&wiki).await.unwrap()[0]
            .id
            .clone();
        fixture.follow(&wiki).await;
        let staging = fixture.environment("staging", "develop").await;
        let pinned = fixture.environment("pinned", "main").await;
        let pin = Mutation::SetBranch {
            id: pinned.clone(),
            branch: TrackedBranch::new("main".into(), Some(OLD.into())).unwrap(),
        };
        fixture
            .service
            .accept(Actor::Daemon, pin, None, true, None)
            .await
            .unwrap();
        // Deployed from `main` too, but never opted in.
        let manual = fixture.environment("manual", "main").await;
        let opt_out = Mutation::SetSync {
            id: manual.clone(),
            enabled: false,
        };
        fixture
            .service
            .accept(Actor::Daemon, opt_out, None, false, None)
            .await
            .unwrap();
        let moved = fixture.preview("feat/login").await;
        let gone = fixture.preview("feat/gone").await;
        let lister = Fake::with(vec![("main", NEW), ("develop", OLD), ("feat/login", NEW)]);
        let mut schedule = Schedule::default();
        let now = Instant::now();
        fixture.service.sync_due(&lister, &mut schedule, now).await;

        assert_eq!(lister.listings(), 1);
        let deployed = vec![(NEW.to_owned(), "sync:poll".to_owned())];
        assert_eq!(fixture.synced(&production).await, deployed);
        assert_eq!(fixture.synced(&wiki).await, deployed);
        assert_eq!(fixture.synced(&moved).await, deployed);
        for unchanged in [&staging, &pinned, &manual, &gone] {
            assert_eq!(fixture.synced(unchanged).await, []);
        }
        // A gone branch keeps its preview.
        assert!(fixture.service.store.get(&gone).await.is_ok());

        // Nothing is listed again before the interval elapses.
        fixture
            .service
            .sync_due(&lister, &mut schedule, now + Duration::from_secs(59))
            .await;
        assert_eq!(lister.listings(), 1);
        fixture
            .service
            .sync_due(&lister, &mut schedule, now + Duration::from_secs(61))
            .await;
        assert_eq!(lister.listings(), 2);
        assert_eq!(fixture.synced(&production).await.len(), 1);
    }

    #[tokio::test]
    async fn failed_listings_deploy_nothing_and_back_off() {
        let fixture = Fixture::new("{mode='poll',interval_seconds=60}").await;
        let production = fixture.production().await;
        let lister = Fake::default();
        let mut schedule = Schedule::default();
        let now = Instant::now();
        fixture.service.sync_due(&lister, &mut schedule, now).await;
        assert_eq!(lister.listings(), 1);
        assert_eq!(fixture.synced(&production).await, []);
        let check = fixture
            .service
            .store
            .sync_check(&fixture.application)
            .await
            .unwrap()
            .unwrap();
        assert!(check.error.unwrap().contains("ls-remote failed"));

        // Retried after 60 s, then after 120 s more.
        fixture
            .service
            .sync_due(&lister, &mut schedule, now + Duration::from_secs(59))
            .await;
        assert_eq!(lister.listings(), 1);
        fixture
            .service
            .sync_due(&lister, &mut schedule, now + Duration::from_secs(61))
            .await;
        assert_eq!(lister.listings(), 2);
        fixture
            .service
            .sync_due(&lister, &mut schedule, now + Duration::from_secs(180))
            .await;
        assert_eq!(lister.listings(), 2);

        *lister.heads.lock().unwrap() = Some(vec![("main", NEW)]);
        fixture
            .service
            .sync_due(&lister, &mut schedule, now + Duration::from_secs(182))
            .await;
        assert_eq!(fixture.synced(&production).await.len(), 1);
        let check = fixture
            .service
            .store
            .sync_check(&fixture.application)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(check.error, None);
    }
}
