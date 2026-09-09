//! Declarative checks a request makes against the response it gets back.
//!
//! A request file may carry an `assertions` block:
//!
//! ```yaml
//! method: GET
//! url: https://httpbin.org/get
//! assertions:
//!   status: 200
//!   status_in: [200, 201, 204]
//!   headers:
//!     content-type: application/json   # present, with this exact value
//!     x-request-id:                    # present, value not checked
//!   body_contains: '"url"'
//!   body_matches: '^\{"url"'
//!   elapsed_ms_under: 2000
//!   json:
//!     $.headers.Accept: application/json           # bare value: equality
//!     $.count: { greater_than: 5 }                 # operator object: comparison
//!     $.id: { matches: '^[0-9a-f-]{36}$' }         # operator object: regex
//!   not:
//!     status: 404
//! ```
//!
//! Every key is optional and a request with no `assertions` block behaves
//! exactly as it did before this module existed.
//!
//! Two things need a word up front, because they are not obvious from the
//! schema alone:
//!
//! **`json:` disambiguates bare values from operators by shape.** A path's
//! expected value is read as an equality check unless it is a YAML mapping
//! with exactly one key drawn from `greater_than`, `greater_than_or_equal`,
//! `less_than`, `less_than_or_equal`, `contains`, `length` or `matches`, in
//! which case it is that operator instead. This means an equality check
//! against a genuine one-key object shaped like `{greater_than: 5}` is not
//! expressible — a real limitation, accepted because the alternative (a
//! separate block for operators) would make every path assertion say twice
//! which kind it is, and `{greater_than: 5}` as a literal expected value is
//! not a shape real APIs return. A multi-key object (`{id: 1, name: ada}`)
//! is never ambiguous and is always equality, exactly as before.
//!
//! There is deliberately no `not_equal` operator: `not: {json: {$.count:
//! 5}}` already says "not equal to 5" precisely, and a dedicated operator
//! would only be a shorter spelling of the negation wrapper that already
//! exists — unlike `greater_than`/`less_than`, which are comparisons `not:`
//! cannot express at all (`not: {json: {$.count: {greater_than: 5}}}` means
//! `<= 5`, not `< 5`, and there is no operator-free way to write "less than
//! 5" as a negation).
//!
//! **`not:` wraps a whole assertions block, not one assertion.** It takes
//! the same keys as the top level (minus `not` itself — nesting `not` inside
//! `not` is a parse error, not a double negative) and negates each one
//! independently: `not: {status: 404, body_contains: error}` is "status is
//! not 404" *and* "body does not contain `error`", not "status is 404 and
//! body contains `error`" negated as a pair. A wrapper was chosen over a
//! `not_status` / `not_body_contains` key for every assertion type because
//! it composes for free with whatever assertion kind is added next, rather
//! than doubling the schema's key count every time one is.
//!
//! A hard error — a malformed JSON path, an invalid regex (whether from
//! `body_matches` or a `matches` operator), a body that is not JSON, a JSON
//! path selecting zero or several values, a comparison or `length` operator
//! applied to a value of the wrong type — is not something `not:` can turn
//! into a pass. These are facts about the request or the file, not a
//! condition to be true or false, so `not: {json: {$.a: {greater_than: 5}}}`
//! against a body where `$.a` is a string still fails, the same way it would
//! unwrapped.
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

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::Response;

mod json;

#[cfg(test)]
mod test_support;

