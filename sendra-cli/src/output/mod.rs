//! Everything the two subcommands put on the terminal, and the one clap error
//! raised in place of output.

mod errors;
mod human;
mod json;

use std::cell::RefCell;
use std::io::Write;

use owo_colors::{OwoColorize, Stream};
use sendra_core::{
    AssertionReport, CaptureReport, Response, ScriptOutcome, ScriptOutput, SendraError,
};

use crate::exit::Summary;

use self::errors::print_error_line;
use self::human::{
    print_assertions, print_capture, print_no_assertions, print_post_request, print_response,
    print_status_line, print_summary,
};
use self::json::{
    error_message, AssertionsRecord, CaptureRecord, PostRequestRecord, RequestRecord,
    ResponseRecord, RunDocument, SummaryRecord,
};

pub(crate) use self::errors::{print_environment_error, print_error, reject_allow_error_status};

/// How much of a response the human-readable output shows.
///
/// The two subcommands print the same *assertion* block — issue 6's format,
/// unchanged, because a second way to render a passed check would be a second
/// thing to learn — and differ only in how much of the response they put above
/// it. `run` exists to show you what came back, so it shows all of it. `test`
/// answers a yes/no question about a whole collection, and burying that answer
/// under four JSON bodies would make the summary the hardest line to find in
/// its own output; it prints the status line, which is one line, carries the
/// timing, and says which response the checks below it are about.
///
/// This is a fact about the *human* rendering only. `--json` carries the whole
/// response either way — see [`Format::Json`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Detail {
    /// Status line, headers and body.
    Full,
    /// The status line alone.
    StatusOnly,
}

/// Which rendering a run produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    /// The terminal output Sendra has always printed: a response, then its
    /// assertions, then (under `test`) a summary, as each request finishes.
    Human,

    /// One JSON object describing the whole run, written to stdout when the run
    /// is over — `--json`.
    ///
    /// Not one object per request: a stream of objects would make
    /// `sendra run collection.yaml --json | jq .` a stream of documents rather
    /// than a document, and the summary `test` ends with has nowhere to live in
    /// it. The cost is that nothing is printed until the run finishes, which is
    /// the honest trade for output that is parseable as a whole.
    ///
    /// Every request carries its full response — status, headers, body, elapsed
    /// — under both subcommands, so [`Detail`] does not apply. `test` printing
    /// less than `run` is a decision about what is *readable* on a terminal,
    /// and a program reading the output has no such problem; a script that
    /// wants only the status can select it.
    Json,
}

impl Format {
    /// What `--json`, present or absent, means. One function so the flag is
    /// read the same way by both subcommands.
    pub(crate) fn for_json_flag(json: bool) -> Self {
        if json {
            Format::Json
        } else {
            Format::Human
        }
    }
}

/// The one place a run's results become output.
///
/// Every subcommand reports through this, and it decides — once, from the
/// `--json` flag — whether that means printing as the run goes or recording for
/// a single document at the end. That is the whole of the guarantee `--json`
/// makes: **in [`Format::Json`] nothing but the final object is written to
/// stdout**, because in that mode none of the `print_*` functions above are
/// reached. The `→` labels and every error stay on stderr in both modes, where
/// they already were, so a redirected stdout is a clean JSON document and a
/// terminal still shows what went wrong as it happens.
///
/// Interior mutability rather than `&mut self`: the sending loop holds the
/// reporter *and* hands it to the closure that sends each request, and those
/// two shared borrows are simpler than threading one exclusive borrow through
/// an async closure. Nothing here is `Send`, which is fine — the whole binary
/// runs on one thread.
pub(crate) struct Reporter {
    format: Format,
    detail: Detail,
    /// Whether `capture.values` in `--json` output carries the values
    /// Sendra captured, rather than [`json::REDACTED_CAPTURE_VALUE`] in
    /// their place — `--show-captures`. Read only under [`Format::Json`];
    /// meaningless, and unread, under [`Format::Human`], which never prints
    /// captured values at all.
    show_captures: bool,
    /// One entry per request the run announced, in file order. Stays empty
    /// under [`Format::Human`], which has nothing to record because it has
    /// already printed.
    requests: RefCell<Vec<RequestRecord>>,
}

impl Reporter {
    pub(crate) fn new(format: Format, detail: Detail, show_captures: bool) -> Self {
        Self {
            format,
            detail,
            show_captures,
            requests: RefCell::new(Vec::new()),
        }
    }

