//! End-to-end coverage for `-o`/`--output`.
//!
//! Exercised through the real binary, like `dry_run.rs`, because the point of
//! this flag is what actually lands on stdout — a unit test against
//! `Reporter` directly would miss a mode that quietly printed one extra byte
//! or one extra line.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};

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

fn assert_usage_error(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(2),
        "a rejected flag combination is a usage error: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A one-shot HTTP server that answers exactly one connection with a fixed
/// response, then stops. Bound before `sendra` runs, so the address is known
/// up front; the accept happens on a background thread since the client
/// connects only after this function returns.
fn respond_once(status_line: &str, body: &str) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
    let addr = listener.local_addr().expect("the listener has an address");

    let status_line = status_line.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let response = format!(
                "{status_line}\r\nContent-Type: application/json\r\nX-Custom: marker\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    addr
}

fn write_get_request(dir: &Path, addr: std::net::SocketAddr) {
    std::fs::write(
        dir.join("req.yaml"),
        format!("method: GET\nurl: http://{addr}/\n"),
    )
    .unwrap();
}

// --- omitting `-o` preserves each subcommand's default -------------------

#[test]
fn omitting_output_keeps_runs_full_default() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let addr = respond_once("HTTP/1.1 200 OK", r#"{"id":1}"#);
    write_get_request(dir.path(), addr);

    let output = sendra(dir.path(), &["run", "req.yaml"]);
    assert_success(&output);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("200 OK"), "{stdout}");
    assert!(stdout.contains("x-custom: marker"), "{stdout}");
    assert!(stdout.contains(r#""id": 1"#), "{stdout}");
}

#[test]
fn omitting_output_keeps_tests_status_only_default() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let addr = respond_once("HTTP/1.1 200 OK", r#"{"id":1}"#);
    write_get_request(dir.path(), addr);

    let output = sendra(dir.path(), &["test", "req.yaml"]);
    assert_success(&output);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("200 OK"), "{stdout}");
    assert!(
        !stdout.contains("x-custom"),
        "`test` prints no headers by default: {stdout}"
    );
    assert!(
        !stdout.contains(r#""id""#),
        "`test` prints no body by default: {stdout}"
    );
}

// --- each mode renders correctly, for both subcommands -------------------

#[test]
fn output_full_shows_status_headers_and_body_on_both_subcommands() {
    for subcommand in ["run", "test"] {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let addr = respond_once("HTTP/1.1 200 OK", r#"{"id":1}"#);
        write_get_request(dir.path(), addr);

        let output = sendra(dir.path(), &[subcommand, "req.yaml", "-o", "full"]);
        assert_success(&output);

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("200 OK"), "{subcommand}: {stdout}");
        assert!(
            stdout.contains("x-custom: marker"),
            "{subcommand}: {stdout}"
        );
        assert!(stdout.contains(r#""id": 1"#), "{subcommand}: {stdout}");
    }
}

#[test]
fn output_status_shows_the_status_line_alone_on_both_subcommands() {
    for subcommand in ["run", "test"] {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let addr = respond_once("HTTP/1.1 200 OK", r#"{"id":1}"#);
        write_get_request(dir.path(), addr);

        let output = sendra(dir.path(), &[subcommand, "req.yaml", "-o", "status"]);
        assert_success(&output);

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("200 OK"), "{subcommand}: {stdout}");
        assert!(!stdout.contains("x-custom"), "{subcommand}: {stdout}");
        assert!(!stdout.contains(r#""id""#), "{subcommand}: {stdout}");
    }
}

#[test]
fn output_body_shows_the_body_alone_on_both_subcommands() {
    for subcommand in ["run", "test"] {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let addr = respond_once("HTTP/1.1 200 OK", r#"{"id":1}"#);
        write_get_request(dir.path(), addr);

        let output = sendra(dir.path(), &[subcommand, "req.yaml", "-o", "body"]);
        assert_success(&output);

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains(r#""id": 1"#), "{subcommand}: {stdout}");
        assert!(!stdout.contains("200"), "{subcommand}: {stdout}");
        assert!(!stdout.contains("x-custom"), "{subcommand}: {stdout}");
    }
}

#[test]
fn output_headers_shows_the_headers_alone_on_both_subcommands() {
    for subcommand in ["run", "test"] {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let addr = respond_once("HTTP/1.1 200 OK", r#"{"id":1}"#);
        write_get_request(dir.path(), addr);

        let output = sendra(dir.path(), &[subcommand, "req.yaml", "-o", "headers"]);
        assert_success(&output);

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("x-custom: marker"),
            "{subcommand}: {stdout}"
        );
        assert!(!stdout.contains("200"), "{subcommand}: {stdout}");
        assert!(!stdout.contains(r#""id""#), "{subcommand}: {stdout}");
    }
}

#[test]
fn output_none_suppresses_the_response_but_not_the_assertions_on_both_subcommands() {
    for subcommand in ["run", "test"] {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let addr = respond_once("HTTP/1.1 200 OK", r#"{"id":1}"#);
        std::fs::write(
            dir.path().join("req.yaml"),
            format!("method: GET\nurl: http://{addr}/\nassertions:\n  status: 200\n"),
        )
        .unwrap();

        let output = sendra(dir.path(), &[subcommand, "req.yaml", "-o", "none"]);
        assert_success(&output);

        let stdout = String::from_utf8_lossy(&output.stdout);
        // The status line (`200 OK  N ms`) is gone — checked via "OK" rather
        // than "200", since the assertion line below legitimately says
        // "status is 200".
        assert!(!stdout.contains("OK"), "{subcommand}: {stdout}");
        assert!(!stdout.contains("x-custom"), "{subcommand}: {stdout}");
        assert!(!stdout.contains(r#""id""#), "{subcommand}: {stdout}");
        // The checks below the (suppressed) response still print.
        assert!(
            stdout.contains("status is 200"),
            "{subcommand}: assertions must survive -o none: {stdout}"
        );
    }
}

#[test]
fn an_invalid_output_mode_is_a_clear_cli_error() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: GET\nurl: https://example.com\n",
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml", "-o", "bogus"]);
    assert_usage_error(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("bogus"), "{stderr}");
}

// --- `-o` combined with `--json` is refused, not silently ignored --------

#[test]
fn output_with_json_is_refused_on_both_subcommands() {
    for subcommand in ["run", "test"] {
        let dir = tempfile::tempdir().expect("a temporary directory");
        std::fs::write(
            dir.path().join("req.yaml"),
            "method: GET\nurl: https://example.com\n",
        )
        .unwrap();

        let output = sendra(
            dir.path(),
            &[subcommand, "req.yaml", "-o", "body", "--json"],
        );
        assert_usage_error(&output);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--json"),
            "{subcommand}: the message should name the conflicting flag: {stderr}"
        );
        assert!(
            stdout_is_empty(&output),
            "{subcommand}: a rejected run must not have started"
        );
    }
}

fn stdout_is_empty(output: &Output) -> bool {
    output.stdout.is_empty()
}

// --- `-o` combined with `--dry-run` ---------------------------------------

#[test]
fn dry_run_output_full_is_the_ordinary_dry_run_rendering() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: POST\nurl: https://example.com/users\njson:\n  name: ada\n",
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml", "--dry-run", "-o", "full"]);
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

#[test]
fn dry_run_output_body_shows_only_the_resolved_body() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: POST\nurl: https://example.com/users\njson:\n  name: ada\n",
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml", "--dry-run", "-o", "body"]);
    assert_success(&output);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(r#"{"name":"ada"}"#), "{stdout}");
    assert!(!stdout.contains("POST"), "{stdout}");
    assert!(!stdout.contains("Content-Type"), "{stdout}");
}

#[test]
fn dry_run_output_headers_shows_only_the_resolved_headers() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: POST\nurl: https://example.com/users\njson:\n  name: ada\n",
    )
    .unwrap();

    let output = sendra(
        dir.path(),
        &["run", "req.yaml", "--dry-run", "-o", "headers"],
    );
    assert_success(&output);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Content-Type: application/json"),
        "{stdout}"
    );
    assert!(!stdout.contains("POST"), "{stdout}");
    assert!(!stdout.contains(r#"{"name":"ada"}"#), "{stdout}");
}

#[test]
fn dry_run_output_none_prints_nothing() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: POST\nurl: https://example.com/users\njson:\n  name: ada\n",
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml", "--dry-run", "-o", "none"]);
    assert_success(&output);
    assert!(
        stdout_is_empty(&output),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn dry_run_output_status_is_refused() {
    // A dry run never sends the request, so there is no status line for
    // `-o status` to show.
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "method: GET\nurl: https://example.com\n",
    )
    .unwrap();

    let output = sendra(
        dir.path(),
        &["run", "req.yaml", "--dry-run", "-o", "status"],
    );
    assert_usage_error(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--dry-run"), "{stderr}");
    assert!(
        stdout_is_empty(&output),
        "a rejected run must not have started"
    );
}