use json::check_json_path;

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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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

    /// The status code must be one of these.
    ///
    /// Its own key rather than folding into `status` (a list there would
    /// change what a bare `status: 200` means) — `status` is "exactly this
    /// code", `status_in` is "one of these codes", and a file should be able
    /// to write either without the other's presence changing its meaning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_in: Option<Vec<u16>>,

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

    /// A regular expression the response body must match, anywhere in it —
    /// the same "somewhere in the body" reach as `body_contains`, not an
    /// anchored whole-body match, so `body_matches: '"id":\s*\d+'` finds that
    /// pattern wherever it sits.
    ///
    /// The engine is [`regex`](https://docs.rs/regex), already in the
    /// dependency tree as a transitive dependency of `jsonpath-rust` — this
    /// adds no new crate, only a direct declaration of one already being
    /// built.
    ///
    /// The pattern is checked when the assertion runs, not when the file is
    /// loaded, for the same reason a JSON path is: a stricter release of
    /// `regex` should not start rejecting files that used to load, for a
    /// request Sendra could still send. An invalid pattern is a failed
    /// assertion naming the parse error, not a panic or a load-time error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_matches: Option<String>,

    /// The response must have arrived in under this many milliseconds.
    ///
    /// Backed by [`Response::elapsed`], which is wall-clock time for the
    /// request as sent — DNS, connect and TLS included, the same number a
    /// person timing the request by hand would get. A strict "under", not
    /// "at or under": a threshold is normally chosen as a round number the
    /// response should beat, and `elapsed_ms_under: 500` reads as "faster
    /// than half a second," which an exact 500ms response is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms_under: Option<u64>,

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
    /// Beyond a bare equality value, a path may map to an *operator object*
    /// with exactly one of these keys:
    ///
    /// ```yaml
    /// json:
    ///   $.count: { greater_than: 5 }             # numeric: actual > 5
    ///   $.count: { greater_than_or_equal: 5 }    # numeric: actual >= 5
    ///   $.count: { less_than: 5 }                # numeric: actual < 5
    ///   $.count: { less_than_or_equal: 5 }       # numeric: actual <= 5
    ///   $.tags: { contains: b }   # substring of a string, or array membership
    ///   $.tags: { length: 2 }                     # array/string length equals 2
    ///   $.tags: { length: { greater_than: 1 } }   # length compared, not just equal
    ///   $.id: { matches: '^[0-9a-f-]{36}$' }      # regex, scoped to this path's
    ///                                              # string value — see `body_matches`
    ///                                              # for the whole-body equivalent
    /// ```
    ///
    /// See the module docs for exactly how a bare value is told apart from an
    /// operator, and what that costs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub json: BTreeMap<String, serde_json::Value>,

    /// Every assertion in this block, inverted: passes exactly when the
    /// wrapped one would have failed, and vice versa. See the module docs for
    /// what a wrapper buys over a `not_`-prefixed key per assertion type, and
    /// for why a hard error underneath — a malformed path, an invalid regex,
    /// a type mismatch — is not something this can turn into a pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not: Option<NotAssertions>,
}

/// The inner block of a `not:` wrapper: every key `Assertions` has, except
/// `not` itself.
///
/// A separate type rather than `not: Option<Box<Assertions>>` with a runtime
/// check against a nested `not`, so that `not: {not: {...}}` is rejected by
/// the same `deny_unknown_fields` machinery as every other unknown key,
/// rather than by a bespoke check that could drift from it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct NotAssertions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_in: Option<Vec<u16>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_matches: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms_under: Option<u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub json: BTreeMap<String, serde_json::Value>,
}

impl NotAssertions {
    fn is_empty(&self) -> bool {
        self.status.is_none()
            && self.status_in.is_none()
            && self.headers.is_empty()
            && self.body_contains.is_none()
            && self.body_matches.is_none()
            && self.elapsed_ms_under.is_none()
            && self.json.is_empty()
    }

    fn fields(&self) -> Fields<'_> {
        Fields {
            status: self.status,
            status_in: self.status_in.as_deref(),
            headers: &self.headers,
            body_contains: self.body_contains.as_deref(),
            body_matches: self.body_matches.as_deref(),
            elapsed_ms_under: self.elapsed_ms_under,
            json: &self.json,
        }
    }
}

/// The checkable fields shared by [`Assertions`] and [`NotAssertions`],
/// borrowed rather than duplicated so [`push_checks`] has exactly one
/// implementation for both the plain block and the `not:` block underneath
/// it.
struct Fields<'a> {
    status: Option<u16>,
    status_in: Option<&'a [u16]>,
    headers: &'a BTreeMap<String, Option<String>>,
    body_contains: Option<&'a str>,
    body_matches: Option<&'a str>,
    elapsed_ms_under: Option<u64>,
    json: &'a BTreeMap<String, serde_json::Value>,
}

