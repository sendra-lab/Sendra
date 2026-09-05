//! Everything the two subcommands put on the terminal, and the one clap error
//! raised in place of output.

use std::borrow::Cow;
use std::cell::RefCell;
use std::io::{IsTerminal, Write};
use std::path::Path;

use owo_colors::{OwoColorize, Stream};
use sendra_core::environment::environment_path;
use sendra_core::{AssertionKind, AssertionReport, Response, SendraError};
use serde::Serialize;

use crate::cli::Cli;
use crate::exit::Summary;
use crate::run::EnvironmentError;

/// Refuse `sendra test --allow-error-status`, and say why.
///
/// The flag has no meaning here. `test`'s exit code is decided by assertions
/// and never by a raw status — a `404` that no assertion mentions already
/// exits `0` — so there is no status-based failure for it to suppress. It
/// would be a no-op, and Sendra does not have silently-ignored inputs: an
/// assertion typo is an error, an unknown config key is an error, a `--env`
/// naming a file that is not there is an error, all for the same reason. A
/// flag accepted and quietly discarded reads, to whoever typed it, exactly
/// like one that worked.
///
/// Raised through clap rather than as a `SendraError` because it is a fact
/// about the command line and nothing else, which puts it in exit code `2`
/// with every other usage error — the same reasoning that keeps
/// [`EnvironmentError`] out of core.
pub(crate) fn reject_allow_error_status() -> ! {
    use clap::CommandFactory;

    Cli::command()
        .error(
            clap::error::ErrorKind::UnknownArgument,
            "`--allow-error-status` does not apply to `sendra test`.\n\n  \
             `test` decides its exit code from assertions, not from response \
             statuses: a 4xx or 5xx that no assertion mentions does not fail a \
             test run in the first place, so there is nothing here for the \
             flag to forgive.\n\n  \
             To check a status under `test`, assert it (`assertions:` with \
             `status: 404` under it). To inspect an error response without \
             failing the surrounding script, that is what \
             `sendra run --allow-error-status` is for.",
        )
        .exit()
}

/// The one line every response gets, whichever subcommand asked for it:
/// `200 OK  412 ms`.
///
/// Split out of [`print_response`] so that `sendra test`, which prints no
/// headers and no body, still says which response the assertions under it are
/// about — and says it in the same words and the same colours, rather than in a
/// second rendering of the same fact.
fn print_status_line(response: &Response) {
    let status_line = format!("{} {}", response.status, response.status_text);
    let status_line = status_line.trim_end().to_string();

    // Green for 2xx, red otherwise — a 404 is a completed run but a failed
    // request, and the colour is what says so at a glance.
    let painted = if response.is_success() {
        status_line
            .if_supports_color(Stream::Stdout, |t| t.green())
            .to_string()
    } else {
        status_line
            .if_supports_color(Stream::Stdout, |t| t.red())
            .to_string()
    };

    println!(
        "{}  {}",
        painted.if_supports_color(Stream::Stdout, |t| t.bold()),
        format!("{} ms", response.elapsed.as_millis())
            .if_supports_color(Stream::Stdout, |t| t.dimmed())
    );
}

fn print_response(response: &Response) {
    print_status_line(response);

    for (name, value) in &response.headers {
        println!(
            "{}: {}",
            name.if_supports_color(Stream::Stdout, |t| t.cyan()),
            value
        );
    }

    if !response.body.is_empty() {
        println!();
        println!("{}", body_for_display(response));
    }
}

