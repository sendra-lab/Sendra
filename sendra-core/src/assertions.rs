//! Declarative checks a request makes against the response it gets back.
//!
//! A request file may carry an `assertions` block:
//!
//! ```yaml
//! method: GET
//! url: https://httpbin.org/get
//! assertions:
//!   status: 200
//!   headers:
//!     content-type: application/json   # present, with this exact value
//!     x-request-id:                    # present, value not checked
//!   body_contains: '"url"'
//!   json:
//!     $.headers.Accept: application/json
//! ```
//!
//! Every key is optional and a request with no `assertions` block behaves
//! exactly as it did before this module existed.
//!
//! **Evaluation never fails.** [`Assertions::evaluate`] returns an
//! [`AssertionReport`] and no `Result`: everything that could go wrong — a body
//! that is not JSON, a JSON path that does not parse, a header that is not there
//! — is a *failed assertion with a message*, not an error in the surrounding
//! run. The response has already arrived by the time any of this happens, so
//! there is nothing left to abort; the only useful thing to do with a broken
//! expectation is to say precisely how it broke, next to the ones that held.
//!
//! **Nothing here decides an exit code.** Evaluating and reporting is all this
//! module does; whether a failed assertion should fail the process is a
//! front-end decision, and today the answer is no. See the exit-code table in
//! `sendra-cli`.

use std::collections::BTreeMap;

use jsonpath_rust::JsonPath;
use serde::{Deserialize, Serialize};

use crate::Response;

/// The `assertions` block of a request, exactly as it appears on disk.
///
/// Each field is a separate *kind* of check, and each entry within a field is
/// one assertion — `headers` with three entries is three assertions, reported
/// individually. All of them are evaluated on every response; none short-circuit
/// the others, because "which of my expectations held" is the question this
/// feature exists to answer and stopping at the first failure would answer it
/// only partially.
///
/// Unknown keys are rejected, like everywhere else in Sendra's schema: an
/// assertion silently ignored because of a typo is worse than no assertion at
/// all, since it reads as a check that is passing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assertions {
    /// The exact status code the response must carry.
    ///
    /// Equality against one code rather than a class (`2xx`) or a range: the
    /// two are not the same assertion, and "this endpoint answers 201" is the
    /// one worth writing down. A class matcher can be added as its own key
    /// later without changing what this one means.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,

    /// Headers the response must carry, by name.
    ///
    /// A value asserts the header is present *and* equal to it; a null value
    /// (`x-request-id:` with nothing after it) asserts only that the header is
    /// there. One key covers both because they are the same assertion with and
    /// without an expectation about the value, and a second key
    /// (`headers_present`) would make the file say twice what the value's
    /// presence already says.
    ///
    /// Names are matched case-insensitively, because HTTP header names are.
    /// Values are matched exactly: `content-type: application/json` does *not*
    /// match `application/json; charset=utf-8`. That is the strict reading, and
    /// the honest one — a substring match would quietly accept
    /// `application/json-seq` too. When a server decorates a value, assert the
    /// whole value or drop to presence-only.
    ///
    /// A repeated header (`set-cookie`) passes if *any* of its values matches.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, Option<String>>,

    /// A substring the response body must contain, matched case-sensitively on
    /// the body as printed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_contains: Option<String>,

    /// JSON path expressions mapped to the value each must select.
    ///
    /// ```yaml
    /// json:
    ///   $.user.id: 42
    ///   $.user.name: ada
    ///   $.tags: [a, b]
    /// ```
    ///
    /// The expected value is written as YAML and held as a [`serde_json::Value`]
    /// — parsed once, when the file is loaded, into the form it will be compared
    /// against, so a value that has no JSON equivalent is a parse error naming
    /// the file rather than a surprise at response time.
    ///
    /// A path must select **exactly one** value. Nothing matched, or several
    /// matched, is a failure with that stated: `$.users[*].id` against three
    /// users is a question with no single answer, and picking the first would
    /// make the assertion depend on ordering the author never specified.
    ///
    /// The engine is [`jsonpath-rust`](https://docs.rs/jsonpath-rust), chosen
    /// over `serde_json_path`, the other RFC 9535 implementation, on
    /// maintenance and stability: at the time of writing jsonpath-rust is at
    /// `1.0` with releases landing this year, while `serde_json_path` has not
    /// released since February 2025 and is still pre-`1.0`. Both are correct
    /// and both query `serde_json::Value` directly, which is what keeps this
    /// dependency swappable if that ever changes: it is confined to
    /// [`Assertions::evaluate`], behind a path string and a value comparison.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub json: BTreeMap<String, serde_json::Value>,
}

