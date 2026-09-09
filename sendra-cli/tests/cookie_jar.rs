//! End-to-end coverage for `--cookie-jar`/`cookie_jar:` — the invocation-level
//! setting that reaches [`sendra_core::build_client`] through
//! [`Config`](sendra_core::Config), same tier as `--insecure`/`--proxy` in
//! [`insecure_and_proxy`](insecure_and_proxy.rs).
//!
//! The server here is a small hand-rolled `TcpListener`, like every other
//! integration test's fixture in this crate: it records the `Cookie` header
//! (or its absence) of every request it receives, in order, and answers
//! `/login` with a `Set-Cookie` and `/profile` with `200` only when the
//! request already carries the cookie `/login` set — exactly the
//! login-flow shape the feature exists for.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

fn sendra(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sendra"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("the binary under test runs")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "the run should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "the run should fail: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A login-flow-shaped server: `/login` always sets a session cookie,
/// `/profile` answers `200` only when the request already carries it and
/// `401` otherwise. Also records the `Cookie` header (or `None`) of every
/// request it receives, in order — `--repeat`'s reset test reads this
/// directly rather than through an assertion, so it can name exactly which
/// pass a leaked cookie would show up on.
struct LoginServer {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Option<String>>>>,
}

impl LoginServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
        let addr = listener.local_addr().expect("the listener has an address");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stored = seen.clone();

        std::thread::spawn(move || {
            // A rebuilt client (`--cookie-jar` under `--repeat`) opens a new
            // connection, so this server must keep accepting connections
            // after one closes rather than stopping the whole thread the
            // way this file's other single-connection fixtures do.
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let mut writer = stream.try_clone().expect("the socket clones");
                let mut reader = BufReader::new(stream);

                'requests: loop {
                    let mut request_line = String::new();
                    match reader.read_line(&mut request_line) {
                        Ok(0) | Err(_) => break 'requests,
                        Ok(_) => {}
                    }
                    let path = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_string();

                    let mut cookie = None;
                    loop {
                        let mut header = String::new();
                        match reader.read_line(&mut header) {
                            Ok(0) | Err(_) => break 'requests,
                            Ok(_) if header == "\r\n" => break,
                            Ok(_) => {
                                if let Some((name, value)) = header.split_once(':') {
                                    if name.trim().eq_ignore_ascii_case("cookie") {
                                        cookie = Some(value.trim().to_string());
                                    }
                                }
                            }
                        }
                    }
                    stored.lock().unwrap().push(cookie.clone());

                    let response: Vec<u8> = if path == "/login" {
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                          Set-Cookie: session=abc123; Path=/\r\nContent-Length: 10\r\n\r\nlogged in!"
                            .to_vec()
                    } else if path == "/profile" && cookie.as_deref() == Some("session=abc123") {
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 7\r\n\r\nwelcome"
                            .to_vec()
                    } else {
                        b"HTTP/1.1 401 Unauthorized\r\nContent-Type: text/plain\r\nContent-Length: 6\r\n\r\ndenied"
                            .to_vec()
                    };

                    if writer.write_all(&response).is_err() {
                        break 'requests;
                    }
                    let _ = writer.flush();
                }
            }
        });

        Self { addr, seen }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn seen(&self) -> Vec<Option<String>> {
        self.seen.lock().unwrap().clone()
    }
}

fn login_flow_collection(server: &LoginServer) -> String {
    format!(
        "requests:\n  \
         - name: Login\n    method: GET\n    url: '{}'\n  \
         - name: Profile\n    method: GET\n    url: '{}'\n    assertions:\n      status: 200\n",
        server.url("/login"),
        server.url("/profile"),
    )
}

// --- opt-in: on vs off ------------------------------------------------------

#[test]
fn without_cookie_jar_the_follow_up_request_does_not_see_the_session_cookie() {
    // The default, proven rather than assumed: no `--cookie-jar` means
    // `/profile` never receives the cookie `/login` set, so its `status:
    // 200` assertion fails and `sendra test` reports the run as a failure.
    let server = LoginServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("collection.yaml"),
        login_flow_collection(&server),
    )
    .unwrap();

    assert_failure(&sendra(dir.path(), &["test", "collection.yaml"]));

    let seen = server.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[1], None,
        "no --cookie-jar: the session cookie must not have reached /profile"
    );
}

