//! End-to-end coverage for `-q`/`--quiet`.
//!
//! `-q` is sugar over two existing mechanisms rather than a parallel "what to
//! print" code path: it suppresses the `→ <label>` lines the way nothing else
//! does, and it folds into `-o none` for the response half — see
//! `output_mode.rs` for that half's own coverage. These tests are about the
//! combination as the real binary actually renders it.

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
/// response, then stops.
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

// --- `-q` suppresses labels and response, keeps assertions/summary -------

#[test]
fn quiet_suppresses_labels_and_response_but_not_assertions_on_both_subcommands() {
    for subcommand in ["run", "test"] {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let addr = respond_once("HTTP/1.1 200 OK", r#"{"id":1}"#);
        std::fs::write(
            dir.path().join("req.yaml"),
            format!(
                "name: Get user\nmethod: GET\nurl: http://{addr}/\nassertions:\n  status: 200\n"
            ),
        )
        .unwrap();

        let output = sendra(dir.path(), &[subcommand, "req.yaml", "-q"]);
        assert!(
            output.status.success(),
            "{subcommand}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // No `→` label on stderr.
        assert!(
            !stderr.contains("Get user") && !stderr.contains("→"),
            "{subcommand}: stderr should carry no label: {stderr}"
        );
        // No response — no status line, no header, no body.
        assert!(!stdout.contains("OK"), "{subcommand}: {stdout}");
        assert!(!stdout.contains("x-custom"), "{subcommand}: {stdout}");
        assert!(!stdout.contains(r#""id""#), "{subcommand}: {stdout}");
        // The pass/fail answer is still there.
        assert!(
            stdout.contains("status is 200"),
            "{subcommand}: assertions must survive -q: {stdout}"
        );
    }
}

#[test]
fn quiet_test_still_prints_the_summary() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let addr = respond_once("HTTP/1.1 200 OK", "ok");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!("method: GET\nurl: http://{addr}/\n"),
    )
    .unwrap();

    let output = sendra(dir.path(), &["test", "req.yaml", "-q"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("summary"), "{stdout}");
    assert!(
        stdout.contains("1 passed") || stdout.contains("without assertions"),
        "{stdout}"
    );
}

// --- `-q` + a conflicting explicit `-o <mode>` is refused -----------------

#[test]
fn quiet_with_an_explicit_conflicting_output_mode_is_refused() {
    for mode in ["full", "status", "body", "headers"] {
        let dir = tempfile::tempdir().expect("a temporary directory");
        std::fs::write(
            dir.path().join("req.yaml"),
            "method: GET\nurl: https://example.com\n",
        )
        .unwrap();

        let output = sendra(dir.path(), &["run", "req.yaml", "-q", "-o", mode]);
        assert_usage_error(&output);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--quiet") || stderr.contains("-q"),
            "mode {mode}: {stderr}"
        );
        assert!(
            output.stdout.is_empty(),
            "mode {mode}: a rejected run must not have started"
        );
    }
}

#[test]
fn quiet_with_an_explicit_agreeing_output_none_is_accepted() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let addr = respond_once("HTTP/1.1 200 OK", "ok");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!("method: GET\nurl: http://{addr}/\n"),
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml", "-q", "-o", "none"]);
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
}

// --- `-q` + `--json`: no rejection, but the labels still go quiet --------

#[test]
fn quiet_with_json_is_not_rejected_and_still_suppresses_labels() {
    for subcommand in ["run", "test"] {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let addr = respond_once("HTTP/1.1 200 OK", r#"{"id":1}"#);
        std::fs::write(
            dir.path().join("req.yaml"),
            format!("name: Get user\nmethod: GET\nurl: http://{addr}/\n"),
        )
        .unwrap();

        let output = sendra(dir.path(), &[subcommand, "req.yaml", "-q", "--json"]);
        assert!(
            output.status.success(),
            "{subcommand}: -q --json must not be rejected: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        // The document is unaffected: full response, same as --json alone.
        let document: serde_json::Value = serde_json::from_slice(&output.stdout)
            .expect("--json output must still be one parseable document");
        assert_eq!(document["requests"][0]["response"]["status"], 200);
        assert_eq!(document["requests"][0]["response"]["body"], r#"{"id":1}"#);

        // But the label narration on stderr is gone.
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("Get user") && !stderr.contains("→"),
            "{subcommand}: -q must still suppress stderr labels under --json: {stderr}"
        );
    }
}

#[test]
fn json_without_quiet_still_prints_labels_to_stderr() {
    // The control for the test above: --json alone does not suppress labels,
    // so the difference just observed is actually -q's doing.
    let dir = tempfile::tempdir().expect("a temporary directory");
    let addr = respond_once("HTTP/1.1 200 OK", r#"{"id":1}"#);
    std::fs::write(
        dir.path().join("req.yaml"),
        format!("name: Get user\nmethod: GET\nurl: http://{addr}/\n"),
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml", "--json"]);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Get user"), "{stderr}");
}

// --- `-q` + `--dry-run`: the resolved request is suppressed too ----------

#[test]
fn quiet_dry_run_prints_nothing() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "name: Create user\nmethod: POST\nurl: https://example.com/users\njson:\n  name: ada\n",
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml", "--dry-run", "-q"]);
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("Create user") && !stderr.contains("→"),
        "{stderr}"
    );
}

// --- omitting `-q` changes nothing ----------------------------------------

#[test]
fn omitting_quiet_still_prints_labels() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let addr = respond_once("HTTP/1.1 200 OK", "ok");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!("name: Get user\nmethod: GET\nurl: http://{addr}/\n"),
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml"]);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Get user"), "{stderr}");
}
