//! Branch previews: pruning, local snapshot uploads and the preview permissions.

use super::*;

/// A Git repository with one empty commit on `main` and on each of `branches`.
fn repository_with_branches(branches: &[&str]) -> Repository {
    let repository = Repository::new();
    repository.commit(&[]);
    for branch in branches {
        repository.git(&["branch", branch]);
    }
    repository
}

/// Pruning deletes only the previews whose branch the repository confirms is
/// gone, and nothing at all while the repository cannot be read.
#[tokio::test]
async fn pruning_deletes_previews_only_when_their_branch_is_confirmed_gone() {
    use piqueld_client::{BranchState, CreatePreviewRequest, PrunePreviewsRequest};
    let temp = tempfile::tempdir().unwrap();
    let repository = repository_with_branches(&["feat/gone", "feat/kept"]);
    let (server, application) = connected_application(&temp, &repository, None).await;
    let client = server.client();
    let application = application.as_str();
    let mut previews = Vec::new();
    for branch in ["feat/gone", "feat/kept"] {
        let request = CreatePreviewRequest {
            branch: branch.into(),
            slot: None,
        };
        previews.push(
            client
                .create_preview(application, &request)
                .await
                .unwrap()
                .preview
                .id,
        );
    }
    let prune = PrunePreviewsRequest {
        previews: previews.clone(),
    };
    let deleting = async || {
        let mut deleting = Vec::new();
        for preview in &previews {
            deleting.push(
                client
                    .preview(preview.as_str())
                    .await
                    .unwrap()
                    .preview
                    .delete_intent,
            );
        }
        deleting
    };
    repository.git(&["branch", "-D", "feat/gone"]);
    let states = async || {
        client
            .previews(application)
            .await
            .unwrap()
            .into_iter()
            .map(|view| view.branch)
            .collect::<Vec<_>>()
    };
    assert!(matches!(
        states().await.as_slice(),
        [BranchState::Gone, BranchState::Exists { .. }]
    ));

    // An unreadable repository leaves every branch unknown and deletes nothing.
    let moved = temp.path().join("moved");
    std::fs::rename(repository.path(), &moved).unwrap();
    assert!(matches!(
        states().await.as_slice(),
        [BranchState::Unknown { .. }, BranchState::Unknown { .. }]
    ));
    let error = client
        .prune_previews(application, &prune)
        .await
        .unwrap_err();
    assert_eq!(api_error(error).status, 502);
    assert_eq!(deleting().await, [false, false]);

    std::fs::rename(&moved, repository.path()).unwrap();
    let deleted = client.prune_previews(application, &prune).await.unwrap();
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0].preview.id, previews[0]);
    assert_eq!(deleting().await, [true, false]);
}

/// Serves the contract API over TCP, with `max_upload_bytes` if given, and
/// saves the contract manifest connected to `repository`. Returns the
/// server and the application ID.
async fn connected_application(
    temp: &TempDir,
    repository: &Repository,
    max_upload_bytes: Option<std::num::NonZeroU64>,
) -> (Server, String) {
    let server = Server::tcp(router(state_with(temp, max_upload_bytes).await, FakeAuth)).await;
    let saved = server
        .client()
        .apply_application(&ApplyApplicationRequest {
            manifest: connected_manifest(repository),
            ..new_application()
        })
        .await
        .unwrap();
    (server, saved.application_id)
}

/// The contract manifest, connected to `app.toml` on `repository`'s `main`.
fn connected_manifest(repository: &Repository) -> ApplicationManifest {
    let mut manifest = manifest();
    manifest.spec.manifest = Some(
        serde_json::from_value(serde_json::json!({
            "repository": {"url": repository.path().display().to_string(), "branch": "main"},
            "path": "app.toml",
        }))
        .unwrap(),
    );
    manifest
}

