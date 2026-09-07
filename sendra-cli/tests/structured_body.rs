//! End-to-end coverage for `json:`, `body_file:`, `form:` and `multipart:` —
//! the four structured ways to specify a request body, alongside the
//! existing plain `body:` string.
//!
//! A hand-rolled server records the raw headers and body bytes it received,
//! the same pattern [`connection_reuse`](../connection_reuse.rs) uses: what
//! is under test is what actually goes out on the wire, which the unit tests
//! in `sendra-core` cannot see since they stop at `Request::resolve_body`
//! without a socket.
//!
//! `body_file` is exercised from a **different working directory than the
//! request file's own** — the CLI is launched from a parent directory while
//! the request file and its `payload.json` sit together in a subdirectory —
//! specifically to prove the path resolves against the request file's
//! location and not the process's cwd, which is the documented design
//! decision on [`sendra_core::Request::resolve_body`].

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use tempfile::TempDir;

/// One HTTP request as the server saw it: status line aside, just the
/// headers (lower-cased names) and the raw body bytes.
struct Captured {
    headers: Vec<(String, String)>,
    body: Vec<u8>,
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

            *stored.lock().unwrap() = Some(Captured { headers, body });

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
fn a_json_body_is_sent_serialized_with_the_default_content_type() {
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: POST\nurl: {}/\njson:\n  name: ada\n  roles: [admin, user]\n",
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    let captured = server.captured();
    assert_eq!(captured.header("content-type"), Some("application/json"));
    let body: serde_json::Value = serde_json::from_slice(&captured.body).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"name": "ada", "roles": ["admin", "user"]})
    );
}

#[test]
fn body_file_reads_relative_to_the_request_file_not_the_process_cwd() {
    let server = CapturingServer::start();
    // The parent the CLI is actually launched from — deliberately holding no
    // `payload.json` of its own, so a resolution against the cwd would fail
    // to find the file at all rather than quietly finding the wrong one.
    let launch_dir: TempDir = tempfile::tempdir().expect("a temporary directory");
    let request_subdir = launch_dir.path().join("requests");
    std::fs::create_dir_all(&request_subdir).unwrap();

    std::fs::write(
        request_subdir.join("req.yaml"),
        format!(
            "method: POST\nurl: {}/\nbody_file: ./payload.json\n",
            server.base_url()
        ),
    )
    .unwrap();
    std::fs::write(request_subdir.join("payload.json"), r#"{"id":1}"#).unwrap();

    assert_success(&sendra(launch_dir.path(), &["run", "requests/req.yaml"]));

    let captured = server.captured();
    assert_eq!(captured.body, br#"{"id":1}"#);
    // No content-type is set automatically for `body_file`: an arbitrary
    // file's content is not something Sendra can label on the request's
    // behalf.
    assert!(captured.header("content-type").is_none());
}

#[test]
fn a_form_body_is_sent_url_encoded_with_the_default_content_type() {
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: POST\nurl: {}/\nform:\n  username: ada lovelace\n  remember_me: \"true\"\n",
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    let captured = server.captured();
    assert_eq!(
        captured.header("content-type"),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(
        String::from_utf8(captured.body).unwrap(),
        "username=ada+lovelace&remember_me=true"
    );
}

#[test]
fn a_multipart_body_sends_a_text_part_and_a_file_part() {
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(dir.path().join("cat.txt"), "meow").unwrap();
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: POST\nurl: {}/\nmultipart:\n  \
             - name: description\n    value: a photo of my cat\n  \
             - name: photo\n    path: ./cat.txt\n",
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    let captured = server.captured();
    let content_type = captured
        .header("content-type")
        .expect("multipart sets its own content-type")
        .to_string();
    assert!(
        content_type.starts_with("multipart/form-data; boundary="),
        "got {content_type}"
    );
    let boundary = content_type
        .strip_prefix("multipart/form-data; boundary=")
        .unwrap();

    let body = String::from_utf8(captured.body).unwrap();
    assert!(body.contains(&format!("--{boundary}\r\n")));
    assert!(body
        .contains("Content-Disposition: form-data; name=\"description\"\r\n\r\na photo of my cat"));
    assert!(body.contains(
        "Content-Disposition: form-data; name=\"photo\"; filename=\"cat.txt\"\r\n\r\nmeow"
    ));
    assert!(body.trim_end().ends_with(&format!("--{boundary}--")));
}

#[test]
fn a_plain_body_is_unaffected_by_the_new_fields() {
    // The non-goal, exercised through the real binary: a request written
    // before this feature existed still sends exactly the body it always
    // did, with no content-type invented on its behalf.
    let server = CapturingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "method: POST\nurl: {}/\nbody: '{{\"name\": \"ada\"}}'\n",
            server.base_url()
        ),
    )
    .unwrap();

    assert_success(&sendra(dir.path(), &["run", "req.yaml"]));

    let captured = server.captured();
    assert_eq!(captured.body, br#"{"name": "ada"}"#);
    assert!(captured.header("content-type").is_none());
}
