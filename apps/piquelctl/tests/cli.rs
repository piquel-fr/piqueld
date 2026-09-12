//! Black-box command, transport, pagination, safety, and output tests.

use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::TcpListener,
    os::unix::net::UnixListener,
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use tempfile::{TempDir, tempdir};

const MANIFEST: &str = r#"api_version = "piqueld.dev/v1alpha1"
kind = "Application"

[metadata]
name = "notes"

[[spec.services]]
name = "web"

[spec.services.source]
type = "image"
image = "ghcr.io/example/notes:1.4.0"
"#;

#[derive(Clone, Debug)]
struct Request {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

#[derive(Clone, Debug)]
struct Reply {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
    drop_connection: bool,
}

impl Reply {
    fn json(data: Value) -> Self {
        let data = serde_json::to_value(data).expect("JSON value");
        Self {
            status: "200 OK",
            content_type: "application/json",
            body: serde_json::to_vec(&json!({"data": data})).expect("JSON response"),
            drop_connection: false,
        }
    }

    fn accepted(data: Value) -> Self {
        let data = serde_json::to_value(data).expect("JSON value");
        Self {
            status: "202 Accepted",
            content_type: "application/json",
            body: serde_json::to_vec(&json!({"data": data})).expect("JSON response"),
            drop_connection: false,
        }
    }

