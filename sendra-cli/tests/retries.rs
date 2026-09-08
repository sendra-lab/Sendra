//! End-to-end coverage for a request's `retry:` block.
//!
//! These run the real binary against a real (if tiny) TCP server that can be
//! made to fail a request's first few attempts on purpose — a closed
//! connection with nothing written back, the same shape a DNS/connection
//! failure takes — so the retry loop in `sendra-cli/src/run.rs::send` is
//! exercised the way it actually runs, not just unit-tested against a
//! `Result` built by hand.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Command, Output};

fn sendra(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sendra"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("the binary under test runs")
}

/// A server that drops its first `fail_times` connections with nothing
/// written back — a true "no response" failure, the only kind `retry`
/// reacts to — then answers the next one with a fixed 200 and stops.
fn fail_then_succeed(fail_times: usize, body: &str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
    let addr = listener.local_addr().expect("the listener has an address");
    let body = body.to_string();

    std::thread::spawn(move || {
        for (i, stream) in listener.incoming().enumerate() {
            let Ok(stream) = stream else { continue };

            if i < fail_times {
                // Closed with nothing read or written: the client's request
                // never gets a reply, which is exactly the "no response"
                // category `retry` exists for.
                drop(stream);
                continue;
            }

            let mut writer = stream.try_clone().expect("the socket clones");
            let mut reader = BufReader::new(stream);
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                break;
            }
            loop {
                let mut header = String::new();
                match reader.read_line(&mut header) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if header == "\r\n" => break,
                    Ok(_) => {}
                }
            }

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = writer.write_all(response.as_bytes());
            let _ = writer.flush();
            break;
        }
    });

    addr
}

/// A listener bound to a real port and immediately dropped: every connection
/// attempt against it is refused, forever — for a retry that has to exhaust
/// every attempt and still fail.
fn always_refuses() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
    let addr = listener.local_addr().expect("the listener has an address");
    drop(listener);
    addr
}

// --- A retry recovers a flaky request -------------------------------------

#[test]
fn a_retry_recovers_a_request_that_fails_once() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let addr = fail_then_succeed(1, "ok");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "name: Flaky\nmethod: GET\nurl: http://{addr}/\nretry:\n  count: 2\n  delay_ms: 10\n"
        ),
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml"]);
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("200 OK"), "{stdout}");

    // The failed attempt is visible on stderr, for a person watching the run.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("attempt 1 of 3"),
        "the retry should be logged: {stderr}"
    );
}

#[test]
fn a_retry_that_needs_every_attempt_still_recovers_on_the_last_one() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    // Two failures, `retry.count: 2` allows exactly two extra attempts — the
    // third and final one is the one that succeeds.
    let addr = fail_then_succeed(2, "ok");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "name: Flaky\nmethod: GET\nurl: http://{addr}/\nretry:\n  count: 2\n  delay_ms: 5\n"
        ),
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml"]);
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("attempt 1 of 3"), "{stderr}");
    assert!(stderr.contains("attempt 2 of 3"), "{stderr}");
}

// --- Retries exhausted is still a genuine failure -------------------------

#[test]
fn retries_exhausted_still_reports_as_a_genuine_failure() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let addr = always_refuses();
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "name: NeverUp\nmethod: GET\nurl: http://{addr}/\nretry:\n  count: 2\n  delay_ms: 1\n"
        ),
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Both retries happened...
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("attempt 1 of 3"), "{stderr}");
    assert!(stderr.contains("attempt 2 of 3"), "{stderr}");
    // ...but the run genuinely failed, same as it would have with no
    // `retry:` block at all.
    assert!(!output.status.success());
}

#[test]
fn a_request_with_no_retry_block_fails_on_the_first_attempt_as_before() {
    // The no-op guarantee: a file written before this feature existed
    // behaves exactly as it always did.
    let dir = tempfile::tempdir().expect("a temporary directory");
    let addr = always_refuses();
    std::fs::write(
        dir.path().join("req.yaml"),
        format!("name: NeverUp\nmethod: GET\nurl: http://{addr}/\n"),
    )
    .unwrap();

    let output = sendra(dir.path(), &["run", "req.yaml"]);
    assert_eq!(output.status.code(), Some(1));

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("retrying") && !stderr.contains("attempt"),
        "no `retry:` block was declared, so nothing should have retried: {stderr}"
    );
}

// --- Only the final attempt is reported -----------------------------------