/// Which kind of check produced a result, for a front-end that wants to group,
/// filter or colour by kind rather than parse the rendered text.
///
/// Negation does not add variants of its own: `not: {status: 404}` reports as
/// [`AssertionKind::Status`], the same kind a bare `status: 404` would,
/// because it is a statement about the same thing, only inverted — the
/// [`AssertionResult::expectation`] wording is what says which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssertionKind {
    Status,
    StatusIn,
    Header,
    BodyContains,
    BodyMatches,
    ElapsedMsUnder,
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

/// Picks the expectation text a negated check should show: the negative
/// wording under `not:`, the positive one otherwise. The wording is the same
/// whichever way the check itself comes out — pass or fail always says what
/// was asserted, never what happened — so one call up front covers both
/// outcomes.
fn expectation_text(positive: String, negative: String, negate: bool) -> String {
    if negate {
        negative
    } else {
        positive
    }
}

/// Turns a plain (unnegated) condition into the [`AssertionResult`] a check
/// function returns, honouring `negate`.
///
/// `holds` is whether the condition as written — ignoring `not:` — is true of
/// the response. Whether that is a pass depends on `negate`: a plain check
/// passes when `holds`, a negated one passes when `!holds`. `detail_if_false`
/// explains a plain failure (`holds` was false); `detail_if_true` explains a
/// negated failure (`holds` was true, which is exactly what `not:` forbade).
/// Both are computed unconditionally since every detail here is a cheap
/// `format!`, not worth deferring behind a closure.
fn finish(
    kind: AssertionKind,
    holds: bool,
    expectation: String,
    negate: bool,
    detail_if_false: String,
    detail_if_true: String,
) -> AssertionResult {
    let failed = if negate { holds } else { !holds };
    if !failed {
        AssertionResult::pass(kind, expectation)
    } else {
        let detail = if negate {
            detail_if_true
        } else {
            detail_if_false
        };
        AssertionResult::fail(kind, expectation, detail)
    }
}

/// Every assertion on one request, evaluated against one response, in a fixed
/// order: status, `status_in`, headers, `body_contains`, `body_matches`,
/// `elapsed_ms_under`, then JSON paths — with the entries of each map in
/// sorted order — followed by the same order again for the `not:` block, if
/// there is one. Deterministic because the output is read by people and
/// diffed by scripts, and neither is served by an order that depends on how a
/// `BTreeMap` happened to be filled.
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
            && self.status_in.is_none()
            && self.headers.is_empty()
            && self.body_contains.is_none()
            && self.body_matches.is_none()
            && self.elapsed_ms_under.is_none()
            && self.json.is_empty()
            && self.not.as_ref().is_none_or(NotAssertions::is_empty)
    }

    fn fields(&self) -> Fields<'_> {
        Fields {
            status: self.status,
            status_in: self.status_in.as_deref(),
            headers: &self.headers,
            body_contains: self.body_contains.as_deref(),
            body_matches: self.body_matches.as_deref(),
            elapsed_ms_under: self.elapsed_ms_under,
            json: &self.json,
        }
    }

    /// Check every assertion against `response` and report all of them.
    ///
    /// The response must be the one that actually came back from the request as
    /// sent — after variable substitution and after config was applied — since
    /// that is the request the assertions were written about.
    pub fn evaluate(&self, response: &Response) -> AssertionReport {
        let mut results = Vec::new();
        push_checks(&mut results, self.fields(), false, response);
        if let Some(not) = &self.not {
            push_checks(&mut results, not.fields(), true, response);
        }
        AssertionReport { results }
    }
}