    fn dropped() -> Self {
        Self {
            status: "200 OK",
            content_type: "application/json",
            body: Vec::new(),
            drop_connection: true,
        }
    }
}

enum Endpoint {
    Tcp(String),
    Unix(PathBuf),
}

struct TestServer {
    endpoint: Endpoint,
    records: Arc<Mutex<Vec<Request>>>,
    _directory: TempDir,
    join: Option<thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl TestServer {
    fn stop(self) -> Vec<Request> {
        self.stop.store(true, Ordering::Relaxed);
        self.finish()
    }

    fn finish(mut self) -> Vec<Request> {
        self.join
            .take()
            .expect("server join handle")
            .join()
            .expect("server thread");
        self.records.lock().expect("request records").clone()
    }
}

const ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);

fn start_server<F>(unix: bool, expected_requests: usize, handler: F) -> TestServer
where
    F: FnMut(Request) -> Reply + Send + 'static,
{
    let directory = tempdir().expect("temporary server directory");
    let records = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_thread = Arc::clone(&stop);
    let handler = Arc::new(Mutex::new(handler));
    let records_for_thread = Arc::clone(&records);
    let handler_for_thread = Arc::clone(&handler);

    let (endpoint, join) = if unix {
        let path = directory.path().join("piqueld.sock");
        let listener = UnixListener::bind(&path).expect("Unix listener");
        listener
            .set_nonblocking(true)
            .expect("non-blocking accept applies");
        let join = thread::spawn(move || {
            let mut deadline = std::time::Instant::now() + ACCEPT_TIMEOUT;
            let mut served = 0;
            while served < expected_requests && !stop_for_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        served += 1;
                        deadline = std::time::Instant::now() + ACCEPT_TIMEOUT;
                        serve_stream(stream, &records_for_thread, &handler_for_thread);
                    }
                    // A request-count mismatch must fail fast, not hang forever.
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if std::time::Instant::now() >= deadline {
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        (Endpoint::Unix(path), join)
    } else {
        let listener = TcpListener::bind("127.0.0.1:0").expect("TCP listener");
        let address = listener.local_addr().expect("TCP address");
        listener
            .set_nonblocking(true)
            .expect("non-blocking accept applies");
        let join = thread::spawn(move || {
            let mut deadline = std::time::Instant::now() + ACCEPT_TIMEOUT;
            let mut served = 0;
            while served < expected_requests && !stop_for_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        served += 1;
                        deadline = std::time::Instant::now() + ACCEPT_TIMEOUT;
                        serve_stream(stream, &records_for_thread, &handler_for_thread);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if std::time::Instant::now() >= deadline {
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        (Endpoint::Tcp(format!("http://{address}/")), join)
    };

    TestServer {
        endpoint,
        records,
        _directory: directory,
        join: Some(join),
        stop,
    }
}

fn serve_stream<S>(
    mut stream: S,
    records: &Arc<Mutex<Vec<Request>>>,
    handler: &Arc<Mutex<impl FnMut(Request) -> Reply>>,
) where
    S: Read + Write,
{
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    records
        .lock()
        .expect("request records")
        .push(request.clone());
    let reply = handler.lock().expect("request handler")(request);
    if reply.drop_connection {
        return;
    }
    let header = format!(
        "HTTP/1.1 {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        reply.status,
        reply.content_type,
        reply.body.len()
    );
    stream
        .write_all(header.as_bytes())
        .expect("HTTP response headers");
    stream.write_all(&reply.body).expect("HTTP response body");
}

fn read_request<S: Read>(stream: &mut S) -> Option<Request> {
    let mut bytes = Vec::new();
    let header_end;
    loop {
        let mut buffer = [0_u8; 4096];
        let read = stream.read(&mut buffer).expect("HTTP request");
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(position) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            header_end = position + 4;
            break;
        }
    }

    let body_start = header_end;
    let header_text = String::from_utf8_lossy(&bytes[..body_start - 4]);
    let mut lines = header_text.lines();
    let request_line = lines.next().expect("request line");
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().expect("HTTP method").to_owned();
    let path = normalize_target(request_parts.next().expect("HTTP path"));
    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    while bytes.len() < body_start + content_length {
        let mut buffer = [0_u8; 4096];
        let read = stream.read(&mut buffer).expect("HTTP request body");
        assert_ne!(read, 0, "request ended before Content-Length");
        bytes.extend_from_slice(&buffer[..read]);
    }

    Some(Request {
        method,
        path,
        headers,
        body: bytes[body_start..body_start + content_length].to_vec(),
    })
}

fn normalize_target(target: &str) -> String {
    let Some(authority_and_path) = target.split_once("://").map(|(_, value)| value) else {
        return target.to_owned();
    };
    authority_and_path.find('/').map_or_else(
        || "/".to_owned(),
        |index| authority_and_path[index..].to_owned(),
    )
}

fn run(server: &TestServer, arguments: &[&str]) -> Output {
    run_with_timeout(server, arguments, "2s")
}

fn run_with_timeout(server: &TestServer, arguments: &[&str], timeout: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_piquelctl"));
    match &server.endpoint {
        Endpoint::Tcp(url) => {
            command.args(["--url", url]);
        }
        Endpoint::Unix(path) => {
            command.args(["--socket", path.to_str().expect("socket path")]);
        }
    }
    command
        .args(["--json", "--timeout", timeout])
        .args(arguments)
        .output()
        .expect("piquelctl process")
}

fn write_manifest(directory: &TempDir) -> PathBuf {
    let path = directory.path().join("application.toml");
    fs::write(&path, MANIFEST).expect("manifest");
    path
}

fn app_view(id: &str, name: &str) -> Value {
    json!({
        "application": {
            "id": id,
            "api_version": "piqueld.dev/v1alpha1",
            "kind": "Application",
            "metadata": {"name": name},
            "spec": {
                "services": [{
                    "name": "web",
                    "source": {"type": "image", "image": "ghcr.io/example/notes:1.4.0"},
                    "replicas": 2,
                    "environment": {},
                    "command": [],
                    "arguments": [],
                    "mounts": [],
                    "healthcheck": null,
                    "resources": null
                }],
                "volumes": [{"name": "data"}]
            }
        },
        "generation": 1, "resolved_generation": 1, "spec_hash": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "delete_intent": false,
        "created_at_ms": 1,
        "updated_at_ms": 1
    })
}

fn app_summary(id: &str, name: &str) -> Value {
    json!({
        "id": id,
        "name": name,
        "generation": 1,
        "resolved_generation": 1,
        "delete_intent": false,
        "created_at_ms": 1,
        "updated_at_ms": 1
    })
}

fn page(items: Vec<Value>, next_cursor: Option<&str>) -> Value {
    let items = items.into_iter().collect::<Vec<_>>();
    json!({"items": items, "next_cursor": next_cursor})
}

fn status(id: &str, state: &str) -> Value {
    json!({
        "application_id": id,
        "state": state,
        "message": null,
        "updated_at_ms": 1
    })
}

fn plan(id: &str) -> Value {
    json!({
        "application_id": id,
        "generation": u64::from(!id.starts_with("preview-")),
        "identical": false, "operation": null, "changes": [],
        "plan": {
            "actions": [],
            "diagnostics": []
        }
    })
}

fn accepted(id: &str) -> Value {
    json!({
        "generation": 1, "operation_id": "operation-01",
        "application_id": id
    })
}

fn operation(state: &str) -> Value {
    let failed = state == "failed";
    json!({
        "id": "operation-01",
        "application_id": "app-notes-01",
        "kind": "apply", "generation": 1, "attempt": 1,
        "state": state,
        "error_code": if failed { json!("runtime_failed") } else { Value::Null },
        "error_message": if failed { json!("runtime reconciliation failed") } else { Value::Null },
        "created_at_ms": 1,
        "updated_at_ms": 2,
        "started_at_ms": 1,
        "finished_at_ms": if state == "succeeded" { json!(2) } else { Value::Null }
    })
}

fn assert_json_success(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.stdout.is_empty(), "JSON stdout must not be empty");
    serde_json::from_slice(&output.stdout).expect("clean JSON stdout")
}

#[test]
fn status_works_over_tcp_and_unix_with_clean_json() {
    for unix in [false, true] {
        let server = start_server(unix, 1, |request| {
            assert_eq!(request.method, "GET");
            assert_eq!(request.path, "/api/v1/system/status");
            Reply::json(json!({
                "status": "running",
                "api_version": "v1",
                "daemon_version": "0.1.0",
                "instance_id": "instance-test"
            }))
        });
        let output = run(&server, &["status"]);
        let value = assert_json_success(&output);
        assert_eq!(value["instance_id"], "instance-test");
        assert!(output.stderr.is_empty());
        let _ = server.finish();
    }
}

#[test]
fn list_paginates_and_includes_reconciliation_status() {
    for unix in [false, true] {
        let mut page_number = 0;
        let server = start_server(unix, 4, move |request| match request.path.as_str() {
            "/api/v1/applications?limit=100"
            | "/api/v1/applications?cursor=v1%3Aapp-first-01&limit=100" => {
                page_number += 1;
                if page_number == 1 {
                    Reply::json(page(
                        vec![app_summary("app-first-01", "first")],
                        Some("v1:app-first-01"),
                    ))
                } else {
                    Reply::json(page(vec![app_summary("app-notes-01", "notes")], None))
                }
            }
            "/api/v1/applications/app-first-01/status" => {
                Reply::json(status("app-first-01", "ready"))
            }
            "/api/v1/applications/app-notes-01/status" => {
                Reply::json(status("app-notes-01", "degraded"))
            }
            path => panic!("unexpected path {path}"),
        });
        let output = run(&server, &["list"]);
        let value = assert_json_success(&output);
        assert_eq!(value["items"].as_array().expect("items").len(), 2);
        assert_eq!(value["items"][1]["status"]["state"], "degraded");
        assert!(output.stderr.is_empty());
        let _ = server.finish();
    }
}

#[test]
fn repeated_pagination_cursor_is_rejected() {
    let mut page_number = 0;
    let server = start_server(false, 2, move |request| {
        assert!(matches!(
            request.path.as_str(),
            "/api/v1/applications?limit=100"
                | "/api/v1/applications?cursor=v1%3Aapp-first-01&limit=100"
        ));
        page_number += 1;
        Reply::json(page(
            if page_number == 1 {
                vec![app_summary("app-first-01", "first")]
            } else {
                Vec::new()
            },
            Some("v1:app-first-01"),
        ))
    });

    let output = run(&server, &["list"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("repeated pagination cursor"));
    let _ = server.finish();
}

#[test]
fn show_resolves_name_across_pages_and_id_directly() {
    let first_server = start_server(false, 4, move |request| match request.path.as_str() {
        "/api/v1/applications?limit=100" => Reply::json(page(
            vec![app_summary("app-first-01", "first")],
            Some("v1:app-first-01"),
        )),
        "/api/v1/applications?cursor=v1%3Aapp-first-01&limit=100" => {
            Reply::json(page(vec![app_summary("app-notes-01", "notes")], None))
        }
        "/api/v1/applications/app-notes-01" => Reply::json(app_view("app-notes-01", "notes")),
        "/api/v1/applications/app-notes-01/status" => Reply::json(status("app-notes-01", "ready")),
        path => panic!("unexpected path {path}"),
    });
    let output = run(&first_server, &["show", "notes"]);
    let value = assert_json_success(&output);
    assert_eq!(
        value["application"]["application"]["metadata"]["name"],
        "notes"
    );
    let _ = first_server.finish();

    let second_server = start_server(false, 2, move |request| match request.path.as_str() {
        "/api/v1/applications/app-notes-01" => Reply::json(app_view("app-notes-01", "notes")),
        "/api/v1/applications/app-notes-01/status" => Reply::json(status("app-notes-01", "ready")),
        path => panic!("unexpected path {path}"),
    });
    let output = run(&second_server, &["show", "app-notes-01"]);
    let value = assert_json_success(&output);
    assert_eq!(value["status"]["state"], "ready");
    let _ = second_server.finish();
}

#[test]
fn plan_before_apply_confirmation_and_transport_retry_are_exercised() {
    for unix in [false, true] {
        let directory = tempdir().expect("manifest directory");
        let manifest = write_manifest(&directory);
        let mut mutation_requests = Vec::new();
        let server = start_server(unix, 3, move |request| match request.path.as_str() {
            "/api/v1/applications/plan" => Reply::json(plan("preview-00000001")),
            "/api/v1/applications/apply" => {
                mutation_requests.push(request.clone());
                assert_eq!(request.body, MANIFEST.as_bytes());
                if mutation_requests.len() == 1 {
                    Reply::dropped()
                } else {
                    assert_eq!(
                        request.headers.get("content-type"),
                        Some(&"application/toml".to_owned())
                    );
                    Reply::accepted(accepted("app-notes-01"))
                }
            }
            path => panic!("unexpected path {path}"),
        });
        let output = run(
            &server,
            &[
                "apply",
                "--file",
                manifest.to_str().expect("manifest path"),
                "--yes",
                "--no-wait",
            ],
        );
        let value = assert_json_success(&output);
        assert_eq!(value["application_id"], "app-notes-01");
        assert!(
            !output.stderr.is_empty(),
            "the plan must be displayed on stderr"
        );
        let records = server.finish();
        let retries = records
            .iter()
            .filter(|request| request.path == "/api/v1/applications/apply")
            .map(|request| request.body.clone())
            .collect::<Vec<_>>();
        assert_eq!(retries.len(), 2);
        assert_eq!(retries[0], retries[1]);
        let mutations = records
            .iter()
            .filter(|request| request.path == "/api/v1/applications/apply")
            .collect::<Vec<_>>();
        let key = mutations[0]
            .headers
            .get("idempotency-key")
            .expect("command has a replay identity");
        assert_eq!(Some(key), mutations[1].headers.get("idempotency-key"));
        assert_eq!(
            mutations[0]
                .headers
                .get("x-expected-generation")
                .map(String::as_str),
            Some("0")
        );
    }
}

#[test]
fn noninteractive_apply_stops_after_displaying_the_plan() {
    let directory = tempdir().expect("manifest directory");
    let manifest = write_manifest(&directory);
    let server = start_server(false, 1, move |request| match request.path.as_str() {
        "/api/v1/applications/plan" => Reply::json(plan("preview-00000001")),
        path => panic!("mutation must not be sent; got {path}"),
    });
    let output = run(
        &server,
        &["apply", "--file", manifest.to_str().expect("manifest path")],
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("confirmation"));
    let _ = server.finish();
}

#[test]
fn apply_reports_a_failed_operation_with_a_nonzero_exit() {
    let directory = tempdir().expect("manifest directory");
    let manifest = write_manifest(&directory);
    let server = start_server(false, 3, move |request| match request.path.as_str() {
        "/api/v1/applications/plan" => Reply::json(plan("preview-00000001")),
        "/api/v1/applications/apply" => Reply::accepted(accepted("app-notes-01")),
        "/api/v1/operations/operation-01" => Reply::json(operation("failed")),
        path => panic!("unexpected path {path}"),
    });
    let output = run(
        &server,
        &[
            "apply",
            "--file",
            manifest.to_str().expect("manifest path"),
            "--yes",
        ],
    );
    assert_eq!(output.status.code(), Some(5));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("runtime_failed"));
    assert!(stderr.contains("runtime reconciliation failed"));
    let _ = server.finish();
}

#[test]
fn manifest_input_is_missing_or_oversized_before_network_use() {
    let directory = tempdir().expect("manifest directory");
    let missing = directory.path().join("missing.toml");
    let oversized = directory.path().join("oversized.toml");
    fs::write(&oversized, vec![b'x'; 4 * 1024 * 1024 + 1]).expect("oversized manifest");
    let server = start_server(false, 0, |_| panic!("manifest input must fail locally"));

    let missing_output = run(
        &server,
        &["plan", "--file", missing.to_str().expect("manifest path")],
    );
    assert_eq!(missing_output.status.code(), Some(2));
    assert!(missing_output.stdout.is_empty());

    let oversized_output = run(
        &server,
        &["plan", "--file", oversized.to_str().expect("manifest path")],
    );
    assert_eq!(oversized_output.status.code(), Some(2));
    assert!(oversized_output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&oversized_output.stderr);
    assert!(stderr.contains("exceeds"));
    let _ = server.finish();
}

#[test]
fn delete_reports_named_volume_retention_and_operation_completion() {
    for unix in [false, true] {
        let server = start_server(unix, 4, move |request| match request.path.as_str() {
            "/api/v1/applications?limit=100" => {
                Reply::json(page(vec![app_summary("app-notes-01", "notes")], None))
            }
            "/api/v1/applications/app-notes-01" => Reply::json(app_view("app-notes-01", "notes")),
            "/api/v1/applications/app-notes-01?expected_generation=1" => {
                Reply::accepted(accepted("app-notes-01"))
            }
            "/api/v1/operations/operation-01" => Reply::json(operation("succeeded")),
            path => panic!("unexpected path {path}"),
        });
        let output = run(&server, &["delete", "notes", "--yes"]);
        let value = assert_json_success(&output);
        assert_eq!(value["volumes_retained"], true);
        assert!(String::from_utf8_lossy(&output.stderr).contains("named volumes are retained"));
        let _ = server.finish();
    }
}

#[test]
fn operation_polls_by_default_and_no_wait_fetches_once() {
    let mut calls = 0;
    let server = start_server(false, 2, move |request| {
        assert_eq!(request.path, "/api/v1/operations/operation-01");
        calls += 1;
        Reply::json(if calls == 1 {
            operation("requested")
        } else {
            operation("succeeded")
        })
    });
    let output = run(&server, &["operation", "operation-01"]);
    let value = assert_json_success(&output);
    assert_eq!(value["state"], "succeeded");
    let _ = server.finish();

    let server = start_server(false, 1, move |request| {
        assert_eq!(request.path, "/api/v1/operations/operation-01");
        Reply::json(operation("requested"))
    });
    let output = run(&server, &["operation", "operation-01", "--no-wait"]);
    let value = assert_json_success(&output);
    assert_eq!(value["state"], "requested");
    let _ = server.finish();
}

#[test]
fn timeout_and_ctrl_c_end_only_the_local_wait() {
    let timeout_server = start_server(false, 1, move |request| {
        assert_eq!(request.path, "/api/v1/operations/operation-01");
        Reply::json(operation("requested"))
    });
    let output = run_with_timeout(&timeout_server, &["operation", "operation-01"], "50ms");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("timed out"));
    let _ = timeout_server.finish();

    let interrupt_server = start_server(false, usize::MAX, move |request| {
        assert_eq!(request.path, "/api/v1/operations/operation-01");
        // Keep the request pending when SIGINT arrives, rather than only
        // testing interruption during the sleep between completed requests.
        thread::sleep(Duration::from_millis(300));
        Reply::dropped()
    });
    let mut output = None;
    for _ in 0..3 {
        // Rebuild the command each attempt; `Command::args` accumulates, so
        // reusing one command would append duplicate arguments.
        let mut command = Command::new(env!("CARGO_BIN_EXE_piquelctl"));
        if let Endpoint::Tcp(url) = &interrupt_server.endpoint {
            command.args(["--url", url]);
        } else {
            panic!("interrupt fixture uses TCP");
        }
        let child = command
            .args(["--timeout", "5s", "operation", "operation-01"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("piquelctl child");
        thread::sleep(Duration::from_millis(150));
        Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .expect("send SIGINT");
        let attempt = child.wait_with_output().expect("interrupted child");
        // A signal death before the handler was installed is a startup race;
        // retry instead of failing the test.
        if attempt.status.code() == Some(130) {
            output = Some(attempt);
            break;
        }
    }
    let output = output.expect("interrupted run reported an exit code");
    assert_eq!(output.status.code(), Some(130));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("was not cancelled"));
    let _ = interrupt_server.stop();
}

#[test]
fn unknown_names_exit_with_input_error_and_no_mutation() {
    let server = start_server(false, 1, move |request| match request.path.as_str() {
        "/api/v1/applications?limit=100" => Reply::json(page(Vec::new(), None)),
        path => panic!("unexpected path {path}"),
    });
    let output = run(&server, &["show", "missing"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("was not found"));
    let _ = server.finish();
}

#[test]
fn ambiguous_names_report_the_match_count() {
    let server = start_server(false, 1, move |request| match request.path.as_str() {
        "/api/v1/applications?limit=100" => Reply::json(page(
            vec![
                app_summary("app-notes-01", "notes"),
                app_summary("app-notes-02", "notes"),
            ],
            None,
        )),
        path => panic!("unexpected path {path}"),
    });
    let output = run(&server, &["show", "notes"]);
    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("matched 2 applications"));
    let _ = server.finish();
}

#[test]
fn read_request_uses_the_full_command_timeout() {
    let server = start_server(false, 1, |_| {
        thread::sleep(Duration::from_millis(650));
        Reply::json(
            json!({"status": "ok", "daemon_version": "test", "api_version": "v1", "instance_id": "test"}),
        )
    });
    let output = run_with_timeout(&server, &["status"], "1s");
    assert_json_success(&output);
    server.finish();
}

#[test]
fn fifo_manifest_is_rejected_before_opening() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("manifest.fifo");
    assert!(
        Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo")
            .success()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_piquelctl"))
        .args(["--timeout", "100ms", "plan", "--file"])
        .arg(path)
        .output()
        .expect("CLI rejects FIFO");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("not a regular file"));
}

#[test]
fn human_output_reports_a_closed_pipe_without_panicking() {
    let server = start_server(false, 1, |_| Reply::json(operation("requested")));
    let Endpoint::Tcp(url) = &server.endpoint else {
        panic!("TCP fixture")
    };
    let (reader, writer) = std::os::unix::net::UnixStream::pair().expect("output pipe");
    drop(reader);
    let output = Command::new(env!("CARGO_BIN_EXE_piquelctl"))
        .args(["--url", url, "operation", "operation-01", "--no-wait"])
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(writer)))
        .output()
        .expect("CLI reports output failure");
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("could not write output"), "stderr: {error}");
    assert!(!error.contains("panicked"));
    server.finish();
}

