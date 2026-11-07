//! Exec sessions and event streams, and how they end when access is lost.

use super::*;

/// Exec rechecks the caller's grants when the command starts, so access lost
/// after the connection was authorized cannot start one. History names who
/// started a command.
#[tokio::test]
async fn exec_rechecks_grants_when_the_command_starts() {
    use piqueld::store::{Caller, StoreError};
    use piqueld_core::access::Denied;
    let temp = tempfile::tempdir().unwrap();
    let state = state(&temp).await;
    let saved = save(&state, manifest()).await;
    let database = database(&temp);
    let grant = [("apps:exec", Some(saved.application_id.as_str()))];
    seed_account(&database, "operator", &grant).await;
    let environment = production(&state, &saved.application_id).await;
    let request = piqueld_core::exec::ExecRequest {
        service: "web".parse().unwrap(),
        command: piqueld_core::exec::ExecCommand::parse(vec!["true".into()]).unwrap(),
        stdin: false,
        tty: None,
    };
    let caller = piqueld::api::Actor::Account(Caller {
        credential_id: "operator",
        user_id: "operator",
    });
    let session = state
        .exec(caller, &environment, &request, "operator")
        .await
        .unwrap();
    // The start is recorded once the command runs; without input it exits.
    let (_, input) = tokio::sync::mpsc::channel(1);
    let (output, _output) = tokio::sync::mpsc::channel(1);
    let io = piqueld::docker::ExecIo {
        input,
        output,
        stop: tokio_util::sync::CancellationToken::new(),
    };
    assert_eq!(session.run(io).await.unwrap(), 0);
    let filter = piqueld_core::observability::EventFilter {
        kind: Some("command_started".into()),
        ..Default::default()
    };
    let started = state
        .filtered_events(&filter, &piqueld::store::Visibility::ALL, None, 10)
        .await
        .unwrap()
        .items;
    assert_eq!(
        started[0].actor,
        Some(piqueld_core::EventActor::Account {
            user_id: "operator".into(),
            credential_id: Some("operator".into()),
        })
    );
    execute(
        &database,
        "DELETE FROM auth_grants WHERE user_id='operator'",
    )
    .await;
    assert!(matches!(
        state.exec(caller, &environment, &request, "operator").await,
        Err(piqueld::api::ApplicationError::Store(StoreError::Denied(
            Denied::Hidden
        )))
    ));
}

#[tokio::test]
async fn exec_streams_over_both_transports_and_records_history_without_the_command() {
    use futures_util::{SinkExt, StreamExt};
    use piqueld_client::exec::{ExecCommand, ExecInput, ExecOutput, ExecRequest};
    use tokio_tungstenite::tungstenite::Message;
    let temp = tempfile::tempdir().unwrap();
    let state = state(&temp).await;
    let unix = Server::unix(
        api_router(state.clone(), FakeAuth),
        &temp.path().join("exec.sock"),
    );
    let tcp = Server::tcp(router(state, FakeAuth)).await;
    let request = |service: &str| ExecRequest {
        service: service.parse().unwrap(),
        command: ExecCommand::parse(vec!["invite".into(), "create".into()]).unwrap(),
        stdin: true,
        tty: None,
    };
    let app = unix
        .client()
        .apply_and_deploy(&new_application())
        .await
        .unwrap();
    for client in [unix.client(), tcp.client()] {
        let (mut output, mut input) = client
            .exec(&app.environment_id, &request("web"))
            .await
            .unwrap();
        input
            .send(&ExecInput::Stdin(b"hello".to_vec()))
            .await
            .unwrap();
        assert!(
            matches!(output.next().await, Ok(Some(ExecOutput::Stdout(data))) if data == b"hello")
        );
        input.send(&ExecInput::CloseStdin).await.unwrap();
        assert!(
            matches!(output.next().await, Ok(Some(ExecOutput::Stderr(data))) if data == b"closed")
        );
        assert!(matches!(output.next().await, Ok(Some(ExecOutput::Exit(5)))));
        assert!(matches!(output.next().await, Ok(None)));

        // Start failures arrive after the handshake, with their HTTP status.
        let (mut output, _input) = client
            .exec(&app.environment_id, &request("worker"))
            .await
            .unwrap();
        assert!(matches!(
            output.next().await,
            Ok(Some(ExecOutput::Failed { status: 409, error }))
                if error.code == "service_not_running" && !error.request_id.is_empty()
        ));
    }
    // Completion is recorded before the final frame is sent.
    let commands = unix
        .client()
        .events(None, Some(&app.environment_id), None, false, None, 100)
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|event| event.kind.starts_with("command_"))
        .collect::<Vec<_>>();
    assert_eq!(commands.len(), 4);
    for event in &commands {
        assert_eq!(event.resource.as_deref(), Some("web"));
        assert!(!event.message.as_deref().unwrap().contains("invite"));
    }
    assert_eq!(
        commands[1].message.as_deref(),
        Some("Command in task task-1 exited with code 5")
    );

    // A client detaching from a quiet session gets a clean Close reply.
    let url = format!(
        "ws://{}/api/v1/environments/{}/exec",
        tcp.address(),
        app.environment_id
    );
    let (mut socket, _) = connect_exec(tcp.address(), url, None).await;
    let start = serde_json::to_string(&request("web")).unwrap();
    socket.send(Message::text(start)).await.unwrap();
    socket.close(None).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), socket.next()).await;
    assert!(
        matches!(reply, Ok(Some(Ok(Message::Close(_))))),
        "{reply:?}"
    );
}

