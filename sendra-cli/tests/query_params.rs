//! End-to-end coverage for `query:` — a map of parameter name to value,
//! merged onto `url`'s own query string and percent-encoded properly.
//!
//! A hand-rolled server records the raw request line and headers it
//! received, the same pattern [`structured_body`](../structured_body.rs)
//! uses for headers and bodies: what is under test is what actually goes out
//! on the wire, which the unit tests in `sendra-core` cannot see since they
//! stop at `Request::resolve_query` without a socket.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

/// The request line and headers a [`CapturingServer`] saw.
struct Captured {
    request_line: String,
    headers: Vec<(String, String)>,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The path and query the server actually saw, pulled out of the request
    /// line (`GET /search?a=1 HTTP/1.1`).
    fn path_and_query(&self) -> &str {
        self.request_line
            .split_whitespace()
            .nth(1)
            .expect("a well-formed request line has a path")
    }
}

/// A server that records the request line and headers of the one request it
/// expects and answers 200 to it.
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
            loop {
                let mut header_line = String::new();
                reader
                    .read_line(&mut header_line)
                    .expect("a header line reads");
                if header_line == "\r\n" {
                    break;
                }
                let (name, value) = header_line
                    .trim_end()
                    .split_once(':')
                    .expect("a well-formed header line");
                headers.push((name.to_string(), value.trim_start().to_string()));
            }

            *stored.lock().unwrap() = Some(Captured {
                request_line: request_line.trim_end().to_string(),
                headers,
            });

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

#[test]
fn a_query_map_merges_onto_a_url_with_no_existing_query_string() {
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/search\nquery:\n  a: '1'\n  b: '2'\n",
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    assert_eq!(server.captured().path_and_query(), "/search?a=1&b=2");
}

#[test]
fn query_wins_over_a_key_already_in_the_url() {
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/search?a=from-url&b=kept\nquery:\n  a: from-query\n",
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    assert_eq!(
        server.captured().path_and_query(),
        "/search?b=kept&a=from-query"
    );
}

#[test]
fn special_characters_are_percent_encoded_on_the_wire() {
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/search\nquery:\n  q: 'coffee & tea'\n",
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    let captured = server.captured();
    let sent = captured.path_and_query();
    assert!(!sent.contains(' '), "got {sent}");
    assert!(sent.contains("q=coffee+%26+tea"), "got {sent}");
}

#[test]
fn a_repeated_query_key_sends_one_pair_per_list_entry() {
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/search\nquery:\n  tag:\n    - hot\n    - iced\n",
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    assert_eq!(
        server.captured().path_and_query(),
        "/search?tag=hot&tag=iced"
    );
}

#[test]
fn environment_substitution_reaches_query_values() {
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/search\nquery:\n  tenant: '{{{{tenant}}}}'\n",
            server.base_url()
        ),
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join(".sendra/environments")).unwrap();
    std::fs::write(
        dir.path().join(".sendra/environments/default.yaml"),
        "tenant: acme\n",
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    assert_eq!(server.captured().path_and_query(), "/search?tenant=acme");
}

#[test]
fn a_pre_request_script_sees_the_query_already_merged_into_the_url() {
    // The script inspects `request.url` and only sets a header if it already
    // contains the merged query string — proving `query:` was folded into
    // `url` before the script ever ran, per `Request::resolve_query`'s
    // documented ordering.
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/search\nquery:\n  a: '1'\n\
             pre_request: |\n  \
             if request.url.contains(\"a=1\") {{\n    \
             request.headers[\"X-Saw-Merged-Query\"] = \"yes\";\n  \
             }}\n",
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    let captured = server.captured();
    assert_eq!(captured.path_and_query(), "/search?a=1");
    assert_eq!(captured.header("x-saw-merged-query"), Some("yes"));
}

#[test]
fn a_url_only_request_with_no_query_field_is_unaffected() {
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: GET\nurl: {}/search?existing=1\n",
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    assert_eq!(server.captured().path_and_query(), "/search?existing=1");
}
