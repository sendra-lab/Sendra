//! Values a request pulls out of its response and hands to the requests after
//! it.
//!
//! A request file may carry a `capture` block: variable names mapped to JSON
//! paths, evaluated against the response body once it arrives.
//!
//! ```yaml
//! name: Log in
//! method: POST
//! url: https://api.example.com/login
//! capture:
//!   auth_token: $.token
//!   user_id: $.user.id
//! ```
//!
//! Every name captured this way becomes usable as `{{auth_token}}` in **every
//! request after this one, in file order**, through the same substitution pass
//! an environment file feeds. Nothing is written anywhere: a capture lives for
//! the rest of one `sendra run` or `sendra test` invocation and no longer. A
//! fresh process starts with nothing captured, which is the same non-goal
//! environments shipped with.
//!
//! # A capture is not a check
//!
//! [`Captures::evaluate`] returns a [`CaptureReport`] and no `Result`, exactly
//! as [`Assertions::evaluate`](crate::Assertions::evaluate) does, and for the
//! same reason: the response has already arrived, so there is nothing left to
//! abort, and the only useful thing to do with a capture that did not work is
//! to say precisely how it did not work, next to the ones that did.
//!
//! But it is not an assertion either, and the difference decides how a
//! front-end counts it. An assertion is an expectation about the response; a
//! capture is a *dependency of the rest of the run*. So a capture that succeeds
//! says nothing about whether the response was correct — a request that
//! captured a token and asserted nothing was still not checked — while a
//! capture that fails is a genuine failure, because a value the file promised
//! to the requests downstream is not there. See [`CaptureReport::passed`] and
//! the `Summary` type in `sendra-cli` for where that lands.
//!
//! # Why the failures are typed rather than [`SendraError`]s
//!
//! A [`SendraError`] means "this request could not be completed", and every
//! variant of it is raised on a path where there is no response: a file that
//! does not parse, a `{{var}}` with nothing behind it, a refused connection, a
//! `pre_request` script that threw. A capture failure is the opposite shape —
//! the response arrived, was read, and did not contain what the file said it
//! would — and folding it into that enum would have put it in the one category
//! it is definitely not in.
//!
//! Typing it as [`CaptureFailure`] instead also keeps the block's entries
//! independent: three captures against one response produce three results, the
//! way three assertions do, rather than the first failure discarding whatever
//! the other two would have found.

use std::collections::BTreeMap;
use std::path::PathBuf;

use jsonpath_rust::JsonPath;
use serde::{Deserialize, Serialize};

use crate::environment::describe_environment;
use crate::{Environment, Response};

/// The `capture` block of a request, exactly as it appears on disk: variable
/// name to JSON path.
///
/// A map with author-chosen keys, so unlike every *struct* in Sendra's schema
/// there is no `deny_unknown_fields` to apply — every key here is data. The
/// rule that a typo must not pass silently still holds, one level down: a path
/// that selects nothing is a reported failure rather than a variable that
/// quietly does not exist.
///
/// Names are not validated against a pattern. A variable is whatever
/// `{{...}}` can spell, and an environment file has never restricted its own
/// keys either; a name nothing references is harmless, and one that cannot be
/// referenced is a mistake visible the moment it is used.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Captures {
    entries: BTreeMap<String, String>,
}

