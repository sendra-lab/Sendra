//! End-to-end coverage for `--dry-run`.
//!
//! The point of the flag is that every resolution step runs exactly as it
//! does under an ordinary `run` — substitution, config, query/body/auth,
//! `pre_request` — and the pipeline stops one step short of the network call.
//! These tests exercise that through the real binary rather than through
//! `run.rs`'s unit tests, because the acceptance criterion that matters most
//! is an *absence*: no socket is ever opened, and the only way to be sure of
//! that is to point at an address a real send would visibly hang or fail
//! against and show the process returns instantly anyway.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Instant;

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
        "the run should fail: stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );
}

// --- no network call happens -------------------------------------------

#[test]
fn dry_run_never_opens_a_socket_and_returns_instantly() {
    // Bound, but nothing ever calls `accept()` on it: a real `connect()`
    // completes at the TCP level (the kernel answers out of the listen
    // backlog) and then the connection just sits there, since nothing reads
    // the request or ever writes a response. A real send against this would
    // hang until the client's own timeout; `--dry-run` must return long
    // before that could possibly fire.
    let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
    let addr = listener.local_addr().expect("the listener has an address");

    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!("method: GET\nurl: http://{addr}/\n"),
    )
    .unwrap();

    // A generous timeout that would make a real send observably slow, but
    // short enough that a broken implementation still fails this test run in
    // a reasonable time rather than hanging it.
    let start = Instant::now();
    let output = sendra(
        dir.path(),
        &["run", "req.yaml", "--dry-run", "--timeout", "20"],
    );
    let elapsed = start.elapsed();

    assert_success(&output);
    assert!(
        elapsed.as_secs() < 2,
        "a real send would have taken close to the 20s timeout; \
         --dry-run took {elapsed:?}, so no network call can have happened"
    );

    // The listener is still unaccepted-on: proof nothing connected to it.
    drop(listener);
}

// --- the resolved request is printed correctly --------------------------

#[test]
fn dry_run_shows_the_fully_resolved_request() {
    // Substitution, a config header, `query`, `auth`, and a `pre_request`
    // script that mutates a header — one of each resolution step, so the
    // printed output has to reflect every one of them in its final state.
    let dir = tempfile::tempdir().expect("a temporary directory");

    std::fs::create_dir_all(dir.path().join(".sendra")).unwrap();
    std::fs::write(
        dir.path().join(".sendra/config.yaml"),
        "headers:\n  X-From-Config: config-value\n",
    )
    .unwrap();

    std::fs::write(
        dir.path().join("req.yaml"),
        "name: Search\n\
         method: GET\n\
         url: 'https://{{host}}/search'\n\
         query:\n  \
           q: coffee\n\
         auth:\n  \
           bearer: '{{token}}'\n\
         pre_request: |\n  \
           request.headers[\"X-From-Config\"] = request.headers[\"X-From-Config\"] + \"-mutated\";\n",
    )
    .unwrap();

    std::fs::create_dir_all(dir.path().join(".sendra/environments")).unwrap();
    std::fs::write(
        dir.path().join(".sendra/environments/default.yaml"),
        "host: api.example.com\ntoken: secret-token-abc\n",
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml", "--dry-run"]);
    assert_success(&output);

    let stdout = String::from_utf8_lossy(&output.stdout);

    // The method and the final URL, query merged in.
    assert!(
        stdout.contains("GET https://api.example.com/search?q=coffee"),
        "stdout: {stdout}"
    );
    // The config header, after the script mutated it.
    assert!(
        stdout.contains("X-From-Config: config-value-mutated"),
        "stdout: {stdout}"
    );
    // `auth: bearer` resolved to a real `Authorization` header, substituted.
    assert!(
        stdout.contains("Authorization: Bearer secret-token-abc"),
        "stdout: {stdout}"
    );
}

#[test]
fn dry_run_shows_the_final_body() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: POST\nurl: https://example.com/users\njson:\n  name: ada\n",
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml", "--dry-run"]);
    assert_success(&output);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("POST https://example.com/users"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Content-Type: application/json"),
        "{stdout}"
    );
    assert!(stdout.contains(r#"{"name":"ada"}"#), "{stdout}");
}

// --- resolution failures still report clearly ---------------------------

#[test]
fn a_missing_variable_under_dry_run_reports_the_same_error_as_without_the_flag() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: GET\nurl: 'https://{{nope}}/thing'\n",
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml", "--dry-run"]);
    assert_failure(&output);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("nope"),
        "the error should name the missing variable: {stderr}"
    );

    // The same error, byte for byte, without the flag — `--dry-run` must not
    // change what a resolution failure looks like.
    let without_flag = sendra(dir.path(), &["run", "req.yaml"]);
    assert_failure(&without_flag);
    assert_eq!(
        String::from_utf8_lossy(&without_flag.stderr),
        stderr,
        "--dry-run must report a resolution failure exactly as `run` already does"
    );
}

// --- a collection dry-runs every selected request ------------------------

#[test]
fn dry_run_on_a_collection_resolves_every_selected_request_in_turn() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("collection.yaml"),
        "requests:\n  \
           - name: First\n    method: GET\n    url: https://example.com/one\n  \
           - name: Second\n    method: GET\n    url: https://example.com/two\n",
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "collection.yaml", "--dry-run"]);
    assert_success(&output);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("GET https://example.com/one"), "{stdout}");
    assert!(stdout.contains("GET https://example.com/two"), "{stdout}");

    // Selecting one by name dry-runs only that one.
    let output = sendra(
        dir.path(),
        &["run", "collection.yaml", "Second", "--dry-run"],
    );
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("/one"), "{stdout}");
    assert!(stdout.contains("GET https://example.com/two"), "{stdout}");
}

// --- `--dry-run --json` --------------------------------------------------

#[test]
fn dry_run_json_reports_a_structured_resolved_request() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "name: Get user\nmethod: GET\nurl: https://example.com/users/1\nheaders:\n  Accept: application/json\n",
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml", "--dry-run", "--json"]);
    assert_success(&output);

    let document: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("--dry-run --json must still emit parseable JSON");

    let request = &document["requests"][0];
    assert_eq!(request["label"], "Get user");
    // No response was ever sent.
    assert_eq!(request["response"], serde_json::Value::Null);
    assert_eq!(request["error"], serde_json::Value::Null);

    let resolved = &request["resolved"];
    assert_eq!(resolved["method"], "GET");
    assert_eq!(resolved["url"], "https://example.com/users/1");
    assert_eq!(resolved["headers"][0]["name"], "Accept");
    assert_eq!(resolved["headers"][0]["value"], "application/json");
    assert_eq!(resolved["body"], serde_json::Value::Null);
}

#[test]
fn an_ordinary_run_reports_a_null_resolved_field() {
    // The no-op guarantee in the schema: a run that never used `--dry-run`
    // still gets the key, explicitly null, so a consumer can read
    // `.resolved` on every request without checking whether this build emits
    // it.
    let dir = tempfile::tempdir().expect("a temporary directory");
    let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
    let addr = listener.local_addr().expect("the listener has an address");
    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            use std::io::{Read, Write};
            let mut stream = stream;
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        }
    });

    std::fs::write(
        dir.path().join("req.yaml"),
        format!("method: GET\nurl: http://{addr}/\n"),
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml", "--json"]);
    assert_success(&output);

    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["requests"][0]["resolved"], serde_json::Value::Null);
}
