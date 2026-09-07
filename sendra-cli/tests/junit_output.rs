//! End-to-end coverage for `sendra test --junit`: that the file it writes is
//! well-formed XML a real parser accepts, that its counts match the run's own
//! summary, and that it coexists with the terminal output rather than
//! replacing it.
//!
//! The unit tests in `output::junit` check the shape of a report built by
//! hand from `Case`s; these run the real binary against a real (if tiny)
//! server, so the path from `main` through `Reporter` to a file on disk is
//! actually exercised — the same division of labour `json_output.rs` uses for
//! `--json`.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

/// A collection with one of every outcome `--junit` has to represent:
///
/// - `Passes` gets a 200 it asserts, and a `post_request` script that agrees
///   — a pass by both mechanisms at once.
/// - `Fails` asserts a status the server will not give it, twice over (a
///   second assertion too), so its `<failure>` has to carry more than one
///   line.
/// - `Unchecked` declares no assertions and no script: `<skipped>`.
/// - `NoResponse` references a variable nothing defines, so it is never even
///   sent: `<error>`.
const MIXED_COLLECTION: &str = "\
requests:
  - name: Passes
    method: GET
    url: '{{base}}/passes'
    assertions:
      status: 200
    post_request: |
      if response.status != 200 { throw \"unexpected status\"; }
  - name: Fails
    method: GET
    url: '{{base}}/fails'
    assertions:
      status: 404
      body_contains: not-in-the-body
  - name: Unchecked
    method: GET
    url: '{{base}}/unchecked'
  - name: NoResponse
    method: GET
    url: '{{nowhere}}/gone'
";

/// A server that always answers 200 with a fixed body, which is enough: the
/// point of this file is the report's shape, not the responses behind it.
struct FixedServer {
    addr: SocketAddr,
}

impl FixedServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
        let addr = listener.local_addr().expect("the listener has an address");

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let mut writer = stream.try_clone().expect("the socket clones");
                let mut reader = BufReader::new(stream);

                let mut request_line = String::new();
                if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                    continue;
                }
                loop {
                    let mut header = String::new();
                    match reader.read_line(&mut header) {
                        Ok(0) | Err(_) => break,
                        Ok(_) if header == "\r\n" => break,
                        Ok(_) => {}
                    }
                }

                let _ = writer.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
                let _ = writer.flush();
            }
        });

        Self { addr }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

/// A project directory holding the collection and an environment pointing at
/// `server` — no `.sendra/` beyond that, so nothing left over from the
/// repository these tests run in leaks in.
fn project(server: &FixedServer) -> TempDir {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(dir.path().join("req.yaml"), MIXED_COLLECTION)
        .expect("the collection is written");
    std::fs::create_dir_all(dir.path().join(".sendra/environments"))
        .expect("the environment directory is created");
    std::fs::write(
        dir.path().join(".sendra/environments/default.yaml"),
        format!("base: {}\n", server.base_url()),
    )
    .expect("the environment is written");
    dir
}

fn sendra(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sendra"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("the binary under test runs")
}

#[test]
fn junit_report_is_well_formed_and_matches_the_summary() {
    let server = FixedServer::start();
    let dir = project(&server);
    let report_path = dir.path().join("report.xml");

    let output = sendra(dir.path(), &["test", "req.yaml", "--junit", "report.xml"]);

    // The run genuinely fails — both `Fails` (a failed assertion) and
    // `NoResponse` (never sent). `NoResponse` is the more serious of the two
    // and outranks it in `worst`, so the process exits `Failure` (1) rather
    // than `TestFailed` (4); the report itself still carries both kinds, and
    // the assertions below are where that distinction actually matters.
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr was {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The ordinary terminal output still happened — `--junit` is additive,
    // not a replacement.
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    assert!(stdout.contains("summary"), "{stdout:?}");
    assert!(stdout.contains("1 passed"), "{stdout:?}");

    let xml = std::fs::read_to_string(&report_path).expect("the report was written");

    // A real XML parser accepts it — not just "no raw `<`/`&` leaked", which
    // the unit tests already cover, but that the whole document is
    // well-formed.
    let document = roxmltree::Document::parse(&xml)
        .unwrap_or_else(|err| panic!("the report must be well-formed XML ({err}): {xml}"));

    let suite = document
        .descendants()
        .find(|node| node.has_tag_name("testsuite"))
        .expect("one <testsuite>");

    // The counts match `sendra test`'s own summary — 4 requests: 1 passed,
    // 1 failed, 1 without assertions, 1 no response.
    assert_eq!(suite.attribute("tests"), Some("4"));
    assert_eq!(suite.attribute("failures"), Some("1"));
    assert_eq!(suite.attribute("errors"), Some("1"));
    assert_eq!(suite.attribute("skipped"), Some("1"));

    let cases: Vec<_> = document
        .descendants()
        .filter(|node| node.has_tag_name("testcase"))
        .collect();
    assert_eq!(cases.len(), 4, "one <testcase> per request");

    let case = |name: &str| {
        cases
            .iter()
            .find(|node| node.attribute("name") == Some(name))
            .unwrap_or_else(|| panic!("no <testcase name=\"{name}\">"))
    };

    // `Passes`: a plain pass, no child element.
    let passes = case("Passes");
    assert!(passes.children().all(|child| !child.is_element()), "{xml}");

    // `Fails`: one `<failure>`, carrying both assertions that did not hold.
    let fails = case("Fails");
    let failure_children: Vec<_> = fails
        .children()
        .filter(|child| child.has_tag_name("failure"))
        .collect();
    assert_eq!(
        failure_children.len(),
        1,
        "one <failure> even though two assertions failed: {xml}"
    );
    let message = failure_children[0].text().unwrap_or_default();
    assert!(message.contains("status is 404"), "{message}");
    assert!(message.contains("body contains"), "{message}");

    // `Unchecked`: `<skipped>`, not a bare pass.
    let unchecked = case("Unchecked");
    assert!(
        unchecked
            .children()
            .any(|child| child.has_tag_name("skipped")),
        "{xml}"
    );

    // `NoResponse`: `<error>`, not `<failure>` — it never got a response at
    // all, so there was no expectation to fail.
    let no_response = case("NoResponse");
    assert!(
        no_response
            .children()
            .any(|child| child.has_tag_name("error")),
        "{xml}"
    );
    assert!(
        !no_response
            .children()
            .any(|child| child.has_tag_name("failure")),
        "{xml}"
    );
}

#[test]
fn junit_coexists_with_json() {
    let server = FixedServer::start();
    let dir = project(&server);

    let output = sendra(
        dir.path(),
        &["test", "req.yaml", "--json", "--junit", "report.xml"],
    );

    // The `--json` document is exactly what it always is — `--junit` writing
    // a second file must not touch stdout.
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    let document: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("stdout must still be one JSON document ({err}): {stdout:?}"));
    assert_eq!(document["summary"]["total"], 4);

    let xml = std::fs::read_to_string(dir.path().join("report.xml"))
        .expect("the report was written alongside --json");
    roxmltree::Document::parse(&xml).expect("the report is well-formed XML");
}

