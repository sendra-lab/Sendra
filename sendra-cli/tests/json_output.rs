//! What `--json` actually puts on the process's stdout.
//!
//! The unit tests in `output` check the shape of the document against a writer
//! they hand in. They cannot check the thing `--json` really promises — that
//! **nothing else is written to stdout** — because a `println!` somewhere else
//! in the binary is invisible to them. So these run the binary and read its two
//! streams.
//!
//! No server, and all but one of them no socket either: a request whose URL
//! holds a `{{variable}}` the environment does not define fails before anything
//! is sent, which is the same "never got a response" outcome a refused
//! connection produces, reported by the same path. One test does attempt a
//! real connection, to a closed port on the loopback address, so that the
//! `send`-and-fail path is covered too; it is one test rather than five because
//! waiting for a connection to be refused is slower than every other test in
//! this repository put together.
//!
//! Response bodies, headers and assertion results — everything that needs a
//! server to produce — are the unit tests' job.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

/// Nothing listens on port 1, so the request fails while connecting rather than
/// leaving the test waiting on a timeout.
const CLOSED_PORT_URL: &str = "http://127.0.0.1:1/";

/// Two requests that cannot be built: `{{nowhere}}` is defined by no
/// environment, and the temporary directory these run in has no `.sendra/` for
/// one to be found in.
const TWO_UNSENDABLE_REQUESTS: &str = "\
requests:
  - name: First
    method: GET
    url: '{{nowhere}}/first'
  - name: Second
    method: GET
    url: '{{nowhere}}/second'
";

/// One request that will really be sent, and really refused.
const ONE_UNREACHABLE_REQUEST: &str = "name: Unreachable\nmethod: GET\nurl: http://127.0.0.1:1/\n";

/// A directory holding `req.yaml`, and nothing else — in particular no
/// `.sendra/`, so the run is not affected by a config or an environment left in
/// the repository this test was launched from.
fn project(request_file: &str) -> TempDir {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(dir.path().join("req.yaml"), request_file).expect("the request file is written");
    dir
}

/// Run `sendra` in `dir` with `args`, and hand back what it wrote.
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

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is UTF-8")
}

/// Parse the whole of stdout, or fail saying what was there instead. Parsing
/// *all* of it is the assertion: one stray human line and this is not JSON.
fn document(output: &Output) -> serde_json::Value {
    let stdout = stdout(output);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("stdout must be one JSON document ({err}): {stdout:?}"))
}

#[test]
fn run_json_writes_one_document_and_nothing_else_to_stdout() {
    let dir = project(TWO_UNSENDABLE_REQUESTS);
    let output = sendra(dir.path(), &["run", "req.yaml", "--json"]);

    let stdout = stdout(&output);
    let document = document(&output);

    // One document for the run, holding both requests in file order, rather
    // than one document per request.
    assert_eq!(document["requests"].as_array().map(Vec::len), Some(2));
    assert_eq!(document["requests"][0]["label"], "First");
    assert_eq!(document["requests"][1]["label"], "Second");

    // Neither the labels nor the error lines leaked onto stdout. Parsing the
    // whole of stdout already proves that; these name what would have gone
    // wrong. (An `error:` *inside* a JSON string is a reported failure, which
    // is the point — what must not be there is the human `error:` line, at the
    // start of a line of its own.)
    assert!(!stdout.contains('→'), "no labels on stdout: {stdout:?}");
    assert!(
        !stdout.lines().any(|line| line.starts_with("error:")),
        "no error lines on stdout: {stdout:?}"
    );

    // Both are still on stderr, where someone watching the run can see them
    // even while stdout is being redirected to a file.
    let stderr = stderr(&output);
    assert!(stderr.contains("→ First"), "{stderr:?}");
    assert!(stderr.contains("→ Second"), "{stderr:?}");
    assert!(stderr.contains("error:"), "{stderr:?}");

    // `run` reports no summary; that key belongs to `test`.
    assert!(document.get("summary").is_none(), "{document}");
}

#[test]
fn a_request_that_never_connected_is_reported_with_its_error() {
    let dir = project(ONE_UNREACHABLE_REQUEST);
    let output = sendra(dir.path(), &["run", "req.yaml", "--json"]);

    let request = &document(&output)["requests"][0];

    assert_eq!(request["label"], "Unreachable");
    assert_eq!(request["response"], serde_json::Value::Null);
    assert!(
        request["error"]
            .as_str()
            .expect("a request with no response carries an error string")
            .contains(CLOSED_PORT_URL),
        "the error names the request that failed: {request}"
    );
    // Present and empty rather than absent, for a request that got no response
    // and so had nothing to assert against.
    assert_eq!(request["assertions"]["total"], 0);
    assert_eq!(request["assertions"]["results"], serde_json::json!([]));
}

#[test]
fn test_json_adds_the_summary_to_the_same_document() {
    let dir = project(TWO_UNSENDABLE_REQUESTS);
    let output = sendra(dir.path(), &["test", "req.yaml", "--json"]);

    let stdout = stdout(&output);
    let document = document(&output);

    assert_eq!(document["requests"].as_array().map(Vec::len), Some(2));
    assert_eq!(document["summary"]["total"], 2);
    assert_eq!(document["summary"]["no_response"], 2);
    assert_eq!(document["summary"]["passed"], 0);

    // And the human summary block was not printed as well as the document.
    assert!(
        !stdout.contains("summary\n"),
        "stdout must carry the document alone: {stdout:?}"
    );
}

#[test]
fn json_changes_the_output_and_not_the_exit_code() {
    let dir = project(TWO_UNSENDABLE_REQUESTS);

    // A request that could not be built is exit 1 under both subcommands —
    // `--json` is a serialisation of the result, never part of deciding it.
    for command in ["run", "test"] {
        let human = sendra(dir.path(), &[command, "req.yaml"]);
        let json = sendra(dir.path(), &[command, "req.yaml", "--json"]);

        assert_eq!(
            human.status.code(),
            Some(1),
            "`sendra {command}` on an unsendable request exits 1"
        );
        assert_eq!(
            json.status.code(),
            human.status.code(),
            "`--json` must not change what `sendra {command}` returns"
        );
    }
}

#[test]
fn without_json_stdout_is_what_it_always_was() {
    let dir = project(TWO_UNSENDABLE_REQUESTS);
    let output = sendra(dir.path(), &["run", "req.yaml"]);

    // A request that never got a response prints nothing to stdout — the label
    // and the error are stderr's, and always have been. Asserting it here is
    // what says that adding `--json` moved nothing between the two streams for
    // the runs that do not use it.
    //
    // "Nothing" here is the blank line that separates one request's results
    // from the next, which is all two failed requests leave behind — and is
    // the one piece of human output `--json` suppresses rather than replaces.
    assert_eq!(stdout(&output), "\n");
    assert!(stderr(&output).contains("→ First"));
}
