//! End-to-end coverage for `--repeat`.
//!
//! These run the real binary against real (if tiny) TCP servers so the whole
//! path — `main` parsing the flag, `run.rs`'s pass loop, a fresh capture
//! store per pass, and `--json`/`--junit` distinguishing the passes — is
//! exercised together, the same division of labour `quiet.rs` and
//! `junit_output.rs` use for their own flags.

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

fn read_one_request(reader: &mut BufReader<std::net::TcpStream>) -> Option<String> {
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return None;
    }
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) | Err(_) => break,
            Ok(_) if header == "\r\n" => break,
            Ok(_) => {}
        }
    }
    Some(request_line.trim().to_string())
}

/// A server that answers every connection 200 and records the request line
/// of each one it accepted, in order — so a test can tell how many requests
/// actually reached it.
struct CountingServer {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<String>>>,
}

impl CountingServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
        let addr = listener.local_addr().expect("the listener has an address");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_thread = seen.clone();

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let mut writer = stream.try_clone().expect("the socket clones");
                let mut reader = BufReader::new(stream);
                let Some(request_line) = read_one_request(&mut reader) else {
                    continue;
                };
                seen_thread.lock().unwrap().push(request_line);

                let _ = writer.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
                let _ = writer.flush();
            }
        });

        Self { addr, seen }
    }

    fn request_count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

/// A server that answers exactly its first connection with 200, then stops
/// accepting — its listener drops with it, so every connection after that is
/// refused. For proving the exit code is worst-wins *across* passes, not just
/// within one.
fn succeed_once_then_refuse() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
    let addr = listener.local_addr().expect("the listener has an address");

    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let mut writer = stream.try_clone().expect("the socket clones");
            let mut reader = BufReader::new(stream);
            if read_one_request(&mut reader).is_some() {
                let _ = writer.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
                let _ = writer.flush();
            }
        }
        // `listener` drops here: anything that connects after this point is
        // refused rather than answered.
    });

    addr
}

// --- `--repeat` runs N full sequential passes -----------------------------

#[test]
fn repeat_sends_the_whole_collection_n_times_sequentially() {
    let server = CountingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "requests:\n  - name: First\n    method: GET\n    url: http://{addr}/first\n  - name: Second\n    method: GET\n    url: http://{addr}/second\n",
            addr = server.addr
        ),
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml", "--repeat", "3"]);
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        server.request_count(),
        6,
        "3 passes of 2 requests each should reach the server exactly 6 times"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("(iteration 1 of 3)"), "{stderr}");
    assert!(stderr.contains("(iteration 2 of 3)"), "{stderr}");
    assert!(stderr.contains("(iteration 3 of 3)"), "{stderr}");
}

#[test]
fn omitting_repeat_sends_one_pass_with_no_iteration_suffix() {
    // The no-op guarantee: a run that never passes `--repeat` must look
    // exactly as it did before the flag existed.
    let server = CountingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!("name: Only\nmethod: GET\nurl: http://{}/\n", server.addr),
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml"]);
    assert!(output.status.success());
    assert_eq!(server.request_count(), 1);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Only"), "{stderr}");
    assert!(
        !stderr.contains("iteration"),
        "no --repeat was passed, so no iteration marker should appear: {stderr}"
    );
}

// --- Captured variables reset at the start of each pass -------------------

#[test]
fn repeat_resets_captured_variables_at_the_start_of_each_pass() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
    let addr = listener.local_addr().expect("the listener has an address");

    std::thread::spawn(move || {
        for (i, stream) in listener.incoming().enumerate() {
            let Ok(stream) = stream else { continue };
            let mut writer = stream.try_clone().expect("the socket clones");
            let mut reader = BufReader::new(stream);
            if read_one_request(&mut reader).is_none() {
                continue;
            }

            // Only the very first request that reaches the server — pass 1's
            // `Login` — carries a token to capture. Every later one,
            // including pass 2's own `Login`, comes back without one. If
            // pass 2 somehow still had pass 1's captured token, its
            // `UseToken` request would substitute and succeed; the test
            // below is that it does not.
            let body = if i == 0 {
                r#"{"token":"abc123"}"#
            } else {
                "{}"
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = writer.write_all(response.as_bytes());
            let _ = writer.flush();
        }
    });

    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "requests:\n  \
             - name: Login\n    method: GET\n    url: http://{addr}/login\n    capture:\n      token: $.token\n  \
             - name: UseToken\n    method: GET\n    url: http://{addr}/use/{{{{token}}}}\n"
        ),
    )
    .unwrap();

    let output = sendra(dir.path(), &["test", "req.yaml", "--repeat", "2", "--json"]);
    // Pass 2's `UseToken` fails to substitute — pass 1's capture must not
    // have carried forward — so the whole invocation fails even though pass
    // 1 was clean.
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let document: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("--json output must be one parseable document");
    let requests = document["requests"]
        .as_array()
        .expect("requests is an array");
    assert_eq!(requests.len(), 4, "2 passes x 2 requests: {document}");

    assert_eq!(requests[0]["label"], "Login (iteration 1 of 2)");
    assert_eq!(requests[1]["label"], "UseToken (iteration 1 of 2)");
    assert_eq!(
        requests[1]["response"]["status"], 200,
        "pass 1 captured its own token and used it: {document}"
    );

    assert_eq!(requests[2]["label"], "Login (iteration 2 of 2)");
    assert_eq!(requests[3]["label"], "UseToken (iteration 2 of 2)");
    assert!(
        requests[3]["error"]
            .as_str()
            .unwrap_or("")
            .contains("token"),
        "pass 2 must not see pass 1's captured token: {document}"
    );
}

// --- The exit code is worst-wins across every pass ------------------------

#[test]
fn repeat_exit_code_is_worst_across_every_pass() {
    let addr = succeed_once_then_refuse();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!("name: Flaky\nmethod: GET\nurl: http://{addr}/\n"),
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml", "--repeat", "2"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "pass 2's failure must fail the whole invocation even though pass 1 succeeded: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("200 OK"),
        "pass 1's response should still have printed: {stdout}"
    );
}

// --- `--json`/`--junit` distinguish repeated iterations -------------------

#[test]
fn repeat_junit_report_has_one_distinctly_named_testcase_per_pass() {
    let server = CountingServer::start();
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!("name: Ping\nmethod: GET\nurl: http://{}/\n", server.addr),
    )
    .unwrap();
    let report_path = dir.path().join("report.xml");

    let output = sendra(
        dir.path(),
        &["test", "req.yaml", "--repeat", "3", "--junit", "report.xml"],
    );
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let xml = std::fs::read_to_string(&report_path).expect("the report was written");
    let doc = roxmltree::Document::parse(&xml).expect("the report is well-formed XML");
    let names: Vec<&str> = doc
        .descendants()
        .filter(|node| node.has_tag_name("testcase"))
        .filter_map(|node| node.attribute("name"))
        .collect();

    assert_eq!(
        names,
        vec![
            "Ping (iteration 1 of 3)",
            "Ping (iteration 2 of 3)",
            "Ping (iteration 3 of 3)",
        ],
        "each pass must produce its own distinctly named <testcase>: {xml}"
    );
}
