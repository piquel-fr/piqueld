//! What a caller may do: grants, the audit trail and the permissions guarding each feature.

use super::*;

/// Hidden applications and their environments look absent; visible ones name
/// the missing permission. Environments are checked on their application.
/// Accounts without application grants list nothing rather than being refused.
#[tokio::test]
async fn grants_hide_applications_and_name_missing_permissions() {
    let f = GrantFixture::new().await;
    let none = serde_json::Value::Null;
    for uri in ["/api/v1/applications", "/api/v1/builds"] {
        let (status, body) = f.router.call(&f.lead, "GET", uri, none.clone()).await;
        assert_eq!(status, 200, "{uri}");
        assert_eq!(body["data"]["items"], serde_json::json!([]), "{uri}");
    }
    let (status, body) = f
        .router
        .call(&f.deployer, "GET", "/api/v1/applications", none.clone())
        .await;
    assert_eq!(status, 200);
    let listed: Vec<_> = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert_eq!(listed, [f.blog.as_str()]);
    for uri in [
        format!("/api/v1/applications/{}", f.shop),
        format!("/api/v1/environments/{}", f.shop),
        format!("/api/v1/environments/{}/exec", f.shop),
        "/api/v1/environments/env-unknown".to_owned(),
    ] {
        assert_eq!(
            f.router.status(&f.deployer, "GET", &uri).await,
            404,
            "{uri}"
        );
    }
    let deploy = format!("/api/v1/environments/{}/deploy?force=true", f.blog);
    assert_eq!(f.router.status(&f.deployer, "POST", &deploy).await, 202);
    let staging = serde_json::json!({"name": "staging"});
    for (uri, method, body, permission) in [
        (
            format!("/api/v1/applications/{}?force=true", f.blog),
            "DELETE",
            none.clone(),
            "apps:delete",
        ),
        (
            format!("/api/v1/environments/{}?force=true", f.blog),
            "DELETE",
            none.clone(),
            "apps:delete",
        ),
        (
            format!("/api/v1/applications/{}/environments?force=true", f.blog),
            "POST",
            staging,
            "apps:write",
        ),
        (
            format!("/api/v1/environments/{}/exec", f.blog),
            "GET",
            none.clone(),
            "apps:exec",
        ),
        (
            "/api/v1/system/configuration".to_owned(),
            "GET",
            none.clone(),
            "system:read",
        ),
        (
            "/api/v1/system/checks".to_owned(),
            "GET",
            none.clone(),
            "system:read",
        ),
    ] {
        let (status, body) = f.router.call(&f.deployer, method, &uri, body).await;
        assert_eq!(status, 403, "{method} {uri}");
        assert_eq!(body["code"], "permission_denied");
        assert_eq!(body["details"]["permission"], permission);
    }
}