    fn recording(&self) -> bool {
        self.format == Format::Json
    }

    /// The blank line between one request's output and the next.
    ///
    /// Nothing under `--json`: the separator is whitespace on stdout, and
    /// stdout in that mode holds one document and nothing else.
    pub(crate) fn separate(&self) {
        if !self.recording() {
            println!();
        }
    }

    /// Announce the request about to be sent, and open its record.
    ///
    /// The `→` label goes to stderr in both modes, unchanged: in a collection
    /// run it is the only thing that says *which* request the next lines are
    /// about, and a "no variable named X" message names the variable, not the
    /// request.
    pub(crate) fn request_started(&self, label: &str) {
        eprintln!(
            "{} {}",
            "→".if_supports_color(Stream::Stderr, |t| t.dimmed()),
            label.if_supports_color(Stream::Stderr, |t| t.bold())
        );

        if self.recording() {
            self.requests.borrow_mut().push(RequestRecord::new(label));
        }
    }

    /// Whatever a script printed with `print` or `debug`, one line at a time.
    ///
    /// **To stderr, in both formats**, alongside the `→` labels and every error,
    /// and for the same two reasons. A `--json` run must keep stdout holding one
    /// document and nothing else, and a script's `print` is not part of the
    /// result — it is the script author talking to whoever is watching the run.
    ///
    /// Core hands these back rather than printing them; this is where they
    /// become output. That is the whole of the arrangement: `sendra-core` has no
    /// `println!` or `eprintln!` in it, so a `sendra-tui` reusing the crate can
    /// put a script's chatter somewhere a redrawn frame will not wipe out,
    /// instead of having a library write over its interface.
    ///
    /// Nothing is recorded for `--json`. Capturing script output into the
    /// document is a debugging feature that has not been asked for, and adding
    /// a key nobody reads would freeze a shape before anyone knows what it
    /// should be. The lines are on stderr either way, which is where a person
    /// watching a run will look for them.
    pub(crate) fn script_output(&self, output: &ScriptOutput) {
        for line in output.lines() {
            eprintln!("{line}");
        }
    }

    /// A request came back.
    ///
    /// `script` is what its `post_request` script decided, or `None` when it
    /// declared no script; `assertions` is its report, empty when it declared
    /// none; `capture` is its capture report, empty when it declared no
    /// `capture` block. All three are printed under the response in the order
    /// they ran in: script, assertions, capture.
    pub(crate) fn responded(
        &self,
        response: &Response,
        script: Option<&ScriptOutcome>,
        assertions: &AssertionReport,
        capture: &CaptureReport,
    ) {
        if self.recording() {
            self.with_current(|record| {
                record.response = Some(ResponseRecord::from(response));
                record.post_request = script.map(PostRequestRecord::from);
                record.assertions = AssertionsRecord::from(assertions);
                // Null for a request that declared no block, the way
                // `post_request` is — an empty report is exactly that case,
                // since a block with entries always produces a result per entry.
                record.capture = (!capture.is_empty())
                    .then(|| CaptureRecord::new(capture, self.show_captures));
            });
            return;
        }

        match self.detail {
            Detail::Full => print_response(response),
            Detail::StatusOnly => print_status_line(response),
        }

        // Nothing at all when the request declared no script, so a file written
        // before this feature existed still prints what it always printed.
        if let Some(script) = script {
            print_post_request(script);
        }

        if assertions.is_empty() && script.is_none() && self.detail == Detail::StatusOnly {
            // `run` says nothing here, and must keep saying nothing. Under
            // `test` the silence is the problem: the summary is about to count
            // this request as one of N "without assertions", and without a
            // marker there is nothing to match that number against.
            //
            // A request with a `post_request` script is not one of those: it
            // was checked, the block above says so, and the summary counts it
            // as a pass or a failure.
            //
            // A `capture` block does not suppress it either, and deliberately:
            // a capture is not a check, the summary will count a request that
            // only captures as one of the uncovered, and this marker is what
            // that number points at.
            print_no_assertions();
        } else {
            print_assertions(assertions);
        }

        // Last, after the checks: those are about this response, the capture is
        // about the requests still to come. Nothing at all when the request
        // declared no `capture` block.
        print_capture(capture);
    }

