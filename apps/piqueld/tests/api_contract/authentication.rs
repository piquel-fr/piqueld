//! Who a caller is: tokens, the host operator over the Unix socket, tailnet-bound tokens and device sign-ins.

use super::*;

/// Every documented operation except the public sign-in endpoints must reject
/// anonymous callers on both the website and Unix-socket routers.
#[tokio::test]
async fn every_documented_operation_requires_authentication() {
    const PUBLIC: &[&str] = &[
        "/api/v1/auth/status",
        "/api/v1/auth/setup-link",
        "/api/v1/auth/recovery",
        "/api/v1/auth/sign-in-link",
        "/api/v1/auth/login/operator",
        "/api/v1/auth/register/start",
        "/api/v1/auth/register/finish",
        "/api/v1/auth/login/start",
        "/api/v1/auth/login/finish",
        "/api/v1/auth/device/start",
        "/api/v1/auth/device/poll",
    ];
    let temp = TempDir::new().unwrap();
    let store = Arc::new(Store::open(database(&temp)).await.unwrap());
    let auth = piqueld::auth::Auth::new(&store, "https://piqueld.example").unwrap();
    let runtime = Arc::new(FakeRuntime::new(&store));
    let state = ApiState::new(store, runtime).unwrap();
    let document = piqueld::api::http::openapi_document();
    let paths = document["paths"].as_object().unwrap();
    let mut checked = 0;
    for listener in [
        web_router(state.clone(), UiAssets::Embedded(TEST_BUNDLE), auth.clone()),
        api_router(state.clone(), auth.clone()),
    ] {
        for (path, item) in paths {
            if PUBLIC.contains(&path.as_str()) {
                continue;
            }
            let uri = path
                .split('/')
                .map(|segment| {
                    if segment.starts_with('{') {
                        "placeholder"
                    } else {
                        segment
                    }
                })
                .collect::<Vec<_>>()
                .join("/");
            for method in ["get", "post", "put", "patch", "delete"] {
                if item.get(method).is_none() {
                    continue;
                }
                let method = method.to_ascii_uppercase();
                let response = listener
                    .send(
                        Request::builder()
                            .method(method.as_str())
                            .uri(&uri)
                            .header("host", "localhost")
                            .header("content-type", "application/json")
                            .header("x-request-id", "anonymous-contract")
                            .body(Body::from("{}"))
                            .unwrap(),
                    )
                    .await;
                assert_eq!(
                    response.status,
                    StatusCode::UNAUTHORIZED,
                    "{method} {path} must require authentication"
                );
                // The daemon replaces a client-supplied request ID.
                let request_id = response.header("x-request-id").unwrap();
                assert_ne!(request_id, "anonymous-contract");
                let error: piqueld_core::api::ErrorBody =
                    serde_json::from_slice(&response.body).unwrap();
                assert_eq!(error.request_id, request_id);
                checked += 1;
            }
        }
    }
    assert!(checked > 60, "only {checked} operations were checked");
}

/// A CLI login's approval and the poll that signs it in are audited for the
/// approving account; polls still awaiting approval are not.
#[tokio::test]
async fn device_sign_ins_are_audited() {
    use serde_json::json;
    let f = GrantFixture::new().await;
    let start = "/api/v1/auth/device/start";
    let poll = "/api/v1/auth/device/poll";
    let mut codes = Vec::new();
    for _ in 0..2 {
        let (status, body) = f.router.call(&f.deployer, "POST", start, json!({})).await;
        assert_eq!(status, 200, "{body}");
        codes.push((body["device_code"].clone(), body["user_code"].clone()));
    }
    let [(pending, _), (approved, user_code)] = <[_; 2]>::try_from(codes).unwrap();
    let (_, body) = f
        .router
        .call(&f.deployer, "POST", poll, json!({"device_code": pending}))
        .await;
    assert_eq!(body["status"], "authorization_pending");
    let approve = json!({"user_code": user_code});
    let (status, _) = f
        .router
        .call(&f.deployer, "POST", "/api/v1/auth/device/approve", approve)
        .await;
    assert_eq!(status, 200);
    let (_, body) = f
        .router
        .call(&f.deployer, "POST", poll, json!({"device_code": approved}))
        .await;
    assert_eq!(body["status"], "complete", "{body}");
    // Two starts, the approval, and the completing poll.
    let body = f.audit(&f.auditor, "/api/v1/audit", 4).await;
    let polls: Vec<_> = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["action"] == format!("POST {poll}"))
        .map(|event| (event["outcome"].clone(), event["user_id"].clone()))
        .collect();
    assert_eq!(polls, [(json!("allowed"), json!("deployer"))]);
}