/// Refusals (including unauthenticated ones), writes, and sensitive reads
/// enter the audit trail with their caller and outcome, routine reads do not,
/// and only `audit:read` reveals other accounts' trails.
#[tokio::test]
async fn audit_trail_records_callers_and_outcomes() {
    let f = GrantFixture::new().await;
    let none = serde_json::Value::Null;
    let blog = format!("/api/v1/applications/{}", f.blog);
    let shop = format!("/api/v1/applications/{}", f.shop);
    let staging = f.environment(&f.blog, "staging").await;
    for (method, uri, status) in [
        ("DELETE", format!("{blog}?force=true"), 403),
        ("GET", shop, 404),
        ("POST", format!("{staging}/deploy?force=true"), 202),
        ("GET", format!("{staging}/exec"), 403),
        ("GET", format!("{blog}/manifest"), 200),
        ("GET", "/api/v1/applications".to_owned(), 200),
    ] {
        assert_eq!(f.router.status(&f.deployer, method, &uri).await, status);
    }
    let body = f.audit(&f.deployer, "/api/v1/audit", 5).await;
    // Records are written in the background, so their order is not asserted.
    // Requests about an environment also name its application.
    let trail: std::collections::BTreeSet<_> = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| {
            assert_eq!(event["user_id"], "deployer");
            assert_eq!(event["credential_kind"], "token");
            if !event["environment_id"].is_null() {
                assert_eq!(event["application_id"], f.blog.as_str());
            }
            (
                event["outcome"].as_str().unwrap().to_owned(),
                event["action"].as_str().unwrap().to_owned(),
                event["permission"].as_str().map(str::to_owned),
            )
        })
        .collect();
    let (id, environment) = ("/api/v1/applications/{id}", "/api/v1/environments/{id}");
    assert_eq!(
        trail,
        [
            ("allowed".into(), format!("POST {environment}/deploy"), None),
            (
                "denied".into(),
                format!("GET {environment}/exec"),
                Some("apps:exec".into())
            ),
            ("allowed".into(), format!("GET {id}/manifest"), None),
            ("denied".into(), format!("GET {id}"), None),
            (
                "denied".into(),
                format!("DELETE {id}"),
                Some("apps:delete".into())
            ),
        ]
        .into()
    );
    let others = "/api/v1/audit?user_id=creator";
    assert_eq!(f.router.status(&f.deployer, "GET", others).await, 403);
    // A cross-site write is refused, yet still names its valid caller.
    let foreign = Request::post("/api/v1/applications")
        .header("authorization", format!("Bearer {}", f.deployer))
        .header("origin", "https://attacker.example")
        .body(Body::empty())
        .unwrap();
    assert_eq!(f.router.send(foreign).await.status, 403);
    // So is a throttled one, once the peer's sign-in start allowance is spent.
    let mut status = 0;
    for _ in 0..31 {
        let start = serde_json::json!({});
        status = f
            .router
            .call(&f.deployer, "POST", "/api/v1/auth/device/start", start)
            .await
            .0;
    }
    assert_eq!(status, 429);
    let unknown = "pqd_0000000000000000000000000000000000000000000";
    let (status, _) = f
        .router
        .call(unknown, "GET", "/api/v1/applications", none)
        .await;
    assert_eq!(status, 401);
    let body = f.audit(&f.auditor, "/api/v1/audit?outcome=denied", 7).await;
    let anonymous: Vec<_> = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["user_id"].is_null())
        .map(|event| event["status"].as_u64().unwrap())
        .collect();
    assert_eq!(anonymous, [401]);
    let deployer = "/api/v1/audit?user_id=deployer&outcome=denied";
    let body = f.audit(&f.auditor, deployer, 6).await;
    assert_eq!(body["data"]["items"].as_array().unwrap().len(), 6);
}

/// Callers read their own trail, so it reveals nothing they could not see:
/// a hidden environment's application is not recorded, and an API token
/// reads only its own requests, not everything its account did.
#[tokio::test]
async fn own_audit_trails_reveal_nothing_hidden() {
    let f = GrantFixture::new().await;
    let hidden = format!("/api/v1/environments/{}", f.shop);
    assert_eq!(f.router.status(&f.deployer, "GET", &hidden).await, 404);
    // Refusals before authorization, like a cross-site request, are no hint.
    let foreign = Request::delete(&hidden)
        .header("authorization", format!("Bearer {}", f.deployer))
        .header("origin", "https://attacker.example")
        .body(Body::empty())
        .unwrap();
    assert_eq!(f.router.send(foreign).await.status, 403);
    let body = f.audit(&f.deployer, "/api/v1/audit", 2).await;
    for event in body["data"]["items"].as_array().unwrap() {
        assert_eq!(event["environment_id"], f.shop.as_str());
        assert!(event["application_id"].is_null(), "{event}");
    }
    // Usernames match regardless of case, like accounts' own.
    f.audit(&f.auditor, "/api/v1/audit?username=DEPLOYER", 2)
        .await;
    let create = serde_json::json!({
        "action": "create_token",
        "grants": [{"permission": "apps:deploy", "applications": [f.blog]}],
        "name": "ci",
        "days": 1
    });
    let (status, created) = f
        .router
        .call(&f.deployer, "POST", "/api/v1/auth/manage", create)
        .await;
    assert_eq!(status, 200, "{created}");
    // The trail records what an account action changed.
    let body = f.audit(&f.deployer, "/api/v1/audit", 3).await;
    let deploy = format!("apps:deploy on {}", f.blog);
    let recorded = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| {
            let action = event["action"].as_str().unwrap();
            action.starts_with("POST /api/v1/auth/manage create_token \"ci\" (")
                && action.contains(&deploy)
        });
    assert!(recorded, "{body}");
    let token = created["token"].as_str().unwrap();
    let manifest = format!("/api/v1/applications/{}/manifest", f.blog);
    assert_eq!(f.router.status(token, "GET", &manifest).await, 200);
    let body = f.audit(token, "/api/v1/audit", 1).await;
    let items = body["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{body}");
    assert_eq!(items[0]["action"], "GET /api/v1/applications/{id}/manifest");
    let account = "/api/v1/audit?credential_id=deployer";
    assert_eq!(f.router.status(token, "GET", account).await, 403);
}