#[tokio::test]
async fn event_stream_replays_after_cursor_and_rejects_pruned_history() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let app = api.deploy().await;
    api.finish_operation(
        &app.operation_id,
        Some(("service_update_failed", "Update paused")),
    )
    .await;
    let events = api
        .client
        .events(None, None, None, false, None, 100)
        .await
        .unwrap()
        .items;
    let after = events[events.len() - 2].id;
    let router = api.router();
    for limit in [0, 101] {
        let response = router
            .send(request(&format!("/api/v1/events/stream?limit={limit}")))
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
    }
    let response = router
        .open(
            Request::builder()
                .uri("/api/v1/events/stream?errors_only=true&limit=1")
                .header("host", "localhost")
                .header("last-event-id", format!("v1:{after}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let mut body = response.into_body();
    let frame = tokio::time::timeout(Duration::from_secs(2), async {
        let mut bytes = Vec::new();
        loop {
            let frame = body.frame().await.unwrap().unwrap();
            if let Ok(data) = frame.into_data() {
                bytes.extend_from_slice(&data);
                if let Some(end) = bytes.windows(2).position(|window| window == b"\n\n") {
                    bytes.truncate(end);
                    return bytes;
                }
            }
        }
    })
    .await
    .unwrap();
    let text = std::str::from_utf8(&frame).unwrap();
    assert!(text.contains("operation_failed"));
    assert!(text.contains(&format!("id: v1:{}", events.last().unwrap().id)));
    api.store.prune_events(i64::MAX).await.unwrap();
    let response = router
        .send(request(&format!("/api/v1/events/stream?cursor=v1:{after}")))
        .await;
    assert_eq!(response.status, StatusCode::GONE);
}

/// A command refused when it starts, after its connection was authorized
/// and audited as allowed, is audited and counted as a refusal too.
#[tokio::test]
async fn exec_refusals_after_the_upgrade_are_audited() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let f = GrantFixture::new().await;
    let grants = [
        ("apps:exec", Some(f.blog.as_str())),
        ("apps:read", Some(f.blog.as_str())),
    ];
    let operator = seed_account(&database(&f.temp), "operator", &grants).await;
    let server = f.serve().await;
    let url = format!(
        "ws://{}/api/v1/environments/{}/exec",
        server.address(),
        f.blog_production
    );
    let (mut socket, _) = connect_exec(server.address(), url, Some(&operator)).await;
    execute(
        &database(&f.temp),
        "DELETE FROM auth_grants WHERE user_id='operator' AND permission='apps:exec'",
    )
    .await;
    let start = serde_json::json!({"service": "web", "command": ["true"], "stdin": false});
    socket.send(Message::text(start.to_string())).await.unwrap();
    while let Some(Ok(message)) = socket.next().await {
        if message.is_close() {
            break;
        }
    }
    let body = f
        .audit(&f.auditor, "/api/v1/audit?user_id=operator", 2)
        .await;
    let outcomes: Vec<_> = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| {
            assert_eq!(event["action"], "GET /api/v1/environments/{id}/exec");
            (event["outcome"].clone(), event["permission"].clone())
        })
        .collect();
    assert!(
        outcomes.contains(&("denied".into(), "apps:exec".into())),
        "{outcomes:?}"
    );
    assert!(
        outcomes.contains(&("allowed".into(), serde_json::Value::Null)),
        "{outcomes:?}"
    );
}

/// Connects an exec WebSocket to `url` at `address`, as `token` when given.
async fn connect_exec(
    address: std::net::SocketAddr,
    url: String,
    token: Option<&str>,
) -> (
    tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    tokio_tungstenite::tungstenite::handshake::client::Response,
) {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = url.into_client_request().unwrap();
    if let Some(token) = token {
        let bearer = format!("Bearer {token}").parse().unwrap();
        request.headers_mut().insert("authorization", bearer);
    }
    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    tokio_tungstenite::client_async(request, stream)
        .await
        .unwrap()
}

