use super::*;
use crate::api::http::Authenticator as _;
use crate::store::{Lockout, NewPasskey, PasskeyOwner, StoreError};
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
    async fn account(
        &self,
        name: &str,
        kind: CredentialKind,
        expires: Option<i64>,
    ) -> (String, String) {
        let user = User {
            id: Auth::id(),
            username: name.into(),
            display_name: String::new(),
        };
        self.auth.0.store.seed_auth_user(&user).await;
        let (token, credential) = Auth::new_credential(kind, "Test", expires).unwrap();
        self.auth
            .0
            .store
            .insert_credential(&user.id, &credential)
            .await
            .unwrap();
        (user.id, token)
    }

    // Management tests only need a stored key; no WebAuthn ceremony reads it.
    async fn passkey(&self, user_id: &str) -> String {
        let id = Auth::id();
        let passkey = NewPasskey {
            id: &id,
            name: "Test key",
            credential: "{}",
        };
        let store = &self.auth.0.store;
        assert!(
            store
                .add_passkey(PasskeyOwner::Existing(user_id), passkey)
                .await
                .unwrap()
        );
        id
    }
}

#[tokio::test]
async fn sessions_expire_revoke_and_survive_restart_without_storing_secrets() {
    let f = Fixture::new().await;
    let (id, token) = f
        .account("alice", CredentialKind::Browser, Some(now_secs() + 7 * DAY))
        .await;
    let identity = f.auth.authenticate(&token).await.unwrap();
    assert_eq!(identity.user.id, id);
    // Only the hash is stored, so the secret itself finds nothing.
    assert!(
        f.auth
            .0
            .store
            .credential_owner(&token)
            .await
            .unwrap()
            .is_none()
    );
    let store = crate::store::Store::open(f.dir.path().join("state.db"))
        .await
        .unwrap();
    let restarted = Auth::new(&store, "http://localhost:7845").unwrap();
    restarted.authenticate(&token).await.unwrap();
    f.auth.0.store.age_auth_credentials(DAY).await;
    assert!(matches!(
        f.auth.authenticate(&token).await,
        Err(AuthError::Unauthorized)
    ));
    let (_, cli) = f
        .account("bob", CredentialKind::Cli, Some(now_secs() - 1))
        .await;
    assert!(matches!(
        f.auth.authenticate(&cli).await,
        Err(AuthError::Unauthorized)
    ));
    let (other, api) = f.account("carol", CredentialKind::Token, None).await;
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
    let (alice, _) = f.account("alice", CredentialKind::Token, None).await;
    let (bob, bob_token) = f.account("bob", CredentialKind::Token, None).await;
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
    f.passkey(&alice).await;
    f.passkey(&bob).await;
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
    let (alice, token) = f.account("alice", CredentialKind::Token, None).await;
    let (bob, _) = f.account("bob", CredentialKind::Token, None).await;
    f.passkey(&bob).await;
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
async fn last_passkey_removal_and_owner_deletion_preserve_access() {
    let f = Fixture::new().await;
    let (alice, token) = f.account("alice", CredentialKind::Token, None).await;
    let (bob, _) = f.account("bob", CredentialKind::Token, None).await;
    let key = f.passkey(&alice).await;
    for command in [
        Manage::RemovePasskey { id: key.clone() },
        Manage::DeleteUser {
            user_id: alice.clone(),
        },
    ] {
        assert!(matches!(
            f.auth.manage(&bob, command).await,
            Err(AuthError::Store(StoreError::Lockout(Lockout::LastPasskey)))
        ));
        let directory = f.auth.directory().await.unwrap();
        assert_eq!(directory.users.len(), 2);
        assert_eq!(directory.passkeys.len(), 1);
        assert_eq!(directory.passkeys[0].id, key);
        f.auth.authenticate(&token).await.unwrap();
    }
    // An account may have no passkeys, provided another account retains one.
    f.auth
        .manage(&alice, Manage::DeleteUser { user_id: bob })
        .await
        .unwrap();
    assert_eq!(f.auth.directory().await.unwrap().users.len(), 1);
}

#[tokio::test]
async fn concurrent_passkey_and_account_deletions_keep_one_passkey() {
    for delete_account in [false, true] {
        let f = Fixture::new().await;
        let (alice, _) = f.account("alice", CredentialKind::Token, None).await;
        let (bob, _) = f.account("bob", CredentialKind::Token, None).await;
        let alice_key = f.passkey(&alice).await;
        let bob_key = f.passkey(&bob).await;
        let second = if delete_account {
            Manage::DeleteUser { user_id: bob }
        } else {
            Manage::RemovePasskey { id: bob_key }
        };
        let (first, second) = tokio::join!(
            f.auth
                .manage(&alice, Manage::RemovePasskey { id: alice_key }),
            f.auth.manage(&alice, second),
        );
        let error = match (first, second) {
            (Ok(_), Err(error)) | (Err(error), Ok(_)) => error,
            results => panic!("exactly one deletion must succeed: {results:?}"),
        };
        assert!(matches!(
            error,
            AuthError::Store(StoreError::Lockout(Lockout::LastPasskey))
        ));
        assert_eq!(f.auth.directory().await.unwrap().passkeys.len(), 1);
    }
}

#[tokio::test]
async fn device_approval_is_explicit_single_use_and_bound_to_a_live_session() {
    let f = Fixture::new().await;
    let (_, token) = f
        .account("alice", CredentialKind::Browser, Some(now_secs() + DAY))
        .await;
    let identity = f.auth.authenticate(&token).await.unwrap();
    let start = f.auth.device_start(None).await.unwrap();
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
    let start = f.auth.device_start(None).await.unwrap();
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
    assert_eq!(f.auth.setup_link().await.unwrap().url, link.trim());
    f.account("alice", CredentialKind::Token, None).await;
    assert!(matches!(
        f.auth.setup_link().await,
        Err(AuthError::SetupCompleted)
    ));
    f.auth.prepare_setup(&path).await.unwrap();
    assert!(!path.exists());
    f.auth.0.store.clear_auth_users().await;
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
    let (_, token) = f.account("alice", CredentialKind::Token, None).await;
    let router = f.auth.clone().guard(
        Router::new()
            .route(
                "/api/v1/private",
                get(|| async { "ok" }).post(|| async { "ok" }),
            )
            .route("/health", get(|| async { "ok" })),
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
            Some(("cookie", format!("piqueld_session_7845={token}"))),
            None,
            StatusCode::FORBIDDEN,
        ),
        (
            "/api/v1/private",
            "POST",
            Some(("cookie", format!("piqueld_session_7845={token}"))),
            Some("https://evil.example"),
            StatusCode::FORBIDDEN,
        ),
        (
            "/api/v1/private",
            "POST",
            Some(("cookie", format!("piqueld_session_7845={token}"))),
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
        // Rejections are marked too, so no API response is ever cached.
        let cache_control = response
            .headers()
            .get("cache-control")
            .map(|value| value.to_str().unwrap());
        let expected_cache_control = path.starts_with("/api/").then_some("no-store");
        assert_eq!(cache_control, expected_cache_control, "{method} {path}");
    }
    // Browsers send cookies on cross-site WebSocket handshakes, which use GET.
    for (origin, expected) in [
        ("https://evil.example", StatusCode::FORBIDDEN),
        ("http://localhost:7845", StatusCode::OK),
    ] {
        let request = Request::get("/api/v1/private")
            .header("cookie", format!("piqueld_session_7845={token}"))
            .header("origin", origin)
            .header("upgrade", "websocket")
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), expected, "{origin}");
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

#[tokio::test]
async fn login_start_limits_share_listeners_ignore_forwarded_ips_and_leave_sessions_usable() {
    use axum::{
        Router,
        body::Body,
        extract::ConnectInfo,
        http::{Request, StatusCode},
        routing::{get, post},
    };
    use tower::ServiceExt;
    let f = Fixture::new().await;
    let (_, token) = f.account("alice", CredentialKind::Token, None).await;
    let router = f.auth.clone().guard(
        Router::new()
            .route("/api/v1/auth/login/start", post(|| async { "ok" }))
            .route("/api/v1/auth/device/start", post(|| async { "ok" }))
            .route("/api/v1/private", get(|| async { "ok" })),
    );
    for attempt in 0..31 {
        let request = Request::builder()
            .method("POST")
            .uri(if attempt % 2 == 0 {
                "/api/v1/auth/login/start"
            } else {
                "/api/v1/auth/device/start"
            })
            .header("origin", f.auth.origin())
            .header("x-forwarded-for", format!("192.0.2.{attempt}"))
            .extension(ConnectInfo(
                "192.0.2.1:1234".parse::<std::net::SocketAddr>().unwrap(),
            ))
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            if attempt == 30 {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::OK
            }
        );
        if attempt == 30 {
            assert_eq!(response.headers()["retry-after"], "60");
        }
    }
    // Another router/listener must use the same budget.
    let other = f
        .auth
        .guard(Router::new().route("/api/v1/auth/device/start", post(|| async { "ok" })));
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/auth/device/start")
        .extension(ConnectInfo(
            "192.0.2.1:5678".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        other.oneshot(request).await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    let request = Request::builder()
        .uri("/api/v1/private")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router.oneshot(request).await.unwrap().status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn https_cookies_use_host_prefix_and_ignore_unprefixed_names() {
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        routing::get,
    };
    use tower::ServiceExt;
    let f = Fixture::new().await;
    let (_, token) = f
        .account("alice", CredentialKind::Browser, Some(now_secs() + DAY))
        .await;
    let auth = Auth::new(&f.auth.0.store, "https://piqueld.example").unwrap();
    assert_eq!(
        auth.cookie("piqueld_session", "secret", 60),
        "__Host-piqueld_session=secret; Path=/; HttpOnly; SameSite=Strict; Max-Age=60; Secure"
    );
    assert_eq!(
        f.auth.cookie("piqueld_session", "secret", 60),
        "piqueld_session_7845=secret; Path=/; HttpOnly; SameSite=Strict; Max-Age=60"
    );
    assert_eq!(
        Auth::new(&f.auth.0.store, "https://piqueld.example:8443")
            .unwrap()
            .cookie_name("piqueld_session"),
        "__Host-piqueld_session_8443"
    );
    let router = auth.guard(Router::new().route("/api/v1/private", get(|| async { "ok" })));
    for (cookie, expected) in [
        (format!("piqueld_session={token}"), StatusCode::UNAUTHORIZED),
        (format!("__Host-piqueld_session={token}"), StatusCode::OK),
    ] {
        let request = Request::builder()
            .uri("/api/v1/private")
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            router.clone().oneshot(request).await.unwrap().status(),
            expected
        );
    }
}

#[tokio::test]
async fn device_inspection_reports_the_requester_until_approval() {
    let f = Fixture::new().await;
    let (_, token) = f
        .account("alice", CredentialKind::Browser, Some(now_secs() + DAY))
        .await;
    let identity = f.auth.authenticate(&token).await.unwrap();
    let peer = "192.0.2.7".parse().unwrap();
    let start = f.auth.device_start(Some(peer)).await.unwrap();
    assert_eq!(start.requester.as_deref(), Some("192.0.2.7"));
    let request = f
        .auth
        .device_inspect(&format!(" {} ", start.user_code.to_lowercase()))
        .await
        .unwrap();
    assert_eq!(request.user_code, start.user_code);
    assert_eq!(request.requester.as_deref(), Some("192.0.2.7"));
    assert!(request.age <= 1 && (599..=600).contains(&request.expires_in));
    // Inspection does not approve anything.
    assert_eq!(
        f.auth.device_poll(&start.device_code).await.unwrap().status,
        "authorization_pending"
    );
    assert!(f.auth.device_inspect("AAAA-AAAA").await.is_err());
    f.auth
        .device_approve(&start.user_code, &identity)
        .await
        .unwrap();
    assert!(f.auth.device_inspect(&start.user_code).await.is_err());
    let local = f.auth.device_start(None).await.unwrap();
    assert!(local.requester.is_none());
    assert!(
        f.auth
            .device_inspect(&local.user_code)
            .await
            .unwrap()
            .requester
            .is_none()
    );
}
