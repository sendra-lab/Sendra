//! Sendra's command-line front-end.
//!
//! Everything here is presentation: argument parsing, terminal output and exit
//! codes. The request model and HTTP execution live in `sendra-core`.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use owo_colors::{OwoColorize, Stream};
use sendra_core::{Document, Request, Response, SendraError};

/// Sendra's exit-code convention, in one place.
///
/// ```text
/// 0  ok          every request sent, and no response was an error status (or
///                the user opted out of status-based failure with
///                --allow-error-status)
/// 1  failure     some request never got a response: file missing or malformed,
///                no such request name, invalid header, DNS/TLS/connection
///                failure
/// 2  (reserved)  bad command-line usage — clap exits with this itself
/// 3  status      every request got a response, but at least one was 4xx/5xx
/// ```
///
/// A collection run sends many requests under one exit code, so these are
/// aggregates; [`worst`] is where they combine.
///
/// Codes 4 and up are deliberately free: `sendra test` will need its own
/// outcome (assertions failed) that is neither "could not send" nor "one bad
/// status". Every exit path in the binary returns one of these variants rather
/// than calling `std::process::exit` inline, so adding a code later means
/// adding a row here and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    Ok = 0,
    Failure = 1,
    // 2 belongs to clap; see the table above.
    ErrorStatus = 3,
}

impl From<Exit> for ExitCode {
    fn from(exit: Exit) -> Self {
        ExitCode::from(exit as u8)
    }
}

/// Decide the exit code for a response that came back.
///
/// 1xx/2xx/3xx are "the server answered and did not object"; 4xx and 5xx are
/// failures unless the caller opted out. Anything at or above 400 counts,
/// including non-standard 6xx codes — a status we do not recognise is not a
/// status we should report as success.
///
/// Takes a bare `u16` rather than a `&Response` so the decision stays pure and
/// testable without constructing a response or touching the network.
fn exit_for_status(status: u16, allow_error_status: bool) -> Exit {
    if allow_error_status || status < 400 {
        Exit::Ok
    } else {
        Exit::ErrorStatus
    }
}

/// Fold the outcomes of several requests into the single code the process can
/// return. The worst outcome wins, ranked `Ok` < `ErrorStatus` < `Failure`.
///
/// "Worst wins" rather than "the last request wins": the exit code should
/// answer "did anything go wrong?", and tying it to the last request would make
/// it depend on the order the file happens to list requests in, so reordering a
/// collection could change whether a script proceeds. This keeps
/// `sendra run collection.yaml && deploy.sh` meaning for a collection what it
/// means for a single request — exit 0 is a promise that nothing in the run
/// failed.
///
/// `Failure` outranks `ErrorStatus` because "never got a response" is the
/// bigger problem: a 404 is an answer, a DNS failure is not. A run reports the
/// most serious thing that happened, not the most recent.
fn worst(a: Exit, b: Exit) -> Exit {
    // Rank by severity, not by the exit numbers: 3 (ErrorStatus) is the milder
    // outcome of the two failures, so the numeric order is the wrong order.
    fn severity(exit: Exit) -> u8 {
        match exit {
            Exit::Ok => 0,
            Exit::ErrorStatus => 1,
            Exit::Failure => 2,
        }
    }

    if severity(b) > severity(a) {
        b
    } else {
        a
    }
}

#[derive(Parser)]
#[command(
    name = "sendra",
    version,
    about = "Terminal-native HTTP client — send requests defined in YAML files."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Send the request, or collection of requests, defined in a YAML file.
    Run {
        /// Path to the request or collection file.
        path: PathBuf,

        /// Name of one request to send, when the file is a collection.
        ///
        /// Omit it to send every request in the collection, in file order.
        /// Passing a name to a file that holds a single request is an error:
        /// there is nothing to choose between.
        request: Option<String>,

        /// Exit 0 even when a response status is 4xx or 5xx.
        ///
        /// Responses are printed either way; this only changes the exit code,
        /// for inspecting an error response without failing the surrounding
        /// script.
        #[arg(long)]
        allow_error_status: bool,
    },
}

// Current-thread runtime: a collection is sent sequentially, in file order, so
// there is still nothing to spread across worker threads. Sending a collection
// concurrently would scramble both the request order and the output, and the
// file is what is meant to control those. See the tokio features in Cargo.toml.
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Command::Run {
            path,
            request,
            allow_error_status,
        } => run(&path, request.as_deref(), allow_error_status)
            .await
            .into(),
    }
}

/// Load `path` and send either the one request named, or all of them.
///
/// Returns an exit code rather than a `Result` because a collection run can
/// half-succeed: one request failing does not stop the rest, so there is no
/// single error to propagate. Every outcome is printed as it happens and folded
/// into the code with [`worst`].
async fn run(path: &Path, name: Option<&str>, allow_error_status: bool) -> Exit {
    let document = match Document::from_path(path) {
        Ok(document) => document,
        Err(err) => {
            print_error(&err);
            return Exit::Failure;
        }
    };

    let requests: Vec<&Request> = match name {
        Some(name) => match document.get(name) {
            Ok(request) => vec![request],
            Err(err) => {
                print_error(&err);
                return Exit::Failure;
            }
        },
        // No name: a single-request file yields its one request, a collection
        // yields all of them, in file order.
        None => document.requests().iter().collect(),
    };

    let mut exit = Exit::Ok;
    for (index, request) in requests.iter().enumerate() {
        // Blank line between results so a multi-request run stays readable.
        if index > 0 {
            println!();
        }
        exit = worst(exit, send(request, allow_error_status).await);
    }
    exit
}