    /// A request never got a response: it could not be built, or it could not
    /// be sent.
    ///
    /// The error is printed to stderr in both modes — that is where it already
    /// went, and a `--json` run whose output is being redirected should still
    /// say on the terminal that something failed.
    pub(crate) fn request_failed(&self, err: &SendraError) {
        print_error(err);

        if self.recording() {
            let message = error_message(err);
            self.with_current(|record| record.error = Some(message));
        }
    }

    /// `sendra run` is over.
    pub(crate) fn finish_run(&self) {
        if self.recording() {
            self.emit(None);
        }
    }

    /// `sendra test` is over: same document as [`finish_run`](Self::finish_run)
    /// with the counts added, or the human summary block.
    pub(crate) fn finish_test(&self, summary: &Summary) {
        if self.recording() {
            self.emit(Some(summary));
        } else {
            print_summary(summary);
        }
    }

    /// Apply `fill` to the request currently being reported on.
    ///
    /// The record was opened by [`request_started`](Self::request_started),
    /// which every request goes through before anything can be said about it,
    /// so there is always one to fill. Doing nothing if there somehow is not is
    /// the right failure: a missing field in the output beats a panic that
    /// takes the run with it.
    fn with_current(&self, fill: impl FnOnce(&mut RequestRecord)) {
        if let Some(record) = self.requests.borrow_mut().last_mut() {
            fill(record);
        }
    }

    /// Write the document to stdout.
    ///
    /// A write that fails — a closed pipe, most likely — is reported on stderr
    /// and changes nothing else. `--json` is a serialisation of the result, not
    /// part of deciding it, so the exit code is the one the run earned either
    /// way.
    fn emit(&self, summary: Option<&Summary>) {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();

        if let Err(err) = self.write_json(&mut out, summary) {
            print_error_line(format!("could not write --json output: {err}"));
        }
    }

