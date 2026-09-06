//! The `--json` schema.
//!
//! The shape is defined here, in the front-end, rather than by deriving
//! `Serialize` on the core types, and that is deliberate. `--json` is a promise
//! this CLI makes about its stdout; core's types are a model of HTTP that a TUI
//! will share and are free to change shape without anyone's pipeline breaking.
//! Deriving on them would also have made the schema whatever serde happened to
//! do with a `Duration` and a `Vec<(String, String)>` — `{"secs":0,"nanos":...}`
//! and nested two-element arrays — instead of `elapsed_ms` and named header
//! pairs. Nothing in core needed changing to add this.

use sendra_core::{
    AssertionKind, AssertionReport, CaptureReport, Response, ScriptOutcome, SendraError,
};
use serde::Serialize;

use crate::exit::Summary;

/// One error, flattened to a single string: the message, then its causes,
/// separated by `: `.
///
/// The human output prints the cause chain as its own indented `caused by:`
/// lines; a JSON field is one value, and dropping the chain would lose the part
/// that says *why* — "request to `x` failed" without the DNS error under it
/// names the request and not the problem.
pub(super) fn error_message(err: &SendraError) -> String {
    let mut message = err.to_string();

    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }

    message
}

/// The single object `--json` writes: `{"requests": [...]}` from `run`, the
/// same with a `"summary"` key from `test`.
///
/// An object rather than a bare array of requests, so that `test`'s counts have
/// somewhere to go that is not a magic last element, and so that anything later
/// (a run-level timing, a schema version) can be added without moving what is
/// already there. `summary` is absent rather than null under `run`: `run` has
/// no summary — it is not a summary that is empty — and `jq` reads a missing
/// key as null anyway.
#[derive(Debug, Serialize)]
pub(super) struct RunDocument<'a> {
    pub(super) requests: &'a [RequestRecord],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) summary: Option<SummaryRecord>,
}

/// What became of one request, in the order it was sent.
///
/// `response` and `error` are always both present, exactly one of them null:
/// a request either came back or it did not, and a consumer can branch on
/// either key without having to know which one this build happens to emit.
/// `assertions` is always an object, with an empty `results` list for a request
/// that declared none — the same thing the human output says by printing
/// nothing.
///
/// `post_request` is null for a request that declared no script, which is a
/// different thing from a script that ran and passed — the same distinction the
/// terminal draws by printing nothing versus printing `✓ passed`. There is no
/// matching `pre_request` key: a `pre_request` script that fails means the
/// request was never sent, which `error` already says, and one that succeeds has
/// nothing to report beyond the request that went out.
#[derive(Debug, Serialize)]
pub(super) struct RequestRecord {
    /// The request's `name`, or its URL when it has none — the same label the
    /// `→` line on stderr shows.
    label: String,
    pub(super) response: Option<ResponseRecord>,
    /// Why there was no response, message and causes joined with `: `.
    pub(super) error: Option<String>,
    pub(super) post_request: Option<PostRequestRecord>,
    pub(super) assertions: AssertionsRecord,
    pub(super) capture: Option<CaptureRecord>,
}

impl RequestRecord {
    pub(super) fn new(label: &str) -> Self {
        Self {
            label: label.to_string(),
            response: None,
            error: None,
            post_request: None,
            assertions: AssertionsRecord::default(),
            capture: None,
        }
    }
}

