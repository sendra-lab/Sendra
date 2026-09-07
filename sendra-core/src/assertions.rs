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

use jsonpath_rust::JsonPath;
use regex::Regex;
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

/// One JSON path assertion, against the already-parsed body.
///
/// `body` is the shared parse result, so a body that is not JSON reports the
/// parser's own message — position included — rather than a vague "not JSON".
fn check_json_path(
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
