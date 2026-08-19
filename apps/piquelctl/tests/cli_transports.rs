//! Grouped CLI, bearer-token, binary-output, and transport contract checks.

use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    process::{Command, Output},
    thread,
};
use tempfile::TempDir;

const TOKEN: &str = "operator-token-canary";
const SECRET: &str = "secret-value-canary";
const APP: &str = r#"{"application":{"id":"app-notes-01","api_version":"piqueld.dev/v1alpha1","kind":"Application","metadata":{"name":"notes"},"spec":{"services":[{"name":"web","source":{"type":"image","image":"example.test/notes:1"},"replicas":1,"environment":{},"command":[],"arguments":[],"ports":[],"mounts":[],"secrets":[],"healthcheck":null,"resources":null}],"volumes":[],"routes":[]}},"generation":1,"spec_hash":"sha256:test","delete_intent":false,"created_at_ms":1,"updated_at_ms":1}"#;
const OPERATION: &str = r#"{"id":"operation-01","application_id":"app-notes-01","generation":1,"kind":"reconcile","state":"completed","error_code":null,"error_message":null,"created_at_ms":1,"updated_at_ms":2,"started_at_ms":1,"finished_at_ms":2,"steps":[]}"#;

#[derive(Clone)]
struct Reply {
    request: &'static str,
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
    body_canary: Option<&'static str>,
}