/// The trail's hash chain verifies intact, and checking it needs `audit:read`.
#[tokio::test]
async fn verifying_the_audit_chain_needs_audit_read() {
    let f = GrantFixture::new().await;
    let none = serde_json::Value::Null;
    let deploy = format!("/api/v1/environments/{}/deploy?force=true", f.blog);
    assert_eq!(f.router.status(&f.deployer, "POST", &deploy).await, 202);
    f.audit(&f.auditor, "/api/v1/audit", 1).await;
    let verify = "/api/v1/audit/verify";
    let (status, body) = f.router.call(&f.auditor, "GET", verify, none.clone()).await;
    assert_eq!(
        (status, &body["data"]["broken_at"]),
        (200, &serde_json::Value::Null)
    );
    assert_eq!(body["data"]["checked"], 1);
    assert_eq!(f.router.status(&f.deployer, "GET", verify).await, 403);
}

/// Creating requires `apps:create` and grants the creator matching access;
/// saving an unreadable application by name is refused rather than absent.
#[tokio::test]
async fn creators_receive_matching_grants_on_new_applications() {
    let f = GrantFixture::new().await;
    let name = |value: &str| serde_json::json!({"value": value});
    let (status, body) = f
        .router
        .call(&f.creator, "POST", "/api/v1/applications", name("notes"))
        .await;
    assert_eq!(status, 200, "{body}");
    let created = format!(
        "/api/v1/applications/{}",
        body["data"]["application_id"].as_str().unwrap()
    );
    assert_eq!(f.router.status(&f.creator, "GET", &created).await, 200);
    let mut input = manifest();
    input.metadata.name = "shop".into();
    let apply = serde_json::json!({"manifest": input});
    let (status, body) = f
        .router
        .call(
            &f.creator,
            "POST",
            "/api/v1/applications/apply?force=true",
            apply,
        )
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["details"]["permission"], "apps:write");
    let (status, _) = f
        .router
        .call(&f.deployer, "POST", "/api/v1/applications", name("other"))
        .await;
    assert_eq!(status, 403);
}

/// Granting telemetry takes `telemetry:grant`, which `apps:write` does not
/// include, and reading the application, which stays hidden otherwise.
#[tokio::test]
async fn telemetry_grants_need_the_telemetry_grant_permission() {
    use piqueld::api::{Actor, ApplicationError, Mutation, MutationResponse};
    use piqueld::store::{Caller, StoreError};
    use piqueld_core::access::Denied;
    let temp = tempfile::tempdir().unwrap();
    let state = state(&temp).await;
    let database = database(&temp);
    let saved = save(&state, manifest()).await;
    let id = piqueld_core::EnvironmentId::parse(saved.application_id).unwrap();
    let account = |name| {
        Actor::Account(Caller {
            credential_id: name,
            user_id: name,
        })
    };
    seed_account(&database, "writer", &[("apps:write", None)]).await;
    seed_account(&database, "blind", &[("telemetry:grant", None)]).await;
    let grants = [("telemetry:grant", None), ("apps:read", None)];
    seed_account(&database, "granter", &grants).await;
    let grant = |actor| {
        let mutation = Mutation::SetTelemetry {
            id: id.clone(),
            granted: true,
        };
        state.accept(actor, mutation, None, false, None)
    };
    assert!(matches!(
        grant(account("writer")).await,
        Err(ApplicationError::Store(StoreError::Denied(
            Denied::Missing(_)
        )))
    ));
    assert!(matches!(
        grant(account("blind")).await,
        Err(ApplicationError::Store(StoreError::Denied(Denied::Hidden)))
    ));
    let Ok(MutationResponse::Environment(environment)) = grant(account("granter")).await else {
        panic!("granted");
    };
    assert!(environment.telemetry);
}