#[test]
fn only_the_final_attempt_is_reported_in_json_and_the_summary() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let addr = fail_then_succeed(2, "ok");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "name: Flaky\nmethod: GET\nurl: http://{addr}/\nretry:\n  count: 2\n  delay_ms: 5\n"
        ),
    )
    .unwrap();

    let output = sendra(dir.path(), &["test", "req.yaml", "--json"]);
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let document: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("--json output must be one parseable document");

    // One record, not three: the two failed attempts are not separate
    // entries, and nothing about them counts as a second failure — the
    // record is a plain success. `attempts` is the one trace they leave
    // behind: see `a_recovered_retry_reports_its_attempt_count_in_json_and_junit`
    // below for that in detail.
    let requests = document["requests"]
        .as_array()
        .expect("requests is an array");
    assert_eq!(requests.len(), 1, "{document}");
    assert_eq!(requests[0]["response"]["status"], 200);
    assert_eq!(requests[0]["error"], serde_json::Value::Null);

    // And the summary counts it once, as a plain success — nothing about the
    // two attempts that failed before it shows up as a failure anywhere.
    assert_eq!(document["summary"]["failed"], 0);
    assert_eq!(document["summary"]["no_response"], 0);
    assert_eq!(
        document["summary"]["passed"].as_u64().unwrap_or(0)
            + document["summary"]["without_assertions"]
                .as_u64()
                .unwrap_or(0),
        1
    );
}

// --- Retries are visible as an `attempts` count, past the run itself -----
//
// `attempts` is always present, on every request record in both formats,
// and defaults to `1` — see `Reporter::responded`/`Reporter::request_failed`
// in `sendra-cli/src/output/mod.rs`. Retries recovering a request no longer
// leave a trace on stderr alone: the count they took survives into
// `--json`/`--junit`, which is what a CI log actually keeps.

#[test]
fn a_recovered_retry_reports_its_attempt_count_in_json_and_junit() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    // One failure, then success: two attempts total.
    let addr = fail_then_succeed(1, "ok");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "name: Flaky\nmethod: GET\nurl: http://{addr}/\nretry:\n  count: 2\n  delay_ms: 5\n"
        ),
    )
    .unwrap();
    let report_path = dir.path().join("report.xml");

    let output = sendra(
        dir.path(),
        &["test", "req.yaml", "--json", "--junit", "report.xml"],
    );
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let document: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("--json output must be one parseable document");
    assert_eq!(
        document["requests"][0]["attempts"], 2,
        "one failed attempt plus the one that succeeded: {document}"
    );

    let xml = std::fs::read_to_string(&report_path).expect("the report was written");
    assert!(
        xml.contains("attempts=\"2\""),
        "the JUnit testcase should carry the same count: {xml}"
    );
}

#[test]
fn retries_exhausted_reports_the_full_attempt_count_on_the_error_record() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let addr = always_refuses();
    std::fs::write(
        dir.path().join("req.yaml"),
        format!(
            "name: NeverUp\nmethod: GET\nurl: http://{addr}/\nretry:\n  count: 2\n  delay_ms: 1\n"
        ),
    )
    .unwrap();
    let report_path = dir.path().join("report.xml");

    let output = sendra(
        dir.path(),
        &["test", "req.yaml", "--json", "--junit", "report.xml"],
    );
    assert_eq!(output.status.code(), Some(1));

    let document: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("--json output must be one parseable document");
    assert_eq!(
        document["requests"][0]["attempts"], 3,
        "the original attempt plus both retries: {document}"
    );
    assert!(document["requests"][0]["error"].is_string());

    let xml = std::fs::read_to_string(&report_path).expect("the report was written");
    assert!(xml.contains("attempts=\"3\""), "{xml}");
    assert!(xml.contains("<error"), "{xml}");
}

#[test]
fn a_request_with_no_retry_block_reports_attempts_one_by_default() {
    // The documented default: a file that never mentions `retry:` still gets
    // an `attempts` key, always `1`, so a consumer never has to branch on
    // whether this build of Sendra supports the field.
    let dir = tempfile::tempdir().expect("a temporary directory");
    let addr = fail_then_succeed(0, "ok");
    std::fs::write(
        dir.path().join("req.yaml"),
        format!("name: Plain\nmethod: GET\nurl: http://{addr}/\n"),
    )
    .unwrap();
    let report_path = dir.path().join("report.xml");

    let output = sendra(
        dir.path(),
        &["test", "req.yaml", "--json", "--junit", "report.xml"],
    );
    assert!(output.status.success());

    let document: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("--json output must be one parseable document");
    assert_eq!(document["requests"][0]["attempts"], 1, "{document}");

    let xml = std::fs::read_to_string(&report_path).expect("the report was written");
    assert!(xml.contains("attempts=\"1\""), "{xml}");
}

#[test]
fn a_request_that_never_reaches_the_network_reports_attempts_one() {
    // A substitution failure never calls `send_prepared` at all, so `retry`
    // (even if declared) never has anything to widen — the pre-send failure
    // categories all report the same default as a request with no `retry`
    // block.
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("req.yaml"),
        "name: Broken\nmethod: GET\nurl: '{{nope}}/broken'\nretry:\n  count: 3\n",
    )
    .unwrap();
    let report_path = dir.path().join("report.xml");

    let output = sendra(
        dir.path(),
        &["test", "req.yaml", "--json", "--junit", "report.xml"],
    );
    assert_eq!(output.status.code(), Some(1));

    let document: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("--json output must be one parseable document");
    assert_eq!(document["requests"][0]["attempts"], 1, "{document}");

    let xml = std::fs::read_to_string(&report_path).expect("the report was written");
    assert!(xml.contains("attempts=\"1\""), "{xml}");
}