#[test]
fn junit_does_not_change_the_exit_code() {
    let server = FixedServer::start();
    let dir = project(&server);

    let without = sendra(dir.path(), &["test", "req.yaml"]);
    let with = sendra(dir.path(), &["test", "req.yaml", "--junit", "report.xml"]);

    assert_eq!(
        with.status.code(),
        without.status.code(),
        "--junit is a serialisation of the result, never part of deciding it"
    );
}

#[test]
fn junit_for_a_single_named_request_has_one_testcase() {
    // `test <file> <name>` is additive over `test <file>`: the same
    // machinery, fed one request instead of all four, should just naturally
    // produce a report with one `<testsuite>`/`<testcase>` pair rather than
    // needing special-casing.
    let server = FixedServer::start();
    let dir = project(&server);
    let report_path = dir.path().join("report.xml");

    let output = sendra(
        dir.path(),
        &["test", "req.yaml", "Passes", "--junit", "report.xml"],
    );

    assert_eq!(
        output.status.code(),
        Some(0),
        "`Passes` alone should pass: stderr was {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let xml = std::fs::read_to_string(&report_path).expect("the report was written");
    let document = roxmltree::Document::parse(&xml)
        .unwrap_or_else(|err| panic!("the report must be well-formed XML ({err}): {xml}"));

    let suite = document
        .descendants()
        .find(|node| node.has_tag_name("testsuite"))
        .expect("one <testsuite>");
    assert_eq!(suite.attribute("tests"), Some("1"));
    assert_eq!(suite.attribute("failures"), Some("0"));
    assert_eq!(suite.attribute("errors"), Some("0"));
    assert_eq!(suite.attribute("skipped"), Some("0"));

    let cases: Vec<_> = document
        .descendants()
        .filter(|node| node.has_tag_name("testcase"))
        .collect();
    assert_eq!(cases.len(), 1, "one <testcase> for the one request tested");
    assert_eq!(cases[0].attribute("name"), Some("Passes"));
}

#[test]
fn test_with_a_nonexistent_request_name_fails_like_run_does() {
    let server = FixedServer::start();
    let dir = project(&server);

    let run_output = sendra(dir.path(), &["run", "req.yaml", "Nonexistent"]);
    let test_output = sendra(dir.path(), &["test", "req.yaml", "Nonexistent"]);

    assert_eq!(
        test_output.status.code(),
        run_output.status.code(),
        "the same missing name should fail the same way under both subcommands"
    );
    assert_ne!(test_output.status.code(), Some(0));

    let stderr = String::from_utf8_lossy(&test_output.stderr);
    assert!(
        stderr.contains("Nonexistent"),
        "the error should name the request that was not found: {stderr}"
    );
}

#[test]
fn run_refuses_junit() {
    // The explicit non-goal: `run` produces no verdict for a JUnit report to
    // describe, so the flag is `test`'s alone and clap refuses it as unknown.
    let server = FixedServer::start();
    let dir = project(&server);

    let output = sendra(dir.path(), &["run", "req.yaml", "--junit", "report.xml"]);

    assert_eq!(output.status.code(), Some(2), "a clap usage error");
    assert!(!dir.path().join("report.xml").exists());
}
