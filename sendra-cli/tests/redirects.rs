//! End to end: a redirecting server, seen through the actual binary rather
//! than through `sendra-core`'s own unit tests.
//!
//! The unit tests in `sendra-core` prove the chain is captured correctly on a
//! `Response`; these prove it actually reaches both output modes — the `→`
//! lines on stdout under the human renderer, and the `redirects` array under
//! `--json` — and that `.sendra/config.yaml`'s `follow_redirects` key is read
//! and applied by a real run, not just by a `Config` built by hand in a test.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

/// A server that answers a fixed table of `path -> raw HTTP response`, over as
/// many requests on one connection as the client sends — which is what
/// following a redirect chain to the same host looks like on the wire.
///
/// Hand-rolled for the same reason `connection_reuse.rs` rolls its own: what
/// is under test is a handful of exact status lines and `Location` headers,
/// and a mock-server dependency would be more setup than the protocol these
/// tests actually need.
struct RouteServer {
    addr: SocketAddr,
}

impl RouteServer {
    fn start(routes: Vec<(&'static str, Vec<u8>)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
        let addr = listener.local_addr().expect("the listener has an address");

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let mut writer = stream.try_clone().expect("the socket clones");
                let mut reader = BufReader::new(stream);

                loop {
                    let mut request_line = String::new();
                    match reader.read_line(&mut request_line) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                    let path = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_string();

                    loop {
                        let mut header = String::new();
                        match reader.read_line(&mut header) {
                            Ok(0) | Err(_) => return,
                            Ok(_) if header == "\r\n" => break,
                            Ok(_) => {}
                        }
                    }

                    let response = routes
                        .iter()
                        .find(|(route, _)| *route == path)
                        .map(|(_, body)| body.clone())
                        .unwrap_or_else(|| {
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()
                        });

                    if writer.write_all(&response).is_err() {
                        return;
                    }
                    let _ = writer.flush();
                }
            }
        });

        Self { addr }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

fn redirect_response(status: u16, reason: &str, location: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status} {reason}\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n")
        .into_bytes()
}

fn ok_response(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A directory holding `req.yaml` pointed at `server`'s `/start`, and — when
/// given — a `.sendra/config.yaml` setting `follow_redirects`.
fn project(server: &RouteServer, follow_redirects: Option<&str>) -> TempDir {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "name: Redirected\nmethod: GET\nurl: '{}'\n",
            server.url("/start")
        ),
    )
    .expect("the request file is written");

    if let Some(value) = follow_redirects {
        std::fs::create_dir_all(dir.path().join(".sendra")).expect("the config dir is created");
        std::fs::write(
            dir.path().join(".sendra/config.yaml"),
            format!("follow_redirects: {value}\n"),
        )
        .expect("the config is written");
    }

    dir
}

fn sendra(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sendra"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("the binary under test runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

/// A two-hop chain: `/start` -> 301 -> `/next` -> 302 -> `/end` -> 200.
fn two_hop_server() -> RouteServer {
    RouteServer::start(vec![
        (
            "/start",
            redirect_response(301, "Moved Permanently", "/next"),
        ),
        ("/next", redirect_response(302, "Found", "/end")),
        ("/end", ok_response("done")),
    ])
}

#[test]
fn default_config_follows_a_redirect_and_prints_the_chain_in_human_output() {
    let server = two_hop_server();
    let dir = project(&server, None);

    let output = sendra(dir.path(), &["run", "req.yaml"]);
    assert!(
        output.status.success(),
        "the run should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = stdout(&output);
    // Each hop, oldest first, then the final response — not just the
    // response it ended on.
    assert!(
        stdout.contains(&format!("301 {}", server.url("/next"))),
        "got {stdout:?}"
    );
    assert!(
        stdout.contains(&format!("302 {}", server.url("/end"))),
        "got {stdout:?}"
    );
    assert!(stdout.contains("200 OK"), "got {stdout:?}");
    assert!(stdout.contains("done"), "got {stdout:?}");
}

#[test]
fn default_config_reports_the_chain_under_json() {
    let server = two_hop_server();
    let dir = project(&server, None);

    let output = sendra(dir.path(), &["run", "req.yaml", "--json"]);
    assert!(
        output.status.success(),
        "the run should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let document: serde_json::Value =
        serde_json::from_str(&stdout(&output)).expect("stdout is one JSON document");
    let response = &document["requests"][0]["response"];

    // The final response is what is reported as *the* response...
    assert_eq!(response["status"], 200);
    assert_eq!(response["body"], "done");

    // ...with the chain that got there carried alongside it.
    let redirects = response["redirects"].as_array().expect("an array");
    assert_eq!(redirects.len(), 2);
    assert_eq!(redirects[0]["status"], 301);
    assert_eq!(redirects[0]["location"], server.url("/next"));
    assert_eq!(redirects[1]["status"], 302);
    assert_eq!(redirects[1]["location"], server.url("/end"));
}

#[test]
fn follow_redirects_false_in_config_reports_the_3xx_response_itself() {
    let server = two_hop_server();
    let dir = project(&server, Some("false"));

    let output = sendra(dir.path(), &["run", "req.yaml", "--json"]);
    assert!(
        output.status.success(),
        "a 3xx with redirects off is a normal response, not a failure: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let document: serde_json::Value =
        serde_json::from_str(&stdout(&output)).expect("stdout is one JSON document");
    let response = &document["requests"][0]["response"];

    assert_eq!(response["status"], 301);
    assert_eq!(
        response["redirects"].as_array().expect("an array").len(),
        0,
        "nothing was followed, so there is no chain to report"
    );
}

#[test]
fn a_custom_maximum_lower_than_the_chain_fails_the_request() {
    // One hop allowed, two are needed to reach `/end`.
    let server = two_hop_server();
    let dir = project(&server, Some("1"));

    let output = sendra(dir.path(), &["run", "req.yaml"]);
    assert!(
        !output.status.success(),
        "a chain past the configured maximum must not succeed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("redirect"),
        "the error should say why: {stderr}"
    );
}

#[test]
fn a_custom_maximum_covering_the_chain_still_succeeds() {
    let server = two_hop_server();
    let dir = project(&server, Some("5"));

    let output = sendra(dir.path(), &["run", "req.yaml", "--json"]);
    assert!(
        output.status.success(),
        "two hops is well within a maximum of five: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let document: serde_json::Value =
        serde_json::from_str(&stdout(&output)).expect("stdout is one JSON document");
    let response = &document["requests"][0]["response"];
    assert_eq!(response["status"], 200);
    assert_eq!(response["redirects"].as_array().expect("an array").len(), 2);
}