impl Reply {
    fn json(request: &'static str, data: &str) -> Self {
        Self {
            request,
            status: "200 OK",
            content_type: "application/json",
            body: format!(r#"{{"data":{data}}}"#).into_bytes(),
            body_canary: None,
        }
    }

    fn raw(request: &'static str, content_type: &'static str, body: &[u8]) -> Self {
        Self {
            request,
            status: "200 OK",
            content_type,
            body: body.to_vec(),
            body_canary: None,
        }
    }
}

fn page(items: &str) -> String {
    format!(r#"{{"items":[{items}],"next_cursor":null}}"#)
}

fn replies() -> Vec<Reply> {
    let app_page = page(APP);
    let plan = r#"{"application_id":"app-notes-01","proposed_generation":1,"plan":{"actions":[],"diagnostics":[],"summary":{"action_count":0,"mutation_count":0,"destructive_count":0,"blocking_conflicts":0,"by_action":{}}}}"#;
    let accepted =
        r#"{"operation_id":"operation-01","application_id":"app-notes-01","generation":1}"#;
    let metadata = r#"{"name":"database","value_is_set":true,"generation":1,"created_at_ms":1,"updated_at_ms":1,"references":[]}"#;
    let mut secret = Reply::json("POST /api/v1/secrets/database ", metadata);
    secret.body_canary = Some(SECRET);
    vec![
        Reply::json(
            "GET /api/v1/system/status ",
            r#"{"status":"ok","api_version":"v1","daemon_version":"0.1.0","instance_id":"test-instance"}"#,
        ),
        Reply::json(
            "GET /api/v1/system/capabilities ",
            r#"{"persistence":true,"source_resolution":true,"runtime_observation":true,"runtime_execution":true,"secret_management":true,"reason":null}"#,
        ),
        Reply::json("GET /api/v1/applications?", &app_page),
        Reply::json(
            "GET /api/v1/applications/app-notes-01/status ",
            r#"{"application_id":"app-notes-01","state":"ready","observed_generation":1,"message":null,"updated_at_ms":1,"infrastructure":"ready","services":[]}"#,
        ),
        Reply::json("GET /api/v1/applications?", &app_page),
        Reply::json(
            "GET /api/v1/applications/app-notes-01/status ",
            r#"{"application_id":"app-notes-01","state":"ready","observed_generation":1,"message":null,"updated_at_ms":1,"infrastructure":"ready","services":[]}"#,
        ),
        Reply::json("GET /api/v1/applications?", &app_page),
        Reply::json("POST /api/v1/applications/app-notes-01/plan ", plan),
        Reply::json("GET /api/v1/applications?", &app_page),
        Reply::json("POST /api/v1/applications/app-notes-01/plan ", plan),
        Reply::json("PUT /api/v1/applications/app-notes-01 ", accepted),
        Reply::json("GET /api/v1/applications?", &app_page),
        Reply::raw(
            "GET /api/v1/applications/app-notes-01/export?include_resolved=false ",
            "application/toml",
            b"api_version = \"piqueld.dev/v1alpha1\"\n",
        ),
        Reply::json("GET /api/v1/applications?", &app_page),
        Reply::raw(
            "GET /api/v1/applications/app-notes-01/export?include_resolved=false ",
            "application/toml",
            b"api_version = \"piqueld.dev/v1alpha1\"\n",
        ),
        Reply::json("GET /api/v1/applications?", &app_page),
        Reply::json("DELETE /api/v1/applications/app-notes-01 ", accepted),
        Reply::json("GET /api/v1/operations/operation-01 ", OPERATION),
        Reply::json("GET /api/v1/applications?", &app_page),
        Reply::json(
            "GET /api/v1/applications/app-notes-01/logs?",
            r#"[{"service":"web","task_id":"task","container_id":"container","timestamp":"2026-01-01T00:00:00Z","stream":"stdout","message":"hello","display_message":"hello"}]"#,
        ),
        Reply::json("GET /api/v1/secrets?", &page(metadata)),
        Reply {
            request: "GET /api/v1/secrets/database ",
            status: "404 Not Found",
            content_type: "application/json",
            body: br#"{"code":"secret_not_found","message":"not found","request_id":"request-1"}"#
                .to_vec(),
            body_canary: None,
        },
        secret,
        Reply::json("GET /api/v1/secrets/database ", metadata),
        Reply {
            request: "DELETE /api/v1/secrets/database ",
            status: "204 No Content",
            content_type: "application/json",
            body: Vec::new(),
            body_canary: None,
        },
        Reply::json("GET /api/v1/operations/operation-01 ", OPERATION),
        Reply::json("GET /api/v1/operations/operation-01/builds ", &page("")),
        Reply::raw(
            "GET /api/v1/state/export?mode=portable ",
            "application/vnd.piqueld.state-v1+tar",
            b"archive-bytes",
        ),
        Reply::json(
            "POST /api/v1/state/import/confirm ",
            r#"{"token":"replace-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","archive_digest":"sha256:0c982986710a026635603031674053ca851fc0e3ea760094a34f59b84f7f6da6","expires_at_ms":999999}"#,
        ),
        Reply::json(
            "POST /api/v1/state/import ",
            r#"{"operation_id":"import-operation","archive_digest":"sha256:0c982986710a026635603031674053ca851fc0e3ea760094a34f59b84f7f6da6","applications_imported":1,"secrets_imported":0,"dependencies":{"source_instance_id":"source","target_instance_id":"target","ownership_compatible":false,"missing_secret_values":[],"incompatible_secret_keys":[],"image_references_to_verify":[],"git_sources_to_resolve":[],"runtime_secrets_to_recreate":[],"retained_volumes_to_verify":["notes-data"],"notes":["volumes retained"]}}"#,
        ),
    ]
}

enum Transport {
    Tcp(String),
    Unix(PathBuf),
}

fn serve(mut stream: impl Read + Write, reply: &Reply) {
    let mut request = Vec::new();
    let header_end = loop {
        let mut buffer = [0_u8; 4096];
        let read = stream.read(&mut buffer).expect("request");
        assert_ne!(read, 0);
        request.extend_from_slice(&buffer[..read]);
        if let Some(position) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
    assert!(headers.contains(&format!("authorization: bearer {TOKEN}")));
    let length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while request.len() < header_end + length {
        let mut buffer = [0_u8; 4096];
        let read = stream.read(&mut buffer).expect("request body");
        assert_ne!(read, 0);
        request.extend_from_slice(&buffer[..read]);
    }
    let first = headers.lines().next().expect("request line");
    let expected = reply.request.to_ascii_lowercase();
    assert!(
        first.starts_with(expected.split_whitespace().next().unwrap()),
        "expected {:?}, got {:?}",
        reply.request,
        first
    );
    assert!(
        first.contains(expected.split_whitespace().nth(1).unwrap()),
        "expected {:?}, got {:?}",
        reply.request,
        first
    );
    if let Some(canary) = reply.body_canary {
        assert_eq!(&request[header_end..header_end + length], canary.as_bytes());
    }
    let response = format!(
        "HTTP/1.1 {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        reply.status,
        reply.content_type,
        reply.body.len()
    );
    stream
        .write_all(response.as_bytes())
        .expect("response headers");
    stream.write_all(&reply.body).expect("response body");
}

fn spawn_server(
    directory: &Path,
    replies: Vec<Reply>,
    unix: bool,
) -> (Transport, thread::JoinHandle<()>) {
    if unix {
        let path = directory.join("api.sock");
        let listener = UnixListener::bind(&path).expect("unix listener");
        let handle = thread::spawn(move || {
            for reply in &replies {
                serve(listener.accept().expect("unix connection").0, reply);
            }
        });
        (Transport::Unix(path), handle)
    } else {
        let listener = TcpListener::bind("127.0.0.1:0").expect("tcp listener");
        let url = format!("http://{}", listener.local_addr().expect("address"));
        let handle = thread::spawn(move || {
            for reply in &replies {
                serve(listener.accept().expect("tcp connection").0, reply);
            }
        });
        (Transport::Tcp(url), handle)
    }
}

fn run(transport: &Transport, profiles: &Path, args: &[String]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_piquelctl"));
    match transport {
        Transport::Tcp(url) => command.args(["--url", url]),
        Transport::Unix(path) => command.args(["--socket", path.to_str().expect("socket")]),
    };
    command
        .args([
            "--profiles-file",
            profiles.to_str().expect("profiles"),
            "--output",
            "json",
        ])
        .args(args)
        .env("PIQUELD_TOKEN", TOKEN)
        .output()
        .expect("piquelctl process")
}

fn assert_success(output: &Output, kind: &str) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).expect("clean JSON output");
    assert_eq!(value["schema"], "piquelctl.v1");
    assert_eq!(value["kind"], kind);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stdout.contains(TOKEN) && !stderr.contains(TOKEN));
    assert!(!stdout.contains(SECRET) && !stderr.contains(SECRET));
}