/// The body as [`print_response`] shows it: re-indented when the response says
/// it is JSON and the bytes agree, and the raw body in every other case.
///
/// **The decision is made from `content-type` and nowhere else.** A body
/// starting with `{` is not a promise of anything — a text/plain body, an HTML
/// error page from a proxy, a JSON-looking log line — and guessing from the
/// first byte would mean the same server reply is displayed two different ways
/// depending on what its first character happens to be. `application/json` and
/// anything with a `+json` suffix ([RFC 6839]) count; the parameters after a
/// `;` are ignored, so `application/json; charset=utf-8` is JSON like the bare
/// form is.
///
/// **A body that claims to be JSON and is not, prints raw.** A truncated
/// response, or an error page served under the wrong content-type, is exactly
/// when the body is worth looking at, so a parse failure falls back rather than
/// hiding it or failing the run. The parse failure itself is not reported: the
/// body is on screen, and whether it is valid JSON is a question the assertions
/// exist to answer.
///
/// Returns a [`Cow`] because the common case — a non-JSON body — must not copy
/// the response body to print it.
///
/// One caveat, from re-serialising through [`serde_json::Value`]: numbers are
/// carried as `f64`, so a JSON number with more precision than that holds is
/// re-printed rounded. Key order is preserved (see the `preserve_order`
/// feature in `Cargo.toml`); duplicate keys in one object are not — the last
/// wins, as it does for every other JSON reader.
///
/// [RFC 6839]: https://www.rfc-editor.org/rfc/rfc6839
fn body_for_display(response: &Response) -> Cow<'_, str> {
    if !claims_json(&response.headers) {
        return Cow::Borrowed(&response.body);
    }

    match serde_json::from_str::<serde_json::Value>(&response.body) {
        Ok(value) => match serde_json::to_string_pretty(&value) {
            Ok(pretty) => Cow::Owned(pretty),
            Err(_) => Cow::Borrowed(&response.body),
        },
        Err(_) => Cow::Borrowed(&response.body),
    }
}

/// Whether these response headers say the body is JSON.
///
/// Header names are matched case-insensitively because HTTP header names are,
/// and so is the media type, because [RFC 9110] says media types are. A
/// repeated `content-type` — which is malformed, but happens — counts if any of
/// them says JSON.
///
/// [RFC 9110]: https://www.rfc-editor.org/rfc/rfc9110#name-media-type
fn claims_json(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .any(|(_, value)| {
            let media_type = value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            media_type == "application/json" || media_type.ends_with("+json")
        })
}

/// Print one request's assertion results under its response.
///
/// ```text
/// assertions
///   ✓ status is 200
///   ✓ header `content-type` is `application/json`
///   ✗ body contains `widget` — not found in the 429-byte body
///   ✗ `$.json.count` is 3 — got 2
///   2 passed, 2 failed
/// ```
///
/// Green for a `✓` and for the pass count, red for a `✗`, its detail and the
/// fail count.
///
/// An empty report prints nothing at all, so a request with no `assertions`
/// block produces byte-for-byte the output it did before assertions existed.
///
/// This goes to stdout, with the response rather than with the `→` label on
/// stderr: an assertion result is a statement *about the response*, produced
/// only because the file asked for it, and belongs next to the thing it
/// describes. The indented block under a dimmed `assertions` heading is what
/// keeps it apart from the raw body above it — a body can end in anything,
/// including a line that looks like a checkmark, so the separation is carried
/// by a blank line and a heading rather than by the symbols alone.
///
/// The wording of each line comes from core, so a future TUI reports the same
/// failure in the same words. Only the symbols, colour and layout are decided
/// here.
fn print_assertions(report: &AssertionReport) {
    if report.is_empty() {
        return;
    }

    println!();
    println!(
        "{}",
        "assertions".if_supports_color(Stream::Stdout, |t| t.dimmed())
    );

    for result in report.results() {
        match &result.failure {
            None => println!(
                "  {} {}",
                "✓".if_supports_color(Stream::Stdout, |t| t.green()),
                result.expectation
            ),
            // The detail is on the same line as the expectation it belongs to:
            // "what I asked for" and "what I got" are one sentence, and reading
            // them together is the whole reason the detail exists.
            Some(detail) => println!(
                "  {} {} {} {}",
                "✗".if_supports_color(Stream::Stdout, |t| t.red()),
                result.expectation,
                "—".if_supports_color(Stream::Stdout, |t| t.dimmed()),
                detail.if_supports_color(Stream::Stdout, |t| t.red())
            ),
        }
    }

    // A count line even for a single assertion: it is the one line worth
    // grepping for, and a summary that appears only sometimes is worse to
    // script against than one that is always there.
    //
    // Each half is coloured like the symbol it counts — green for the passes,
    // red for the failures — rather than the line taking one colour from
    // whether anything failed. Colouring the whole line red made "4 passed"
    // read as bad news.
    let passed = format!("{} passed", report.passed_count())
        .if_supports_color(Stream::Stdout, |t| t.green())
        .to_string();

    if report.passed() {
        println!("  {passed}");
    } else {
        let failed = format!("{} failed", report.failed_count())
            .if_supports_color(Stream::Stdout, |t| t.red())
            .to_string();
        println!("  {passed}, {failed}");
    }
}