/// Before the first account exists, only the Unix socket reveals the setup
/// link; remote requests for it are audited as refusals.
#[tokio::test]
async fn setup_link_is_served_only_over_the_unix_socket() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp).await;
    let auth = auth(&temp).await;
    let path = temp.path().join("setup-link");
    auth.prepare_setup(&path).await.unwrap();
    let link = std::fs::read_to_string(&path).unwrap();
    for (listener, status) in [
        (
            web_router(state.clone(), UiAssets::Embedded(TEST_BUNDLE), auth.clone()),
            404,
        ),
        (api_router(state.clone(), auth.clone()), 200),
    ] {
        let response = listener.send(request("/api/v1/auth/setup-link")).await;
        assert_eq!(response.status, status);
        if status == 200 {
            let setup: piqueld_core::auth::SetupLink =
                serde_json::from_slice(&response.body).unwrap();
            assert_eq!(setup.url, link.trim());
        }
    }
    // The refused remote request is audited in the background.
    let event = refusals(&state, |events| match events.as_slice() {
        [event] => Some(event.clone()),
        _ => None,
    })
    .await
    .expect("the refused setup link request was not audited");
    assert_eq!(
        (event.action.as_str(), event.status),
        ("GET /api/v1/auth/setup-link", 404)
    );
}

/// Waits for the refusals audited in the background until `check` finds
/// what it expects in them.
async fn refusals<T>(
    state: &ApiState,
    check: impl Fn(Vec<piqueld_core::audit::AuditEvent>) -> Option<T>,
) -> Option<T> {
    let denied = piqueld_core::audit::AuditFilter {
        outcome: Some(piqueld_core::audit::AuditOutcome::Denied),
        ..Default::default()
    };
    // Allow for a loaded machine.
    eventually(
        Duration::from_secs(10),
        Duration::from_millis(20),
        async || check(state.audit_events(&denied, None, 10).await.unwrap().items),
    )
    .await
}

/// Sends `request` to `router` as if from a Unix socket peer running as
/// `uid` (TCP routers ignore it), with an optional JSON `body`. Returns the
/// status, the JSON response body, and any `Set-Cookie` value.
async fn as_peer(
    router: &axum::Router,
    request: axum::http::request::Builder,
    uid: Option<u32>,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value, Option<String>) {
    use axum::http::header;
    let request = request.header(header::HOST, "localhost");
    let mut request = match body {
        Some(body) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .unwrap();
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(piqueld::api::http::UnixPeer {
            uid,
        }));
    let response = router.send(request).await;
    let cookie = response.header(header::SET_COOKIE).map(str::to_owned);
    (response.status.as_u16(), response.json(), cookie)
}

/// The daemon's own user, the host operator, and another user, a stranger.
fn unix_peers() -> (u32, u32) {
    let operator = rustix::process::geteuid().as_raw();
    (operator, if operator == 0 { 1 } else { operator + 1 })
}

/// Admin recovery is served only over the Unix socket, only to root or the
/// daemon's own user, and only once setup has completed.
#[tokio::test]
async fn admin_recovery_requires_the_host_operator_over_the_unix_socket() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp).await;
    let database = database(&temp);
    let auth = auth(&temp).await;
    let (operator, stranger) = unix_peers();
    let unix = api_router(state.clone(), auth.clone());
    let web = web_router(state.clone(), UiAssets::Embedded(TEST_BUNDLE), auth);
    let recover = async |router: &axum::Router, uid: Option<u32>| {
        let (status, body, _) =
            as_peer(router, Request::post("/api/v1/auth/recovery"), uid, None).await;
        (status, body)
    };
    assert_eq!(
        recover(&unix, Some(operator)).await.0,
        409,
        "setup is still open"
    );
    seed_account(&database, "admin", &[("admin", None)]).await;
    execute(&database, "UPDATE auth_setup SET initialized=1").await;
    for (router, uid) in [
        (&unix, None),
        (&unix, Some(stranger)),
        (&web, Some(operator)),
    ] {
        assert_eq!(recover(router, uid).await.0, 404, "{uid:?}");
    }
    // A credential keeps its meaning, even an unusable one.
    let request = Request::post("/api/v1/auth/recovery")
        .header(axum::http::header::AUTHORIZATION, "Bearer pqd_unknown");
    assert_eq!(as_peer(&unix, request, Some(operator), None).await.0, 404);
    let (status, body) = recover(&unix, Some(operator)).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body["url"]
            .as_str()
            .unwrap()
            .contains("/dashboard/auth#invite=")
    );
}