    /// The whole of `--json`, against any writer, so the schema can be tested
    /// without a process to capture the stdout of.
    fn write_json(&self, out: &mut impl Write, summary: Option<&Summary>) -> std::io::Result<()> {
        let requests = self.requests.borrow();
        let document = RunDocument {
            requests: &requests,
            summary: summary.map(SummaryRecord::from),
        };

        // Pretty rather than compact: the output is as likely to be read by a
        // person scrolling a redirected file as by a program, and every JSON
        // parser is indifferent. `jq` output is pretty for the same reason.
        //
        // The serialisation itself cannot fail — these are owned strings,
        // numbers and bools, with no map keys that are not strings and no
        // `Serialize` impl of our own that could error.
        let json = serde_json::to_string_pretty(&document)
            .expect("the record types hold nothing that can fail to serialise");

        writeln!(out, "{json}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use sendra_core::ScriptOutcome;

    use crate::test_support::{assertions_from, response_with};

    /// The document the reporter would have written to stdout, parsed back.
    fn document(reporter: &Reporter, summary: Option<&Summary>) -> serde_json::Value {
        let mut out = Vec::new();
        reporter
            .write_json(&mut out, summary)
            .expect("a `Vec` never fails to be written to");

        let text = String::from_utf8(out).expect("the document is UTF-8");
        assert!(
            text.ends_with("}\n"),
            "the document is one object, newline-terminated: {text}"
        );

        serde_json::from_str(&text).expect("`--json` must emit parseable JSON")
    }

    // --- The `--json` document ------------------------------------------

    #[test]
    fn run_reports_one_object_holding_every_request() {
        let reporter = Reporter::new(Format::Json, Detail::Full, false);

        reporter.request_started("Get user");
        let response = response_with("application/json", r#"{"id":1}"#);
        let assertions =
            assertions_from("method: GET\nurl: https://example.com\nassertions:\n  status: 404\n")
                .evaluate(&response);
        reporter.responded(&response, None, &assertions, &CaptureReport::default());

        let document = document(&reporter, None);

        // `run` has no summary — the key is absent rather than null.
        assert_eq!(
            document.as_object().map(|object| object.len()),
            Some(1),
            "`run` emits `requests` and nothing else: {document}"
        );

        let request = &document["requests"][0];
        assert_eq!(request["label"], "Get user");
        assert_eq!(request["error"], serde_json::Value::Null);

        let reported = &request["response"];
        assert_eq!(reported["status"], 200);
        assert_eq!(reported["status_text"], "OK");
        assert_eq!(reported["elapsed_ms"], 12);
        assert_eq!(reported["headers"][0]["name"], "content-type");
        assert_eq!(reported["headers"][0]["value"], "application/json");
        // The raw body, not the indented one: rewriting the server's bytes
        // inside a document about them would be a lie about what came back.
        assert_eq!(reported["body"], r#"{"id":1}"#);

        let assertions = &request["assertions"];
        assert_eq!(assertions["total"], 1);
        assert_eq!(assertions["passed"], 0);
        assert_eq!(assertions["failed"], 1);
        assert_eq!(assertions["results"][0]["kind"], "status");
        assert_eq!(assertions["results"][0]["expectation"], "status is 404");
        assert_eq!(assertions["results"][0]["passed"], false);
        assert_eq!(assertions["results"][0]["failure"], "got 200");
    }

    #[test]
    fn a_request_that_never_got_a_response_carries_the_error_instead() {
        let reporter = Reporter::new(Format::Json, Detail::Full, false);

        reporter.request_started("GET https://example.com");
        reporter.request_failed(&SendraError::Io {
            path: "req.yaml".into(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
        });

        let request = &document(&reporter, None)["requests"][0];

        assert_eq!(request["response"], serde_json::Value::Null);
        // Message and cause on one line: the message alone names the file and
        // not the problem.
        assert_eq!(
            request["error"],
            "could not read request file `req.yaml`: no such file"
        );
        // Always an object, even with nothing in it, so a consumer can read
        // `.assertions.failed` for every request without checking for null.
        assert_eq!(request["assertions"]["total"], 0);
        assert_eq!(
            request["assertions"]["results"].as_array().unwrap().len(),
            0
        );
    }

    #[test]
    fn a_passing_assertion_reports_a_null_failure() {
        let reporter = Reporter::new(Format::Json, Detail::Full, false);

        reporter.request_started("Get user");
        let response = response_with("application/json", r#"{"id":1}"#);
        let assertions = assertions_from(
            "method: GET\nurl: https://example.com\nassertions:\n  status: 200\n  json:\n    $.id: 1\n",
        )
        .evaluate(&response);
        reporter.responded(&response, None, &assertions, &CaptureReport::default());

        let assertions = &document(&reporter, None)["requests"][0]["assertions"];

        assert_eq!(assertions["total"], 2);
        assert_eq!(assertions["passed"], 2);
        assert_eq!(assertions["failed"], 0);
        for result in assertions["results"].as_array().unwrap() {
            assert_eq!(result["passed"], true);
            assert_eq!(result["failure"], serde_json::Value::Null);
        }
        // The kinds are named after the YAML keys they were written as.
        assert_eq!(assertions["results"][0]["kind"], "status");
        assert_eq!(assertions["results"][1]["kind"], "json_path");
    }

    #[test]
    fn every_request_appears_in_file_order() {
        let reporter = Reporter::new(Format::Json, Detail::Full, false);

        for label in ["First", "Second", "Third"] {
            reporter.request_started(label);
            reporter.responded(
                &response_with("text/plain", "ok"),
                None,
                &AssertionReport::default(),
                &CaptureReport::default(),
            );
        }

        let requests = document(&reporter, None)["requests"].clone();
        let labels: Vec<&str> = requests
            .as_array()
            .unwrap()
            .iter()
            .map(|request| request["label"].as_str().unwrap())
            .collect();

        assert_eq!(labels, vec!["First", "Second", "Third"]);
    }

    #[test]
    fn test_reports_the_same_requests_plus_the_summary() {
        let reporter = Reporter::new(Format::Json, Detail::StatusOnly, false);

        reporter.request_started("Get user");
        let response = response_with("application/json", r#"{"id":1}"#);
        reporter.responded(
            &response,
            None,
            &AssertionReport::default(),
            &CaptureReport::default(),
        );

        let summary = Summary {
            total: 3,
            passed: 1,
            failed: 0,
            without_assertions: 1,
            no_response: 1,
        };
        let document = document(&reporter, Some(&summary));

        // `test` prints a status line only, but reports the whole response: the
        // terminal's brevity is about what is readable on a screen, and a
        // program reading this has no such problem.
        assert_eq!(document["requests"][0]["response"]["body"], r#"{"id":1}"#);
        assert_eq!(
            document["requests"][0]["response"]["headers"][0]["name"],
            "content-type"
        );

        // Every count, zeroes included — the terminal leaves a zero out, and a
        // consumer reading `.summary.failed` should not have to know that.
        assert_eq!(document["summary"]["total"], 3);
        assert_eq!(document["summary"]["passed"], 1);
        assert_eq!(document["summary"]["failed"], 0);
        assert_eq!(document["summary"]["without_assertions"], 1);
        assert_eq!(document["summary"]["no_response"], 1);
    }

    // --- `post_request` in the document ----------------------------------

    #[test]
    fn a_request_with_no_script_reports_a_null_post_request() {
        // The no-op guarantee in the schema: a file written before scripts
        // existed produces the document it always produced, plus one key that
        // is explicitly null rather than absent — so a consumer can read
        // `.post_request` on every request without checking whether this build
        // emits the key.
        let reporter = Reporter::new(Format::Json, Detail::Full, false);

        reporter.request_started("Get user");
        reporter.responded(
            &response_with("text/plain", "ok"),
            None,
            &AssertionReport::default(),
            &CaptureReport::default(),
        );

        let request = &document(&reporter, None)["requests"][0];
        assert_eq!(request["post_request"], serde_json::Value::Null);
    }

    #[test]
    fn a_passing_script_reports_a_null_failure() {
        // Same pair, in the same spelling, as a passing assertion result.
        let reporter = Reporter::new(Format::Json, Detail::Full, false);

        reporter.request_started("Create order");
        reporter.responded(
            &response_with("application/json", r#"{"id":1}"#),
            Some(&ScriptOutcome::Passed),
            &AssertionReport::default(),
            &CaptureReport::default(),
        );

        let script = &document(&reporter, None)["requests"][0]["post_request"];
        assert_eq!(script["passed"], true);
        assert_eq!(script["failure"], serde_json::Value::Null);
    }

    #[test]
    fn a_failed_script_carries_the_message_it_threw() {
        let reporter = Reporter::new(Format::Json, Detail::Full, false);

        reporter.request_started("Create order");
        reporter.responded(
            &response_with("application/json", "{}"),
            Some(&ScriptOutcome::Failed {
                message: "expected 201, got 500".to_string(),
            }),
            &AssertionReport::default(),
            &CaptureReport::default(),
        );

        let request = &document(&reporter, None)["requests"][0];
        assert_eq!(request["post_request"]["passed"], false);
        // Core's own wording, verbatim — the same string the terminal shows.
        assert_eq!(request["post_request"]["failure"], "expected 201, got 500");
        // And the two mechanisms stay separate in the document as they do in
        // the run: a failed script does not invent a failed assertion.
        assert_eq!(request["assertions"]["total"], 0);
        assert_eq!(request["assertions"]["failed"], 0);
    }

    #[test]
    fn a_script_and_assertions_are_reported_side_by_side() {
        let reporter = Reporter::new(Format::Json, Detail::StatusOnly, false);

        reporter.request_started("Create order");
        let response = response_with("application/json", r#"{"id":1}"#);
        let assertions =
            assertions_from("method: GET\nurl: https://example.com\nassertions:\n  status: 200\n")
                .evaluate(&response);
        reporter.responded(
            &response,
            Some(&ScriptOutcome::Passed),
            &assertions,
            &CaptureReport::default(),
        );

        let request = &document(&reporter, None)["requests"][0];
        assert_eq!(request["post_request"]["passed"], true);
        assert_eq!(request["assertions"]["total"], 1);
        assert_eq!(request["assertions"]["passed"], 1);
    }

    #[test]
    fn a_run_that_sent_nothing_is_still_a_document() {
        // Nothing reaches this today — an empty collection is refused when the
        // file is parsed — but `jq` should get a document rather than an empty
        // file if anything ever does.
        let reporter = Reporter::new(Format::Json, Detail::Full, false);

        assert_eq!(
            document(&reporter, None)["requests"],
            serde_json::json!([]),
            "an empty run is an empty list, not a missing key"
        );
    }

    #[test]
    fn the_human_reporter_records_nothing() {
        // The two formats are one decision, made once: a human run keeps no
        // records, so there is no second code path that could disagree with
        // what was printed.
        let reporter = Reporter::new(Format::Human, Detail::Full, false);

        reporter.request_started("Get user");
        reporter.responded(
            &response_with("text/plain", "ok"),
            None,
            &AssertionReport::default(),
            &CaptureReport::default(),
        );

        assert!(reporter.requests.borrow().is_empty());
    }
    // --- `capture` in the `--json` document ------------------------------

    /// The capture report a request declaring `capture` would produce against
    /// `body`, with nothing in the environment to collide with.
    fn capture_report(yaml: &str, body: &str) -> sendra_core::CaptureReport {
        let response = response_with("application/json", body);
        sendra_core::Document::from_yaml_str(yaml)
            .expect("the test request should parse")
            .requests()[0]
            .capture
            .as_ref()
            .expect("the test request has a capture block")
            .evaluate(&response, &sendra_core::Environment::default())
    }

    #[test]
    fn a_request_that_declared_no_capture_block_reports_null() {
        // The same distinction `post_request` draws: null is "nothing was
        // declared", which is not the same as a block that captured nothing.
        let reporter = Reporter::new(Format::Json, Detail::Full, false);
        reporter.request_started("Get user");
        reporter.responded(
            &response_with("application/json", "{}"),
            None,
            &AssertionReport::default(),
            &CaptureReport::default(),
        );

        let request = &document(&reporter, None)["requests"][0];
        assert_eq!(request["capture"], serde_json::Value::Null);
    }

    #[test]
    fn a_capture_reports_its_values_as_a_name_to_value_object() {
        // `show_captures: false` — Sendra's default — is exercised here: the
        // names are the real names a consumer chains off of
        // (`.capture.values.auth_token`), but the values behind them are
        // redacted, not the auth token and user id the response carried.
        let reporter = Reporter::new(Format::Json, Detail::Full, false);
        let body = r#"{"token":"abc123","user":{"id":42}}"#;
        let capture = capture_report(
            "method: POST\nurl: https://example.com\ncapture:\n  auth_token: $.token\n  user_id: $.user.id\n",
            body,
        );

        reporter.request_started("Log in");
        reporter.responded(
            &response_with("application/json", body),
            None,
            &AssertionReport::default(),
            &capture,
        );

        let capture = &document(&reporter, None)["requests"][0]["capture"];
        // Directly addressable: `.capture.values.auth_token`, not a search
        // through a list for the right `variable` — the name is real, only
        // the value behind it is not.
        assert_eq!(capture["values"]["auth_token"], "<redacted>");
        assert_eq!(capture["values"]["user_id"], "<redacted>");
        assert_eq!(capture["failures"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn show_captures_opts_back_into_the_raw_values() {
        // `--show-captures` — `false` was the shape every test above this one
        // exercised before this issue; this is the flag that gets the
        // pre-existing behaviour back.
        let reporter = Reporter::new(Format::Json, Detail::Full, true);
        let body = r#"{"token":"abc123","user":{"id":42}}"#;
        let capture = capture_report(
            "method: POST\nurl: https://example.com\ncapture:\n  auth_token: $.token\n  user_id: $.user.id\n",
            body,
        );

        reporter.request_started("Log in");
        reporter.responded(
            &response_with("application/json", body),
            None,
            &AssertionReport::default(),
            &capture,
        );

        let capture = &document(&reporter, None)["requests"][0]["capture"];
        assert_eq!(capture["values"]["auth_token"], "abc123");
        // A JSON number captures as the text it will be substituted as.
        assert_eq!(capture["values"]["user_id"], "42");
    }

    #[test]
    fn a_failed_capture_reports_the_name_the_path_and_the_reason_regardless_of_redaction() {
        let body = r#"{"token":"abc123"}"#;

        for show_captures in [false, true] {
            let reporter = Reporter::new(Format::Json, Detail::Full, show_captures);
            let capture = capture_report(
                "method: POST\nurl: https://example.com\ncapture:\n  auth_token: $.token\n  user_id: $.user.id\n",
                body,
            );

            reporter.request_started("Log in");
            reporter.responded(
                &response_with("application/json", body),
                None,
                &AssertionReport::default(),
                &capture,
            );

            let capture = &document(&reporter, None)["requests"][0]["capture"];
            // The one that worked is still reported, beside the one that did
            // not — redacted or not, depending on the flag.
            assert_eq!(
                capture["values"]["auth_token"],
                if show_captures { "abc123" } else { "<redacted>" }
            );
            assert_eq!(capture["values"]["user_id"], serde_json::Value::Null);

            // `failures` never carries a value in the first place, so the flag
            // changes nothing about it: variable names and paths were already
            // visible in the source YAML.
            let failures = capture["failures"].as_array().unwrap();
            assert_eq!(failures.len(), 1);
            assert_eq!(failures[0]["variable"], "user_id");
            assert_eq!(failures[0]["path"], "$.user.id");
            assert_eq!(
                failures[0]["failure"],
                "matched nothing in the response body"
            );
        }
    }
}
