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
};
use crate::run::{run, test};

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
            allow_error_status,
            json,
            show_captures,
            dry_run,
            output,
        } => {
            if output.is_some() && json {
                reject_output_with_json();
            }
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
                allow_error_status,
                json,
                show_captures,
                dry_run,
                output,
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
            json,
            show_captures,
            junit,
            allow_error_status,
            output,
        } => {
            if allow_error_status {
                reject_allow_error_status();
            }
            if output.is_some() && json {
                reject_output_with_json();
            }
            test(
                &path,
                request.as_deref(),
                env.as_deref(),
                &header,
                &var,
                timeout,
                json,
                show_captures,
                junit,
                output,
            )
            .await
            .into()
        }

        Command::Init => init().into(),
    }
}