/// Over the Unix socket, root and the daemon's own user act as the host
/// operator without a token; anyone else, like a member of the socket's
/// group, still needs one, and a presented token keeps its meaning. Only the
/// operator gets a sign-in link, which signs one browser in, once. The audit
/// trail names the operator.
#[tokio::test]
async fn the_host_operator_acts_without_a_token_over_the_unix_socket() {
    use axum::http::header;
    use piqueld_core::{audit::AuditFilter, auth::HostOperator};
    let temp = TempDir::new().unwrap();
    let state = state(&temp).await;
    let auth = auth(&temp).await;
    let (operator, stranger) = unix_peers();
    let unix = api_router(state.clone(), auth.clone());
    let web = web_router(state.clone(), UiAssets::Embedded(TEST_BUNDLE), auth);
    let me = "/api/v1/auth/me";
    for uid in [operator, 0] {
        let (status, body, _) = as_peer(&unix, Request::get(me), Some(uid), None).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["operator"]["uid"].as_u64(), Some(u64::from(uid)));
        assert_eq!(body["grants"][0]["permission"], "admin");
    }
    for (router, uid) in [
        (&unix, Some(stranger)),
        (&unix, None),
        (&web, Some(operator)),
    ] {
        let status = as_peer(router, Request::get(me), uid, None).await.0;
        assert_eq!(status, 401, "{uid:?}");
    }
    let issue = "/api/v1/auth/sign-in-link";
    for (router, uid) in [(&unix, Some(stranger)), (&web, Some(operator))] {
        assert_eq!(
            as_peer(router, Request::post(issue), uid, None).await.0,
            404
        );
    }
    let (status, link, _) = as_peer(&unix, Request::post(issue), Some(operator), None).await;
    assert_eq!(status, 200, "{link}");
    let secret = link["url"]
        .as_str()
        .unwrap()
        .split_once("#operator=")
        .unwrap()
        .1;
    let login = || {
        Request::post("/api/v1/auth/login/operator")
            .header(header::ORIGIN, "https://piqueld.example")
    };
    let redeem = Some(serde_json::json!({ "secret": secret }));
    let (status, body, cookie) = as_peer(&web, login(), None, redeem.clone()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["operator"]["uid"].as_u64(), Some(u64::from(operator)));
    let cookie = cookie.unwrap();
    assert!(cookie.contains("Max-Age=43200"), "{cookie}");
    assert_eq!(
        as_peer(&web, login(), None, redeem).await.0,
        401,
        "single use"
    );
    let session = cookie.split(';').next().unwrap();
    let request = Request::get(me).header(header::COOKIE, session);
    let (status, body, _) = as_peer(&web, request, None, None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["operator"]["uid"].as_u64(), Some(u64::from(operator)));

    let foreign = Request::post(issue).header(header::ORIGIN, "https://attacker.example");
    assert_eq!(as_peer(&unix, foreign, Some(operator), None).await.0, 403);

    // Issuing the link and signing in with it are audited, in the background,
    // as the operator.
    let operator = Some(HostOperator { uid: operator });
    eventually(
        Duration::from_secs(10),
        Duration::from_millis(20),
        async || {
            let page = state
                .audit_events(&AuditFilter::default(), None, 50)
                .await
                .unwrap();
            let audited = |action: &str, status: u16| {
                page.items.iter().any(|event| {
                    event.action == action && event.status == status && event.operator == operator
                })
            };
            // Refusals before authentication, like a cross-site request, too.
            (audited("POST /api/v1/auth/sign-in-link", 200)
                && audited("POST /api/v1/auth/login/operator", 200)
                && audited("POST /api/v1/auth/sign-in-link", 403))
            .then_some(())
        },
    )
    .await
    .expect("the host operator's requests were not audited as the operator");
}