#[test]
fn reconcile_and_refresh_target_current_intent_and_retry_transport() {
    for action in ["reconcile", "refresh"] {
        let mut attempts = 0;
        let server = start_server(false, 3, move |request| {
            if request.method == "GET" {
                return Reply::json(app_view("app-notes-01", "notes"));
            }
            assert_eq!(request.method, "POST");
            assert_eq!(
                request.path,
                format!("/api/v1/applications/app-notes-01/{action}")
            );
            attempts += 1;
            if attempts == 1 {
                Reply::dropped()
            } else {
                Reply::accepted(accepted("app-notes-01"))
            }
        });
        let output = run(&server, &[action, "app-notes-01", "--yes", "--no-wait"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["generation"], 1);
        assert_eq!(server.finish().len(), 3);
    }
}

#[test]
fn events_cli_reads_a_filtered_page() {
    let server = start_server(false, 1, |request| {
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.path,
            "/api/v1/events?application_id=app-notes-01&cursor=v1%3A7&limit=2"
        );
        Reply::json(
            json!({"items":[{"id":8,"application_id":"app-notes-01","operation_id":"operation-01","generation":1,"attempt":2,"kind":"operation_succeeded","message":null,"error_code":null,"phase":null,"resource":null,"created_at_ms":123}],"next_cursor":null}),
        )
    });
    let output = run(
        &server,
        &[
            "events",
            "--application",
            "app-notes-01",
            "--cursor",
            "v1:7",
            "--limit",
            "2",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["items"][0]["attempt"], 2);
    assert_eq!(server.finish().len(), 1);
}

#[test]
fn identical_apply_waits_without_confirmation_or_another_mutation() {
    for final_state in ["succeeded", "superseded", "failed", "cancelled"] {
        let directory = tempdir().unwrap();
        let manifest = write_manifest(&directory);
        let server = start_server(false, 2, move |request| {
            if request.path == "/api/v1/applications/plan" {
                let mut preview = plan("app-notes-01");
                preview["identical"] = json!(true);
                preview["operation"] = operation("running");
                return Reply::json(preview);
            }
            assert_eq!(request.method, "GET");
            assert_eq!(request.path, "/api/v1/operations/operation-01");
            Reply::json(operation(final_state))
        });
        let output = run(&server, &["apply", "--file", manifest.to_str().unwrap()]);
        if matches!(final_state, "succeeded" | "superseded") {
            let result = assert_json_success(&output);
            assert_eq!(result["identical"], true);
            assert_eq!(result["outcome"], final_state);
            assert_eq!(result["operation"]["state"], final_state);
        } else {
            assert_eq!(output.status.code(), Some(5));
            assert!(String::from_utf8_lossy(&output.stderr).contains("reconcile"));
        }
        assert_eq!(server.finish().len(), 2);
    }
}

#[test]
fn identical_apply_does_not_poll_when_finished_or_no_wait_is_requested() {
    for (state, no_wait) in [("succeeded", false), ("requested", true), ("running", true)] {
        let directory = tempdir().unwrap();
        let manifest = write_manifest(&directory);
        let server = start_server(false, 1, move |request| {
            assert_eq!(request.path, "/api/v1/applications/plan");
            let mut preview = plan("app-notes-01");
            preview["identical"] = json!(true);
            preview["operation"] = operation(state);
            Reply::json(preview)
        });
        let mut args = vec!["apply", "--file", manifest.to_str().unwrap()];
        if no_wait {
            args.push("--no-wait");
        }
        let output = run(&server, &args);
        let result = assert_json_success(&output);
        assert_eq!(result["operation"]["state"], state);
        assert_eq!(server.finish().len(), 1);
    }
}

#[test]
fn apply_protects_the_preview_identity_and_revision_with_confirmation_skipped() {
    let directory = tempdir().unwrap();
    let manifest = write_manifest(&directory);
    let server = start_server(false, 2, move |request| {
        if request.path == "/api/v1/applications/plan" {
            let mut preview = plan("app-notes-01");
            preview["generation"] = json!(7);
            return Reply::json(preview);
        }
        assert_eq!(
            request
                .headers
                .get("x-expected-generation")
                .map(String::as_str),
            Some("7")
        );
        assert_eq!(
            request
                .headers
                .get("x-expected-application-id")
                .map(String::as_str),
            Some("app-notes-01")
        );
        let mut reply = Reply::json(Value::Null);
        reply.status = "409 Conflict";
        reply.body=serde_json::to_vec(&json!({"code":"generation_conflict","message":"Application changed since inspection; run the command again"})).unwrap();
        reply
    });
    let output = run(
        &server,
        &["apply", "--file", manifest.to_str().unwrap(), "--yes"],
    );
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("run the command again"));
    assert_eq!(
        server.finish().len(),
        2,
        "generation conflicts must not retry"
    );
}