// This is one deliberately ordered end-to-end fixture covering the complete
// grouped command contract on both transports.
#[allow(clippy::too_many_lines)]
fn exercise(unix: bool) {
    let directory = tempfile::tempdir().expect("directory");
    let profiles = directory.path().join("missing.toml");
    let manifest = directory.path().join("notes.toml");
    fs::write(&manifest, "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='notes'\n[[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='example.test/notes:1'\n").expect("manifest");
    let secret = directory.path().join("secret");
    fs::write(&secret, SECRET).expect("secret");
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).expect("permissions");
    let archive = directory.path().join("import.tar");
    fs::write(&archive, b"archive-bytes").expect("archive");
    let exported = directory.path().join("export.tar");
    let exported_application = directory.path().join("notes-export.toml");
    let (transport, server) = spawn_server(directory.path(), replies(), unix);
    let cases: Vec<(Vec<String>, &str)> = vec![
        (vec!["status".into()], "status"),
        (
            vec!["application".into(), "list".into()],
            "application_list",
        ),
        (
            vec!["application".into(), "show".into(), "notes".into()],
            "application_show",
        ),
        (
            vec![
                "application".into(),
                "plan".into(),
                "--file".into(),
                manifest.to_string_lossy().into_owned(),
            ],
            "application_plan",
        ),
        (
            vec![
                "application".into(),
                "apply".into(),
                "--file".into(),
                manifest.to_string_lossy().into_owned(),
                "--yes".into(),
                "--no-wait".into(),
            ],
            "application_apply",
        ),
        (
            vec!["application".into(), "export".into(), "notes".into()],
            "application_export",
        ),
        (
            vec![
                "application".into(),
                "export".into(),
                "notes".into(),
                "--file".into(),
                exported_application.to_string_lossy().into_owned(),
            ],
            "application_export",
        ),
        (
            vec![
                "application".into(),
                "delete".into(),
                "notes".into(),
                "--yes".into(),
            ],
            "application_delete",
        ),
        (
            vec!["application".into(), "logs".into(), "notes".into()],
            "application_logs",
        ),
        (vec!["secret".into(), "list".into()], "secret_list"),
        (
            vec![
                "secret".into(),
                "set".into(),
                "database".into(),
                "--file".into(),
                secret.to_string_lossy().into_owned(),
            ],
            "secret_set",
        ),
        (
            vec![
                "secret".into(),
                "delete".into(),
                "database".into(),
                "--yes".into(),
            ],
            "secret_delete",
        ),
        (
            vec!["operation".into(), "watch".into(), "operation-01".into()],
            "operation_watch",
        ),
        (
            vec![
                "state".into(),
                "export".into(),
                "--file".into(),
                exported.to_string_lossy().into_owned(),
            ],
            "state_export",
        ),
        (
            vec![
                "state".into(),
                "import".into(),
                archive.to_string_lossy().into_owned(),
                "--replace".into(),
                "--yes".into(),
            ],
            "state_import",
        ),
    ];
    for (args, kind) in cases {
        assert_success(&run(&transport, &profiles, &args), kind);
    }
    server.join().expect("server");
    assert_eq!(fs::read(exported).expect("state output"), b"archive-bytes");
    assert_eq!(
        fs::read_to_string(exported_application).expect("application output"),
        "api_version = \"piqueld.dev/v1alpha1\"\n"
    );
}

#[test]
fn grouped_commands_use_tcp_with_stable_json() {
    exercise(false);
}

#[test]
fn grouped_commands_use_unix_with_stable_json() {
    exercise(true);
}

#[test]
fn unsafe_binary_stdout_and_missing_replace_are_refused_before_network_use() {
    let directory: TempDir = tempfile::tempdir().expect("directory");
    let archive = directory.path().join("archive.tar");
    fs::write(&archive, b"archive").expect("archive");
    let profiles = directory.path().join("missing.toml");
    let output = Command::new(env!("CARGO_BIN_EXE_piquelctl"))
        .args([
            "--profiles-file",
            profiles.to_str().expect("profiles"),
            "--output",
            "json",
            "state",
            "import",
            archive.to_str().expect("archive"),
        ])
        .output()
        .expect("process");
    assert_eq!(output.status.code(), Some(9));
    assert!(output.stdout.is_empty());
}