/// Opens an exec WebSocket on `environment` at `address` as `token` and
/// starts `command` with standard input attached.
async fn open_exec(
    address: std::net::SocketAddr,
    environment: &str,
    token: Option<&str>,
    command: &str,
) -> tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;
    let url = format!("ws://{address}/api/v1/environments/{environment}/exec");
    let (mut socket, _) = connect_exec(address, url, token).await;
    let start = serde_json::json!({"service": "web", "command": [command], "stdin": true});
    socket.send(Message::text(start.to_string())).await.unwrap();
    socket
}

/// A running command ends with the refusal an equivalent request would get
/// once its caller's credential is revoked, its account deleted, or its
/// grants no longer include `apps:exec`, even if the runtime never stops it.
#[tokio::test]
async fn exec_sessions_end_once_access_is_lost() {
    use futures_util::{SinkExt, StreamExt};
    use piqueld_core::exec::{ExecFrame, ExecInput, ExecOutput};
    use serde_json::json;
    use tokio_tungstenite::tungstenite::Message;
    let f = GrantFixture::new().await;
    let database = database(&f.temp);
    // Access changes are noticed, and a runtime that never stops is given
    // up on, within milliseconds.
    let timing = piqueld::api::http::ConnectionTiming {
        tick: Duration::from_millis(10),
        close: Duration::from_millis(100),
        ..Default::default()
    };
    let state = f.state.clone().with_connection_timing(timing);
    let server = Server::tcp(api_router(state, f.auth.clone())).await;
    let admin = admin_token();
    // Deleting an account needs another administrator able to sign in.
    execute(&database, "INSERT INTO auth_passkeys(id,user_id,name,credential,created_at) VALUES('key','contract-admin','key','{}',1)").await;
    let reader = json!([{"permission": "apps:read", "applications": [f.blog]}]);
    let changes = [
        (
            "revoked",
            "hang",
            json!({"action": "revoke_credential", "id": "revoked"}),
            401,
        ),
        (
            "deleted",
            "cat",
            json!({"action": "delete_user", "user_id": "deleted"}),
            401,
        ),
        (
            "demoted",
            "cat",
            json!({"action": "set_grants", "user_id": "demoted", "grants": reader}),
            403,
        ),
    ];
    let grants = [
        ("apps:exec", Some(f.blog.as_str())),
        ("apps:read", Some(f.blog.as_str())),
    ];
    for (name, command, change, status) in changes {
        let token = seed_account(&database, name, &grants).await;
        let mut socket =
            open_exec(server.address(), &f.blog_production, Some(&token), command).await;
        let echo = ExecInput::Stdin(b"hi".to_vec()).encode();
        socket.send(Message::binary(echo)).await.unwrap();
        let mut next = async || loop {
            match socket.next().await {
                Some(Ok(Message::Binary(data))) => return Some(ExecOutput::decode(&data).unwrap()),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => return None,
            }
        };
        assert!(matches!(next().await, Some(ExecOutput::Stdout(data)) if data == b"hi"));
        let manage = "/api/v1/auth/manage";
        assert_eq!(
            f.router.call(&admin, "POST", manage, change).await.0,
            200,
            "{name}"
        );
        // A runtime that never stops gets `timing.close` to wind down.
        let last = tokio::time::timeout(Duration::from_secs(10), next())
            .await
            .expect("the session ends promptly");
        assert!(
            matches!(&last, Some(ExecOutput::Failed { status: got, .. }) if *got == status),
            "{name}: {last:?}"
        );
    }
}

/// Access lost while a command is being created is noticed before it runs
/// or is recorded as started, even if it would finish at once.
#[tokio::test]
async fn exec_rechecks_access_once_the_command_is_created() {
    use futures_util::{SinkExt, StreamExt};
    use piqueld_core::exec::{ExecFrame, ExecInput, ExecOutput};
    use tokio_tungstenite::tungstenite::Message;
    let f = GrantFixture::new().await;
    let grants = [("apps:exec", Some(f.blog.as_str()))];
    let token = seed_account(&database(&f.temp), "operator", &grants).await;
    let server = f.serve().await;
    let mut socket = open_exec(server.address(), &f.blog_production, Some(&token), "held").await;
    // Input that would let the command finish as soon as it runs.
    for input in [ExecInput::Stdin(b"hi".to_vec()), ExecInput::CloseStdin] {
        socket.send(Message::binary(input.encode())).await.unwrap();
    }
    let gate = &f.runtime.exec_gate;
    tokio::time::timeout(Duration::from_secs(10), gate.started.notified())
        .await
        .expect("the command is being created");
    let revoke = serde_json::json!({"action": "revoke_credential", "id": "operator"});
    let manage = "/api/v1/auth/manage";
    assert_eq!(f.router.call(&token, "POST", manage, revoke).await.0, 200);
    gate.release.notify_one();
    let mut outputs = Vec::new();
    while let Some(Ok(message)) = socket.next().await {
        if let Message::Binary(data) = message {
            outputs.push(ExecOutput::decode(&data).unwrap());
        }
    }
    assert!(
        matches!(&outputs[..], [ExecOutput::Failed { status: 401, .. }]),
        "{outputs:?}"
    );
    // History does not claim the command started.
    let filter = piqueld_core::observability::EventFilter {
        kind: Some("command_started".into()),
        ..Default::default()
    };
    let started = f
        .state
        .filtered_events(&filter, &piqueld::store::Visibility::ALL, None, 10)
        .await
        .unwrap();
    assert!(started.items.is_empty());
}