/// Over the Unix socket, a credential the host operator presents keeps its
/// meaning: the scheme is case-insensitive, an unusable credential never
/// falls back to the operator, and only the token-less operator gets a
/// sign-in link.
#[tokio::test]
async fn credentials_presented_by_the_host_operator_keep_their_meaning() {
    use axum::http::header;
    let temp = TempDir::new().unwrap();
    let state = state(&temp).await;
    let auth = auth(&temp).await;
    let (operator, _) = unix_peers();
    let unix = api_router(state, auth);
    let me = "/api/v1/auth/me";
    let token = admin_token();
    for scheme in ["Bearer", "bearer"] {
        let bearer = format!("{scheme} {token}");
        let request = Request::get(me).header(header::AUTHORIZATION, bearer);
        let (_, body, _) = as_peer(&unix, request, Some(operator), None).await;
        assert_eq!(body["user"]["username"], "contract-admin", "{scheme}");
    }
    for unusable in ["Basic dXNlcjpwYXNz", "Bearer pqd_unknown"] {
        let request = Request::get(me).header(header::AUTHORIZATION, unusable);
        let status = as_peer(&unix, request, Some(operator), None).await.0;
        assert_eq!(status, 401, "{unusable}");
    }
    // So does a session cookie, even in a later `Cookie` header.
    let cookies = |request: axum::http::request::Builder| {
        request
            .header(header::COOKIE, "other=one")
            .header(header::COOKIE, "__Host-piqueld_session=pqd_unknown")
    };
    let status = as_peer(&unix, cookies(Request::get(me)), Some(operator), None)
        .await
        .0;
    assert_eq!(status, 401);
    // Even one that cannot be parsed.
    let raw = header::HeaderValue::from_bytes(b"other=\xff; __Host-piqueld_session=x").unwrap();
    let request = Request::get(me).header(header::COOKIE, raw);
    assert_eq!(as_peer(&unix, request, Some(operator), None).await.0, 401);
    let issue = Request::post("/api/v1/auth/sign-in-link")
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    assert_eq!(as_peer(&unix, issue, Some(operator), None).await.0, 404);
    // A cookie-authenticated write without the site's origin is refused
    // outright, rather than issuing a link.
    let issue = cookies(Request::post("/api/v1/auth/sign-in-link"));
    assert_eq!(as_peer(&unix, issue, Some(operator), None).await.0, 403);
}

/// A fixed tailnet: `100.64.0.1` is Alice's laptop, `100.64.0.2` a CI runner,
/// `100.64.0.3` a runner that just lost `tag:ci`, which only a cached (not
/// fresh) lookup still reports, `100.64.0.4` a runner that just left the
/// tailnet, which only a cached lookup still finds, and `100.64.0.5` a runner
/// whose fresh lookups time out.
struct FakeTailnet;

#[async_trait]
impl piqueld::tailnet::TailnetLookup for FakeTailnet {
    async fn whois(
        &self,
        peer: std::net::SocketAddr,
        fresh: bool,
    ) -> Result<Option<piqueld_core::tailnet::TailnetPeer>, piqueld::tailnet::LookupTimedOut> {
        let (login, node, tags) = match peer.ip().to_string().as_str() {
            "100.64.0.1" => (Some("alice@example.com".into()), "laptop", Vec::new()),
            "100.64.0.2" => (None, "runner", vec!["tag:ci".to_owned()]),
            "100.64.0.3" if fresh => (None, "retired", vec!["tag:old".to_owned()]),
            "100.64.0.3" => (None, "retired", vec!["tag:ci".to_owned()]),
            "100.64.0.4" if fresh => return Ok(None),
            "100.64.0.4" => (None, "vanished", vec!["tag:ci".to_owned()]),
            "100.64.0.5" if fresh => return Err(piqueld::tailnet::LookupTimedOut),
            "100.64.0.5" => (None, "busy", vec!["tag:ci".to_owned()]),
            _ => return Ok(None),
        };
        Ok(Some(piqueld_core::tailnet::TailnetPeer {
            login,
            node: node.into(),
            tags,
        }))
    }
}

