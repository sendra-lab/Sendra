//! The `json:` operator sub-language: telling a bare equality value apart
//! from an operator object, and evaluating whichever it turns out to be
//! against the one value a JSON path selected.
//!
//! See the `assertions` module docs for the disambiguation rule between a
//! bare value and an operator, and for why a hard error here — a malformed
//! path, a type mismatch, a malformed operator argument — is not something
//! `not:` can turn into a pass.

use jsonpath_rust::JsonPath;
use regex::Regex;

use super::{expectation_text, finish, AssertionKind, AssertionResult};
use crate::Response;

/// One JSON path assertion, against the already-parsed body.
///
/// `body` is the shared parse result, so a body that is not JSON reports the
/// parser's own message — position included — rather than a vague "not JSON".
pub(super) fn check_json_path(
    path: &str,
    value: &serde_json::Value,
    body: Result<&serde_json::Value, &serde_json::Error>,
    response: &Response,
    negate: bool,
) -> AssertionResult {
    // Used for every hard-error return before the operator has even been
    // parsed — as close to the bare-equality wording as a malformed value
    // allows, since there is no operator description to fall back on yet.
    let fallback_expectation = expectation_text(
        format!("`{path}` is {}", render(value)),
        format!("`{path}` is not {}", render(value)),
        negate,
    );

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
        return AssertionResult::fail(
            AssertionKind::JsonPath,
            fallback_expectation,
            format!("not a valid JSON path: {err}"),
        );
    }

    // Disambiguating a bare equality value from an operator object is a
    // property of the written assertion, not of the response, so it is
    // resolved (and can hard-fail) before the body is even looked at — same
    // tier as the path syntax check above.
    let spec = match JsonSpec::parse(value) {
        Ok(spec) => spec,
        Err(err) => {
            return AssertionResult::fail(AssertionKind::JsonPath, fallback_expectation, err)
        }
    };

    let expectation = expectation_text(
        spec.describe(path, false),
        spec.describe(path, true),
        negate,
    );
    let fail = |detail: String| {
        AssertionResult::fail(AssertionKind::JsonPath, expectation.clone(), detail)
    };

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

    let actual = match selected.as_slice() {
        [only] => *only,
        [] => return fail("matched nothing in the response body".to_string()),
        many => {
            return fail(format!(
                "matched {} values ({}); an assertion needs a path that selects exactly one",
                many.len(),
                many.iter()
                    .take(3)
                    .map(|value| render(value))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    };

    match spec.evaluate(actual) {
        Ok(outcome) => finish(
            AssertionKind::JsonPath,
            outcome.holds,
            expectation,
            negate,
            outcome.detail_if_false,
            outcome.detail_if_true,
        ),
        Err(type_error) => fail(type_error),
    }
}

/// A `json:` path's expected value, once told apart from a bare equality
/// value. See the module docs for the disambiguation rule.
///
/// There is no `not_equal`: `not: {json: {$.path: value}}` already says that,
/// and precisely — see the module docs for why that generalises to equality
/// but not to the comparison operators here.
enum JsonSpec<'a> {
    Equals(&'a serde_json::Value),
    GreaterThan(f64, &'a serde_json::Value),
    GreaterThanOrEqual(f64, &'a serde_json::Value),
    LessThan(f64, &'a serde_json::Value),
    LessThanOrEqual(f64, &'a serde_json::Value),
    Contains(&'a serde_json::Value),
    Length(LengthSpec),
    /// A regular expression matched against the path's string value. The
    /// scoped counterpart of `body_matches`: the same engine, applied to one
    /// selected value instead of the whole body.
    Matches(Regex, &'a str),
}

/// The value under a `length` operator: a bare number (equality) or a nested
/// comparison — `{greater_than: n}`, `{greater_than_or_equal: n}`,
/// `{less_than: n}` or `{less_than_or_equal: n}`.
enum LengthSpec {
    Equals(u64),
    GreaterThan(u64),
    GreaterThanOrEqual(u64),
    LessThan(u64),
    LessThanOrEqual(u64),
}

/// The outcome of comparing an already-selected JSON value against a
/// [`JsonSpec`]: whether the (unnegated) comparison holds, and the detail
/// string for each direction it could fail — see [`finish`].
struct JsonOutcome {
    holds: bool,
    detail_if_false: String,
    detail_if_true: String,
}

impl<'a> JsonSpec<'a> {
    /// Reads `value` as an operator object if it is a single-key mapping with
    /// one of the recognised keys, and as a bare equality value otherwise.
    /// `Err` is a malformed operator — a comparison whose argument is not a
    /// number, a `matches` whose argument is not a string or not a valid
    /// regular expression, or a `length` whose argument is none of the
    /// above — which is a fact about the file, not the response.
    fn parse(value: &'a serde_json::Value) -> Result<Self, String> {
        if let serde_json::Value::Object(map) = value {
            if map.len() == 1 {
                if let Some(arg) = map.get("greater_than") {
                    let n = arg.as_f64().ok_or_else(|| {
                        format!("`greater_than` expects a number, got {}", render(arg))
                    })?;
                    return Ok(JsonSpec::GreaterThan(n, arg));
                }
                if let Some(arg) = map.get("greater_than_or_equal") {
                    let n = arg.as_f64().ok_or_else(|| {
                        format!(
                            "`greater_than_or_equal` expects a number, got {}",
                            render(arg)
                        )
                    })?;
                    return Ok(JsonSpec::GreaterThanOrEqual(n, arg));
                }
                if let Some(arg) = map.get("less_than") {
                    let n = arg.as_f64().ok_or_else(|| {
                        format!("`less_than` expects a number, got {}", render(arg))
                    })?;
                    return Ok(JsonSpec::LessThan(n, arg));
                }
                if let Some(arg) = map.get("less_than_or_equal") {
                    let n = arg.as_f64().ok_or_else(|| {
                        format!("`less_than_or_equal` expects a number, got {}", render(arg))
                    })?;
                    return Ok(JsonSpec::LessThanOrEqual(n, arg));
                }
                if let Some(arg) = map.get("contains") {
                    return Ok(JsonSpec::Contains(arg));
                }
                if let Some(arg) = map.get("length") {
                    return LengthSpec::parse(arg).map(JsonSpec::Length);
                }
                if let Some(arg) = map.get("matches") {
                    let pattern = arg.as_str().ok_or_else(|| {
                        format!("`matches` expects a string pattern, got {}", render(arg))
                    })?;
                    let regex = Regex::new(pattern).map_err(|err| {
                        format!("`matches` is not a valid regular expression: {err}")
                    })?;
                    return Ok(JsonSpec::Matches(regex, pattern));
                }
            }
        }
        Ok(JsonSpec::Equals(value))
    }

    /// The expectation phrase for this operator, negated or not.
    fn describe(&self, path: &str, negate: bool) -> String {
        match self {
            JsonSpec::Equals(expected) => expectation_text(
                format!("`{path}` is {}", render(expected)),
                format!("`{path}` is not {}", render(expected)),
                negate,
            ),
            JsonSpec::GreaterThan(_, arg) => expectation_text(
                format!("`{path}` is greater than {}", render(arg)),
                format!("`{path}` is not greater than {}", render(arg)),
                negate,
            ),
            JsonSpec::GreaterThanOrEqual(_, arg) => expectation_text(
                format!("`{path}` is greater than or equal to {}", render(arg)),
                format!("`{path}` is not greater than or equal to {}", render(arg)),
                negate,
            ),
            JsonSpec::LessThan(_, arg) => expectation_text(
                format!("`{path}` is less than {}", render(arg)),
                format!("`{path}` is not less than {}", render(arg)),
                negate,
            ),
            JsonSpec::LessThanOrEqual(_, arg) => expectation_text(
                format!("`{path}` is less than or equal to {}", render(arg)),
                format!("`{path}` is not less than or equal to {}", render(arg)),
                negate,
            ),
            JsonSpec::Contains(arg) => expectation_text(
                format!("`{path}` contains {}", render(arg)),
                format!("`{path}` does not contain {}", render(arg)),
                negate,
            ),
            JsonSpec::Length(length) => length.describe(path, negate),
            JsonSpec::Matches(_, pattern) => expectation_text(
                format!("`{path}` matches `{pattern}`"),
                format!("`{path}` does not match `{pattern}`"),
                negate,
            ),
        }
    }

    /// Compares `actual` — the single value the path selected — against this
    /// operator. `Err` is a type mismatch: a comparison against a
    /// non-numeric value, `contains` against a value that supports neither
    /// substring nor membership, `length` against a value with no length,
    /// `matches` against a non-string value. These are facts about the
    /// response, evaluated here rather than at parse time because the
    /// response does not exist until now — same tier as "body is not JSON"
    /// above.
    fn evaluate(&self, actual: &serde_json::Value) -> Result<JsonOutcome, String> {
        match self {
            JsonSpec::Equals(expected) => Ok(JsonOutcome {
                holds: actual == *expected,
                detail_if_false: format!("got {}", render(actual)),
                detail_if_true: format!("got {}", render(actual)),
            }),
            JsonSpec::GreaterThan(n, _)
            | JsonSpec::GreaterThanOrEqual(n, _)
            | JsonSpec::LessThan(n, _)
            | JsonSpec::LessThanOrEqual(n, _) => {
                let actual_n = actual
                    .as_f64()
                    .ok_or_else(|| format!("is not a number: got {}", render(actual)))?;
                let holds = match self {
                    JsonSpec::GreaterThan(..) => actual_n > *n,
                    JsonSpec::GreaterThanOrEqual(..) => actual_n >= *n,
                    JsonSpec::LessThan(..) => actual_n < *n,
                    JsonSpec::LessThanOrEqual(..) => actual_n <= *n,
                    // The outer match already narrowed `self` to one of these
                    // four variants; the others do not reach this arm.
                    _ => unreachable!(),
                };
                Ok(JsonOutcome {
                    holds,
                    detail_if_false: format!("got {}", render(actual)),
                    detail_if_true: format!("got {}", render(actual)),
                })
            }
            JsonSpec::Contains(expected) => match actual {
                serde_json::Value::String(s) => {
                    let needle = expected.as_str().ok_or_else(|| {
                        format!(
                            "`contains` needs a string when the value is a string, got {}",
                            render(expected)
                        )
                    })?;
                    Ok(JsonOutcome {
                        holds: s.contains(needle),
                        detail_if_false: format!("not found in {}", render(actual)),
                        detail_if_true: format!("found in {}", render(actual)),
                    })
                }
                serde_json::Value::Array(items) => Ok(JsonOutcome {
                    holds: items.iter().any(|item| item == *expected),
                    detail_if_false: format!("not found in {}", render(actual)),
                    detail_if_true: format!("found in {}", render(actual)),
                }),
                other => Err(format!(
                    "does not support `contains`: got {}",
                    render(other)
                )),
            },
            JsonSpec::Length(length) => {
                let actual_len = match actual {
                    serde_json::Value::String(s) => s.chars().count() as u64,
                    serde_json::Value::Array(items) => items.len() as u64,
                    other => return Err(format!("has no length: got {}", render(other))),
                };
                let holds = length.holds(actual_len);
                Ok(JsonOutcome {
                    holds,
                    detail_if_false: format!("got length {actual_len}"),
                    detail_if_true: format!("got length {actual_len}"),
                })
            }
            JsonSpec::Matches(regex, _) => {
                let s = actual
                    .as_str()
                    .ok_or_else(|| format!("is not a string: got {}", render(actual)))?;
                Ok(JsonOutcome {
                    holds: regex.is_match(s),
                    detail_if_false: format!("no match in {}", render(actual)),
                    detail_if_true: format!("matched in {}", render(actual)),
                })
            }
        }
    }
}

impl LengthSpec {
    fn parse(value: &serde_json::Value) -> Result<Self, String> {
        let malformed = || {
            format!(
                "`length` expects a whole number, `{{greater_than: n}}`, \
                 `{{greater_than_or_equal: n}}`, `{{less_than: n}}` or \
                 `{{less_than_or_equal: n}}`, got {}",
                render(value)
            )
        };
        match value {
            serde_json::Value::Number(_) => {
                let n = value.as_u64().ok_or_else(malformed)?;
                Ok(LengthSpec::Equals(n))
            }
            serde_json::Value::Object(map) if map.len() == 1 => {
                let field = |key: &str| {
                    map.get(key).map(|arg| {
                        arg.as_u64().ok_or_else(|| {
                            format!("`length.{key}` expects a whole number, got {}", render(arg))
                        })
                    })
                };
                if let Some(n) = field("greater_than") {
                    Ok(LengthSpec::GreaterThan(n?))
                } else if let Some(n) = field("greater_than_or_equal") {
                    Ok(LengthSpec::GreaterThanOrEqual(n?))
                } else if let Some(n) = field("less_than") {
                    Ok(LengthSpec::LessThan(n?))
                } else if let Some(n) = field("less_than_or_equal") {
                    Ok(LengthSpec::LessThanOrEqual(n?))
                } else {
                    Err(malformed())
                }
            }
            _ => Err(malformed()),
        }
    }

    fn describe(&self, path: &str, negate: bool) -> String {
        let (positive_verb, negative_verb, n) = match self {
            LengthSpec::Equals(n) => ("has length", "does not have length", n),
            LengthSpec::GreaterThan(n) => (
                "has length greater than",
                "does not have length greater than",
                n,
            ),
            LengthSpec::GreaterThanOrEqual(n) => (
                "has length greater than or equal to",
                "does not have length greater than or equal to",
                n,
            ),
            LengthSpec::LessThan(n) => {
                ("has length less than", "does not have length less than", n)
            }
            LengthSpec::LessThanOrEqual(n) => (
                "has length less than or equal to",
                "does not have length less than or equal to",
                n,
            ),
        };
        expectation_text(
            format!("`{path}` {positive_verb} {n}"),
            format!("`{path}` {negative_verb} {n}"),
            negate,
        )
    }

    fn holds(&self, actual_len: u64) -> bool {
        match self {
            LengthSpec::Equals(n) => actual_len == *n,
            LengthSpec::GreaterThan(n) => actual_len > *n,
            LengthSpec::GreaterThanOrEqual(n) => actual_len >= *n,
            LengthSpec::LessThan(n) => actual_len < *n,
            LengthSpec::LessThanOrEqual(n) => actual_len <= *n,
        }
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
    use super::super::test_support::{assertions, json_response, only_failure, response};
    use super::super::AssertionKind;

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
        // The edge case worth pinning: not a panic, and not a load-time error
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

    // --- json path operators ------------------------------------------------

    #[test]
    fn json_greater_than_and_less_than_pass_and_fail_on_numeric_comparison() {
        let report =
            assertions("json:\n  $.user.id: {greater_than: 5}\n").evaluate(&json_response());
        assert!(report.passed(), "{report:?}");
        assert_eq!(
            report.results()[0].expectation,
            "`$.user.id` is greater than 5"
        );

        let report =
            assertions("json:\n  $.user.id: {greater_than: 100}\n").evaluate(&json_response());
        let failure = only_failure(&report);
        assert_eq!(failure.failure.as_deref(), Some("got 42"));

        let report =
            assertions("json:\n  $.user.id: {less_than: 100}\n").evaluate(&json_response());
        assert!(report.passed(), "{report:?}");

        let report = assertions("json:\n  $.user.id: {less_than: 5}\n").evaluate(&json_response());
        let failure = only_failure(&report);
        assert_eq!(failure.failure.as_deref(), Some("got 42"));
    }

    #[test]
    fn json_greater_than_or_equal_and_less_than_or_equal_include_the_boundary() {
        // The boundary is the whole point of the `_or_equal` variants: 42 is
        // neither `greater_than` nor `less_than` 42, but is both `_or_equal`
        // forms of it.
        let report = assertions("json:\n  $.user.id: {greater_than_or_equal: 42}\n")
            .evaluate(&json_response());
        assert!(report.passed(), "{report:?}");
        assert_eq!(
            report.results()[0].expectation,
            "`$.user.id` is greater than or equal to 42"
        );

        let report = assertions("json:\n  $.user.id: {greater_than_or_equal: 43}\n")
            .evaluate(&json_response());
        let failure = only_failure(&report);
        assert_eq!(failure.failure.as_deref(), Some("got 42"));

        let report =
            assertions("json:\n  $.user.id: {less_than_or_equal: 42}\n").evaluate(&json_response());
        assert!(report.passed(), "{report:?}");
        assert_eq!(
            report.results()[0].expectation,
            "`$.user.id` is less than or equal to 42"
        );

        let report =
            assertions("json:\n  $.user.id: {less_than_or_equal: 41}\n").evaluate(&json_response());
        let failure = only_failure(&report);
        assert_eq!(failure.failure.as_deref(), Some("got 42"));
    }

    #[test]
    fn json_comparison_operators_against_a_non_numeric_value_are_a_typed_type_mismatch() {
        for op in [
            "greater_than",
            "greater_than_or_equal",
            "less_than",
            "less_than_or_equal",
        ] {
            let report = assertions(&format!("json:\n  $.user.name: {{{op}: 5}}\n"))
                .evaluate(&json_response());
            let failure = only_failure(&report);
            assert_eq!(
                failure.failure.as_deref(),
                Some("is not a number: got \"ada\""),
                "operator {op}: {failure:?}"
            );
        }
    }

    #[test]
    fn json_comparison_operators_with_a_non_numeric_argument_are_a_malformed_operator() {
        for op in [
            "greater_than",
            "greater_than_or_equal",
            "less_than",
            "less_than_or_equal",
        ] {
            let report = assertions(&format!("json:\n  $.user.id: {{{op}: nope}}\n"))
                .evaluate(&json_response());
            let failure = only_failure(&report);
            assert!(
                failure
                    .failure
                    .as_deref()
                    .unwrap()
                    .contains(&format!("`{op}` expects a number")),
                "operator {op}: {failure:?}"
            );
        }
    }

    #[test]
    fn json_contains_matches_a_substring_of_a_string_value() {
        let report =
            assertions("json:\n  $.user.name: {contains: ad}\n").evaluate(&json_response());
        assert!(report.passed(), "{report:?}");
        assert_eq!(
            report.results()[0].expectation,
            "`$.user.name` contains \"ad\""
        );

        let report =
            assertions("json:\n  $.user.name: {contains: zz}\n").evaluate(&json_response());
        let failure = only_failure(&report);
        assert!(
            failure.failure.as_deref().unwrap().contains("not found"),
            "{failure:?}"
        );
    }

    #[test]
    fn json_contains_matches_array_membership() {
        let report = assertions("json:\n  $.tags: {contains: a}\n").evaluate(&json_response());
        assert!(report.passed(), "{report:?}");

        let report = assertions("json:\n  $.tags: {contains: z}\n").evaluate(&json_response());
        assert!(!report.passed());
    }

    #[test]
    fn json_contains_against_a_scalar_is_a_type_mismatch() {
        let report = assertions("json:\n  $.user.id: {contains: 4}\n").evaluate(&json_response());
        let failure = only_failure(&report);
        assert!(
            failure
                .failure
                .as_deref()
                .unwrap()
                .contains("does not support `contains`"),
            "{failure:?}"
        );
    }

    // --- json matches --------------------------------------------------------

    #[test]
    fn json_matches_passes_on_a_matching_path_and_fails_otherwise() {
        let report =
            assertions("json:\n  $.user.name: {matches: '^[a-z]+$'}\n").evaluate(&json_response());
        assert!(report.passed(), "{report:?}");
        assert_eq!(
            report.results()[0].expectation,
            "`$.user.name` matches `^[a-z]+$`"
        );

        let report =
            assertions("json:\n  $.user.name: {matches: '^[0-9]+$'}\n").evaluate(&json_response());
        let failure = only_failure(&report);
        assert!(
            failure.failure.as_deref().unwrap().contains("no match"),
            "{failure:?}"
        );
    }

    #[test]
    fn json_matches_against_a_malformed_regex_is_a_malformed_operator() {
        let report =
            assertions("json:\n  $.user.name: {matches: '['}\n").evaluate(&json_response());
        let failure = only_failure(&report);
        assert!(
            failure
                .failure
                .as_deref()
                .unwrap()
                .contains("not a valid regular expression"),
            "{failure:?}"
        );
    }

    #[test]
    fn json_matches_against_a_non_string_value_is_a_type_mismatch() {
        let report =
            assertions("json:\n  $.user.id: {matches: '^[0-9]+$'}\n").evaluate(&json_response());
        let failure = only_failure(&report);
        assert_eq!(
            failure.failure.as_deref(),
            Some("is not a string: got 42"),
            "{failure:?}"
        );
    }

    #[test]
    fn json_matches_against_a_non_string_argument_is_a_malformed_operator() {
        let report = assertions("json:\n  $.user.name: {matches: 5}\n").evaluate(&json_response());
        let failure = only_failure(&report);
        assert!(
            failure
                .failure
                .as_deref()
                .unwrap()
                .contains("`matches` expects a string pattern"),
            "{failure:?}"
        );
    }

    #[test]
    fn json_length_bare_number_checks_equality() {
        let report = assertions("json:\n  $.tags: {length: 2}\n").evaluate(&json_response());
        assert!(report.passed(), "{report:?}");
        assert_eq!(report.results()[0].expectation, "`$.tags` has length 2");

        let report = assertions("json:\n  $.tags: {length: 3}\n").evaluate(&json_response());
        let failure = only_failure(&report);
        assert_eq!(failure.failure.as_deref(), Some("got length 2"));
    }

    #[test]
    fn json_length_nested_comparison_operators_compare_rather_than_equal() {
        let report =
            assertions("json:\n  $.tags: {length: {greater_than: 1}}\n").evaluate(&json_response());
        assert!(report.passed(), "{report:?}");

        let report =
            assertions("json:\n  $.tags: {length: {less_than: 1}}\n").evaluate(&json_response());
        assert!(!report.passed());

        let report = assertions("json:\n  $.tags: {length: {greater_than_or_equal: 2}}\n")
            .evaluate(&json_response());
        assert!(report.passed(), "boundary included, {report:?}");

        let report = assertions("json:\n  $.tags: {length: {less_than_or_equal: 2}}\n")
            .evaluate(&json_response());
        assert!(report.passed(), "boundary included, {report:?}");
    }

    #[test]
    fn json_length_against_a_scalar_is_a_type_mismatch() {
        let report = assertions("json:\n  $.user.id: {length: 2}\n").evaluate(&json_response());
        let failure = only_failure(&report);
        assert!(
            failure
                .failure
                .as_deref()
                .unwrap()
                .contains("has no length"),
            "{failure:?}"
        );
    }

    #[test]
    fn json_length_with_a_malformed_argument_is_a_malformed_operator() {
        let report = assertions("json:\n  $.tags: {length: nope}\n").evaluate(&json_response());
        let failure = only_failure(&report);
        assert!(
            failure
                .failure
                .as_deref()
                .unwrap()
                .contains("`length` expects"),
            "{failure:?}"
        );
    }

    #[test]
    fn a_multi_key_object_is_still_bare_equality_not_an_operator() {
        // The documented disambiguation rule: only a single-key mapping with a
        // recognised key is read as an operator.
        let with_object = response(
            200,
            &[],
            r#"{"config": {"greater_than": 5, "less_than": 1}}"#,
        );
        let report = assertions("json:\n  $.config: {greater_than: 5, less_than: 1}\n")
            .evaluate(&with_object);
        assert!(
            report.passed(),
            "a two-key object must be equality, {report:?}"
        );
    }
}
