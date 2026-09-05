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

use sendra_core::{AssertionKind, AssertionReport, Response, SendraError};
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
#[derive(Debug, Serialize)]
pub(super) struct RequestRecord {
    /// The request's `name`, or its URL when it has none — the same label the
    /// `→` line on stderr shows.
    label: String,
    pub(super) response: Option<ResponseRecord>,
    /// Why there was no response, message and causes joined with `: `.
    pub(super) error: Option<String>,
    pub(super) assertions: AssertionsRecord,
}

impl RequestRecord {
    pub(super) fn new(label: &str) -> Self {
        Self {
            label: label.to_string(),
            response: None,
            error: None,
            assertions: AssertionsRecord::default(),
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
        }
    }
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
