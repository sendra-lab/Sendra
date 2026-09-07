//! End-to-end coverage for the invocation-only CLI overrides: `-H`/`--header`,
//! `--var` and `--timeout`.
//!
//! These are the highest-precedence layer in the chain documented on
//! [`sendra_cli::run`] (see the crate's `run.rs` module doc comment) — above
//! config, the request file, a resolved `auth:` block, an environment file
//! and captured variables. A hand-rolled server records the raw headers it
//! received, the same pattern [`auth`](auth.rs) and
//! [`structured_body`](structured_body.rs) use: what is under test is what
//! actually goes out on the wire.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One HTTP request as the server saw it: status line aside, just the
/// headers (verbatim casing, as sent).
struct Captured {
    headers: Vec<(String, String)>,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// How many headers exist under this name — the way
    /// `repeated_header_flag_same_name_keeps_only_the_last_value` proves `-H`
    /// replaced rather than added.
    fn header_count(&self, name: &str) -> usize {
        self.headers
            .iter()
            .filter(|(existing, _)| existing.eq_ignore_ascii_case(name))
            .count()
    }
}

/// A server that records the one request it expects and answers 200 to it.
struct CapturingServer {
    addr: SocketAddr,
    captured: Arc<Mutex<Option<Captured>>>,
}

impl CapturingServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
        let addr = listener.local_addr().expect("the listener has an address");
        let captured = Arc::new(Mutex::new(None));

        let stored = captured.clone();
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut writer = stream.try_clone().expect("the socket clones");
            let mut reader = BufReader::new(stream);

            let mut request_line = String::new();
            reader
                .read_line(&mut request_line)
                .expect("the request line reads");

            let mut headers = Vec::new();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("a header line reads");
                if line == "\r\n" {
                    break;
                }
                let (name, value) = line
                    .trim_end()
                    .split_once(':')
                    .expect("a well-formed header line");
                let value = value.trim_start().to_string();
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.parse().unwrap_or(0);
                }
                headers.push((name.to_string(), value));
            }

            let mut body = vec![0u8; content_length];
            if content_length > 0 {
                reader.read_exact(&mut body).expect("the body reads");
            }

            *stored.lock().unwrap() = Some(Captured { headers });

            writer
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .expect("the response writes");
            let _ = writer.flush();
        });

        Self { addr, captured }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The request the server saw, panicking if none arrived — every test
    /// here expects exactly one.
    fn captured(&self) -> Captured {
        self.captured
            .lock()
            .unwrap()
            .take()
            .expect("the server should have received exactly one request")
    }
}

/// A server that reads a request and then goes quiet for `delay` — the same
/// "issue 6" pattern `sendra-core`'s own timeout tests use
/// (`start_stalling_server` in `sendra-core/src/lib.rs`), reproduced here
/// because this test is about `--timeout` reaching the client through the
/// CLI's config override, not about the timeout mechanism itself.
fn start_stalling_server(delay: Duration) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
    let addr = listener.local_addr().expect("the listener has an address");

    std::thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let mut writer = stream.try_clone().expect("the socket clones");
        let mut reader = BufReader::new(stream);

        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            return;
        }
        loop {
            let mut header = String::new();
            match reader.read_line(&mut header) {
                Ok(0) | Err(_) => return,
                Ok(_) if header == "\r\n" => break,
                Ok(_) => {}
            }
        }

        // Never answers within `delay`: the client under test is expected to
        // give up long before this fires.
        std::thread::sleep(delay);
        let _ = writer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    });

    addr
}

/// Comfortably longer than any `--timeout` these tests configure: the server
/// is still holding the connection when the assertions run.
const STALL: Duration = Duration::from_secs(30);

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
        "the run should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "the run should fail: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

// --- `-H`/`--header` ---------------------------------------------------

#[test]
fn header_flag_adds_a_header_the_request_never_set() {
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!("method: GET\nurl: {}/\n", server.base_url()),
    )
    .unwrap();

    assert_success(&sendra(
        dir.path(),
        &["run", "req.yaml", "-H", "X-Extra: added-at-invocation"],
    ));

    assert_eq!(
        server.captured().header("x-extra"),
        Some("added-at-invocation")
    );
}

#[test]
fn header_flag_overrides_a_config_default_header() {
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::create_dir_all(dir.path().join(".sendra")).unwrap();
    std::fs::write(
        dir.path().join(".sendra/config.yaml"),
        "headers:\n  X-Env: from-config\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("req.yaml"),
        format!("method: GET\nurl: {}/\n", server.base_url()),
    )
    .unwrap();

    assert_success(&sendra(
        dir.path(),
        &["run", "req.yaml", "-H", "X-Env: from-cli"],
    ));

    let captured = server.captured();
    assert_eq!(captured.header("x-env"), Some("from-cli"));
    assert_eq!(
        captured.header_count("x-env"),
        1,
        "the override replaces the config header rather than adding to it"
    );
}

#[test]
fn header_flag_overrides_a_request_header() {
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/\nheaders:\n  X-Env: from-file\n",
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(
        dir.path(),
        &["run", "req.yaml", "-H", "X-Env: from-cli"],
    ));

    let captured = server.captured();
    assert_eq!(captured.header("x-env"), Some("from-cli"));
    assert_eq!(captured.header_count("x-env"), 1);
}

#[test]
fn header_flag_overrides_the_resolved_authorization_header() {
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/\nauth:\n  bearer: original\n",
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(
        dir.path(),
        &["run", "req.yaml", "-H", "Authorization: Bearer overridden"],
    ));

    let captured = server.captured();
    assert_eq!(captured.header("authorization"), Some("Bearer overridden"));
    assert_eq!(captured.header_count("authorization"), 1);
}