/// A daemon whose administrator created a token bound to `tag:ci`: its
/// state, authentication, and the token.
async fn tailnet_bound_token(temp: &TempDir) -> (ApiState, piqueld::auth::Auth, String) {
    use piqueld::auth::{Auth, TokenPolicy};
    let state = state(temp).await;
    let database = database(temp);
    let admin = seed_account(&database, "admin", &[("admin", None)]).await;
    let store = Store::open(&database).await.unwrap();
    let tokens = TokenPolicy {
        max_days: None,
        tailnet: true,
    };
    let auth = Auth::configured(&store, "https://piqueld.example", tokens).unwrap();
    let create = piqueld_core::auth::Manage::CreateToken {
        grants: piqueld_core::access::Grants::admin(),
        name: "ci".into(),
        days: Some(1),
        tailnet: Some(piqueld_core::tailnet::TailnetBinding::parse("tag:ci").unwrap()),
    };
    let unix = api_router(state.clone(), auth.clone());
    let (_, managed) = unix
        .call(
            &admin,
            "POST",
            "/api/v1/auth/manage",
            serde_json::to_value(&create).unwrap(),
        )
        .await;
    let token = managed["token"].as_str().expect("token").to_owned();
    (state, auth, token)
}

/// A token bound to a tailnet tag works only through the tailnet node from a
/// device carrying that tag right now; refusals name the tailnet peer in the
/// audit trail.
#[tokio::test]
async fn tailnet_bound_tokens_work_only_from_matching_devices() {
    let temp = TempDir::new().unwrap();
    let (state, auth, token) = tailnet_bound_token(&temp).await;
    let unix = api_router(state.clone(), auth.clone());
    let lookup: Arc<dyn piqueld::tailnet::TailnetLookup> = Arc::new(FakeTailnet);
    let tailnet = web_router(state.clone(), UiAssets::Embedded(TEST_BUNDLE), auth)
        .layer(axum::Extension(lookup));
    let me = async |router: &axum::Router, peer: Option<[u8; 4]>| {
        let mut request = Request::get("/api/v1/auth/me")
            .header("host", "localhost")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        if let Some(ip) = peer {
            let address = std::net::SocketAddr::from((ip, 40_000));
            request
                .extensions_mut()
                .insert(axum::extract::ConnectInfo(address));
        }
        router.send(request).await.status.as_u16()
    };
    assert_eq!(
        me(&tailnet, Some([100, 64, 0, 2])).await,
        200,
        "the CI runner"
    );
    assert_eq!(
        me(&tailnet, Some([100, 64, 0, 1])).await,
        401,
        "Alice's laptop"
    );
    let retagged = me(&tailnet, Some([100, 64, 0, 3])).await;
    assert_eq!(retagged, 401, "a cached identity never authorizes");
    let vanished = me(&tailnet, Some([100, 64, 0, 4])).await;
    assert_eq!(vanished, 401, "nor stands in for a failed lookup");
    let busy = me(&tailnet, Some([100, 64, 0, 5])).await;
    assert_eq!(busy, 503, "a lookup that timed out is no mismatch");
    assert_eq!(me(&unix, None).await, 401, "outside the tailnet");
    let from = refusals(&state, |events| {
        (events.len() >= 4).then(|| {
            events
                .into_iter()
                .map(|event| event.tailnet)
                .collect::<std::collections::BTreeSet<_>>()
        })
    })
    .await
    .expect("the refused requests were not audited");
    // The vanished runner's refusal names no peer, not its cached one.
    let described = ["alice@example.com on laptop", "tag:old on retired"];
    let expected = std::iter::once(None)
        .chain(described.map(|who| Some(who.to_owned())))
        .collect();
    assert_eq!(from, expected);
}

/// Behind the gateway's dashboard hostname, one connection carries many
/// tailnet clients, each named by the `X-Forwarded-For` header the gateway
/// sets, so tailnet-bound tokens work from matching devices there too.
#[tokio::test]
async fn the_dashboard_socket_names_clients_by_the_gateways_header() {
    let temp = TempDir::new().unwrap();
    let (state, auth, token) = tailnet_bound_token(&temp).await;
    let lookup: Arc<dyn piqueld::tailnet::TailnetLookup> = Arc::new(FakeTailnet);
    let gateway = piqueld::api::http::gateway_router(
        web_router(state.clone(), UiAssets::Embedded(TEST_BUNDLE), auth)
            .layer(axum::Extension(lookup)),
    );
    let through_gateway = async |client: Option<&str>| {
        let mut request = Request::get("/api/v1/auth/me")
            .header("host", "localhost")
            .header("authorization", format!("Bearer {token}"));
        if let Some(client) = client {
            request = request.header("x-forwarded-for", client);
        }
        let request = request.body(Body::empty()).unwrap();
        gateway.send(request).await.status.as_u16()
    };
    assert_eq!(
        through_gateway(Some("100.64.0.2")).await,
        200,
        "the CI runner"
    );
    assert_eq!(
        through_gateway(Some("100.64.0.1")).await,
        401,
        "Alice's laptop"
    );
    assert_eq!(through_gateway(None).await, 403, "not through the gateway");
}

