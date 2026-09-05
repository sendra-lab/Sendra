//! Everything the two subcommands put on the terminal, and the one clap error
//! raised in place of output.

use std::io::IsTerminal;
use std::path::Path;

use owo_colors::{OwoColorize, Stream};
use sendra_core::environment::environment_path;
use sendra_core::{AssertionReport, Response, SendraError};

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
pub(crate) fn print_status_line(response: &Response) {
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

pub(crate) fn print_response(response: &Response) {
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
        println!("{}", response.body);
    }
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
pub(crate) fn print_assertions(report: &AssertionReport) {
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
pub(crate) fn print_no_assertions() {
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
pub(crate) fn print_summary(summary: &Summary) {
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