/// Runs one block's worth of checks — the plain block or the `not:` one — in
/// the fixed order documented on [`AssertionReport`], appending each result to
/// `results`. Shared by both so the order and the set of checks cannot drift
/// between a block and its negation.
fn push_checks(
    results: &mut Vec<AssertionResult>,
    fields: Fields<'_>,
    negate: bool,
    response: &Response,
) {
    if let Some(expected) = fields.status {
        results.push(check_status(expected, response, negate));
    }

    if let Some(allowed) = fields.status_in {
        results.push(check_status_in(allowed, response, negate));
    }

    for (name, expected) in fields.headers {
        results.push(check_header(name, expected.as_deref(), response, negate));
    }

    if let Some(needle) = fields.body_contains {
        results.push(check_body_contains(needle, response, negate));
    }

    if let Some(pattern) = fields.body_matches {
        results.push(check_body_matches(pattern, response, negate));
    }

    if let Some(threshold_ms) = fields.elapsed_ms_under {
        results.push(check_elapsed_ms_under(threshold_ms, response, negate));
    }

    if !fields.json.is_empty() {
        // Parsed once for the whole block, not once per path: the body does
        // not change between assertions, and a body that is not JSON should
        // report the same reason against every path rather than a different
        // one each time.
        let body = serde_json::from_str::<serde_json::Value>(&response.body);
        for (path, expected) in fields.json {
            results.push(check_json_path(
                path,
                expected,
                body.as_ref(),
                response,
                negate,
            ));
        }
    }
}

fn check_status(expected: u16, response: &Response, negate: bool) -> AssertionResult {
    let holds = response.status == expected;
    let expectation = expectation_text(
        format!("status is {expected}"),
        format!("status is not {expected}"),
        negate,
    );
    let detail = format!("got {}", response.status);
    finish(
        AssertionKind::Status,
        holds,
        expectation,
        negate,
        detail.clone(),
        detail,
    )
}

fn check_status_in(allowed: &[u16], response: &Response, negate: bool) -> AssertionResult {
    let holds = allowed.contains(&response.status);
    let list = allowed
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let expectation = expectation_text(
        format!("status is one of [{list}]"),
        format!("status is not one of [{list}]"),
        negate,
    );
    let detail = format!("got {}", response.status);
    finish(
        AssertionKind::StatusIn,
        holds,
        expectation,
        negate,
        detail.clone(),
        detail,
    )
}