/// A client that leaves while the command is not reading its input, so
/// that input is no longer read from the connection either, still ends the
/// session.
#[tokio::test]
async fn exec_notices_a_client_that_left_behind_unread_input() {
    use futures_util::SinkExt;
    use piqueld_core::exec::{ExecFrame, ExecInput};
    use tokio_tungstenite::tungstenite::Message;
    let temp = tempfile::tempdir().unwrap();
    let state = state(&temp).await;
    let saved = save(&state, manifest()).await;
    let environment = production(&state, &saved.application_id).await;
    // Pings notice the client is gone within milliseconds.
    let timing = piqueld::api::http::ConnectionTiming {
        ping: Duration::from_millis(50),
        ..Default::default()
    };
    let paced = state.clone().with_connection_timing(timing);
    let server = Server::tcp(router(paced, FakeAuth)).await;
    let mut socket = open_exec(server.address(), environment.as_str(), None, "block").await;
    // More input than the relay buffers, so it stops reading the connection.
    for _ in 0..32 {
        let input = ExecInput::Stdin(vec![0; 1024]).encode();
        socket.send(Message::binary(input)).await.unwrap();
    }
    drop(socket);
    let filter = piqueld_core::observability::EventFilter {
        kind: Some("command_finished".into()),
        ..Default::default()
    };
    let finished = eventually(
        Duration::from_secs(20),
        Duration::from_millis(100),
        async || {
            let events = state
                .filtered_events(&filter, &piqueld::store::Visibility::ALL, None, 10)
                .await
                .unwrap()
                .items;
            (!events.is_empty()).then_some(events)
        },
    )
    .await
    .expect("the session ends once the client is gone");
    assert_eq!(
        finished[0].message.as_deref(),
        Some("Command stream in task task-1 failed")
    );
}

/// Reads an event stream until it ends, or until a message contains
/// `needle` when given, returning what was read.
async fn read_stream(body: &mut Body, needle: Option<&str>) -> String {
    let mut text = String::new();
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame.unwrap().into_data() {
            text.push_str(std::str::from_utf8(&data).unwrap());
            if needle.is_some_and(|needle| text.contains(needle)) {
                break;
            }
        }
    }
    text
}

/// An event stream ends with an `access_ended` message once its caller's
/// credential is revoked, without sending events it read before, and
/// without a message when the daemon shuts down.
#[tokio::test]
async fn event_streams_end_once_access_is_lost_or_the_daemon_stops() {
    let f = GrantFixture::new().await;
    let reader = seed_account(&database(&f.temp), "reader", &[("events:read", None)]).await;
    let shutdown = tokio_util::sync::CancellationToken::new();
    let router = api_router(
        f.state.clone().with_shutdown(shutdown.clone()),
        f.auth.clone(),
    );
    let open = async |token: &str| {
        let request = Request::builder()
            .uri("/api/v1/events/stream")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = router.open(request).await;
        assert_eq!(response.status(), 200);
        response.into_body()
    };
    let mut revoked = open(&reader).await;
    let mut stopped = open(&admin_token()).await;
    // The first event leaves the rest of the batch read but not yet sent.
    read_stream(&mut revoked, Some("event: event")).await;
    let revoke = serde_json::json!({"action": "revoke_credential", "id": "reader"});
    let manage = "/api/v1/auth/manage";
    assert_eq!(f.router.call(&reader, "POST", manage, revoke).await.0, 200);
    let text = tokio::time::timeout(Duration::from_secs(5), read_stream(&mut revoked, None))
        .await
        .expect("the stream ends promptly");
    assert!(
        text.trim_start().starts_with("event: access_ended"),
        "{text}"
    );
    assert!(text.contains("authentication_required"), "{text}");
    assert!(!text.contains("event: event"), "{text}");

    shutdown.cancel();
    let text = tokio::time::timeout(Duration::from_secs(5), read_stream(&mut stopped, None))
        .await
        .expect("the stream ends on shutdown");
    assert!(!text.contains("access_ended"), "{text}");
}