#[test]
fn repeated_header_flag_same_name_keeps_only_the_last_value() {
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!("method: GET\nurl: {}/\n", server.base_url()),
    )
    .unwrap();

    assert_success(&sendra(
        dir.path(),
        &[
            "run",
            "req.yaml",
            "-H",
            "X-Trace: first",
            "-H",
            "X-Trace: second",
        ],
    ));

    let captured = server.captured();
    assert_eq!(captured.header("x-trace"), Some("second"));
    assert_eq!(
        captured.header_count("x-trace"),
        1,
        "a repeated -H for the same name replaces, it does not accumulate"
    );
}

#[test]
fn malformed_header_flag_is_a_clap_level_error_not_a_downstream_one() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: GET\nurl: https://example.com\n",
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml", "-H", "no-colon-here"]);
    assert_failure(&output);
    assert_eq!(
        output.status.code(),
        Some(2),
        "clap's usage-error exit code"
    );
}

// --- `--var` -------------------------------------------------------------

#[test]
fn var_flag_substitutes_with_no_environment_file_present() {
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/\nheaders:\n  X-Token: '{{{{token}}}}'\n",
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(
        dir.path(),
        &["run", "req.yaml", "--var", "token=abc123"],
    ));

    assert_eq!(server.captured().header("x-token"), Some("abc123"));
}

#[test]
fn var_flag_overrides_an_environment_files_value() {
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::create_dir_all(dir.path().join(".sendra/environments")).unwrap();
    std::fs::write(
        dir.path().join(".sendra/environments/default.yaml"),
        "token: from-file\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/\nheaders:\n  X-Token: '{{{{token}}}}'\n",
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(
        dir.path(),
        &["run", "req.yaml", "--var", "token=from-cli"],
    ));

    assert_eq!(server.captured().header("x-token"), Some("from-cli"));
}

#[test]
fn a_capture_colliding_with_a_var_override_is_refused_like_an_environment_value() {
    // `--var` is folded into the same `variables` map an environment file
    // populates, so a `capture` naming the same variable hits the existing
    // `CaptureFailure::Shadowed` check — see the reasoning on `prepare` in
    // `sendra-cli/src/run.rs`. This is the end-to-end proof that the
    // collision is caught with no environment file involved at all.
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/\ncapture:\n  token: $.ok\n",
            server.base_url()
        ),
    )
    .unwrap();

    let output = sendra(dir.path(), &["test", "req.yaml", "--var", "token=from-cli"]);
    assert_failure(&output);
}

#[test]
fn malformed_var_flag_is_a_clap_level_error_not_a_downstream_one() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: GET\nurl: https://example.com\n",
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml", "--var", "no-equals-here"]);
    assert_failure(&output);
    assert_eq!(output.status.code(), Some(2));
}

// --- `--timeout` -----------------------------------------------------------

#[test]
fn timeout_flag_actually_shortens_the_enforced_timeout() {
    let addr = start_stalling_server(STALL);
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!("method: GET\nurl: http://{addr}/\n"),
    )
    .unwrap();

    let started = Instant::now();
    let output = sendra(dir.path(), &["run", "req.yaml", "--timeout", "1"]);
    let waited = started.elapsed();

    assert_failure(&output);
    assert!(
        waited < STALL / 2,
        "the run should have given up around the 1-second override, not waited on the \
         stalling server for {waited:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("timed out"),
        "the failure should be a timeout, not some other error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_config_timeout_is_still_enforced_when_the_override_is_not_used() {
    // The negative control: a run with no `--timeout` still obeys the
    // project config's timeout exactly as it did before this feature
    // existed — the override is additive, not a replacement mechanism.
    let addr = start_stalling_server(STALL);
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::create_dir_all(dir.path().join(".sendra")).unwrap();
    std::fs::write(
        dir.path().join(".sendra/config.yaml"),
        "timeout_seconds: 1\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("req.yaml"),
        format!("method: GET\nurl: http://{addr}/\n"),
    )
    .unwrap();

    let started = Instant::now();
    let output = sendra(dir.path(), &["run", "req.yaml"]);
    let waited = started.elapsed();

    assert_failure(&output);
    assert!(waited < STALL / 2);
}

// --- combining every override in one invocation -------------------------

#[test]
fn every_override_applies_together_in_one_invocation() {
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::create_dir_all(dir.path().join(".sendra/environments")).unwrap();
    std::fs::write(
        dir.path().join(".sendra/environments/default.yaml"),
        "token: from-file\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join(".sendra/config.yaml"),
        "headers:\n  X-Config: config-value\ntimeout_seconds: 20\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/\nheaders:\n  X-Token: '{{{{token}}}}'\nauth:\n  bearer: original\n",
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(
        dir.path(),
        &[
            "run",
            "req.yaml",
            "-H",
            "X-Config: cli-value",
            "-H",
            "Authorization: Bearer overridden",
            "--var",
            "token=from-cli",
            "--timeout",
            "10",
        ],
    ));

    let captured = server.captured();
    assert_eq!(
        captured.header("x-config"),
        Some("cli-value"),
        "-H overrides the config header"
    );
    assert_eq!(
        captured.header("authorization"),
        Some("Bearer overridden"),
        "-H overrides the resolved auth header"
    );
    assert_eq!(
        captured.header("x-token"),
        Some("from-cli"),
        "--var overrides the environment file value substitution reads"
    );
}