/// The `sendra test` counterpart to [`print_assertions`] for a request that
/// declared none:
///
/// ```text
/// no assertions
/// ```
///
/// In the same position an `assertions` heading would occupy, dimmed and with
/// no `✓`/`✗` under it, because it is the absence of results rather than a
/// result. It exists so the summary's `without assertions` count has something
/// to point at: without it, the only way to find which request was uncovered
/// would be to notice which one printed nothing.
///
/// `sendra run` does not print it. A request with no assertions has always
/// produced byte-for-byte the output it produced before assertions existed, and
/// that stays true.
fn print_no_assertions() {
    println!();
    println!(
        "{}",
        "no assertions".if_supports_color(Stream::Stdout, |t| t.dimmed())
    );
}

/// Print the counts `sendra test` ends a run with.
///
/// ```text
/// summary
///   5 requests: 2 passed, 1 failed, 1 without assertions, 1 no response
/// ```
///
/// The total and the passes are always shown; the other three appear only when
/// they are not zero, so a clean run reads `3 requests: 3 passed` and nothing
/// competes with it. That follows [`print_assertions`], which prints
/// `4 passed` and adds `, 2 failed` only when there is something to add — one
/// rule, applied at both levels, rather than a per-request line that hides its
/// zeroes under a summary line that spells them out.
///
/// Each count is coloured like what it counts: green for passes, red for the
/// two that fail the run, dimmed for the one that does not. `without
/// assertions` being dimmed rather than yellow is deliberate — it is not a
/// warning, it is a fact about what the file asked for.
///
/// Under the same dimmed heading style as the per-request `assertions` blocks,
/// and for the same reason: a response body can end in anything, and the
/// heading plus the blank line is what keeps the summary from reading as the
/// tail of whatever printed above it.
fn print_summary(summary: &Summary) {
    println!();
    println!(
        "{}",
        "summary".if_supports_color(Stream::Stdout, |t| t.dimmed())
    );

    let mut counts = vec![format!("{} passed", summary.passed)
        .if_supports_color(Stream::Stdout, |t| t.green())
        .to_string()];

    if summary.failed > 0 {
        counts.push(
            format!("{} failed", summary.failed)
                .if_supports_color(Stream::Stdout, |t| t.red())
                .to_string(),
        );
    }
    if summary.without_assertions > 0 {
        counts.push(
            format!("{} without assertions", summary.without_assertions)
                .if_supports_color(Stream::Stdout, |t| t.dimmed())
                .to_string(),
        );
    }
    if summary.no_response > 0 {
        counts.push(
            format!("{} no response", summary.no_response)
                .if_supports_color(Stream::Stdout, |t| t.red())
                .to_string(),
        );
    }

    println!(
        "  {} {}: {}",
        summary.total,
        if summary.total == 1 {
            "request"
        } else {
            "requests"
        },
        counts.join(", ")
    );
}

// --- The run as a whole ----------------------------------------------------

/// How much of a response the human-readable output shows.
///
/// The two subcommands print the same *assertion* block — issue 6's format,
/// unchanged, because a second way to render a passed check would be a second
/// thing to learn — and differ only in how much of the response they put above
/// it. `run` exists to show you what came back, so it shows all of it. `test`
/// answers a yes/no question about a whole collection, and burying that answer
/// under four JSON bodies would make the summary the hardest line to find in
/// its own output; it prints the status line, which is one line, carries the
/// timing, and says which response the checks below it are about.
///
/// This is a fact about the *human* rendering only. `--json` carries the whole
/// response either way — see [`Format::Json`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Detail {
    /// Status line, headers and body.
    Full,
    /// The status line alone.
    StatusOnly,
}

/// Which rendering a run produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    /// The terminal output Sendra has always printed: a response, then its
    /// assertions, then (under `test`) a summary, as each request finishes.
    Human,

    /// One JSON object describing the whole run, written to stdout when the run
    /// is over — `--json`.
    ///
    /// Not one object per request: a stream of objects would make
    /// `sendra run collection.yaml --json | jq .` a stream of documents rather
    /// than a document, and the summary `test` ends with has nowhere to live in
    /// it. The cost is that nothing is printed until the run finishes, which is
    /// the honest trade for output that is parseable as a whole.
    ///
    /// Every request carries its full response — status, headers, body, elapsed
    /// — under both subcommands, so [`Detail`] does not apply. `test` printing
    /// less than `run` is a decision about what is *readable* on a terminal,
    /// and a program reading the output has no such problem; a script that
    /// wants only the status can select it.
    Json,
}

