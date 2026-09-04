//! Sendra's command-line front-end.
//!
//! Everything here is presentation: argument parsing, terminal output and exit
//! codes. The request model and HTTP execution live in `sendra-core`.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use owo_colors::{OwoColorize, Stream};
use sendra_core::{Request, Response, SendraError};

/// Sendra's exit-code convention, in one place.
///
/// ```text
/// 0  ok          request sent, response was not an error status (or the user
///                opted out of status-based failure with --allow-error-status)
/// 1  failure     we never got a response: file missing or malformed, invalid
///                header, DNS/TLS/connection failure
/// 2  (reserved)  bad command-line usage — clap exits with this itself
/// 3  status      response came back, but the server said 4xx/5xx
/// ```
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
    /// Send the request defined in a YAML file.
    Run {
        /// Path to the request file.
        path: PathBuf,

        /// Exit 0 even when the response status is 4xx or 5xx.
        ///
        /// The response is printed either way; this only changes the exit code,
        /// for inspecting an error response without failing the surrounding
        /// script.
        #[arg(long)]
        allow_error_status: bool,
    },
}

// Current-thread runtime: one request per invocation, so there is nothing to
// schedule across worker threads. See the tokio feature list in Cargo.toml.
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Command::Run {
            path,
            allow_error_status,
        } => match run(&path).await {
            Ok(response) => {
                print_response(&response);
                exit_for_status(response.status, allow_error_status).into()
            }
            Err(err) => {
                print_error(&err);
                Exit::Failure.into()
            }
        },
    }
}

async fn run(path: &PathBuf) -> Result<Response, SendraError> {
    let request = Request::from_path(path)?;
    eprintln!(
        "{} {}",
        "→".if_supports_color(Stream::Stderr, |t| t.dimmed()),
        request.label().if_supports_color(Stream::Stderr, |t| t.bold())
    );
    sendra_core::send(&request).await
}

fn print_response(response: &Response) {
    let status_line = format!(
        "{} {}",
        response.status,
        response.status_text
    );
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

    #[test]
    fn exit_codes_match_the_documented_convention() {
        // The numbers are the contract with anyone scripting sendra, so pin
        // them here rather than only asserting on the variants.
        assert_eq!(Exit::Ok as u8, 0);
        assert_eq!(Exit::Failure as u8, 1);
        assert_eq!(Exit::ErrorStatus as u8, 3);
    }
}