/// What a request's `capture` block produced.
///
/// Null for a request that declared no block, which is a different thing from a
/// block that captured nothing — the same distinction `post_request` draws by
/// being null rather than `{"passed": true}`.
///
/// Two keys rather than one list, because they answer two different questions
/// and a consumer almost always wants only one of them. `values` is a plain
/// name-to-value object, so chaining a captured token into another tool is
/// `.requests[0].capture.values.auth_token` and not a search through a list for
/// the right `variable`. `failures` is the list, because a failure is several
/// facts (which name, which path, what went wrong) and there is usually none of
/// them.
///
/// **`values` is redacted by default.** A capture is often an auth token or
/// other sensitive value pulled out of a response, and `--json` is the format
/// that ends up piped straight into a CI log — a more structured, more
/// attractive target there than the same value sitting inside an escaped
/// response body. Unlike the body, `values` is not the server's response
/// verbatim; it is a key Sendra itself chose to extract and name, so Sendra
/// gets to choose what happens to it here without misrepresenting anything
/// the server sent. `--show-captures` opts back into the raw values, for
/// anyone who wants the pre-existing behaviour. `failures` is never redacted:
/// a failure carries a variable name and a JSON path, both already visible in
/// the source YAML, never the value that would have been captured.
#[derive(Debug, Serialize)]
pub(super) struct CaptureRecord {
    /// Every name this request captured, and what it captured — or
    /// [`REDACTED_CAPTURE_VALUE`] in place of each one, unless
    /// `--show-captures` was given. Empty when every entry failed.
    values: std::collections::BTreeMap<String, String>,
    /// The entries that produced no value, in evaluation order. Empty when
    /// they all did.
    failures: Vec<CaptureFailureRecord>,
}

/// What a redacted `capture.values` entry reads as: unambiguously "a value
/// was captured and Sendra is choosing not to show it", not "capture failed"
/// (that is a missing key, on `failures`) and not an empty string (that would
/// look like a capture that genuinely produced nothing).
pub(super) const REDACTED_CAPTURE_VALUE: &str = "<redacted>";

impl CaptureRecord {
    pub(super) fn new(report: &CaptureReport, show_captures: bool) -> Self {
        let values = report.values();

        Self {
            values: if show_captures {
                values
            } else {
                values
                    .into_keys()
                    .map(|name| (name, REDACTED_CAPTURE_VALUE.to_string()))
                    .collect()
            },
            failures: report
                .failures()
                .map(|result| CaptureFailureRecord {
                    variable: result.variable.clone(),
                    path: result.path.clone(),
                    // Core's own wording, the same string the terminal shows.
                    failure: result
                        .failure()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                })
                .collect(),
        }
    }
}

/// One `capture` entry that produced no value.
#[derive(Debug, Serialize)]
struct CaptureFailureRecord {
    /// The name that is now not defined for the requests downstream.
    variable: String,
    /// The JSON path it was read from, as written in the file.
    path: String,
    /// Why it produced nothing.
    failure: String,
}

/// What a request's `post_request` script decided.
///
/// `passed` and `failure` rather than `failure` alone, so a consumer can read
/// `.post_request.passed` without having to know that null means success —
/// the same pair, in the same spelling, that each entry of `assertions.results`
/// carries, because it is the same kind of statement about the same response.
///
/// `failure` is core's own wording: the message the script threw, verbatim, or
/// Rhai's full error with a position when the script had a bug rather than a
/// complaint about the response.
#[derive(Debug, Serialize)]
pub(super) struct PostRequestRecord {
    passed: bool,
    failure: Option<String>,
}

impl From<&ScriptOutcome> for PostRequestRecord {
    fn from(outcome: &ScriptOutcome) -> Self {
        Self {
            passed: outcome.passed(),
            failure: outcome.failure().map(str::to_owned),
        }
    }
}

/// A response as `--json` reports it.
///
/// The body is the raw string that came over the wire, never re-indented: the
/// pretty-printing [`body_for_display`](super::human::body_for_display) does is for a person reading a
/// terminal, and rewriting a JSON body inside a JSON document would hand the
/// consumer something that is not what the server sent. A consumer that wants
/// it parsed has `fromjson`.
#[derive(Debug, Serialize)]
pub(super) struct ResponseRecord {
    status: u16,
    /// `OK`, `Not Found` — empty for a status with no canonical reason.
    status_text: String,
    /// Milliseconds, matching the `412 ms` the human output prints.
    elapsed_ms: u64,
    headers: Vec<HeaderRecord>,
    body: String,
    /// Every redirect hop that led to this response, oldest first — the same
    /// chain the human output prints as `→ 301 https://...` lines, empty for
    /// the overwhelming majority of responses that were not redirected at
    /// all.
    redirects: Vec<RedirectHopRecord>,
}

