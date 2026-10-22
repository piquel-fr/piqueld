//! Startup must exclude competing processes before touching durable state,
//! and needs nothing else, not even Docker, to serve.

#[path = "support/daemon.rs"]
mod daemon;

use parking_lot::Mutex;
use piqueld::DirectoryLock;
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    process::{Child, Command, Stdio},
    sync::{Arc, Barrier},
    time::{Duration, Instant},
};

// A concurrent spawn can inherit another test's flock until exec closes the
// descriptor. Keep process spawning separate from assertions about immediate
// lock release; the competing threads within the acquisition test still race.
static PROCESS_TEST: Mutex<()> = Mutex::new(());

#[test]
fn simultaneous_acquisition_has_one_owner() {
    let _process_test = PROCESS_TEST.lock();
    let directory = tempfile::tempdir().unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let successes = std::thread::scope(|scope| {
        let jobs: Vec<_> = (0..2)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let path = directory.path();
                scope.spawn(move || {
                    barrier.wait();
                    let lock = DirectoryLock::acquire(path);
                    barrier.wait();
                    match lock {
                        Ok(_held) => true,
                        Err(error) => {
                            assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
                            false
                        }
                    }
                })
            })
            .collect();
        jobs.into_iter()
            .map(|job| job.join().unwrap())
            .filter(|acquired| *acquired)
            .count()
    });
    assert_eq!(successes, 1);
    DirectoryLock::acquire(directory.path()).unwrap();
}

#[test]
fn competing_daemon_preserves_database_and_live_socket() {
    let _process_test = PROCESS_TEST.lock();
    let directory = tempfile::tempdir_in(".").unwrap();
    let path = directory.path().canonicalize().unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let _owner = DirectoryLock::acquire(&path).unwrap();
    let database = path.join("piqueld.db");
    std::fs::write(&database, b"untouched database").unwrap();
    let socket = path.join("piqueld.sock");
    // Bound relative to the working directory, so its path fits a socket
    // address even in deeply nested checkouts.
    let cwd = std::env::current_dir().unwrap();
    let _listener =
        std::os::unix::net::UnixListener::bind(socket.strip_prefix(cwd).unwrap()).unwrap();
    let inode = std::fs::metadata(&socket).unwrap().ino();
    let config = path.join("config.toml");
    // HTTP is disabled so TCP binding cannot accidentally provide exclusivity.
    std::fs::write(
        &config,
        format!("[server]\ndata_dir={:?}\n", path.to_str().unwrap()),
    )
    .unwrap();
    let error = daemon::refused_start(&config);
    assert!(error.contains("directory already in use"), "{error}");
    assert_eq!(std::fs::read(database).unwrap(), b"untouched database");
    assert_eq!(std::fs::metadata(socket).unwrap().ino(), inode);
}

struct LockProcess(Child);

impl LockProcess {
    fn start(path: &std::path::Path) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "lock_holder_process", "--nocapture"])
            .env("PIQUELD_TEST_LOCK_DIRECTORY", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut ready = false;
        for line in BufReader::new(stdout).lines() {
            if line.unwrap() == "lock acquired" {
                ready = true;
                break;
            }
        }
        let process = Self(child);
        assert!(ready, "lock holder exited without acquiring its lock");
        process
    }
}

impl Drop for LockProcess {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
        }
        self.0.wait().unwrap();
    }
}

#[test]
fn process_death_releases_the_directory_lock() {
    let _process_test = PROCESS_TEST.lock();
    let directory = tempfile::tempdir().unwrap();
    let process = LockProcess::start(directory.path());
    assert_eq!(
        DirectoryLock::acquire(directory.path()).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    drop(process); // Kill and reap the owner, without a graceful unlock.
    DirectoryLock::acquire(directory.path()).unwrap();
}

#[test]
fn lock_holder_process() {
    let Some(path) = std::env::var_os("PIQUELD_TEST_LOCK_DIRECTORY") else {
        return;
    };
    let _lock = DirectoryLock::acquire(std::path::Path::new(&path)).unwrap();
    println!("lock acquired");
    std::io::stdout().flush().unwrap();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).unwrap();
}

/// Sends `request` to the Unix API `socket` and returns the whole response,
/// waiting up to 30 seconds for the daemon to listen.
fn unix_request(socket: &std::path::Path, request: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut stream = loop {
        match std::os::unix::net::UnixStream::connect(socket) {
            Ok(stream) => break stream,
            Err(error) => {
                assert!(Instant::now() < deadline, "the API never listened: {error}");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[test]
fn the_api_serves_without_docker_and_stops_on_sigterm() {
    let _process_test = PROCESS_TEST.lock();
    // Not below the working directory: the socket address must stay short.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let runtime = path.join("run");
    std::fs::create_dir(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    // Docker has not started: its socket does not exist.
    let docker = path.join("docker.sock");
    let config = path.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\ndata_dir={:?}\nruntime_dir={runtime:?}\n[docker]\nsocket={docker:?}\n",
            path.join("state")
        ),
    )
    .unwrap();
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_piqueld"))
        .arg("--config")
        .arg(&config)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let response = unix_request(
        &runtime.join("piqueld.sock"),
        "GET /api/v1/system/readiness HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    rustix::process::kill_process(
        rustix::process::Pid::from_child(&daemon),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = daemon.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            daemon.kill().unwrap();
            panic!("the daemon did not stop on SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    assert!(
        response.starts_with("HTTP/1.1 503"),
        "readiness must answer, reporting Docker unavailable: {response}"
    );
    assert!(status.success(), "{status}");
}