impl Captures {
    /// True when the block captures nothing — `capture: {}`.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The variable names this block defines, sorted.
    pub fn variables(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// The name-to-path pairs, sorted by name.
    pub fn entries(&self) -> &BTreeMap<String, String> {
        &self.entries
    }

    /// Extract every value this block names from `response`.
    ///
    /// `environment` is read for one thing only: a captured name that the
    /// environment file already defines is rejected rather than allowed to
    /// shadow it. See [`CaptureFailure::Shadowed`] for that decision.
    ///
    /// Entries are evaluated in sorted-name order and none short-circuits the
    /// others, so a report always has exactly one result per entry — the same
    /// contract [`AssertionReport`](crate::AssertionReport) makes.
    pub fn evaluate(&self, response: &Response, environment: &Environment) -> CaptureReport {
        if self.entries.is_empty() {
            return CaptureReport::default();
        }

        // Parsed once for the whole block, not once per path: the body does not
        // change between entries, and a body that is not JSON should report the
        // same reason against every one of them.
        let body = serde_json::from_str::<serde_json::Value>(&response.body);

        CaptureReport {
            results: self
                .entries
                .iter()
                .map(|(variable, path)| {
                    capture_one(variable, path, body.as_ref(), response, environment)
                })
                .collect(),
        }
    }
}

impl FromIterator<(String, String)> for Captures {
    fn from_iter<T: IntoIterator<Item = (String, String)>>(iter: T) -> Self {
        Self {
            entries: iter.into_iter().collect(),
        }
    }
}

/// Why one entry of a `capture` block did not produce a value.
///
/// Typed rather than a bare string so a front-end can branch on it — and so
/// the granularity the assertion JSON paths already draw ("not a valid JSON
/// path" is a broken file, "the body is not JSON" is a fact about this
/// response, "matched nothing" is a fact about the pair) survives into
/// anything reading a run rather than being recoverable only by matching on
/// prose. [`Display`](std::fmt::Display) renders the wording, in core, so every
/// front-end says the same thing about the same failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureFailure {
    /// The name is already defined by the active environment file. See the
    /// note on this variant's message for why that is rejected rather than
    /// resolved in either direction.
    Shadowed {
        /// The environment file that already defines the name, or `None` when
        /// the environment did not come from a file.
        environment: Option<PathBuf>,
    },

    /// The path is not a JSON path at all.
    InvalidPath { reason: String },

    /// The response body did not parse as JSON, so there was nothing to query.
    BodyNotJson {
        /// serde_json's own message, position included.
        reason: String,
        /// The response's `content-type`, when it had one — a body with none at
        /// all is a different mistake from one that announced `text/html`.
        content_type: Option<String>,
    },

    /// The path is valid and the body is JSON, and the path selected nothing.
    NoMatch,

    /// The path selected more than one value, so there is no single value to
    /// bind the name to.
    Ambiguous {
        count: usize,
        /// The first few matches, rendered, for the message.
        sample: Vec<String>,
    },

    /// The path selected exactly one value and that value has no text form a
    /// `{{name}}` could be replaced with: `null`, an array or an object.
    NotAScalar {
        /// `null`, `an array`, `an object`.
        kind: &'static str,
    },
}

impl std::fmt::Display for CaptureFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureFailure::Shadowed { environment } => write!(
                f,
                "{} already defines this variable; rename the capture or the environment entry",
                describe_environment(environment)
            ),
            CaptureFailure::InvalidPath { reason } => write!(f, "not a valid JSON path: {reason}"),
            CaptureFailure::BodyNotJson {
                reason,
                content_type,
            } => write!(
                f,
                "the response body is not JSON: {reason}{}",
                match content_type {
                    Some(content_type) => format!(" (content-type: {content_type})"),
                    None => " (no content-type header)".to_string(),
                }
            ),
            CaptureFailure::NoMatch => f.write_str("matched nothing in the response body"),
            CaptureFailure::Ambiguous { count, sample } => write!(
                f,
                "matched {count} values ({}); a capture needs a path that selects exactly one",
                sample.join(", ")
            ),
            CaptureFailure::NotAScalar { kind } => write!(
                f,
                "matched {kind}, which has no text form to substitute; capture a string, \
                 number or boolean"
            ),
        }
    }
}

/// One entry of a `capture` block, evaluated.
///
/// `value` and `failure` are the two halves of one answer and exactly one of
/// them is ever set; the constructors below are private so a result cannot be
/// built claiming both or neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureResult {
    /// The variable name this entry defines.
    pub variable: String,
    /// The JSON path it was read from, as written in the file.
    pub path: String,
    value: Option<String>,
    failure: Option<CaptureFailure>,
}

impl CaptureResult {
    pub fn passed(&self) -> bool {
        self.failure.is_none()
    }

    /// The text this entry captured, or `None` if it did not.
    pub fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }

    /// Why the entry produced no value, or `None` if it did.
    pub fn failure(&self) -> Option<&CaptureFailure> {
        self.failure.as_ref()
    }

    fn captured(variable: &str, path: &str, value: String) -> Self {
        Self {
            variable: variable.to_string(),
            path: path.to_string(),
            value: Some(value),
            failure: None,
        }
    }

    fn fail(variable: &str, path: &str, failure: CaptureFailure) -> Self {
        Self {
            variable: variable.to_string(),
            path: path.to_string(),
            value: None,
            failure: Some(failure),
        }
    }
}

