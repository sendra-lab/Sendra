//! The `--junit` schema: JUnit XML, hand-rolled rather than through a crate —
//! the format is small and well-defined, every field here is a string or a
//! number, and the escaping it needs is five characters. A dependency would
//! buy nothing this doesn't already have to write by hand for the counts and
//! layout anyway. See `output/json.rs`'s doc comment for the same argument
//! applied to a different serialisation.
//!
//! This is its own minimal shape, built from the same inputs
//! [`Reporter::responded`](super::Reporter::responded) and
//! [`Reporter::request_failed`](super::Reporter::request_failed) already
//! receive, rather than a second consumer reaching into `json`'s record
//! types — `--json` and `--junit` are two promises about two different file
//! formats, and nothing is gained by making one depend on the other's
//! private schema.
//!
//! ## Where each Sendra outcome lands in JUnit's model
//!
//! - A request whose checks all held is a passing `<testcase>` — no child
//!   element, JUnit's own convention for a pass.
//! - A request that failed a check — a failing assertion, a `post_request`
//!   throw, or a `capture` entry that produced nothing — is a `<testcase>`
//!   with a `<failure>`. Every failure the request has, of any of those three
//!   kinds, is folded into *one* `<failure>` element, one line per failure:
//!   a request is one thing that either held or did not, the same "one
//!   string" choice the terminal's `assertions` block already makes for
//!   several failed checks on one request, and most CI viewers render only
//!   the first `<failure>` of a `<testcase>` that has several — so one
//!   element carrying every line is what actually gets read, where JUnit's
//!   less common multiple-elements-per-testcase form would silently drop all
//!   but the first in most of the systems this exists for.
//! - A request that never got a response — a `{{variable}}` with nothing
//!   behind it, a script that would not compile, a refused connection — is a
//!   `<testcase>` with an `<error>` rather than a `<failure>`. JUnit draws
//!   exactly the distinction Sendra already draws: `<error>` means the test
//!   itself could not run, `<failure>` means it ran and its expectation did
//!   not hold — the same "setup failure vs. expectation failure" line
//!   `no_response` vs. `failed` draws on
//!   [`Summary`](crate::exit::Summary).
//! - A request that checked nothing — no `assertions` block and no
//!   `post_request` script — is `<skipped>` rather than a bare pass.
//!   `without_assertions` is deliberately not `passed` in the terminal
//!   summary and not `passed` in `--json` either — see [`Summary`] — and a
//!   plain `<testcase>` with nothing under it is exactly what most JUnit
//!   readers show as a pass. `<skipped>` is the reading that does not
//!   disagree with the run's own summary about what "passed" means.

use std::fmt::Write as _;
use std::time::Duration;

use sendra_core::{AssertionReport, CaptureReport, Response, ScriptOutcome, SendraError};

use super::json::error_message;
use crate::exit::Summary;

/// One request, translated into JUnit's pass/failure/error/skipped model.
pub(super) struct Case {
    name: String,
    time_seconds: f64,
    status: Status,
    /// How many times `sendra_core::send_prepared` was called — see
    /// `RequestRecord::attempts` in `output/json.rs` for the full reasoning;
    /// this is the same number, carried into `--junit` as the `attempts`
    /// attribute on `<testcase>` rather than into a JSON field. Always at
    /// least `1`.
    attempts: usize,
}

enum Status {
    Passed,
    Skipped,
    /// Every failed check's message, already joined one-per-line — see the
    /// module doc for why this is one `<failure>` and not several.
    Failure(String),
    /// Why there was no response: message and causes joined the way
    /// `--json`'s `error` field already is — see [`error_message`].
    Error(String),
}

impl Case {
    /// Build a case from a response that came back — the same three checks
    /// [`Reporter::responded`](super::Reporter::responded) already has in
    /// hand, read and not modified.
    pub(super) fn from_response(
        name: String,
        response: &Response,
        script: Option<&ScriptOutcome>,
        assertions: &AssertionReport,
        capture: &CaptureReport,
        attempts: usize,
    ) -> Self {
        let mut failures = Vec::new();

        if let Some(message) = script.and_then(ScriptOutcome::failure) {
            failures.push(format!("post_request: {message}"));
        }
        for result in assertions.failures() {
            let detail = result.failure.as_deref().unwrap_or_default();
            failures.push(format!("{} — {detail}", result.expectation));
        }
        for result in capture.failures() {
            let detail = result
                .failure()
                .map(ToString::to_string)
                .unwrap_or_default();
            failures.push(format!(
                "capture `{}` from `{}`: {detail}",
                result.variable, result.path
            ));
        }

        // The same two questions `Summary::of` asks, in the same order —
        // did anything fail, then was anything checked at all — kept
        // separate from it rather than shared because `Summary` classifies a
        // whole `Outcome` (which also carries a status neither question
        // reads), and a `Case` has no status to ignore in the first place.
        let checked = script.is_some() || !assertions.is_empty();

        let status = if !failures.is_empty() {
            Status::Failure(failures.join("\n"))
        } else if !checked {
            Status::Skipped
        } else {
            Status::Passed
        };

        Self {
            name,
            time_seconds: response.elapsed.as_secs_f64(),
            status,
            attempts,
        }
    }