/// Destinations need `notifications:manage` to change, and it or
/// `system:read` to list; they require HTTPS URLs and never return them.
#[tokio::test]
async fn notification_destinations_need_their_permissions_and_hide_urls() {
    let f = GrantFixture::new().await;
    let database = database(&f.temp);
    let manager = [("notifications:manage", None)];
    let manager = seed_account(&database, "notifier", &manager).await;
    let reader = seed_account(&database, "reader", &[("system:read", None)]).await;
    let uri = "/api/v1/notifications/destinations/ops";
    let request = |url: &str| {
        serde_json::json!({
            "settings": {
                "kind": "discord",
                "enabled": true,
                "categories": {"deployment_failures": {"message": "<@&1234>"}},
                "applications": {"only": [f.blog]},
                "previews": false,
            },
            "url": url,
        })
    };
    let url = "https://discord.example/api/webhooks/private-token";
    assert_eq!(
        f.router.call(&reader, "PUT", uri, request(url)).await.0,
        403
    );
    let (status, error) = f
        .router
        .call(&manager, "PUT", uri, request("http://discord.example/hook"))
        .await;
    assert_eq!(
        (status, &error["code"]),
        (400, &"destination_url_invalid".into())
    );
    assert_eq!(
        f.router.call(&manager, "PUT", uri, request(url)).await.0,
        200
    );
    let list = "/api/v1/notifications/destinations";
    let (status, listed) = f
        .router
        .call(&reader, "GET", list, serde_json::Value::Null)
        .await;
    assert_eq!(status, 200);
    let settings = &listed["data"][0]["settings"];
    assert_eq!(settings["applications"]["only"][0], f.blog.as_str());
    assert_eq!(
        settings["categories"]["deployment_failures"]["message"],
        "<@&1234>"
    );
    assert!(!listed.to_string().contains("private-token"), "{listed}");
    // Managers list destinations to change them, without `system:read`.
    let (status, _) = f
        .router
        .call(&manager, "GET", list, serde_json::Value::Null)
        .await;
    assert_eq!(status, 200);
    assert_eq!(f.router.status(&manager, "DELETE", uri).await, 200);
    assert_eq!(f.router.status(&manager, "DELETE", uri).await, 404);
}

/// Deliveries are listed with `system:read` and retried with
/// `system:operate`; a delivery that has not failed is refused with 409,
/// naming why.
#[tokio::test]
async fn delivery_retries_need_system_operate_and_name_refusals() {
    let f = GrantFixture::new().await;
    let database = database(&f.temp);
    let reader = seed_account(&database, "reader", &[("system:read", None)]).await;
    let operator = seed_account(&database, "operator", &[("system:operate", None)]).await;
    let mut connection = sqlite::connect(&database).await;
    sqlx::query(
        "INSERT INTO notification_deliveries(id,event_id,destination,category,state,
            created_at_ms,retry_started_at_ms,next_attempt_ms,updated_at_ms)
        SELECT 'delivery-01',MIN(id),'ops','deployment_failures','pending',1,1,1,1 FROM events",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    let (status, listed) = f
        .router
        .call(
            &reader,
            "GET",
            "/api/v1/notifications/deliveries?limit=1",
            serde_json::Value::Null,
        )
        .await;
    assert_eq!(
        (status, &listed["data"]["items"][0]["id"]),
        (200, &"delivery-01".into())
    );
    let retry = "/api/v1/notifications/deliveries/delivery-01/retry";
    assert_eq!(f.router.status(&reader, "POST", retry).await, 403);
    let (status, error) = f
        .router
        .call(&operator, "POST", retry, serde_json::Value::Null)
        .await;
    assert_eq!(
        (status, &error["code"]),
        (409, &"delivery_not_retryable".into())
    );
    assert!(
        error["message"].as_str().unwrap().contains("pending"),
        "{error}"
    );
    let missing = "/api/v1/notifications/deliveries/missing/retry";
    assert_eq!(f.router.status(&operator, "POST", missing).await, 404);
}
