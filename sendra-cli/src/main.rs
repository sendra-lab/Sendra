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

use crate::cli::{Cli, Command};
use crate::init::init;
use crate::output::reject_allow_error_status;
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
        } => run(
            &path,
            request.as_deref(),
            env.as_deref(),
            &header,
            &var,
            timeout,
            allow_error_status,
            json,
            show_captures,
        )
        .await
        .into(),

        Command::Test {
            path,
            env,
            header,
            var,
            timeout,
            json,
            show_captures,
            allow_error_status,
        } => {
            if allow_error_status {
                reject_allow_error_status();
            }
            test(
                &path,
                env.as_deref(),
                &header,
                &var,
                timeout,
                json,
                show_captures,
            )
            .await
            .into()
        }

        Command::Init => init().into(),
    }
}