/// Every entry of one request's `capture` block, evaluated against its
/// response, in sorted-name order.
///
/// The default is the empty report, which is what a request with no `capture`
/// block produces: nothing captured, nothing failed, and nothing printed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaptureReport {
    results: Vec<CaptureResult>,
}

impl CaptureReport {
    pub fn results(&self) -> &[CaptureResult] {
        &self.results
    }

    /// No `capture` block was declared, or it was empty.
    pub fn is_empty(&self) -> bool {
        self.results.is_empty()
    }

    pub fn len(&self) -> usize {
        self.results.len()
    }

    /// Every entry produced a value (vacuously true when there are none).
    pub fn passed(&self) -> bool {
        self.results.iter().all(CaptureResult::passed)
    }

    pub fn captured_count(&self) -> usize {
        self.results.iter().filter(|result| result.passed()).count()
    }

    pub fn failed_count(&self) -> usize {
        self.results.len() - self.captured_count()
    }

    /// Just the entries that produced no value, in evaluation order.
    pub fn failures(&self) -> impl Iterator<Item = &CaptureResult> {
        self.results.iter().filter(|result| !result.passed())
    }

    /// The name-to-value pairs this request contributes to the run.
    ///
    /// Only the entries that succeeded: a failed capture defines nothing, which
    /// is what makes the downstream `{{name}}` a `VariableNotFound` naming the
    /// variable rather than a request sent with an empty string in it.
    pub fn values(&self) -> BTreeMap<String, String> {
        self.results
            .iter()
            .filter_map(|result| {
                result
                    .value()
                    .map(|value| (result.variable.clone(), value.to_string()))
            })
            .collect()
    }
}

/// One entry of a `capture` block, against the already-parsed body.
///
/// The checks are ordered most-general-first, which is the same order
/// `check_json_path` in [`assertions`](crate::assertions) uses and for the same
/// reason: told about several problems at once, a reader wants the one that is
/// wrong about every response rather than the one that is wrong about this one.
/// A name that collides with the environment is wrong before a request is ever
/// sent; a path that does not parse is wrong about every response there could
/// be; a body that is not JSON is a fact about this response; a path that
/// matched nothing is a fact about the two together.
fn capture_one(
    variable: &str,
    path: &str,
    body: Result<&serde_json::Value, &serde_json::Error>,
    response: &Response,
    environment: &Environment,
) -> CaptureResult {
    let fail = |failure| CaptureResult::fail(variable, path, failure);

    if environment.variables.contains_key(variable) {
        return fail(CaptureFailure::Shadowed {
            environment: environment.source.clone(),
        });
    }

    // Checked here rather than when the file is loaded, even though it could
    // be, for the reason `assertions` gives: loading a request file should
    // never depend on the path grammar of this dependency, or a stricter
    // release would start rejecting files that used to load.
    if let Err(err) = jsonpath_rust::parser::parse_json_path(path) {
        return fail(CaptureFailure::InvalidPath {
            reason: err.to_string(),
        });
    }

    let body = match body {
        Ok(body) => body,
        Err(err) => {
            return fail(CaptureFailure::BodyNotJson {
                reason: err.to_string(),
                content_type: content_type(response).map(str::to_owned),
            })
        }
    };

    let selected = match body.query(path) {
        Ok(selected) => selected,
        Err(err) => {
            return fail(CaptureFailure::InvalidPath {
                reason: err.to_string(),
            })
        }
    };

    match selected.as_slice() {
        [only] => match scalar_text(only) {
            Ok(text) => CaptureResult::captured(variable, path, text),
            Err(kind) => fail(CaptureFailure::NotAScalar { kind }),
        },
        [] => fail(CaptureFailure::NoMatch),
        many => fail(CaptureFailure::Ambiguous {
            count: many.len(),
            sample: many
                .iter()
                .take(3)
                .map(|value| serde_json::to_string(value).unwrap_or_else(|_| value.to_string()))
                .collect(),
        }),
    }
}