#[test]
fn rename_uses_the_inspected_revision_and_preserves_identity() {
    let server = start_server(false, 2, |request| {
        if request.method == "GET" {
            return Reply::json(app_view("app-notes-01", "notes"));
        }
        assert_eq!(request.path, "/api/v1/applications/app-notes-01/rename");
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body, json!({"name":"renamed","expected_generation":1}));
        assert!(request.headers.contains_key("idempotency-key"));
        Reply::json(json!({"application_id":"app-notes-01","name":"renamed","generation":2}))
    });
    let output = run(&server, &["rename", "app-notes-01", "renamed", "--yes"]);
    assert_eq!(
        assert_json_success(&output)["application_id"],
        "app-notes-01"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("metadata.name"));
    assert_eq!(server.finish().len(), 2);
}

#[test]
fn force_does_not_skip_confirmation_and_explicit_force_is_sent_to_the_endpoint() {
    for yes in [false, true] {
        let directory = tempdir().unwrap();
        let manifest = write_manifest(&directory);
        let mut mutations = 0;
        let server = start_server(false, if yes { 3 } else { 1 }, move |request| {
            if request.path == "/api/v1/applications/plan" {
                return Reply::json(plan("app-notes-01"));
            }
            assert_eq!(request.path, "/api/v1/applications/apply?force=true");
            assert!(!request.headers.contains_key("x-expected-generation"));
            assert!(!request.headers.contains_key("x-expected-application-id"));
            mutations += 1;
            if mutations == 1 {
                Reply::dropped()
            } else {
                Reply::accepted(accepted("app-notes-01"))
            }
        });
        let mut args = vec![
            "apply",
            "--file",
            manifest.to_str().unwrap(),
            "--force",
            "--no-wait",
        ];
        if yes {
            args.push("--yes");
        }
        let output = run(&server, &args);
        if yes {
            assert_json_success(&output);
        } else {
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("--yes"));
        }
        let requests = server.finish();
        if yes {
            assert_eq!(
                requests[1].headers.get("idempotency-key"),
                requests[2].headers.get("idempotency-key")
            );
        }
    }
}

#[test]
fn apply_returns_superseded_success_without_following_the_replacement() {
    let directory = tempdir().unwrap();
    let manifest = write_manifest(&directory);
    let server = start_server(false, 3, |request| match request.path.as_str() {
        "/api/v1/applications/plan" => Reply::json(plan("app-notes-01")),
        "/api/v1/applications/apply" => Reply::accepted(accepted("app-notes-01")),
        "/api/v1/operations/operation-01" => Reply::json(operation("superseded")),
        _ => panic!("unexpected request {}", request.path),
    });
    let output = run(
        &server,
        &["apply", "--file", manifest.to_str().unwrap(), "--yes"],
    );
    let result = assert_json_success(&output);
    assert_eq!(result["outcome"], "superseded");
    assert_eq!(result["operation"]["state"], "superseded");
    assert_eq!(server.finish().len(), 3);
}