impl From<&Response> for ResponseRecord {
    fn from(response: &Response) -> Self {
        Self {
            status: response.status,
            status_text: response.status_text.clone(),
            elapsed_ms: response.elapsed.as_millis() as u64,
            headers: response
                .headers
                .iter()
                .map(|(name, value)| HeaderRecord {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
            body: response.body.clone(),
            redirects: response
                .redirects
                .iter()
                .map(|hop| RedirectHopRecord {
                    status: hop.status,
                    location: hop.location.clone(),
                })
                .collect(),
        }
    }
}

/// One redirect hop: the status that redirected, and the (already resolved to
/// absolute) `Location` it pointed at.
#[derive(Debug, Serialize)]
struct RedirectHopRecord {
    status: u16,
    location: String,
}

/// One response header. A list of `{name, value}` objects rather than one
/// object keyed by name, because HTTP allows a header to repeat (`set-cookie`)
/// and a map would silently drop all but one of them. Wire order is preserved.
#[derive(Debug, Serialize)]
struct HeaderRecord {
    name: String,
    value: String,
}

/// One request's assertions: the counts the human output prints as
/// `2 passed, 1 failed`, and the results behind them.
///
/// The counts are derivable from `results` and are here anyway — they are what
/// a CI script actually reads, and every consumer computing them from the list
/// would be three ways to spell the same query.
#[derive(Debug, Default, Serialize)]
pub(super) struct AssertionsRecord {
    total: usize,
    passed: usize,
    failed: usize,
    results: Vec<AssertionRecord>,
}

impl From<&AssertionReport> for AssertionsRecord {
    fn from(report: &AssertionReport) -> Self {
        Self {
            total: report.len(),
            passed: report.passed_count(),
            failed: report.failed_count(),
            results: report
                .results()
                .iter()
                .map(|result| AssertionRecord {
                    kind: kind_name(result.kind),
                    expectation: result.expectation.clone(),
                    passed: result.passed(),
                    failure: result.failure.clone(),
                })
                .collect(),
        }
    }
}

/// One assertion, evaluated. `expectation` and `failure` are core's own
/// wording, the same strings the terminal shows, so a failure reads the same
/// however it is consumed.
#[derive(Debug, Serialize)]
struct AssertionRecord {
    /// Which kind of check this was: `status`, `header`, `body_contains` or
    /// `json_path`.
    kind: &'static str,
    expectation: String,
    passed: bool,
    /// Why it did not hold, or null when it did.
    failure: Option<String>,
}

/// The wire name of an [`AssertionKind`].
///
/// Spelled out here rather than derived from the enum's Rust names, so that
/// renaming a variant in core cannot quietly rename a field in output someone
/// is parsing. The names match the keys the `assertions` block uses in YAML.
fn kind_name(kind: AssertionKind) -> &'static str {
    match kind {
        AssertionKind::Status => "status",
        AssertionKind::Header => "header",
        AssertionKind::BodyContains => "body_contains",
        AssertionKind::JsonPath => "json_path",
    }
}

/// `sendra test`'s counts, one field per number the summary line prints —
/// including the zeroes the terminal leaves out, since a consumer reading
/// `.summary.failed` should not have to know that a key disappears when it is
/// `0`.
#[derive(Debug, Serialize)]
pub(super) struct SummaryRecord {
    total: usize,
    passed: usize,
    failed: usize,
    without_assertions: usize,
    no_response: usize,
}

impl From<&Summary> for SummaryRecord {
    fn from(summary: &Summary) -> Self {
        Self {
            total: summary.total,
            passed: summary.passed,
            failed: summary.failed,
            without_assertions: summary.without_assertions,
            no_response: summary.no_response,
        }
    }
}