    /// Build a case from a request that never got a response — the same
    /// input [`Reporter::request_failed`](super::Reporter::request_failed)
    /// already has in hand.
    pub(super) fn from_error(name: String, err: &SendraError, attempts: usize) -> Self {
        Self {
            name,
            // No response, so no elapsed time to report.
            time_seconds: 0.0,
            status: Status::Error(error_message(err)),
            attempts,
        }
    }
}

/// Escape the five characters XML requires escaped in text content, and the
/// one more (`"`) that matters inside a double-quoted attribute value.
/// Escaping `"` in text too is harmless, and doing both here keeps every
/// caller — attribute or element body — a single function.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Render the whole report: one `<testsuites>` holding the one `<testsuite>`
/// this run produced.
///
/// The wrapping `<testsuites>` is what every major CI JUnit reader — GitHub
/// Actions, GitLab, Jenkins — has actually been built and tested against,
/// even though a lone `<testsuite>` is equally valid XML on its own; nesting
/// it is the safer default for something whose whole point is being read by
/// tools this crate does not control.
///
/// `total_time` is the run's wall-clock elapsed time, not the sum of each
/// request's own: a collection sent sequentially takes at least that long
/// either way, and the wall clock also carries whatever Sendra itself spent
/// between requests — script evaluation, substitution — which is the more
/// honest answer to "how long did this take" for something a CI system times
/// against a job budget.
///
/// The `<testsuite>` counts come straight from `summary` rather than being
/// re-derived from `cases`, so they are the same numbers by construction as
/// the ones the terminal and `--json` end the run with — see the acceptance
/// criteria this exists to meet.
pub(super) fn render(cases: &[Case], summary: &Summary, total_time: Duration) -> String {
    let mut out = String::new();

    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<testsuites>\n");
    let _ = writeln!(
        out,
        "  <testsuite name=\"sendra test\" tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\" time=\"{:.3}\">",
        summary.total,
        summary.failed,
        summary.no_response,
        summary.without_assertions,
        total_time.as_secs_f64(),
    );

    for case in cases {
        let name = escape(&case.name);
        // `attempts` is a Sendra-specific extension attribute — not part of
        // the JUnit spec, but unknown attributes are ignored by every major
        // reader (GitHub Actions, GitLab, Jenkins), and this is the same
        // number `--json`'s `attempts` field reports, in the one place a
        // `<testcase>` has to carry it. Always present, even for the
        // overwhelming majority of cases where it is `1` — a consumer
        // grepping for `attempts="` should not have to also handle it being
        // absent.
        let _ = write!(
            out,
            "    <testcase classname=\"{name}\" name=\"{name}\" time=\"{:.3}\" attempts=\"{}\">",
            case.time_seconds, case.attempts,
        );

        match &case.status {
            Status::Passed => {}
            Status::Skipped => out.push_str("<skipped/>"),
            Status::Failure(message) => {
                let escaped = escape(message);
                let _ = write!(out, "<failure message=\"{escaped}\">{escaped}</failure>");
            }
            Status::Error(message) => {
                let escaped = escape(message);
                let _ = write!(out, "<error message=\"{escaped}\">{escaped}</error>");
            }
        }

        out.push_str("</testcase>\n");
    }

    out.push_str("  </testsuite>\n");
    out.push_str("</testsuites>\n");

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::response_with;

    fn summary(
        total: usize,
        passed: usize,
        failed: usize,
        without: usize,
        no_response: usize,
    ) -> Summary {
        Summary {
            total,
            passed,
            failed,
            without_assertions: without,
            no_response,
        }
    }

    #[test]
    fn a_passing_case_is_a_bare_testcase() {
        let response = response_with("application/json", r#"{"id":1}"#);
        let assertions = sendra_core::Document::from_yaml_str(
            "method: GET\nurl: https://example.com\nassertions:\n  status: 200\n",
        )
        .unwrap()
        .requests()[0]
            .assertions
            .clone()
            .unwrap()
            .evaluate(&response);
        assert!(assertions.passed());

        let case = Case::from_response(
            "Get user".to_string(),
            &response,
            None,
            &assertions,
            &CaptureReport::default(),
            1,
        );
        assert!(matches!(case.status, Status::Passed));
    }

    #[test]
    fn a_request_that_checked_nothing_is_skipped() {
        // No script and no assertions — the third category, and JUnit's
        // `<skipped>` is the reading this issue settled on rather than a
        // bare pass.
        let response = response_with("text/plain", "ok");
        let case = Case::from_response(
            "Login".to_string(),
            &response,
            None,
            &AssertionReport::default(),
            &CaptureReport::default(),
            1,
        );
        let xml = render(&[case], &summary(1, 0, 0, 1, 0), Duration::ZERO);
        assert!(xml.contains("<skipped/>"), "{xml}");
        assert!(!xml.contains("<failure"), "{xml}");
    }

    #[test]
    fn a_capture_that_could_not_produce_a_value_and_is_reachable() {
        // `capture.failures()` is a real, exercised code path — build one
        // through the real evaluator rather than asserting on an empty stub.
        let response = response_with("application/json", r#"{"other":1}"#);
        let capture = sendra_core::Document::from_yaml_str(
            "method: GET\nurl: https://example.com\ncapture:\n  token: $.token\n",
        )
        .unwrap()
        .requests()[0]
            .capture
            .as_ref()
            .unwrap()
            .evaluate(&response, &sendra_core::Environment::default());
        assert!(!capture.passed());

        let case = Case::from_response(
            "Log in".to_string(),
            &response,
            None,
            &AssertionReport::default(),
            &capture,
            1,
        );
        match &case.status {
            Status::Failure(message) => {
                assert!(message.contains("token"), "{message}");
                assert!(message.contains("$.token"), "{message}");
            }
            _ => panic!("a failed capture must fail the case"),
        }
    }

    #[test]
    fn a_request_with_no_response_is_an_error_not_a_failure() {
        let err = sendra_core::SendraError::Io {
            path: "req.yaml".into(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
        };
        let case = Case::from_error("Get user".to_string(), &err, 1);
        let xml = render(&[case], &summary(1, 0, 0, 0, 1), Duration::ZERO);

        assert!(xml.contains("<error"), "{xml}");
        assert!(!xml.contains("<failure"), "{xml}");
        assert!(xml.contains("no such file"), "{xml}");
    }

    #[test]
    fn several_failures_on_one_request_are_one_failure_element() {
        let response = response_with("application/json", r#"{"id":1}"#);
        let assertions = sendra_core::Document::from_yaml_str(
            "method: GET\nurl: https://example.com\nassertions:\n  status: 404\n  body_contains: nope\n",
        )
        .unwrap()
        .requests()[0]
            .assertions
            .clone()
            .unwrap()
            .evaluate(&response);
        assert_eq!(assertions.failed_count(), 2);

        let case = Case::from_response(
            "Get user".to_string(),
            &response,
            None,
            &assertions,
            &CaptureReport::default(),
            1,
        );
        let xml = render(&[case], &summary(1, 0, 1, 0, 0), Duration::ZERO);

        // One element...
        assert_eq!(xml.matches("<failure").count(), 1, "{xml}");
        // ...carrying both messages.
        assert!(xml.contains("status is 404"), "{xml}");
        assert!(xml.contains("body contains"), "{xml}");
    }

    #[test]
    fn the_testsuite_counts_come_from_the_summary() {
        let xml = render(&[], &summary(5, 2, 1, 1, 1), Duration::from_millis(1500));

        assert!(xml.contains("tests=\"5\""), "{xml}");
        assert!(xml.contains("failures=\"1\""), "{xml}");
        assert!(xml.contains("errors=\"1\""), "{xml}");
        assert!(xml.contains("skipped=\"1\""), "{xml}");
        assert!(xml.contains("time=\"1.500\""), "{xml}");
    }

    #[test]
    fn special_characters_in_a_label_or_a_failure_message_are_escaped() {
        let response = response_with("application/json", r#"{"id":1}"#);
        let assertions = sendra_core::Document::from_yaml_str(
            "method: GET\nurl: https://example.com\nassertions:\n  body_contains: '<a> & \"b\"'\n",
        )
        .unwrap()
        .requests()[0]
            .assertions
            .clone()
            .unwrap()
            .evaluate(&response);
        assert!(!assertions.passed());

        let case = Case::from_response(
            "Get <users> & \"stuff\"".to_string(),
            &response,
            None,
            &assertions,
            &CaptureReport::default(),
            1,
        );
        let xml = render(&[case], &summary(1, 0, 1, 0, 0), Duration::ZERO);

        // No raw angle brackets or ampersands leaked into the document from
        // either the label or the failure text.
        assert!(!xml.contains("Get <users>"), "{xml}");
        assert!(
            xml.contains("Get &lt;users&gt; &amp; &quot;stuff&quot;"),
            "{xml}"
        );
        assert!(xml.contains("&lt;a&gt; &amp; &quot;b&quot;"), "{xml}");
    }
}
