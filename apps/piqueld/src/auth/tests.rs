use super::*;
use piqueld_core::auth::Manage;

struct Fixture {
    auth: Auth,
    dir: tempfile::TempDir,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path().join("state.db"))
            .await
            .unwrap();
        Self {
            auth: Auth::new(&store, "http://localhost:7845").unwrap(),
            dir,
        }
    }
    async fn account(&self, name: &str, kind: &str, expires: Option<i64>) -> (String, String) {
        let id = Auth::id();
        let mut tx = self.auth.0.pool.begin().await.unwrap();
        sqlx::query("INSERT INTO auth_users VALUES(?,?,?,?)")
            .bind(&id)
            .bind(name)
            .bind("")
            .bind(Auth::now())
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("UPDATE auth_setup SET initialized=1,secret_hash=NULL")
            .execute(&mut *tx)
            .await
            .unwrap();
        let token = Auth::issue(&mut tx, &id, kind, "Test", expires)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        (id, token)
    }
}

#[tokio::test]
async fn sessions_expire_revoke_and_survive_restart_without_storing_secrets() {
    let f = Fixture::new().await;
    let (id, token) = f
        .account("alice", "browser", Some(Auth::now() + 7 * DAY))
        .await;
    let identity = f.auth.authenticate(&token).await.unwrap();
    assert_eq!(identity.user.id, id);
    let stored: String = sqlx::query_scalar("SELECT secret_hash FROM auth_credentials")
        .fetch_one(&f.auth.0.pool)
        .await
        .unwrap();
    assert_ne!(stored, token);
    let store = crate::store::Store::open(f.dir.path().join("state.db"))
        .await
        .unwrap();
    let restarted = Auth::new(&store, "http://localhost:7845").unwrap();
    restarted.authenticate(&token).await.unwrap();
    sqlx::query("UPDATE auth_credentials SET last_used_at=?")
        .bind(Auth::now() - DAY)
        .execute(&f.auth.0.pool)
        .await
        .unwrap();
    assert!(matches!(
        f.auth.authenticate(&token).await,
        Err(AuthError::Unauthorized)
    ));
    let (_, cli) = f.account("bob", "cli", Some(Auth::now() - 1)).await;
    assert!(matches!(
        f.auth.authenticate(&cli).await,
        Err(AuthError::Unauthorized)
    ));
    let (other, api) = f.account("carol", "token", None).await;
    f.auth
        .manage(&id, Manage::RevokeAll { user_id: other })
        .await
        .unwrap();
    assert!(matches!(
        f.auth.authenticate(&api).await,
        Err(AuthError::Unauthorized)
    ));
}

