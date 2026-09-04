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

/// Exit code for "we ran, but the request or the file was bad".
const EXIT_FAILURE: u8 = 1;

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
    },
}

// Current-thread runtime: one request per invocation, so there is nothing to
// schedule across worker threads. See the tokio feature list in Cargo.toml.
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Command::Run { path } => match run(&path).await {
            Ok(response) => {
                print_response(&response);
                ExitCode::SUCCESS
            }
            Err(err) => {
                print_error(&err);
                ExitCode::from(EXIT_FAILURE)
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

    // Green for 2xx, red otherwise — a 404 is a successful run but a failed
    // request, and the colour is the only thing that says so.
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