/// A snapshot of a never-pushed branch is uploaded thin against the base the
/// repository has, deploys to a new preview whose branch state is `local`,
/// and is never pruned; oversized and invalid bundles get their own errors.
#[tokio::test]
async fn local_snapshots_upload_within_limits_and_are_never_pruned() {
    use piqueld_client::{BranchState, CreatePreviewRequest, PrunePreviewsRequest};
    let temp = tempfile::tempdir().unwrap();
    let repository = repository_with_branches(&["feat/gone"]);
    let limit = std::num::NonZeroU64::new(8192);
    let (server, application) = connected_application(&temp, &repository, limit).await;
    let client = server.client();
    let application = application.as_str();
    let request = CreatePreviewRequest {
        branch: "feat/gone".into(),
        slot: None,
    };
    let gone = client
        .create_preview(application, &request)
        .await
        .unwrap()
        .preview
        .id;
    repository.git(&["branch", "-D", "feat/gone"]);

    // A working copy with a commit on a branch the repository never had.
    let work = repository.working_copy();
    work.git(&["switch", "-qc", "feat/local"]);
    work.commit(&[("app.toml", b"local")]);
    let base = work.git(&["rev-parse", "origin/main"]);
    let upload = async |bytes: Vec<u8>| {
        let length = bytes.len() as u64;
        client
            .deploy_local_preview(application, ("feat/local", None), (bytes, length))
            .await
    };
    let created = upload(work.bundle("feat/local", Some(&base)))
        .await
        .unwrap();
    assert!(created.created);
    let local = created.preview.id;
    let deployments = client.deployments(local.as_str(), None).await.unwrap();
    assert!(matches!(
        &deployments.items[0].origin,
        piqueld_client::DeploymentOrigin::Local(snapshot) if snapshot.base.as_deref() == Some(base.as_str())
    ));

    // The never-pushed branch reads as `local`, not gone, and is kept.
    let states = client.previews(application).await.unwrap();
    let states = states
        .into_iter()
        .map(|view| view.branch)
        .collect::<Vec<_>>();
    assert_eq!(
        states,
        [BranchState::Gone, BranchState::Local { head: None }]
    );
    let prune = PrunePreviewsRequest {
        previews: vec![gone.clone(), local.clone()],
    };
    let deleted = client.prune_previews(application, &prune).await.unwrap();
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0].preview.id, gone);
    assert!(
        !client
            .preview(local.as_str())
            .await
            .unwrap()
            .preview
            .delete_intent
    );

    // Oversized and invalid uploads are refused before deploying anything.
    // The noise is a xorshift sequence, so it doesn't compress.
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let noise = (0..65_536)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect::<Vec<_>>();
    work.commit(&[("noise.bin", &noise)]);
    let large = upload(work.bundle("feat/local", None)).await.unwrap_err();
    assert_eq!(api_error(large), ApiError::new(413, "snapshot_too_large"));
    let invalid = upload(b"# v2 git bundle\nnot a bundle\n\n".to_vec()).await;
    assert_eq!(
        api_error(invalid.unwrap_err()),
        ApiError::new(422, "snapshot_invalid")
    );
    let deployments = client.deployments(local.as_str(), None).await.unwrap();
    assert_eq!(deployments.items.len(), 1);
}

/// An application `blog` with a manifest repository and a production
/// environment, an `owner` holding application permissions on it,
/// an `agent` token the owner limited to previews permissions, and a
/// `limited` account that holds only `previews:deploy` on blog.
struct PreviewFixture {
    _temp: TempDir,
    _repository: Repository,
    database: PathBuf,
    router: axum::Router,
    blog: String,
    production: String,
    owner: String,
    limited: String,
    agent: String,
}

impl PreviewFixture {
    /// The previews permissions `agent` holds.
    const AGENT: [&str; 2] = ["previews:deploy", "previews:delete"];
    /// Every permission the owner holds.
    const PERMISSIONS: [&str; 6] = [
        "apps:deploy",
        "apps:delete",
        "logs:read",
        "events:read",
        "apps:write",
        "secrets:write",
    ];