/// A runner whose tag is `tag:ci` on the first lookup, and `tag:old` on every
/// later one, so authorizing a connection must look it up only once.
struct Retagged(std::sync::atomic::AtomicUsize);

#[async_trait]
impl piqueld::tailnet::TailnetLookup for Retagged {
    async fn whois(
        &self,
        _: std::net::SocketAddr,
        _: bool,
    ) -> Result<Option<piqueld_core::tailnet::TailnetPeer>, piqueld::tailnet::LookupTimedOut> {
        let lookups = self.0.fetch_add(1, Ordering::SeqCst);
        let tag = if lookups == 0 { "tag:ci" } else { "tag:old" };
        Ok(Some(piqueld_core::tailnet::TailnetPeer {
            login: None,
            node: "runner".into(),
            tags: vec![tag.to_owned()],
        }))
    }
}

/// A bound token's binding is rechecked when a command arrives: a device
/// retagged since the connection was authorized cannot start one, and the
/// refusal is audited with the device as it is now.
#[tokio::test]
async fn tailnet_bindings_are_rechecked_when_a_command_starts() {
    use piqueld_client::exec::{ExecCommand, ExecOutput, ExecRequest};
    let temp = TempDir::new().unwrap();
    let (state, auth, token) = tailnet_bound_token(&temp).await;
    let saved = save(&state, manifest()).await;
    let lookup: Arc<dyn piqueld::tailnet::TailnetLookup> =
        Arc::new(Retagged(std::sync::atomic::AtomicUsize::new(0)));
    let tailnet = web_router(state.clone(), UiAssets::Embedded(TEST_BUNDLE), auth)
        .layer(axum::Extension(lookup));
    let server = Server::tcp_with_peers(tailnet).await;
    let client = server.client().with_bearer(&token).unwrap();
    let request = ExecRequest {
        service: "web".parse().unwrap(),
        command: ExecCommand::parse(vec!["true".into()]).unwrap(),
        stdin: false,
        tty: None,
    };
    let environment = production(&state, &saved.application_id).await;
    let (mut output, _input) = client.exec(environment.as_str(), &request).await.unwrap();
    assert!(matches!(
        output.next().await,
        Ok(Some(ExecOutput::Failed { status: 401, error }))
            if error.code == "tailnet_binding_mismatch"
    ));
    let event = refusals(&state, |events| match events.as_slice() {
        [event] => Some(event.clone()),
        _ => None,
    })
    .await
    .expect("the refused command was not audited");
    assert_eq!(event.tailnet.as_deref(), Some("tag:old on runner"));
}