impl Format {
    /// What `--json`, present or absent, means. One function so the flag is
    /// read the same way by both subcommands.
    pub(crate) fn for_json_flag(json: bool) -> Self {
        if json {
            Format::Json
        } else {
            Format::Human
        }
    }
}

/// The one place a run's results become output.
///
/// Every subcommand reports through this, and it decides — once, from the
/// `--json` flag — whether that means printing as the run goes or recording for
/// a single document at the end. That is the whole of the guarantee `--json`
/// makes: **in [`Format::Json`] nothing but the final object is written to
/// stdout**, because in that mode none of the `print_*` functions above are
/// reached. The `→` labels and every error stay on stderr in both modes, where
/// they already were, so a redirected stdout is a clean JSON document and a
/// terminal still shows what went wrong as it happens.
///
/// Interior mutability rather than `&mut self`: the sending loop holds the
/// reporter *and* hands it to the closure that sends each request, and those
/// two shared borrows are simpler than threading one exclusive borrow through
/// an async closure. Nothing here is `Send`, which is fine — the whole binary
/// runs on one thread.
pub(crate) struct Reporter {
    format: Format,
    detail: Detail,
    /// One entry per request the run announced, in file order. Stays empty
    /// under [`Format::Human`], which has nothing to record because it has
    /// already printed.
    requests: RefCell<Vec<RequestRecord>>,
}

impl Reporter {
    pub(crate) fn new(format: Format, detail: Detail) -> Self {
        Self {
            format,
            detail,
            requests: RefCell::new(Vec::new()),
        }
    }

    fn recording(&self) -> bool {
        self.format == Format::Json
    }

    /// The blank line between one request's output and the next.
    ///
    /// Nothing under `--json`: the separator is whitespace on stdout, and
    /// stdout in that mode holds one document and nothing else.
    pub(crate) fn separate(&self) {
        if !self.recording() {
            println!();
        }
    }

    /// Announce the request about to be sent, and open its record.
    ///
    /// The `→` label goes to stderr in both modes, unchanged: in a collection
    /// run it is the only thing that says *which* request the next lines are
    /// about, and a "no variable named X" message names the variable, not the
    /// request.
    pub(crate) fn request_started(&self, label: &str) {
        eprintln!(
            "{} {}",
            "→".if_supports_color(Stream::Stderr, |t| t.dimmed()),
            label.if_supports_color(Stream::Stderr, |t| t.bold())
        );

        if self.recording() {
            self.requests.borrow_mut().push(RequestRecord::new(label));
        }
    }

    /// A request came back. `assertions` is its report, empty when the request
    /// declared none.
    pub(crate) fn responded(&self, response: &Response, assertions: &AssertionReport) {
        if self.recording() {
            self.with_current(|record| {
                record.response = Some(ResponseRecord::from(response));
                record.assertions = AssertionsRecord::from(assertions);
            });
            return;
        }

        match self.detail {
            Detail::Full => print_response(response),
            Detail::StatusOnly => print_status_line(response),
        }

        if assertions.is_empty() && self.detail == Detail::StatusOnly {
            // `run` says nothing here, and must keep saying nothing. Under
            // `test` the silence is the problem: the summary is about to count
            // this request as one of N "without assertions", and without a
            // marker there is nothing to match that number against.
            print_no_assertions();
        } else {
            print_assertions(assertions);
        }
    }

    /// A request never got a response: it could not be built, or it could not
    /// be sent.
    ///
    /// The error is printed to stderr in both modes — that is where it already
    /// went, and a `--json` run whose output is being redirected should still
    /// say on the terminal that something failed.
    pub(crate) fn request_failed(&self, err: &SendraError) {
        print_error(err);

        if self.recording() {
            let message = error_message(err);
            self.with_current(|record| record.error = Some(message));
        }
    }

