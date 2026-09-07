//! Shared fixtures for `assertions`'s test modules.
//!
//! One copy rather than one per submodule, since both the response-level
//! checks and the JSON operator grammar build the same kind of response and
//! parse the same kind of YAML.

use std::time::Duration;

use super::{AssertionReport, AssertionResult, Assertions};
use crate::Response;

/// A response to assert against. Built by hand rather than sent: every test
/// in this module is about the comparison, not about the network.
pub(super) fn response(status: u16, headers: &[(&str, &str)], body: &str) -> Response {
    Response {
        status,
        status_text: "OK".to_string(),
        headers: headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
        body: body.to_string(),
        elapsed: Duration::from_millis(1),
        redirects: Vec::new(),
    }
}

pub(super) fn json_response() -> Response {
    response(
        200,
        &[("content-type", "application/json")],
        r#"{"user": {"id": 42, "name": "ada"}, "tags": ["a", "b"]}"#,
    )
}

pub(super) fn assertions(yaml: &str) -> Assertions {
    serde_yaml::from_str(yaml).expect("test assertions should parse")
}

/// The single failure in a report that is expected to hold exactly one.
pub(super) fn only_failure(report: &AssertionReport) -> &AssertionResult {
    let failures: Vec<&AssertionResult> = report.failures().collect();
    assert_eq!(failures.len(), 1, "expected one failure in {report:?}");
    failures[0]
}
