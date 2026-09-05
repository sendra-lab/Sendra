//! Fixtures shared by more than one module's tests: the hand-built responses,
//! assertion reports and outcomes that stand in for a run without a socket.

use sendra_core::{AssertionReport, Document, Response, ScriptOutcome};

use crate::exit::Outcome;
use crate::output::{Detail, Format, Reporter};

/// A response to hand [`exit_for_response`]. Built by hand: none of these
/// tests need a socket, and the field values other than `status` never
/// enter into the decision.
pub(crate) fn response(status: u16) -> Response {
    Response {
        status,
        status_text: "Test".to_string(),
        headers: vec![("content-type".to_string(), "text/plain".to_string())],
        body: "body".to_string(),
        elapsed: std::time::Duration::from_millis(1),
    }
}

/// A response carrying `content_type` and `body`, for the tests that are
/// about how a body is rendered or reported rather than about its status.
pub(crate) fn response_with(content_type: &str, body: &str) -> Response {
    Response {
        status: 200,
        status_text: "OK".to_string(),
        headers: vec![("content-type".to_string(), content_type.to_string())],
        body: body.to_string(),
        elapsed: std::time::Duration::from_millis(12),
    }
}

/// The `assertions` block of a request file, parsed the way a real run
/// parses it — through `Document`, rather than by reaching for a YAML
/// dependency this crate does not otherwise need.
pub(crate) fn assertions_from(yaml: &str) -> sendra_core::Assertions {
    Document::from_yaml_str(yaml)
        .expect("the test request should parse")
        .requests()[0]
        .assertions
        .clone()
        .expect("the test request has an assertions block")
}

/// The outcome of a request that came back with `status` and checked
/// nothing about it — no assertions, no script: what a fake `send_one`
/// hands back when the response itself is not what the test is about.
pub(crate) fn responded(status: u16) -> Outcome {
    Outcome::Responded {
        status,
        script: None,
        assertions: AssertionReport::default(),
    }
}

/// The outcome of a request that came back with `status` carrying an
/// already-evaluated assertion report and no script.
pub(crate) fn checked(status: u16, assertions: AssertionReport) -> Outcome {
    Outcome::Responded {
        status,
        script: None,
        assertions,
    }
}

/// The outcome of a request that came back with `status` and declared a
/// `post_request` script and nothing else.
pub(crate) fn scripted(status: u16, script: ScriptOutcome) -> Outcome {
    Outcome::Responded {
        status,
        script: Some(script),
        assertions: AssertionReport::default(),
    }
}

/// A `post_request` script that threw.
pub(crate) fn script_failed() -> ScriptOutcome {
    ScriptOutcome::Failed {
        message: "expected 201, got 500".to_string(),
    }
}

/// An outcome that came back with `status` and declared one assertion,
/// which held.
pub(crate) fn all_passed(status: u16) -> Outcome {
    let report = assertions_from(&format!(
        "method: GET\nurl: https://example.com\nassertions:\n  status: {status}\n"
    ))
    .evaluate(&response(status));

    assert!(report.passed(), "the assertion asked for exactly {status}");
    checked(status, report)
}

/// An outcome that came back with `status` and declared both a passing
/// assertion and a passing `post_request` script — the shape a request has
/// when it uses both mechanisms at once.
pub(crate) fn all_passed_with_script(status: u16) -> Outcome {
    let Outcome::Responded { assertions, .. } = all_passed(status) else {
        unreachable!("`all_passed` builds a response")
    };

    Outcome::Responded {
        status,
        script: Some(ScriptOutcome::Passed),
        assertions,
    }
}

/// A reporter for tests that are about the sending loop rather than about
/// output.
///
/// Human format, which is what those tests have always exercised: it prints as
/// it goes, into the harness's captured stdout, and records nothing. A test
/// that is about `--json` builds its own [`Reporter`] and reads the document
/// back — see `output`'s tests.
pub(crate) fn reporter() -> Reporter {
    Reporter::new(Format::Human, Detail::StatusOnly)
}