#[test]
fn cookie_jar_flag_makes_the_login_flow_collection_pass() {
    let server = LoginServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("collection.yaml"),
        login_flow_collection(&server),
    )
    .unwrap();

    assert_success(&sendra(
        dir.path(),
        &["test", "collection.yaml", "--cookie-jar"],
    ));

    let seen = server.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[1].as_deref(),
        Some("session=abc123"),
        "--cookie-jar: /profile should have received the cookie /login set: {:?}",
        seen[1]
    );
}

#[test]
fn cookie_jar_config_key_makes_the_login_flow_collection_pass() {
    let server = LoginServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::create_dir_all(dir.path().join(".sendra")).unwrap();
    std::fs::write(dir.path().join(".sendra/config.yaml"), "cookie_jar: true\n").unwrap();
    std::fs::write(
        dir.path().join("collection.yaml"),
        login_flow_collection(&server),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["test", "collection.yaml"]));
}

// --- manually-set `Cookie` header -------------------------------------------

#[test]
fn a_manually_set_cookie_header_is_sent_as_is_even_with_the_jar_enabled() {
    // Investigated, not assumed: reqwest only fills in the jar's `Cookie`
    // header when a request does not already carry one, so a request's own
    // `headers: { Cookie: ... }` reaches the server unchanged, and Sendra
    // raises no conflict over the combination.
    let server = LoginServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("collection.yaml"),
        format!(
            "requests:\n  \
             - name: Login\n    method: GET\n    url: '{}'\n  \
             - name: Profile\n    method: GET\n    url: '{}'\n    \
               headers:\n      Cookie: session=manual-override\n",
            server.url("/login"),
            server.url("/profile"),
        ),
    )
    .unwrap();

    // `run`, not `test`: `/profile` answers 401 either way here (the manual
    // cookie does not match what `/login` set), and this is about what the
    // server actually received, not about an assertion's verdict.
    // `--allow-error-status` so that 401 does not fail the invocation —
    // this test is about the header the server received, not the status.
    assert_success(&sendra(
        dir.path(),
        &[
            "run",
            "collection.yaml",
            "--cookie-jar",
            "--allow-error-status",
            "-o",
            "none",
        ],
    ));

    let seen = server.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[1].as_deref(),
        Some("session=manual-override"),
        "the request's own Cookie header must reach the server unchanged, \
         not merged with the jar's stored cookie: {:?}",
        seen[1]
    );
}

// --- `--repeat` resets the jar between passes -------------------------------

#[test]
fn repeat_gives_every_pass_a_fresh_cookie_jar() {
    // `Profile` is sent *before* `Login` in this collection on purpose: if
    // the jar were not reset between passes, pass 2's `Profile` — the first
    // request of that pass — would carry the cookie pass 1's `Login` set at
    // the very end of pass 1. Each pass starting with an empty jar is what
    // this asserts directly, by reading the server's own record of what
    // arrived, rather than through an assertion's pass/fail verdict.
    let server = LoginServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("collection.yaml"),
        format!(
            "requests:\n  \
             - name: Profile\n    method: GET\n    url: '{}'\n  \
             - name: Login\n    method: GET\n    url: '{}'\n",
            server.url("/profile"),
            server.url("/login"),
        ),
    )
    .unwrap();

    // `--allow-error-status`: pass 1's `Profile` is expected to answer 401
    // (nothing has logged in yet), and this test is about what the server
    // received, not about the run's exit code.
    assert_success(&sendra(
        dir.path(),
        &[
            "run",
            "collection.yaml",
            "--cookie-jar",
            "--repeat",
            "2",
            "--allow-error-status",
            "-o",
            "none",
        ],
    ));

    let seen = server.seen();
    assert_eq!(seen.len(), 4, "two requests, two passes: {seen:?}");
    // index 0: pass 1's Profile — no cookie exists yet.
    assert_eq!(seen[0], None);
    // index 1: pass 1's Login — sets the cookie.
    // index 2: pass 2's Profile — must NOT carry pass 1's cookie forward.
    assert_eq!(
        seen[2], None,
        "pass 2 must start with an empty jar, not pass 1's leftover cookie: {seen:?}"
    );
    // index 3: pass 2's Login — sets the cookie again, irrelevant here.
}
