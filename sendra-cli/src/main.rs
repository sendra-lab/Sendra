//! Sendra's command-line front-end.
//!
//! Everything here is presentation: argument parsing, terminal output and exit
//! codes. The request model and HTTP execution live in `sendra-core`.

mod cli;
mod exit;
mod init;
mod output;
mod run;
#[cfg(test)]
mod test_support;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::{Cli, Command, OutputMode};
use crate::init::init;
use crate::output::{
    reject_allow_error_status, reject_output_status_with_dry_run, reject_output_with_json,
    reject_quiet_with_output, reject_verbose_with_quiet,
};
use crate::run::{run, test};

/// `-q`/`--quiet` folded into `-o`/`--output`'s value: `None` (`-q` was not
/// passed) leaves `output` untouched; `-q` alone (`output` was omitted)
/// becomes `Some(OutputMode::None)`; `-q -o none` agrees and stays
/// `Some(OutputMode::None)`; `-q` with any other explicit `-o <mode>` is a
/// real conflict, refused before either subcommand ever runs — see
/// `reject_quiet_with_output`.
///
/// One function so `run` and `test` fold the two flags together the same
/// way, checked against the *original* `output` — the value the user
/// actually typed, before this function's own `-q` implication could make
/// every combination look consistent with itself.
fn resolve_output(output: Option<OutputMode>, quiet: bool) -> Option<OutputMode> {
    if quiet {
        match output {
            None | Some(OutputMode::None) => Some(OutputMode::None),
            Some(_) => reject_quiet_with_output(),
        }
    } else {
        output
    }
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
            env,
            header,
            var,
            timeout,
            repeat,
            allow_error_status,
            json,
            show_captures,
            dry_run,
            output,
            quiet,
            verbose,
        } => {
            if verbose && quiet {
                reject_verbose_with_quiet();
            }
            if output.is_some() && json {
                reject_output_with_json();
            }
            let output = resolve_output(output, quiet);
            if dry_run && output == Some(OutputMode::Status) {
                reject_output_status_with_dry_run();
            }
            run(
                &path,
                request.as_deref(),
                env.as_deref(),
                &header,
                &var,
                timeout,
                repeat,
                allow_error_status,
                json,
                show_captures,
                dry_run,
                output,
                quiet,
                verbose,
            )
            .await
            .into()
        }

        Command::Test {
            path,
            request,
            env,
            header,
            var,
            timeout,
            repeat,
            json,
            show_captures,
            junit,
            allow_error_status,
            output,
            quiet,
            verbose,
        } => {
            if allow_error_status {
                reject_allow_error_status();
            }
            if verbose && quiet {
                reject_verbose_with_quiet();
            }
            if output.is_some() && json {
                reject_output_with_json();
            }
            let output = resolve_output(output, quiet);
            test(
                &path,
                request.as_deref(),
                env.as_deref(),
                &header,
                &var,
                timeout,
                repeat,
                json,
                show_captures,
                junit,
                output,
                quiet,
                verbose,
            )
            .await
            .into()
        }

        Command::Init => init().into(),
    }
}