    /// `sendra run` is over.
    pub(crate) fn finish_run(&self) {
        if self.recording() {
            self.emit(None);
        }
    }

    /// `sendra test` is over: same document as [`finish_run`](Self::finish_run)
    /// with the counts added, or the human summary block.
    pub(crate) fn finish_test(&self, summary: &Summary) {
        if self.recording() {
            self.emit(Some(summary));
        } else {
            print_summary(summary);
        }
    }

    /// Apply `fill` to the request currently being reported on.
    ///
    /// The record was opened by [`request_started`](Self::request_started),
    /// which every request goes through before anything can be said about it,
    /// so there is always one to fill. Doing nothing if there somehow is not is
    /// the right failure: a missing field in the output beats a panic that
    /// takes the run with it.
    fn with_current(&self, fill: impl FnOnce(&mut RequestRecord)) {
        if let Some(record) = self.requests.borrow_mut().last_mut() {
            fill(record);
        }
    }

    /// Write the document to stdout.
    ///
    /// A write that fails — a closed pipe, most likely — is reported on stderr
    /// and changes nothing else. `--json` is a serialisation of the result, not
    /// part of deciding it, so the exit code is the one the run earned either
    /// way.
    fn emit(&self, summary: Option<&Summary>) {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();

        if let Err(err) = self.write_json(&mut out, summary) {
            print_error_line(format!("could not write --json output: {err}"));
        }
    }

    /// The whole of `--json`, against any writer, so the schema can be tested
    /// without a process to capture the stdout of.
    fn write_json(&self, out: &mut impl Write, summary: Option<&Summary>) -> std::io::Result<()> {
        let requests = self.requests.borrow();
        let document = RunDocument {
            requests: &requests,
            summary: summary.map(SummaryRecord::from),
        };

        // Pretty rather than compact: the output is as likely to be read by a
        // person scrolling a redirected file as by a program, and every JSON
        // parser is indifferent. `jq` output is pretty for the same reason.
        //
        // The serialisation itself cannot fail — these are owned strings,
        // numbers and bools, with no map keys that are not strings and no
        // `Serialize` impl of our own that could error.
        let json = serde_json::to_string_pretty(&document)
            .expect("the record types hold nothing that can fail to serialise");

        writeln!(out, "{json}")
    }
}

/// One error, flattened to a single string: the message, then its causes,
/// separated by `: `.
///
/// The human output prints the cause chain as its own indented `caused by:`
/// lines; a JSON field is one value, and dropping the chain would lose the part
/// that says *why* — "request to `x` failed" without the DNS error under it
/// names the request and not the problem.
fn error_message(err: &SendraError) -> String {
    let mut message = err.to_string();

    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }

    message
}

// --- The `--json` schema ---------------------------------------------------
//
// The shape is defined here, in the front-end, rather than by deriving
// `Serialize` on the core types, and that is deliberate. `--json` is a promise
// this CLI makes about its stdout; core's types are a model of HTTP that a TUI
// will share and are free to change shape without anyone's pipeline breaking.
// Deriving on them would also have made the schema whatever serde happened to
// do with a `Duration` and a `Vec<(String, String)>` — `{"secs":0,"nanos":...}`
// and nested two-element arrays — instead of `elapsed_ms` and named header
// pairs. Nothing in core needed changing to add this.

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
struct RunDocument<'a> {
    requests: &'a [RequestRecord],
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<SummaryRecord>,
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
struct RequestRecord {
    /// The request's `name`, or its URL when it has none — the same label the
    /// `→` line on stderr shows.
    label: String,
    response: Option<ResponseRecord>,
    /// Why there was no response, message and causes joined with `: `.
    error: Option<String>,
    assertions: AssertionsRecord,
}