    async fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let database = database(&temp);
        let repository = repository_with_branches(&["feat/one", "feat/two"]);
        let state = state(&temp).await;
        let blog = save(&state, connected_manifest(&repository))
            .await
            .application_id;
        let blog_id = piqueld_core::ApplicationId::parse(blog.as_str()).unwrap();
        // The application's only long-lived environment stands for production.
        let production = state.application(&blog_id).await.unwrap().environments[0]
            .id
            .to_string();
        let app = Some(blog.as_str());
        let owner = Self::PERMISSIONS.map(|permission| (permission, app));
        let owner = seed_account(&database, "owner", &owner).await;
        let limited = [("previews:deploy", app), ("accounts:manage", None)];
        let limited = seed_account(&database, "limited", &limited).await;
        let mut fixture = Self {
            router: api_router(state, auth(&temp).await),
            _temp: temp,
            _repository: repository,
            database,
            blog,
            production,
            owner,
            limited,
            agent: String::new(),
        };
        let request = fixture.token(&Self::AGENT);
        let (status, created) = fixture
            .router
            .call(&fixture.owner, "POST", "/api/v1/auth/manage", request)
            .await;
        assert_eq!(status, 200, "{created}");
        created["token"]
            .as_str()
            .unwrap()
            .clone_into(&mut fixture.agent);
        fixture
    }

    /// Grants of `permissions` on blog.
    fn grants(&self, permissions: &[&str]) -> serde_json::Value {
        permissions
            .iter()
            .map(|permission| {
                serde_json::json!({
                    "permission": permission,
                    "applications": [self.blog],
                })
            })
            .collect()
    }

    /// A request creating a token with [`Self::grants`].
    fn token(&self, permissions: &[&str]) -> serde_json::Value {
        let grants = self.grants(permissions);
        serde_json::json!({"action": "create_token", "grants": grants, "name": "agent", "days": 1})
    }

    /// Records a finished build of `environment`, returning its ID.
    async fn build(&self, environment: &str) -> i64 {
        sqlx::query_scalar("INSERT INTO builds(environment_id,operation_id,service,source_json,state,started_at_ms) VALUES(?1,'operation','web','{}','succeeded',1) RETURNING id")
            .bind(environment)
            .fetch_one(&mut sqlite::connect(&self.database).await)
            .await
            .unwrap()
    }

    /// Creates a preview of `branch` as the agent, returning its ID.
    async fn preview(&self, branch: &str) -> String {
        let uri = format!("/api/v1/applications/{}/previews", self.blog);
        let request = serde_json::json!({"branch": branch});
        let (status, body) = self.router.call(&self.agent, "POST", &uri, request).await;
        assert!(status < 300, "{status} {body}");
        body["data"]["preview"]["id"].as_str().unwrap().to_owned()
    }
}

/// A token with previews permissions creates, deploys, reads, reads the logs
/// of, and deletes previews.
#[tokio::test]
async fn previews_permissions_manage_previews() {
    let f = PreviewFixture::new().await;
    let none = serde_json::Value::Null;
    let blog = &f.blog;
    let preview = f.preview("feat/one").await;
    for (method, uri) in [
        ("GET", format!("/api/v1/previews/{preview}")),
        ("GET", format!("/api/v1/environments/{preview}/deployments")),
        ("POST", format!("/api/v1/previews/{preview}/deploy")),
        ("GET", format!("/api/v1/applications/{blog}/previews")),
        ("GET", format!("/api/v1/applications/{blog}")),
    ] {
        let (status, body) = f.router.call(&f.agent, method, &uri, none.clone()).await;
        assert!(status < 300, "{method} {uri}: {status} {body}");
    }
    let logs = format!("/api/v1/environments/{preview}/logs?service=web&tail=5&since_seconds=60");
    let (status, body) = f.router.call(&f.agent, "GET", &logs, none.clone()).await;
    assert_eq!(status, 200, "{body}");
    let build = format!("/api/v1/builds/{}/logs", f.build(&preview).await);
    let (status, body) = f.router.call(&f.agent, "GET", &build, none.clone()).await;
    assert_eq!(status, 200, "{body}");
    // Restoring is allowed too: only the deployment is unknown.
    let restore = format!("/api/v1/previews/{preview}/restore");
    let request = serde_json::json!({"deployment": "operation-00000000000000000000000000000000"});
    let (status, body) = f.router.call(&f.agent, "POST", &restore, request).await;
    assert_eq!(status, 404, "{body}");
    let delete = format!("/api/v1/previews/{preview}");
    assert_eq!(f.router.status(&f.agent, "DELETE", &delete).await, 202);
}