/// Send one request, print whatever came back, and report its outcome.
///
/// A failure is printed and returned rather than propagated: in a collection
/// run the requests after this one still deserve to be sent, and the user still
/// deserves to see them.
async fn send(request: &Request, allow_error_status: bool) -> Exit {
    eprintln!(
        "{} {}",
        "→".if_supports_color(Stream::Stderr, |t| t.dimmed()),
        request
            .label()
            .if_supports_color(Stream::Stderr, |t| t.bold())
    );

    match sendra_core::send(request).await {
        Ok(response) => {
            print_response(&response);
            exit_for_status(response.status, allow_error_status)
        }
        Err(err) => {
            print_error(&err);
            Exit::Failure
        }
    }
}

fn print_response(response: &Response) {
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

fn print_error(err: &SendraError) {
    let label = "error:".if_supports_color(Stream::Stderr, |t| t.red());
    eprintln!("{} {}", label, err);

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

    // Keeps the `IsTerminal` import honest and gives one actionable hint
    // without turning this into a help system.
    if matches!(err, SendraError::Io { .. }) && std::io::stderr().is_terminal() {
        eprintln!(
            "  {} check the path, or see examples/get-request.yaml for the file shape",
            "hint:".if_supports_color(Stream::Stderr, |t| t.dimmed())
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_and_redirect_statuses_exit_zero() {
        for status in [100, 200, 201, 204, 301, 302, 304, 399] {
            assert_eq!(
                exit_for_status(status, false),
                Exit::Ok,
                "{status} should not fail the run"
            );
        }
    }

    #[test]
    fn client_and_server_error_statuses_exit_non_zero() {
        for status in [400, 401, 404, 418, 500, 503] {
            assert_eq!(
                exit_for_status(status, false),
                Exit::ErrorStatus,
                "{status} should fail the run"
            );
        }
    }

    #[test]
    fn allow_error_status_forces_zero_for_every_status() {
        for status in [200, 301, 404, 500] {
            assert_eq!(
                exit_for_status(status, true),
                Exit::Ok,
                "--allow-error-status should keep {status} at exit 0"
            );
        }
    }

    /// The exit code a collection run produces: fold each request's outcome in,
    /// in order, the way `run` does.
    fn aggregate(outcomes: impl IntoIterator<Item = Exit>) -> Exit {
        outcomes.into_iter().fold(Exit::Ok, worst)
    }

    #[test]
    fn an_all_ok_collection_run_exits_zero() {
        assert_eq!(aggregate([Exit::Ok, Exit::Ok, Exit::Ok]), Exit::Ok);
    }

    #[test]
    fn one_error_status_anywhere_fails_the_whole_collection_run() {
        // Whether the 4xx is first, last or in the middle must not matter.
        for outcomes in [
            [Exit::ErrorStatus, Exit::Ok, Exit::Ok],
            [Exit::Ok, Exit::ErrorStatus, Exit::Ok],
            [Exit::Ok, Exit::Ok, Exit::ErrorStatus],
        ] {
            assert_eq!(
                aggregate(outcomes),
                Exit::ErrorStatus,
                "a 4xx/5xx anywhere in {outcomes:?} should fail the run"
            );
        }
    }

    #[test]
    fn a_mixed_status_collection_reports_the_error_status_not_the_last_one() {
        // examples/mixed-status-collection.yaml: 200, then 404, then 500.
        let outcomes = [200, 404, 500].map(|status| exit_for_status(status, false));
        assert_eq!(aggregate(outcomes), Exit::ErrorStatus);

        // A 200 last would be just as much of a failed run.
        let outcomes = [404, 500, 200].map(|status| exit_for_status(status, false));
        assert_eq!(aggregate(outcomes), Exit::ErrorStatus);
    }

    #[test]
    fn allow_error_status_keeps_a_mixed_collection_at_zero() {
        let outcomes = [200, 404, 500].map(|status| exit_for_status(status, true));
        assert_eq!(aggregate(outcomes), Exit::Ok);
    }

    #[test]
    fn a_request_that_never_got_a_response_outranks_a_bad_status() {
        // "could not send" is the more serious outcome, whichever order the two
        // happen in, because a status at least means the server answered.
        assert_eq!(aggregate([Exit::ErrorStatus, Exit::Failure]), Exit::Failure);
        assert_eq!(aggregate([Exit::Failure, Exit::ErrorStatus]), Exit::Failure);
    }

    #[test]
    fn worst_is_order_independent() {
        // Every pair, both ways round: aggregation must not depend on file
        // order, which is the whole point of not using "last request wins".
        for a in [Exit::Ok, Exit::ErrorStatus, Exit::Failure] {
            for b in [Exit::Ok, Exit::ErrorStatus, Exit::Failure] {
                assert_eq!(
                    worst(a, b),
                    worst(b, a),
                    "worst({a:?}, {b:?}) is asymmetric"
                );
            }
        }
    }

    #[test]
    fn exit_codes_match_the_documented_convention() {
        // The numbers are the contract with anyone scripting sendra, so pin
        // them here rather than only asserting on the variants.
        assert_eq!(Exit::Ok as u8, 0);
        assert_eq!(Exit::Failure as u8, 1);
        assert_eq!(Exit::ErrorStatus as u8, 3);
    }
}