/// Authentication short circuits still use the common error correlation layers,
/// including storage failures that must not attempt another database write.
#[tokio::test]
async fn authentication_errors_preserve_request_and_diagnostic_ids() {
    use sqlx::Connection as _;

    let temp = TempDir::new().unwrap();
    let state = state(&temp).await;
    let store = Store::open(database(&temp)).await.unwrap();
    let mut connection = sqlite::connect(&database(&temp)).await;
    // Leave the observability tables healthy, but break authentication reads.
    sqlx::query("ALTER TABLE auth_credentials RENAME TO unavailable_credentials")
        .execute(&mut connection)
        .await
        .unwrap();
    // A diagnostic write would block behind this transaction. Storage failures
    // must return promptly with log-only diagnostic IDs instead.
    let transaction = connection.begin_with("BEGIN IMMEDIATE").await.unwrap();
    for website in [false, true] {
        let auth = piqueld::auth::Auth::new(&store, "https://piqueld.example").unwrap();
        let listener = if website {
            web_router(state.clone(), UiAssets::Embedded(TEST_BUNDLE), auth)
        } else {
            api_router(state.clone(), auth)
        };
        for (method, path, bearer, origin, status, code) in [
            (
                "GET",
                "/api/v1/system/status",
                "invalid".to_owned(),
                "",
                401,
                "authentication_required",
            ),
            (
                "POST",
                "/api/v1/auth/login/start",
                String::new(),
                "https://other.example",
                403,
                "origin_mismatch",
            ),
            (
                "GET",
                "/api/v1/auth/me",
                "x".repeat(43),
                "",
                503,
                "storage_unavailable",
            ),
            // A bearer that could not be checked is no reason to require
            // the ceremony's Origin.
            (
                "POST",
                "/api/v1/auth/register/start",
                "x".repeat(43),
                "",
                503,
                "storage_unavailable",
            ),
        ] {
            let mut request = Request::builder()
                .method(method)
                .uri(path)
                .header("host", "localhost")
                .header("authorization", format!("Bearer {bearer}"))
                .header("x-request-id", "auth-correlation");
            if !origin.is_empty() {
                request = request.header("origin", origin);
            }
            let response = tokio::time::timeout(
                Duration::from_secs(2),
                listener.send(request.body(Body::empty()).unwrap()),
            )
            .await
            .expect("authentication errors must not wait for a diagnostic write");
            assert_eq!(response.status.as_u16(), status);
            let request_id = response.header("x-request-id").unwrap();
            assert_ne!(request_id, "auth-correlation");
            let error: piqueld_core::api::ErrorBody =
                serde_json::from_slice(&response.body).unwrap();
            assert_eq!(error.code, code);
            assert_eq!(error.request_id, request_id);
            if status == 503 {
                let id = error.details["diagnostic_id"].as_str().unwrap();
                assert!(id.starts_with("diagnostic-"));
                assert!(matches!(
                    store.diagnostic(id).await,
                    Err(piqueld::store::StoreError::NotFound)
                ));
            } else {
                assert!(error.details.get("diagnostic_id").is_none());
            }
        }
    }
    transaction.rollback().await.unwrap();
}

/// Authentication endpoints answer undecodable bodies, and bodies over the
/// small limit of public sign-in endpoints, with the usual JSON error, code,
/// and request ID.
#[tokio::test]
async fn authentication_bodies_fail_with_structured_errors() {
    let temp = TempDir::new().unwrap();
    let unix = api_router(state(&temp).await, auth(&temp).await);
    let oversized = format!(r#"{{"device_code":"{}"}}"#, "x".repeat(64 * 1024));
    for (content_type, body, status, code) in [
        ("application/json", "{", 400, "json_malformed"),
        ("text/plain", "{}", 415, "content_type_unsupported"),
        (
            "application/json",
            oversized.as_str(),
            413,
            "request_body_too_large",
        ),
    ] {
        let request = Request::post("/api/v1/auth/device/poll")
            .header("content-type", content_type)
            .body(Body::from(body.to_owned()))
            .unwrap();
        let response = unix.send(request).await;
        assert_eq!(response.status.as_u16(), status);
        let error: piqueld_core::api::ErrorBody = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(error.code, code);
        assert_eq!(
            response.header("x-request-id"),
            Some(error.request_id.as_str())
        );
    }
}

/// Login starts echo the grants they will be limited to, so clients can tell
/// the limit applies, and are kept in memory, so large bodies are refused.
#[tokio::test]
async fn device_starts_echo_their_limits_and_refuse_large_bodies() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp).await;
    let router = api_router(state, auth(&temp).await);
    let start = |body: String| {
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/auth/device/start")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        router.send(request)
    };
    let limited = serde_json::json!({"grants": [{"permission": "apps:read"}]});
    let response = start(limited.to_string()).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json()["grants"], limited["grants"]);
    let large = format!(r#"{{"grants":[],"padding":"{}"}}"#, "x".repeat(16 * 1024));
    let response = start(large).await;
    assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
    // The limit applies while reading, also without a `Content-Length`.
    let chunks = (0..64).map(|_| Ok::<_, std::io::Error>(vec![b' '; 1024]));
    let streamed = Request::builder()
        .method("POST")
        .uri("/api/v1/auth/device/start")
        .header("content-type", "application/json")
        .body(Body::from_stream(futures_util::stream::iter(chunks)))
        .unwrap();
    let response = router.send(streamed).await;
    assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
}