fn check_header(
    name: &str,
    expected: Option<&str>,
    response: &Response,
    negate: bool,
) -> AssertionResult {
    let expectation = expectation_text(
        match expected {
            Some(value) => format!("header `{name}` is `{value}`"),
            None => format!("header `{name}` is present"),
        },
        match expected {
            Some(value) => format!("header `{name}` is not `{value}`"),
            None => format!("header `{name}` is not present"),
        },
        negate,
    );

    // Every value the response carries under this name; more than one is legal
    // (`set-cookie`), so the assertion holds if any of them matches.
    let seen: Vec<&str> = response
        .headers
        .iter()
        .filter(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
        .collect();

    let holds = match expected {
        None => !seen.is_empty(),
        Some(expected) => seen.contains(&expected),
    };

    let detail_if_false = if seen.is_empty() {
        // Name the headers that *are* there, the way a missing request name
        // lists the names a collection does have: the answer is usually a
        // casing or spelling difference visible the moment both are on screen.
        let present = response
            .headers
            .iter()
            .map(|(header, _)| header.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if present.is_empty() {
            "the response carries no headers at all".to_string()
        } else {
            format!("not present (the response has: {present})")
        }
    } else {
        format!(
            "got {}",
            seen.iter()
                .map(|value| format!("`{value}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    // `holds` true implies `seen` is non-empty in both the presence-only and
    // value-match cases, so this is always the "found these values" detail —
    // exactly what a negated assertion needs to say about what it forbade.
    let detail_if_true = format!(
        "got {}",
        seen.iter()
            .map(|value| format!("`{value}`"))
            .collect::<Vec<_>>()
            .join(", ")
    );

    finish(
        AssertionKind::Header,
        holds,
        expectation,
        negate,
        detail_if_false,
        detail_if_true,
    )
}

fn check_body_contains(needle: &str, response: &Response, negate: bool) -> AssertionResult {
    let holds = response.body.contains(needle);
    let expectation = expectation_text(
        format!("body contains `{needle}`"),
        format!("body does not contain `{needle}`"),
        negate,
    );
    finish(
        AssertionKind::BodyContains,
        holds,
        expectation,
        negate,
        format!("not found in the {}-byte body", response.body.len()),
        format!("found in the {}-byte body", response.body.len()),
    )
}

fn check_body_matches(pattern: &str, response: &Response, negate: bool) -> AssertionResult {
    let expectation = expectation_text(
        format!("body matches `{pattern}`"),
        format!("body does not match `{pattern}`"),
        negate,
    );

    // Checked here, not at load time, for the same reason a JSON path is: see
    // the note on `check_json_path`. An invalid pattern is wrong about every
    // response there could ever be, so it is reported the same way whether or
    // not this assertion is wrapped in `not:`.
    let regex = match Regex::new(pattern) {
        Ok(regex) => regex,
        Err(err) => {
            return AssertionResult::fail(
                AssertionKind::BodyMatches,
                expectation,
                format!("not a valid regular expression: {err}"),
            );
        }
    };

    let holds = regex.is_match(&response.body);
    finish(
        AssertionKind::BodyMatches,
        holds,
        expectation,
        negate,
        format!("no match in the {}-byte body", response.body.len()),
        format!("matched in the {}-byte body", response.body.len()),
    )
}

fn check_elapsed_ms_under(threshold_ms: u64, response: &Response, negate: bool) -> AssertionResult {
    let elapsed_ms = response.elapsed.as_millis();
    let holds = elapsed_ms < u128::from(threshold_ms);
    let expectation = expectation_text(
        format!("elapsed time is under {threshold_ms}ms"),
        format!("elapsed time is not under {threshold_ms}ms"),
        negate,
    );
    let detail = format!("took {elapsed_ms}ms");
    finish(
        AssertionKind::ElapsedMsUnder,
        holds,
        expectation,
        negate,
        detail.clone(),
        detail,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_support::{assertions, json_response, only_failure, response};

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

    // --- status_in -----------------------------------------------------

    #[test]
    fn status_in_passes_when_the_status_is_one_of_the_list() {
        let report = assertions("status_in: [200, 201, 204]").evaluate(&response(201, &[], ""));
        assert!(report.passed(), "{report:?}");
        assert_eq!(
            report.results()[0].expectation,
            "status is one of [200, 201, 204]"
        );
    }

    #[test]
    fn status_in_fails_and_says_what_it_got_when_the_status_is_not_listed() {
        let report = assertions("status_in: [200, 201, 204]").evaluate(&response(404, &[], ""));
        let failure = only_failure(&report);
        assert_eq!(failure.kind, AssertionKind::StatusIn);
        assert_eq!(failure.failure.as_deref(), Some("got 404"));
    }

    // --- body_matches ----------------------------------------------------

    #[test]
    fn body_matches_passes_on_a_regex_match_and_fails_otherwise() {
        let body = response(200, &[], "request id: 4471");

        let report = assertions(r"body_matches: 'id:\s*\d+'").evaluate(&body);
        assert!(report.passed(), "{report:?}");

        let report = assertions(r"body_matches: 'id:\s*[a-z]+'").evaluate(&body);
        let failure = only_failure(&report);
        assert_eq!(failure.kind, AssertionKind::BodyMatches);
        assert!(
            failure.failure.as_deref().unwrap().contains("16-byte body"),
            "{failure:?}"
        );
    }

    #[test]
    fn an_invalid_regex_is_a_failed_assertion_not_a_panic() {
        let report = assertions("body_matches: '['").evaluate(&response(200, &[], "anything"));
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

    // --- elapsed_ms_under --------------------------------------------------

    #[test]
    fn elapsed_ms_under_passes_when_faster_than_the_threshold() {
        let mut fast = response(200, &[], "");
        fast.elapsed = std::time::Duration::from_millis(10);
        let report = assertions("elapsed_ms_under: 1000").evaluate(&fast);
        assert!(report.passed(), "{report:?}");
        assert_eq!(
            report.results()[0].expectation,
            "elapsed time is under 1000ms"
        );
    }

    #[test]
    fn elapsed_ms_under_fails_when_slower_than_the_threshold() {
        let mut slow = response(200, &[], "");
        slow.elapsed = std::time::Duration::from_millis(1500);
        let report = assertions("elapsed_ms_under: 1000").evaluate(&slow);
        let failure = only_failure(&report);
        assert_eq!(failure.kind, AssertionKind::ElapsedMsUnder);
        assert_eq!(failure.failure.as_deref(), Some("took 1500ms"));
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

    // --- negation ---------------------------------------------------------

    #[test]
    fn not_status_passes_when_the_status_differs_and_fails_when_it_matches() {
        let report = assertions("not:\n  status: 404\n").evaluate(&response(200, &[], ""));
        assert!(report.passed(), "{report:?}");
        assert_eq!(report.results()[0].expectation, "status is not 404");

        let report = assertions("not:\n  status: 404\n").evaluate(&response(404, &[], ""));
        let failure = only_failure(&report);
        assert_eq!(failure.kind, AssertionKind::Status);
        assert_eq!(failure.expectation, "status is not 404");
        assert_eq!(failure.failure.as_deref(), Some("got 404"));
    }

    #[test]
    fn not_body_contains_passes_when_absent_and_fails_when_present() {
        let ok = response(200, &[], "all good");
        let report = assertions("not:\n  body_contains: error\n").evaluate(&ok);
        assert!(report.passed(), "{report:?}");
        assert_eq!(
            report.results()[0].expectation,
            "body does not contain `error`"
        );

        let bad = response(200, &[], "an error occurred");
        let report = assertions("not:\n  body_contains: error\n").evaluate(&bad);
        let failure = only_failure(&report);
        assert_eq!(failure.expectation, "body does not contain `error`");
        assert!(
            failure.failure.as_deref().unwrap().contains("found in the"),
            "{failure:?}"
        );
    }

    #[test]
    fn not_json_path_negates_equality() {
        let report = assertions("not:\n  json:\n    $.user.id: 7\n").evaluate(&json_response());
        assert!(report.passed(), "{report:?}");
        assert_eq!(report.results()[0].expectation, "`$.user.id` is not 7");

        let report = assertions("not:\n  json:\n    $.user.id: 42\n").evaluate(&json_response());
        let failure = only_failure(&report);
        assert_eq!(failure.expectation, "`$.user.id` is not 42");
        assert_eq!(failure.failure.as_deref(), Some("got 42"));
    }

    #[test]
    fn a_hard_error_under_not_still_fails_rather_than_being_negated_into_a_pass() {
        // A malformed path, an ambiguous match, a type mismatch — these are
        // facts about the file or the response, not a condition `not:` can
        // flip: a `greater_than` operator against the wrong type.
        let report = assertions("not:\n  json:\n    $.user.name: {greater_than: 5}\n")
            .evaluate(&json_response());
        assert!(
            !report.passed(),
            "a type mismatch must still fail under `not:`"
        );
        let failure = only_failure(&report);
        assert!(
            failure
                .failure
                .as_deref()
                .unwrap()
                .contains("is not a number"),
            "{failure:?}"
        );
    }

    #[test]
    fn a_malformed_json_path_under_not_still_fails() {
        let report = assertions("not:\n  json:\n    '$.[': 1\n").evaluate(&json_response());
        assert!(!report.passed());
        let failure = only_failure(&report);
        assert!(
            failure
                .failure
                .as_deref()
                .unwrap()
                .contains("not a valid JSON path"),
            "{failure:?}"
        );
    }

    #[test]
    fn not_and_the_plain_block_can_be_combined_and_both_are_reported() {
        let report = assertions(
            "\
status: 200
not:
  body_contains: error
",
        )
        .evaluate(&response(200, &[], "all good"));
        assert!(report.passed(), "{report:?}");
        assert_eq!(report.len(), 2);
    }

    #[test]
    fn a_nested_not_inside_not_is_a_parse_error() {
        let err = serde_yaml::from_str::<Assertions>("not:\n  not:\n    status: 200\n")
            .expect_err("double negation is not part of the schema");
        assert!(err.to_string().contains("not"), "{err}");
    }

    #[test]
    fn an_empty_not_block_counts_as_no_assertions() {
        let assertions = assertions("not: {}");
        assert!(assertions.is_empty());
    }
}