/// The text a captured JSON value is substituted as, or the name of the kind
/// that has no such text.
///
/// A string captures **unquoted** — `"ada"` becomes `ada`, not `"ada"` — because
/// substitution replaces `{{name}}` inside a URL, a header or a body, and the
/// quotes are JSON's punctuation rather than part of the value.
///
/// Numbers and booleans capture as `serde_json` renders them, which is the
/// value and not the spelling: `42` is `42` and `true` is `true`, but a body
/// that said `1.50` captures as `1.5`, because the body was parsed into an
/// `f64` before anything here saw it. That is a real difference from an
/// environment file, where `port: 8080` is the *string* `8080` and nothing is
/// normalised — and it is the honest one to expose, since the value really did
/// make a round trip through a number. Pretending otherwise would need
/// `serde_json`'s `arbitrary_precision`, which changes how every JSON assertion
/// in the crate compares numbers; an endpoint whose exact digits matter should
/// send them as a JSON string.
///
/// `null`, arrays and objects are refused. `null` has no text form that is not
/// a guess between `""` and `null`. An array or an object has one — compact
/// JSON — but substitution's entire safety argument is that a substituted value
/// cannot change the shape of what it lands in, and pushing `{"a":1}` into a
/// URL or a header is exactly that hazard. Refusing is the reversible choice:
/// it can be relaxed later, while a build that had already been serialising
/// objects into URLs could not be tightened.
fn scalar_text(value: &serde_json::Value) -> Result<String, &'static str> {
    use serde_json::Value;
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Number(number) => Ok(number.to_string()),
        Value::Bool(flag) => Ok(flag.to_string()),
        Value::Null => Err("null"),
        Value::Array(_) => Err("an array"),
        Value::Object(_) => Err("an object"),
    }
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

    fn response(headers: &[(&str, &str)], body: &str) -> Response {
        Response {
            status: 200,
            status_text: "OK".to_string(),
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            body: body.to_string(),
            elapsed: Duration::from_millis(1),
        }
    }

    fn json_response() -> Response {
        response(
            &[("content-type", "application/json")],
            r#"{"token": "abc123", "user": {"id": 42, "admin": true}, "tags": ["a", "b"],
                "price": 1.50, "nothing": null}"#,
        )
    }

    fn captures(yaml: &str) -> Captures {
        serde_yaml::from_str(yaml).expect("test capture block should parse")
    }

    /// The report for `yaml` against `json_response`, with no environment to
    /// collide with.
    fn report(yaml: &str) -> CaptureReport {
        captures(yaml).evaluate(&json_response(), &Environment::default())
    }

    fn only(report: &CaptureReport) -> &CaptureResult {
        assert_eq!(report.len(), 1, "expected one result: {report:?}");
        &report.results()[0]
    }

    #[test]
    fn captures_a_string_without_its_json_quotes() {
        let report = report("auth_token: $.token\n");
        assert_eq!(only(&report).value(), Some("abc123"));
        assert!(report.passed());
        assert_eq!(
            report.values(),
            BTreeMap::from([("auth_token".to_string(), "abc123".to_string())])
        );
    }

    #[test]
    fn captures_numbers_and_booleans_as_their_value_not_their_spelling() {
        // `1.50` in the body captures as `1.5`: the body was parsed into an
        // `f64` before this saw it, and pinning that here is what stops the
        // documented behaviour and the real one drifting apart.
        let report = report("id: $.user.id\nadmin: $.user.admin\nprice: $.price\n");
        assert_eq!(
            report.values(),
            BTreeMap::from([
                ("admin".to_string(), "true".to_string()),
                ("id".to_string(), "42".to_string()),
                ("price".to_string(), "1.5".to_string()),
            ])
        );
    }

    #[test]
    fn a_path_that_matches_nothing_is_a_reported_failure() {
        let report = report("missing: $.nope\n");
        assert!(!report.passed());
        assert_eq!(only(&report).failure(), Some(&CaptureFailure::NoMatch));
        assert_eq!(only(&report).value(), None);
        assert!(report.values().is_empty(), "nothing is defined by a miss");
        assert!(
            only(&report)
                .failure()
                .unwrap()
                .to_string()
                .contains("matched nothing"),
            "the message is the one a user reads"
        );
    }

    #[test]
    fn a_path_matching_several_values_is_ambiguous_rather_than_first_wins() {
        let report = report("tag: $.tags[*]\n");
        match only(&report).failure() {
            Some(CaptureFailure::Ambiguous { count, sample }) => {
                assert_eq!(*count, 2);
                assert_eq!(sample, &[r#""a""#.to_string(), r#""b""#.to_string()]);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn null_arrays_and_objects_have_no_text_form_to_substitute() {
        for (path, kind) in [
            ("$.nothing", "null"),
            ("$.tags", "an array"),
            ("$.user", "an object"),
        ] {
            let report = report(&format!("v: {path}\n"));
            assert_eq!(
                only(&report).failure(),
                Some(&CaptureFailure::NotAScalar { kind }),
                "{path} should not capture"
            );
        }
    }

    #[test]
    fn a_body_that_is_not_json_reports_the_parser_message_and_the_content_type() {
        let captures = captures("v: $.token\n");
        let report = captures.evaluate(
            &response(&[("content-type", "text/html")], "<html></html>"),
            &Environment::default(),
        );
        match only(&report).failure() {
            Some(CaptureFailure::BodyNotJson {
                reason,
                content_type,
            }) => {
                assert!(!reason.is_empty());
                assert_eq!(content_type.as_deref(), Some("text/html"));
            }
            other => panic!("expected BodyNotJson, got {other:?}"),
        }
    }

    #[test]
    fn a_body_with_no_content_type_says_so_rather_than_naming_one() {
        let report =
            captures("v: $.token\n").evaluate(&response(&[], "not json"), &Environment::default());
        let message = only(&report).failure().unwrap().to_string();
        assert!(message.contains("no content-type header"), "got {message}");
    }

    #[test]
    fn a_path_that_is_not_a_json_path_is_told_apart_from_one_that_missed() {
        let report = report("v: not a path\n");
        assert!(
            matches!(
                only(&report).failure(),
                Some(CaptureFailure::InvalidPath { .. })
            ),
            "got {:?}",
            only(&report).failure()
        );
    }

    #[test]
    fn a_name_the_environment_already_defines_is_refused_rather_than_shadowing_it() {
        // The precedence decision, at the point it is made: neither value
        // silently wins, because the same `{{auth_token}}` would otherwise mean
        // the environment's value before this request and the captured one
        // after it.
        let environment = Environment::from_yaml_str("auth_token: from-the-file\n").unwrap();
        let report = captures("auth_token: $.token\n").evaluate(&json_response(), &environment);

        assert!(!report.passed());
        assert!(
            matches!(
                only(&report).failure(),
                Some(CaptureFailure::Shadowed { .. })
            ),
            "got {:?}",
            only(&report).failure()
        );
        assert!(
            report.values().is_empty(),
            "a refused capture defines nothing, so the environment's value stands"
        );
    }

    #[test]
    fn a_collision_is_checked_before_the_path_is_even_read() {
        // A shadowed name is wrong about every response there could be, so it
        // is the failure worth reporting even when the path is also broken.
        let environment = Environment::from_yaml_str("v: x\n").unwrap();
        let report = captures("v: not a path\n").evaluate(&json_response(), &environment);
        assert!(
            matches!(
                only(&report).failure(),
                Some(CaptureFailure::Shadowed { .. })
            ),
            "got {:?}",
            only(&report).failure()
        );
    }

    #[test]
    fn one_entry_failing_does_not_stop_the_others() {
        let report = report("good: $.token\nbad: $.nope\nalso_good: $.user.id\n");
        assert_eq!(report.len(), 3, "one result per entry, always");
        assert_eq!(report.captured_count(), 2);
        assert_eq!(report.failed_count(), 1);
        assert_eq!(report.failures().count(), 1);
        assert_eq!(
            report.values(),
            BTreeMap::from([
                ("also_good".to_string(), "42".to_string()),
                ("good".to_string(), "abc123".to_string()),
            ])
        );
    }

    #[test]
    fn an_empty_block_captures_nothing_and_reports_nothing() {
        let report = captures("{}\n").evaluate(&json_response(), &Environment::default());
        assert!(report.is_empty());
        assert!(report.passed(), "vacuously");
        assert!(report.values().is_empty());
    }
}