impl RequestRecord {
    fn new(label: &str) -> Self {
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
/// pretty-printing [`body_for_display`] does is for a person reading a
/// terminal, and rewriting a JSON body inside a JSON document would hand the
/// consumer something that is not what the server sent. A consumer that wants
/// it parsed has `fromjson`.
#[derive(Debug, Serialize)]
struct ResponseRecord {
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
struct AssertionsRecord {
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
struct SummaryRecord {
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

/// The red `error:` line every failure starts with.
fn print_error_line(message: impl std::fmt::Display) {
    let label = "error:".if_supports_color(Stream::Stderr, |t| t.red());
    eprintln!("{} {}", label, message);
}

/// One dimmed `hint:` line under an error, suppressed when stderr is not a
/// terminal: a hint is for a person reading the message, and a log or a pipe is
/// neither helped by it nor able to act on it.
fn print_hint(message: impl std::fmt::Display) {
    if std::io::stderr().is_terminal() {
        eprintln!(
            "  {} {}",
            "hint:".if_supports_color(Stream::Stderr, |t| t.dimmed()),
            message
        );
    }
}

pub(crate) fn print_environment_error(err: &EnvironmentError) {
    match err {
        // Core's own error, printed like every other one — cause chain and all.
        EnvironmentError::Unreadable(err) => print_error(err),
        EnvironmentError::NotFound {
            name,
            searched_from,
        } => {
            // Name the path that was looked for, not just the environment name:
            // it is where the file has to go to fix this, and it shows the
            // typo back to whoever typed it.
            print_error_line(format!(
                "no environment named `{name}`: no `{}` in `{}` or any parent directory",
                environment_path(Path::new(""), name).display(),
                searched_from.display()
            ));
            print_hint("create that file, or omit --env to run without an environment");
        }
    }
}

pub(crate) fn print_error(err: &SendraError) {
    print_error_line(err);

    // thiserror keeps the cause chain intact; show it so a TLS or DNS failure
    // buried under reqwest is still readable.
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        eprintln!(
            "  {} {}",
            "caused by:".if_supports_color(Stream::Stderr, |t| t.dimmed()),
            cause
        );
        source = cause.source();
    }

    // One actionable hint, without turning this into a help system.
    if matches!(err, SendraError::Io { .. }) {
        print_hint("check the path, or see examples/get-request.yaml for the file shape");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use crate::test_support::assertions_from;

    fn response_with(content_type: &str, body: &str) -> Response {
        Response {
            status: 200,
            status_text: "OK".to_string(),
            headers: vec![("content-type".to_string(), content_type.to_string())],
            body: body.to_string(),
            elapsed: Duration::from_millis(12),
        }
    }

    /// The document the reporter would have written to stdout, parsed back.
    fn document(reporter: &Reporter, summary: Option<&Summary>) -> serde_json::Value {
        let mut out = Vec::new();
        reporter
            .write_json(&mut out, summary)
            .expect("a `Vec` never fails to be written to");

        let text = String::from_utf8(out).expect("the document is UTF-8");
        assert!(
            text.ends_with("}\n"),
            "the document is one object, newline-terminated: {text}"
        );

        serde_json::from_str(&text).expect("`--json` must emit parseable JSON")
    }

    // --- Pretty-printing a JSON body ------------------------------------

    #[test]
    fn a_json_body_is_indented_for_the_terminal() {
        let response = response_with("application/json", r#"{"id":1,"tags":["a","b"]}"#);

        assert_eq!(
            body_for_display(&response),
            "{\n  \"id\": 1,\n  \"tags\": [\n    \"a\",\n    \"b\"\n  ]\n}"
        );
    }

    #[test]
    fn the_servers_key_order_survives_pretty_printing() {
        // Sorting the keys would make the output a rearrangement of the
        // response rather than the response — see the `preserve_order` feature
        // in Cargo.toml. These are in an order no sort would produce.
        let response = response_with("application/json", r#"{"zeta":1,"alpha":2,"mid":3}"#);

        assert_eq!(
            body_for_display(&response),
            "{\n  \"zeta\": 1,\n  \"alpha\": 2,\n  \"mid\": 3\n}"
        );
    }

    #[test]
    fn a_charset_parameter_does_not_stop_a_body_being_json() {
        let response = response_with("application/json; charset=utf-8", r#"{"a":1}"#);
        assert_eq!(body_for_display(&response), "{\n  \"a\": 1\n}");
    }

    #[test]
    fn a_plus_json_suffix_counts_as_json() {
        // RFC 6839: `application/problem+json`, `application/vnd.api+json` and
        // every other structured suffix are JSON, and a tool that only knew
        // `application/json` would print an error document raw.
        let response = response_with("application/problem+json", r#"{"title":"Nope"}"#);
        assert_eq!(body_for_display(&response), "{\n  \"title\": \"Nope\"\n}");

        // Upper case, because a media type is not case-sensitive.
        let response = response_with("APPLICATION/VND.API+JSON", r#"{"data":null}"#);
        assert_eq!(body_for_display(&response), "{\n  \"data\": null\n}");
    }

    #[test]
    fn a_body_that_claims_to_be_json_and_is_not_prints_raw() {
        // A truncated response under the right content-type: exactly when the
        // body is worth seeing, so it is printed as it arrived rather than
        // swallowed — and, above all, not a panic.
        let truncated = r#"{"id": 1, "name": "widg"#;
        let response = response_with("application/json", truncated);

        assert_eq!(body_for_display(&response), truncated);
    }

    #[test]
    fn an_empty_body_under_a_json_content_type_prints_raw() {
        // A `204` with `content-type: application/json` and nothing after it.
        // `print_response` does not reach this for an empty body, but the
        // fallback must hold on its own.
        let response = response_with("application/json", "");
        assert_eq!(body_for_display(&response), "");
    }

    #[test]
    fn a_body_that_is_not_declared_json_is_untouched() {
        // Even when it would parse. The content-type is the only input to this
        // decision; a text/plain body that happens to be JSON is text.
        let response = response_with("text/plain", r#"{"looks":"json"}"#);
        assert_eq!(body_for_display(&response), r#"{"looks":"json"}"#);

        let html = "<html><body>Not found</body></html>";
        let response = response_with("text/html; charset=utf-8", html);
        assert_eq!(body_for_display(&response), html);

        // A near miss that is not JSON: `application/json-seq` neither equals
        // `application/json` nor carries a `+json` suffix.
        let response = response_with("application/json-seq", r#"{"a":1}"#);
        assert_eq!(body_for_display(&response), r#"{"a":1}"#);
    }

    #[test]
    fn a_response_with_no_content_type_at_all_is_untouched() {
        let response = Response {
            headers: Vec::new(),
            ..response_with("text/plain", r#"{"a":1}"#)
        };
        assert_eq!(body_for_display(&response), r#"{"a":1}"#);
    }

    // --- The `--json` document ------------------------------------------

    #[test]
    fn run_reports_one_object_holding_every_request() {
        let reporter = Reporter::new(Format::Json, Detail::Full);

        reporter.request_started("Get user");
        let response = response_with("application/json", r#"{"id":1}"#);
        let assertions =
            assertions_from("method: GET\nurl: https://example.com\nassertions:\n  status: 404\n")
                .evaluate(&response);
        reporter.responded(&response, &assertions);

        let document = document(&reporter, None);

        // `run` has no summary — the key is absent rather than null.
        assert_eq!(
            document.as_object().map(|object| object.len()),
            Some(1),
            "`run` emits `requests` and nothing else: {document}"
        );

        let request = &document["requests"][0];
        assert_eq!(request["label"], "Get user");
        assert_eq!(request["error"], serde_json::Value::Null);

        let reported = &request["response"];
        assert_eq!(reported["status"], 200);
        assert_eq!(reported["status_text"], "OK");
        assert_eq!(reported["elapsed_ms"], 12);
        assert_eq!(reported["headers"][0]["name"], "content-type");
        assert_eq!(reported["headers"][0]["value"], "application/json");
        // The raw body, not the indented one: rewriting the server's bytes
        // inside a document about them would be a lie about what came back.
        assert_eq!(reported["body"], r#"{"id":1}"#);

        let assertions = &request["assertions"];
        assert_eq!(assertions["total"], 1);
        assert_eq!(assertions["passed"], 0);
        assert_eq!(assertions["failed"], 1);
        assert_eq!(assertions["results"][0]["kind"], "status");
        assert_eq!(assertions["results"][0]["expectation"], "status is 404");
        assert_eq!(assertions["results"][0]["passed"], false);
        assert_eq!(assertions["results"][0]["failure"], "got 200");
    }

    #[test]
    fn a_request_that_never_got_a_response_carries_the_error_instead() {
        let reporter = Reporter::new(Format::Json, Detail::Full);

        reporter.request_started("GET https://example.com");
        reporter.request_failed(&SendraError::Io {
            path: "req.yaml".into(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
        });

        let request = &document(&reporter, None)["requests"][0];

        assert_eq!(request["response"], serde_json::Value::Null);
        // Message and cause on one line: the message alone names the file and
        // not the problem.
        assert_eq!(
            request["error"],
            "could not read request file `req.yaml`: no such file"
        );
        // Always an object, even with nothing in it, so a consumer can read
        // `.assertions.failed` for every request without checking for null.
        assert_eq!(request["assertions"]["total"], 0);
        assert_eq!(
            request["assertions"]["results"].as_array().unwrap().len(),
            0
        );
    }

    #[test]
    fn a_passing_assertion_reports_a_null_failure() {
        let reporter = Reporter::new(Format::Json, Detail::Full);

        reporter.request_started("Get user");
        let response = response_with("application/json", r#"{"id":1}"#);
        let assertions = assertions_from(
            "method: GET\nurl: https://example.com\nassertions:\n  status: 200\n  json:\n    $.id: 1\n",
        )
        .evaluate(&response);
        reporter.responded(&response, &assertions);

        let assertions = &document(&reporter, None)["requests"][0]["assertions"];

        assert_eq!(assertions["total"], 2);
        assert_eq!(assertions["passed"], 2);
        assert_eq!(assertions["failed"], 0);
        for result in assertions["results"].as_array().unwrap() {
            assert_eq!(result["passed"], true);
            assert_eq!(result["failure"], serde_json::Value::Null);
        }
        // The kinds are named after the YAML keys they were written as.
        assert_eq!(assertions["results"][0]["kind"], "status");
        assert_eq!(assertions["results"][1]["kind"], "json_path");
    }

    #[test]
    fn every_request_appears_in_file_order() {
        let reporter = Reporter::new(Format::Json, Detail::Full);

        for label in ["First", "Second", "Third"] {
            reporter.request_started(label);
            reporter.responded(
                &response_with("text/plain", "ok"),
                &AssertionReport::default(),
            );
        }

        let requests = document(&reporter, None)["requests"].clone();
        let labels: Vec<&str> = requests
            .as_array()
            .unwrap()
            .iter()
            .map(|request| request["label"].as_str().unwrap())
            .collect();

        assert_eq!(labels, vec!["First", "Second", "Third"]);
    }

    #[test]
    fn test_reports_the_same_requests_plus_the_summary() {
        let reporter = Reporter::new(Format::Json, Detail::StatusOnly);

        reporter.request_started("Get user");
        let response = response_with("application/json", r#"{"id":1}"#);
        reporter.responded(&response, &AssertionReport::default());

        let summary = Summary {
            total: 3,
            passed: 1,
            failed: 0,
            without_assertions: 1,
            no_response: 1,
        };
        let document = document(&reporter, Some(&summary));

        // `test` prints a status line only, but reports the whole response: the
        // terminal's brevity is about what is readable on a screen, and a
        // program reading this has no such problem.
        assert_eq!(document["requests"][0]["response"]["body"], r#"{"id":1}"#);
        assert_eq!(
            document["requests"][0]["response"]["headers"][0]["name"],
            "content-type"
        );

        // Every count, zeroes included — the terminal leaves a zero out, and a
        // consumer reading `.summary.failed` should not have to know that.
        assert_eq!(document["summary"]["total"], 3);
        assert_eq!(document["summary"]["passed"], 1);
        assert_eq!(document["summary"]["failed"], 0);
        assert_eq!(document["summary"]["without_assertions"], 1);
        assert_eq!(document["summary"]["no_response"], 1);
    }

    #[test]
    fn a_run_that_sent_nothing_is_still_a_document() {
        // Nothing reaches this today — an empty collection is refused when the
        // file is parsed — but `jq` should get a document rather than an empty
        // file if anything ever does.
        let reporter = Reporter::new(Format::Json, Detail::Full);

        assert_eq!(
            document(&reporter, None)["requests"],
            serde_json::json!([]),
            "an empty run is an empty list, not a missing key"
        );
    }

    #[test]
    fn the_human_reporter_records_nothing() {
        // The two formats are one decision, made once: a human run keeps no
        // records, so there is no second code path that could disagree with
        // what was printed.
        let reporter = Reporter::new(Format::Human, Detail::Full);

        reporter.request_started("Get user");
        reporter.responded(
            &response_with("text/plain", "ok"),
            &AssertionReport::default(),
        );

        assert!(reporter.requests.borrow().is_empty());
    }
}