#[tokio::test]
async fn any_account_can_edit_another_and_last_account_deletion_is_atomic() {
    let f = Fixture::new().await;
    let (alice, _) = f.account("alice", "token", None).await;
    let (bob, bob_token) = f.account("bob", "token", None).await;
    f.auth
        .manage(
            &alice,
            Manage::UpdateUser {
                user_id: bob.clone(),
                username: "robert".into(),
                display_name: "Bob".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        f.auth.authenticate(&bob_token).await.unwrap().user.username,
        "robert"
    );
    let input = piqueld_core::auth::RegistrationStart {
        invitation: None,
        user_id: Some(bob.clone()),
        username: String::new(),
        display_name: String::new(),
        passkey_name: "Alice's authenticator".into(),
    };
    assert!(
        f.auth
            .registration_start(input, "binding", true)
            .await
            .is_ok()
    );
    let made = f
        .auth
        .manage(
            &alice,
            Manage::CreateToken {
                user_id: bob.clone(),
                name: "automation".into(),
                days: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        f.auth
            .authenticate(made.token.as_ref().unwrap())
            .await
            .unwrap()
            .user
            .id,
        bob
    );
    let (first, second) = tokio::join!(
        f.auth.manage(&alice, Manage::DeleteUser { user_id: bob }),
        f.auth.manage(
            &alice,
            Manage::DeleteUser {
                user_id: alice.clone()
            }
        )
    );
    assert_ne!(first.is_ok(), second.is_ok());
    assert_eq!(f.auth.directory().await.unwrap().users.len(), 1);
    assert!(f.auth.status().await.unwrap().initialized);
}

#[tokio::test]
async fn deleting_an_issuer_invalidates_its_invitations_and_credentials() {
    let f = Fixture::new().await;
    let (alice, token) = f.account("alice", "token", None).await;
    let (bob, _) = f.account("bob", "token", None).await;
    let result = f
        .auth
        .manage(&alice, Manage::CreateInvitation)
        .await
        .unwrap();
    let link = result.invitation_url.unwrap();
    let secret = link.split_once("#invite=").unwrap().1;
    assert!(f.auth.invitation_valid(secret).await.unwrap());
    f.auth
        .manage(&bob, Manage::DeleteUser { user_id: alice })
        .await
        .unwrap();
    assert!(!f.auth.invitation_valid(secret).await.unwrap());
    assert!(f.auth.authenticate(&token).await.is_err());
}

#[tokio::test]
async fn device_approval_is_explicit_single_use_and_bound_to_a_live_session() {
    let f = Fixture::new().await;
    let (_, token) = f.account("alice", "browser", Some(Auth::now() + DAY)).await;
    let identity = f.auth.authenticate(&token).await.unwrap();
    let start = f.auth.device_start().await.unwrap();
    assert_eq!(
        f.auth.device_poll(&start.device_code).await.unwrap().status,
        "authorization_pending"
    );
    assert_eq!(
        f.auth.device_poll(&start.device_code).await.unwrap().status,
        "slow_down"
    );
    f.auth
        .device_approve(&start.user_code, &identity)
        .await
        .unwrap();
    assert!(
        f.auth
            .device_approve(&start.user_code, &identity)
            .await
            .is_err()
    );
    f.auth
        .0
        .devices
        .lock()
        .await
        .get_mut(&Auth::hash(&start.device_code))
        .unwrap()
        .next_poll = 0;
    let result = f.auth.device_poll(&start.device_code).await.unwrap();
    f.auth
        .authenticate(result.token.as_ref().unwrap())
        .await
        .unwrap();
    assert!(f.auth.device_poll(&start.device_code).await.is_err());
    let start = f.auth.device_start().await.unwrap();
    f.auth
        .device_approve(&start.user_code, &identity)
        .await
        .unwrap();
    f.auth.logout(&identity.credential_id).await.unwrap();
    assert!(f.auth.device_poll(&start.device_code).await.is_err());
}

#[tokio::test]
async fn setup_link_is_private_stable_and_never_reopens() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new().await;
    let path = f.dir.path().join("setup-link");
    f.auth.prepare_setup(&path).await.unwrap();
    let link = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    f.auth.prepare_setup(&path).await.unwrap();
    assert_eq!(link, std::fs::read_to_string(&path).unwrap());
    f.account("alice", "token", None).await;
    f.auth.prepare_setup(&path).await.unwrap();
    assert!(!path.exists());
    sqlx::query("DELETE FROM auth_users")
        .execute(&f.auth.0.pool)
        .await
        .unwrap();
    f.auth.prepare_setup(&path).await.unwrap();
    assert!(!path.exists());
}

#[tokio::test]
async fn challenges_require_the_original_browser_and_expire() {
    let f = Fixture::new().await;
    let ceremony = f.auth.login_start("browser-one").await.unwrap();
    let input = piqueld_core::auth::CeremonyFinish {
        id: ceremony.id.clone(),
        credential: serde_json::json!({}),
    };
    assert!(matches!(
        f.auth.login_finish(input.clone(), "browser-two").await,
        Err(AuthError::Unauthorized)
    ));
    assert!(f.auth.0.ceremonies.lock().await.contains_key(&ceremony.id));
    assert!(
        f.auth
            .login_finish(input.clone(), "browser-one")
            .await
            .is_err()
    );
    assert!(!f.auth.0.ceremonies.lock().await.contains_key(&ceremony.id));
    assert!(matches!(
        f.auth.login_finish(input, "browser-one").await,
        Err(AuthError::Unauthorized)
    ));
    let ceremony = f.auth.login_start("browser").await.unwrap();
    f.auth
        .0
        .ceremonies
        .lock()
        .await
        .get_mut(&ceremony.id)
        .unwrap()
        .expires = 0;
    assert!(matches!(
        f.auth
            .login_finish(
                piqueld_core::auth::CeremonyFinish {
                    id: ceremony.id,
                    credential: serde_json::json!({})
                },
                "browser"
            )
            .await,
        Err(AuthError::Unauthorized)
    ));
}

#[tokio::test]
async fn middleware_protects_api_and_enforces_cookie_csrf_without_ownership_checks() {
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        routing::get,
    };
    use tower::ServiceExt;
    let f = Fixture::new().await;
    let (_, token) = f.account("alice", "token", None).await;
    let router = crate::api::http::protect(
        Router::new()
            .route(
                "/api/v1/private",
                get(|| async { "ok" }).post(|| async { "ok" }),
            )
            .route("/health", get(|| async { "ok" })),
        f.auth.clone(),
    );
    for (path, method, credential, origin, expected) in [
        ("/health", "GET", None, None, StatusCode::OK),
        (
            "/api/v1/private",
            "GET",
            None,
            None,
            StatusCode::UNAUTHORIZED,
        ),
        (
            "/api/v1/private",
            "GET",
            Some(("authorization", format!("Bearer {token}"))),
            None,
            StatusCode::OK,
        ),
        (
            "/api/v1/private",
            "POST",
            Some(("cookie", format!("piqueld_session={token}"))),
            None,
            StatusCode::FORBIDDEN,
        ),
        (
            "/api/v1/private",
            "POST",
            Some(("cookie", format!("piqueld_session={token}"))),
            Some("https://evil.example"),
            StatusCode::FORBIDDEN,
        ),
        (
            "/api/v1/private",
            "POST",
            Some(("cookie", format!("piqueld_session={token}"))),
            Some("http://localhost:7845"),
            StatusCode::OK,
        ),
        (
            "/api/v1/private",
            "POST",
            Some(("authorization", format!("Bearer {token}"))),
            None,
            StatusCode::OK,
        ),
    ] {
        let mut request = Request::builder().method(method).uri(path);
        if let Some((name, value)) = credential {
            request = request.header(name, value);
        }
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
}

#[test]
fn webauthn_origins_and_cookie_flags_are_explicit() {
    for origin in ["https://piqueld.example", "http://localhost:7845"] {
        assert!(Auth::validate_origin(origin).is_ok());
    }
    for origin in [
        "http://piqueld.example",
        "https://127.0.0.1",
        "https://user:secret@example.com",
        "https://example.com/path",
        "https://example.com?query",
    ] {
        assert!(Auth::validate_origin(origin).is_err());
    }
}