/// Which kind of check produced a result, for a front-end that wants to group,
/// filter or colour by kind rather than parse the rendered text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssertionKind {
    Status,
    Header,
    BodyContains,
    JsonPath,
}

/// One assertion, evaluated.
///
/// Both strings are rendered in core rather than in the CLI so that every
/// front-end says the same thing about the same failure, and so the wording
/// lives next to the comparison that produced it. A front-end decides layout,
/// colour and symbols; it does not decide what "got 404" means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssertionResult {
    pub kind: AssertionKind,

    /// What was asserted, as a phrase: `status is 200`.
    pub expectation: String,

    /// Why it did not hold — `got 404` — or `None` if it did.
    ///
    /// An `Option` rather than a `bool` plus a message, so a result cannot be
    /// constructed claiming to have failed with nothing to say about it.
    pub failure: Option<String>,
}

impl AssertionResult {
    pub fn passed(&self) -> bool {
        self.failure.is_none()
    }

    fn pass(kind: AssertionKind, expectation: String) -> Self {
        Self {
            kind,
            expectation,
            failure: None,
        }
    }

    fn fail(kind: AssertionKind, expectation: String, failure: String) -> Self {
        Self {
            kind,
            expectation,
            failure: Some(failure),
        }
    }
}

/// Every assertion on one request, evaluated against one response, in a fixed
/// order: status, then headers, then `body_contains`, then JSON paths, with the
/// entries of each map in sorted order. Deterministic because the output is
/// read by people and diffed by scripts, and neither is served by an order that
/// depends on how a `BTreeMap` happened to be filled.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AssertionReport {
    results: Vec<AssertionResult>,
}

impl AssertionReport {
    pub fn results(&self) -> &[AssertionResult] {
        &self.results
    }

    /// No assertions were written, so nothing was checked.
    ///
    /// Distinct from [`passed`](Self::passed), which an empty report also
    /// answers `true` — vacuously. A front-end prints nothing at all for an
    /// empty report: a request with no assertions must look exactly as it did
    /// before assertions existed.
    pub fn is_empty(&self) -> bool {
        self.results.is_empty()
    }

    pub fn len(&self) -> usize {
        self.results.len()
    }

    /// Every assertion held (vacuously true when there are none).
    pub fn passed(&self) -> bool {
        self.results.iter().all(AssertionResult::passed)
    }

    pub fn passed_count(&self) -> usize {
        self.results.iter().filter(|result| result.passed()).count()
    }

    pub fn failed_count(&self) -> usize {
        self.results.len() - self.passed_count()
    }

    /// Just the assertions that did not hold, in evaluation order.
    pub fn failures(&self) -> impl Iterator<Item = &AssertionResult> {
        self.results.iter().filter(|result| !result.passed())
    }
}

impl Assertions {
    /// True when the block asserts nothing — `assertions: {}`, or a block whose
    /// every key was omitted.
    pub fn is_empty(&self) -> bool {
        self.status.is_none()
            && self.headers.is_empty()
            && self.body_contains.is_none()
            && self.json.is_empty()
    }

    /// Check every assertion against `response` and report all of them.
    ///
    /// The response must be the one that actually came back from the request as
    /// sent — after variable substitution and after config was applied — since
    /// that is the request the assertions were written about.
    pub fn evaluate(&self, response: &Response) -> AssertionReport {
        let mut results = Vec::new();

        if let Some(expected) = self.status {
            results.push(check_status(expected, response));
        }

        for (name, expected) in &self.headers {
            results.push(check_header(name, expected.as_deref(), response));
        }

        if let Some(needle) = &self.body_contains {
            results.push(check_body_contains(needle, response));
        }

        if !self.json.is_empty() {
            // Parsed once for the whole block, not once per path: the body does
            // not change between assertions, and a body that is not JSON should
            // report the same reason against every path rather than a different
            // one each time.
            let body = serde_json::from_str::<serde_json::Value>(&response.body);
            for (path, expected) in &self.json {
                results.push(check_json_path(path, expected, body.as_ref(), response));
            }
        }

        AssertionReport { results }
    }
}