/// A token with previews permissions changes no long-lived environment, reads
/// none of its logs, changes no shared configuration, and cannot delete the
/// application or run commands.
#[tokio::test]
async fn previews_permissions_reach_no_production_or_shared_state() {
    let f = PreviewFixture::new().await;
    let preview = f.preview("feat/one").await;
    let (blog, production) = (&f.blog, format!("/api/v1/environments/{}", f.production));
    let build = f.build(&f.production).await;
    for (method, uri, permission) in [
        ("GET", format!("/api/v1/builds/{build}/logs"), "logs:read"),
        ("GET", format!("{production}/logs"), "logs:read"),
        (
            "POST",
            format!("{production}/deploy?force=true"),
            "apps:deploy",
        ),
        ("DELETE", format!("{production}?force=true"), "apps:delete"),
        (
            "GET",
            format!("/api/v1/environments/{preview}/exec"),
            "apps:exec",
        ),
        (
            "DELETE",
            format!("/api/v1/applications/{blog}?force=true"),
            "apps:delete",
        ),
        (
            "PUT",
            format!("/api/v1/applications/{blog}/name?force=true"),
            "apps:write",
        ),
        (
            "PUT",
            format!("/api/v1/applications/{blog}/repository/sync?force=true"),
            "apps:write",
        ),
        (
            "POST",
            format!("/api/v1/applications/{blog}/environments?force=true"),
            "apps:write",
        ),
        (
            "PUT",
            format!("/api/v1/applications/{blog}/secrets/token/access"),
            "secrets:write",
        ),
    ] {
        let (status, body) = f
            .router
            .call(&f.agent, method, &uri, serde_json::json!({}))
            .await;
        assert_eq!(status, 403, "{method} {uri}: {body}");
        assert_eq!(body["details"]["permission"], permission, "{method} {uri}");
    }
}

/// Previews permissions are never handed on as whole-application access, and
/// a token holding them follows its owner's current access and its own
/// revocation.
#[tokio::test]
async fn previews_permissions_never_widen() {
    let f = PreviewFixture::new().await;
    let none = serde_json::Value::Null;
    let manage = "/api/v1/auth/manage";
    let invite =
        serde_json::json!({"action": "create_invitation", "grants": f.grants(&["apps:deploy"])});
    for wider in [f.token(&["apps:deploy"]), invite] {
        let (status, body) = f.router.call(&f.limited, "POST", manage, wider).await;
        assert_eq!(
            (status, body["code"].as_str()),
            (403, Some("permission_exceeded"))
        );
    }
    let narrower = f.token(&["previews:deploy"]);
    assert_eq!(
        f.router.call(&f.limited, "POST", manage, narrower).await.0,
        200
    );

    let preview = format!("/api/v1/previews/{}", f.preview("feat/two").await);
    execute(
        &f.database,
        "DELETE FROM auth_grants WHERE user_id='owner' AND permission='apps:delete'",
    )
    .await;
    let (status, body) = f
        .router
        .call(&f.agent, "DELETE", &preview, none.clone())
        .await;
    assert_eq!(
        (status, body["details"]["permission"].as_str()),
        (403, Some("previews:delete"))
    );

    let (_, directory) = f
        .router
        .call(&f.owner, "GET", "/api/v1/auth/directory", none.clone())
        .await;
    let id = directory["credentials"]
        .as_array()
        .unwrap()
        .iter()
        .find(|credential| credential["name"] == "agent" && credential["user_id"] == "owner")
        .unwrap()["id"]
        .clone();
    let revoke = serde_json::json!({"action": "revoke_credential", "id": id});
    assert_eq!(f.router.call(&f.owner, "POST", manage, revoke).await.0, 200);
    assert_eq!(f.router.status(&f.agent, "GET", &preview).await, 401);
}
