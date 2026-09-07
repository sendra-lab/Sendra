//! End-to-end coverage for `auth:` — the block that resolves `bearer` or
//! `basic` credentials into an `Authorization` header before anything else
//! sees the request.
//!
//! A hand-rolled server records the raw headers it received, the same
//! pattern [`structured_body`](structured_body.rs) uses: what is under test
//! is what actually goes out on the wire, which the unit tests in
//! `sendra-core` cannot see since they stop at `Request::resolve_auth`
//! without a socket.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

/// One HTTP request as the server saw it: status line aside, just the
/// headers (lower-cased names).
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

#[test]
fn auth_bearer_sets_the_authorization_header() {
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/\nauth:\n  bearer: my-token-123\n",
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    let captured = server.captured();
    assert_eq!(
        captured.header("authorization"),
        Some("Bearer my-token-123")
    );
}

#[test]
fn auth_basic_base64_encodes_user_and_pass() {
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/\nauth:\n  basic:\n    user: ada\n    pass: s3cr3t\n",
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    let captured = server.captured();
    // base64("ada:s3cr3t")
    assert_eq!(
        captured.header("authorization"),
        Some("Basic YWRhOnMzY3IzdA==")
    );
}

#[test]
fn auth_values_are_substituted_from_the_environment() {
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::create_dir_all(dir.path().join(".sendra/environments")).unwrap();
    std::fs::write(
        dir.path().join(".sendra/environments/default.yaml"),
        "token: from-the-environment\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/\nauth:\n  bearer: '{{{{token}}}}'\n",
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    let captured = server.captured();
    assert_eq!(
        captured.header("authorization"),
        Some("Bearer from-the-environment")
    );
}

#[test]
fn setting_both_bearer_and_basic_is_rejected() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: GET\nurl: https://example.com\nauth:\n  bearer: x\n  basic:\n    user: a\n    pass: b\n",
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml"]);
    assert_failure(&output);
}

#[test]
fn auth_and_an_explicit_authorization_header_together_is_rejected() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: GET\nurl: https://example.com\nheaders:\n  Authorization: Bearer hand-written\nauth:\n  bearer: x\n",
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml"]);
    assert_failure(&output);
}

#[test]
fn a_pre_request_script_can_read_and_override_the_resolved_authorization_header() {
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/\nauth:\n  bearer: original\npre_request: |\n  request.headers[\"X-Saw\"] = request.headers[\"Authorization\"];\n  request.headers[\"Authorization\"] = \"Bearer overridden\";\n",
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    let captured = server.captured();
    assert_eq!(captured.header("x-saw"), Some("Bearer original"));
    assert_eq!(captured.header("authorization"), Some("Bearer overridden"));
}