fn check_status(expected: u16, response: &Response) -> AssertionResult {
    let expectation = format!("status is {expected}");
    if response.status == expected {
        AssertionResult::pass(AssertionKind::Status, expectation)
    } else {
        AssertionResult::fail(
            AssertionKind::Status,
            expectation,
            format!("got {}", response.status),
        )
    }
}

fn check_header(name: &str, expected: Option<&str>, response: &Response) -> AssertionResult {
    let expectation = match expected {
        Some(value) => format!("header `{name}` is `{value}`"),
        None => format!("header `{name}` is present"),
    };

    // Every value the response carries under this name; more than one is legal
    // (`set-cookie`), so the assertion holds if any of them matches.
    let seen: Vec<&str> = response
        .headers
        .iter()
        .filter(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
        .collect();

    if seen.is_empty() {
        // Name the headers that *are* there, the way a missing request name
        // lists the names a collection does have: the answer is usually a
        // casing or spelling difference visible the moment both are on screen.
        let present = response
            .headers
            .iter()
            .map(|(header, _)| header.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let detail = if present.is_empty() {
            "the response carries no headers at all".to_string()
        } else {
            format!("not present (the response has: {present})")
        };
        return AssertionResult::fail(AssertionKind::Header, expectation, detail);
    }

    match expected {
        None => AssertionResult::pass(AssertionKind::Header, expectation),
        Some(expected) if seen.contains(&expected) => {
            AssertionResult::pass(AssertionKind::Header, expectation)
        }
        Some(_) => {
            let got = seen
                .iter()
                .map(|value| format!("`{value}`"))
                .collect::<Vec<_>>()
                .join(", ");
            AssertionResult::fail(AssertionKind::Header, expectation, format!("got {got}"))
        }
    }
}

fn check_body_contains(needle: &str, response: &Response) -> AssertionResult {
    let expectation = format!("body contains `{needle}`");
    if response.body.contains(needle) {
        AssertionResult::pass(AssertionKind::BodyContains, expectation)
    } else {
        AssertionResult::fail(
            AssertionKind::BodyContains,
            expectation,
            format!("not found in the {}-byte body", response.body.len()),
        )
    }
}

/// One JSON path assertion, against the already-parsed body.
///
/// `body` is the shared parse result, so a body that is not JSON reports the
/// parser's own message — position included — rather than a vague "not JSON".
fn check_json_path(
    path: &str,
    expected: &serde_json::Value,
    body: Result<&serde_json::Value, &serde_json::Error>,
    response: &Response,
) -> AssertionResult {
    let expectation = format!("`{path}` is {}", render(expected));
    let fail = |detail: String| {
        AssertionResult::fail(AssertionKind::JsonPath, expectation.clone(), detail)
    };

    // The path is checked before the body, and deliberately so: a path that
    // does not parse is wrong about every response there could ever be, while a
    // body that is not JSON is a fact about this one. Told both, a reader wants
    // the one they have to go and fix.
    //
    // It is checked *here* rather than when the file is loaded, even though it
    // could be, so that loading a request file never depends on the path
    // grammar of this dependency: a stricter release would otherwise start
    // rejecting files that used to load, for a request Sendra could still have
    // sent.
    if let Err(err) = jsonpath_rust::parser::parse_json_path(path) {
        return fail(format!("not a valid JSON path: {err}"));
    }

    // A JSON path assertion written against a body that is not JSON is a failed
    // assertion, not an error: whether the body parses is a property of the
    // response, which does not exist until the request has been sent, so there
    // is nothing to reject at load time and nothing left to abort at this one.
    let body = match body {
        Ok(body) => body,
        Err(err) => {
            return fail(format!(
                "the response body is not JSON: {err}{}",
                match content_type(response) {
                    Some(content_type) => format!(" (content-type: {content_type})"),
                    // Worth saying: a body with no content-type at all is a
                    // different mistake from one that announced text/html.
                    None => " (no content-type header)".to_string(),
                }
            ));
        }
    };

    // Re-parsed by `query`, which is the cost of keeping the check above and
    // the evaluation below to one obvious call each; a path is a few dozen
    // bytes and this happens once per assertion.
    let selected = match body.query(path) {
        Ok(selected) => selected,
        Err(err) => return fail(format!("not a valid JSON path: {err}")),
    };

    match selected.as_slice() {
        [only] => {
            if *only == expected {
                AssertionResult::pass(AssertionKind::JsonPath, expectation)
            } else {
                fail(format!("got {}", render(only)))
            }
        }
        [] => fail("matched nothing in the response body".to_string()),
        many => fail(format!(
            "matched {} values ({}); an equality assertion needs a path that selects exactly one",
            many.len(),
            many.iter()
                .take(3)
                .map(|value| render(value))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// A JSON value as one line, for a message: `42`, `"ada"`, `{"id":1}`.
fn render(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

fn content_type(response: &Response) -> Option<&str> {
    response
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A response to assert against. Built by hand rather than sent: every test
    /// in this module is about the comparison, not about the network.
    fn response(status: u16, headers: &[(&str, &str)], body: &str) -> Response {
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

    fn json_response() -> Response {
        response(
            200,
            &[("content-type", "application/json")],
            r#"{"user": {"id": 42, "name": "ada"}, "tags": ["a", "b"]}"#,
        )
    }

    fn assertions(yaml: &str) -> Assertions {
        serde_yaml::from_str(yaml).expect("test assertions should parse")
    }

    /// The single failure in a report that is expected to hold exactly one.
    fn only_failure(report: &AssertionReport) -> &AssertionResult {
        let failures: Vec<&AssertionResult> = report.failures().collect();
        assert_eq!(failures.len(), 1, "expected one failure in {report:?}");
        failures[0]
    }

    #[test]
    fn a_matching_status_passes() {
        let report = assertions("status: 200").evaluate(&json_response());
        assert!(report.passed(), "{report:?}");
        assert_eq!(report.len(), 1);
        assert_eq!(report.results()[0].expectation, "status is 200");
    }

    #[test]
    fn a_different_status_fails_and_says_what_it_got() {
        let report = assertions("status: 200").evaluate(&response(404, &[], ""));
        assert!(!report.passed());
        let failure = only_failure(&report);
        assert_eq!(failure.kind, AssertionKind::Status);
        assert_eq!(failure.expectation, "status is 200");
        assert_eq!(failure.failure.as_deref(), Some("got 404"));
    }

    #[test]
    fn a_header_value_match_passes_regardless_of_name_casing() {
        // HTTP header names are case-insensitive, and which casing a server
        // sends is not something a request file should have to know.
        let report =
            assertions("headers:\n  Content-Type: application/json\n").evaluate(&json_response());
        assert!(report.passed(), "{report:?}");
    }

    #[test]
    fn a_header_with_a_null_value_asserts_only_presence() {
        let report = assertions("headers:\n  content-type:\n").evaluate(&json_response());
        assert!(report.passed(), "{report:?}");
        assert_eq!(
            report.results()[0].expectation,
            "header `content-type` is present"
        );
    }

    #[test]
    fn a_missing_header_fails_and_lists_the_ones_that_are_there() {
        let report = assertions("headers:\n  x-request-id:\n").evaluate(&json_response());
        let failure = only_failure(&report);
        assert_eq!(failure.kind, AssertionKind::Header);
        let detail = failure.failure.as_deref().unwrap();
        assert!(detail.contains("not present"), "got {detail}");
        assert!(detail.contains("content-type"), "got {detail}");
    }

    #[test]
    fn a_header_with_the_wrong_value_fails_and_shows_the_value_it_found() {
        let report = assertions("headers:\n  content-type: text/html\n").evaluate(&json_response());
        let failure = only_failure(&report);
        assert_eq!(
            failure.failure.as_deref(),
            Some("got `application/json`"),
            "the value seen is the whole point of the message"
        );
    }

    #[test]
    fn a_header_value_is_matched_exactly_not_by_prefix() {
        // The documented strictness: a decorated content-type is a different
        // value, and quietly accepting it would make the assertion mean
        // something the file does not say.
        let decorated = response(
            200,
            &[("content-type", "application/json; charset=utf-8")],
            "",
        );
        let report =
            assertions("headers:\n  content-type: application/json\n").evaluate(&decorated);
        assert!(!report.passed(), "a prefix must not count as a match");
    }

    #[test]
    fn a_repeated_header_passes_if_any_value_matches() {
        let repeated = response(200, &[("set-cookie", "a=1"), ("set-cookie", "b=2")], "");
        let report = assertions("headers:\n  set-cookie: b=2\n").evaluate(&repeated);
        assert!(report.passed(), "{report:?}");

        let report = assertions("headers:\n  set-cookie: c=3\n").evaluate(&repeated);
        let detail = only_failure(&report).failure.clone().unwrap();
        assert_eq!(detail, "got `a=1`, `b=2`", "both values should be shown");
    }

    #[test]
    fn body_contains_passes_on_a_substring_and_fails_otherwise() {
        let response = response(200, &[], "the operation was a success");

        let report = assertions("body_contains: success").evaluate(&response);
        assert!(report.passed(), "{report:?}");

        let report = assertions("body_contains: failure").evaluate(&response);
        let failure = only_failure(&report);
        assert_eq!(failure.kind, AssertionKind::BodyContains);
        assert_eq!(failure.expectation, "body contains `failure`");
        assert!(
            failure.failure.as_deref().unwrap().contains("27-byte body"),
            "got {failure:?}"
        );
    }

    #[test]
    fn body_contains_is_case_sensitive() {
        let report = assertions("body_contains: SUCCESS").evaluate(&response(200, &[], "success"));
        assert!(!report.passed(), "matching is on the bytes as they arrived");
    }

    #[test]
    fn a_json_path_equality_passes_for_numbers_strings_and_arrays() {
        let report = assertions("json:\n  $.user.id: 42\n  $.user.name: ada\n  $.tags: [a, b]\n")
            .evaluate(&json_response());
        assert!(report.passed(), "{report:?}");
        assert_eq!(report.len(), 3, "three paths are three assertions");
    }

    #[test]
    fn a_json_path_mismatch_fails_and_shows_the_value_found() {
        let report = assertions("json:\n  $.user.id: 7\n").evaluate(&json_response());
        let failure = only_failure(&report);
        assert_eq!(failure.kind, AssertionKind::JsonPath);
        assert_eq!(failure.expectation, "`$.user.id` is 7");
        assert_eq!(failure.failure.as_deref(), Some("got 42"));
    }

    #[test]
    fn a_json_path_that_matches_nothing_fails() {
        let report =
            assertions("json:\n  $.user.email: x@example.com\n").evaluate(&json_response());
        let detail = only_failure(&report).failure.clone().unwrap();
        assert!(detail.contains("matched nothing"), "got {detail}");
    }

    #[test]
    fn a_json_path_that_matches_several_values_fails_rather_than_picking_one() {
        // `$.tags[*]` selects two values; there is no single value to compare,
        // and taking the first would invent an ordering rule the file never
        // asked for.
        let report = assertions("json:\n  $.tags[*]: a\n").evaluate(&json_response());
        let detail = only_failure(&report).failure.clone().unwrap();
        assert!(detail.contains("matched 2 values"), "got {detail}");
    }

    #[test]
    fn a_json_path_against_a_body_that_is_not_json_is_a_failed_assertion() {
        // The case the issue calls out: not a panic, and not a load-time error
        // either — whether the body parses is only knowable once it arrives.
        let html = response(200, &[("content-type", "text/html")], "<html>nope</html>");
        let report = assertions("json:\n  $.user.id: 42\n").evaluate(&html);

        assert!(!report.passed());
        let failure = only_failure(&report);
        assert_eq!(failure.kind, AssertionKind::JsonPath);
        let detail = failure.failure.as_deref().unwrap();
        assert!(
            detail.contains("not JSON") && detail.contains("text/html"),
            "the message should say both what went wrong and what was served: {detail}"
        );
    }

    #[test]
    fn every_json_path_reports_the_same_reason_when_the_body_is_not_json() {
        let html = response(200, &[("content-type", "text/html")], "<html>nope</html>");
        let report = assertions("json:\n  $.a: 1\n  $.b: 2\n").evaluate(&html);
        assert_eq!(report.failed_count(), 2, "both paths are reported, not one");
    }

    #[test]
    fn a_body_that_is_not_json_and_has_no_content_type_says_so() {
        let bare = response(200, &[], "nope");
        let report = assertions("json:\n  $.a: 1\n").evaluate(&bare);
        let detail = only_failure(&report).failure.clone().unwrap();
        assert!(detail.contains("no content-type header"), "got {detail}");
    }

    #[test]
    fn a_json_path_is_evaluated_whatever_the_content_type_says() {
        // A JSON body served as text/plain is still a JSON body. The
        // content-type is used to explain a failure, never to decide whether to
        // try — refusing to look would fail an assertion that is plainly true.
        let mislabelled = response(200, &[("content-type", "text/plain")], r#"{"id": 1}"#);
        let report = assertions("json:\n  $.id: 1\n").evaluate(&mislabelled);
        assert!(report.passed(), "{report:?}");
    }

    #[test]
    fn a_malformed_json_path_is_a_failed_assertion_not_a_panic() {
        let report = assertions("json:\n  '$.[': 1\n").evaluate(&json_response());
        let detail = only_failure(&report).failure.clone().unwrap();
        assert!(detail.contains("not a valid JSON path"), "got {detail}");
    }

    #[test]
    fn a_malformed_path_is_reported_as_such_even_when_the_body_is_not_json() {
        // Both things are wrong; the path is the one the reader can fix, and
        // "your body is not JSON" would send them to look at the server.
        let html = response(200, &[("content-type", "text/html")], "<html></html>");
        let report = assertions("json:\n  '$.[': 1\n").evaluate(&html);
        let detail = only_failure(&report).failure.clone().unwrap();
        assert!(detail.contains("not a valid JSON path"), "got {detail}");
    }

    #[test]
    fn every_assertion_is_reported_not_just_the_first_failure() {
        // The acceptance criterion: a mixed block reports all of its parts, in
        // a fixed order, whichever of them failed.
        let report = assertions(
            "\
status: 201
headers:
  content-type: application/json
  x-missing: whatever
body_contains: ada
json:
  $.user.id: 42
  $.user.name: grace
",
        )
        .evaluate(&json_response());

        assert_eq!(report.len(), 6);
        assert_eq!(report.passed_count(), 3);
        assert_eq!(report.failed_count(), 3);
        assert!(!report.passed());

        // Fixed order: status, headers (sorted), body_contains, json (sorted).
        let expectations: Vec<&str> = report
            .results()
            .iter()
            .map(|result| result.expectation.as_str())
            .collect();
        assert_eq!(
            expectations,
            vec![
                "status is 201",
                "header `content-type` is `application/json`",
                "header `x-missing` is `whatever`",
                "body contains `ada`",
                "`$.user.id` is 42",
                "`$.user.name` is \"grace\"",
            ]
        );

        let failed: Vec<&str> = report
            .failures()
            .map(|result| result.expectation.as_str())
            .collect();
        assert_eq!(
            failed,
            vec![
                "status is 201",
                "header `x-missing` is `whatever`",
                "`$.user.name` is \"grace\"",
            ],
            "the passing assertions must not hide the failing ones, or vice versa"
        );
    }

    #[test]
    fn an_empty_report_is_vacuously_passing_and_knows_it_is_empty() {
        let report = Assertions::default().evaluate(&json_response());
        assert!(report.is_empty(), "nothing was asserted");
        assert!(report.passed(), "and so nothing failed");
        assert_eq!(report.failed_count(), 0);
    }

    #[test]
    fn an_unknown_assertion_key_is_a_parse_error() {
        // A typo'd assertion reads as a check that is passing, which is the
        // worst way for it to fail.
        let err = serde_yaml::from_str::<Assertions>("body_contain: success\n")
            .expect_err("a typo must not be silently ignored");
        assert!(err.to_string().contains("body_contain"), "got {err}");
    }

    #[test]
    fn an_expected_json_value_with_no_json_equivalent_is_a_parse_error() {
        // Held as a `serde_json::Value`, so an expected value with no JSON form
        // is rejected when the file is read rather than when the response
        // arrives. A sequence used as a mapping key is legal YAML and has no
        // JSON equivalent at all.
        let err = serde_yaml::from_str::<Assertions>("json:\n  $.a:\n    ? [x, y]\n    : one\n")
            .expect_err("a sequence key has no JSON equivalent");
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn a_scalar_key_in_an_expected_value_is_read_as_the_string_json_would_use() {
        // YAML allows non-string mapping keys and JSON does not, so `1:` is
        // read as `"1"` — the coercion any YAML-to-JSON conversion makes, and
        // the one that matches the object it will be compared against.
        let assertions = assertions("json:\n  $.a:\n    1: one\n");
        assert_eq!(
            assertions.json["$.a"],
            serde_json::json!({"1": "one"}),
            "a scalar key becomes its string form"
        );
    }
}
