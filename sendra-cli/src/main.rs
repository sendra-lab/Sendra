//! Sendra's command-line front-end.
//!
//! Everything here is presentation: argument parsing, terminal output and exit
//! codes. The request model and HTTP execution live in `sendra-core`.

mod cli;
mod exit;
mod import;
mod init;
mod output;
mod run;
mod schema;
#[cfg(test)]
mod test_support;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::{Cli, Command, ImportTarget, OutputMode};
use crate::import::import_curl;
use crate::init::init;
use crate::output::{
    reject_allow_error_status, reject_output_status_with_dry_run, reject_output_with_json,
    reject_quiet_with_output, reject_verbose_with_quiet,
};
use crate::run::{run, test};
use crate::schema::schema;

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
        // No subcommand at all: launch the TUI with nothing preloaded, the
        // same as `sendra tui` with no path. Decided deliberately, not left
        // as a "maybe later" — see `Command::Tui`'s own doc comment and
        // `Cli::command`'s. A `sendra` with a typo'd subcommand name still
        // hits clap's ordinary usage error, since that never reaches `None`.
        None => run_tui(None),

        Some(Command::Tui { path }) => run_tui(path),

        Some(Command::Run {
            path,
            request,
            env,
            header,
            var,
            timeout,
            repeat,
            insecure,
            proxy,
            client_cert,
            client_key,
            cookie_jar,
            allow_error_status,
            json,
            show_captures,
            dry_run,
            output,
            quiet,
            verbose,
        }) => {
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
                insecure,
                proxy.as_deref(),
                client_cert.as_deref(),
                client_key.as_deref(),
                cookie_jar,
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

        Some(Command::Test {
            path,
            request,
            env,
            header,
            var,
            timeout,
            repeat,
            insecure,
            proxy,
            client_cert,
            client_key,
            cookie_jar,
            json,
            show_captures,
            junit,
            allow_error_status,
            output,
            quiet,
            verbose,
        }) => {
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
                insecure,
                proxy.as_deref(),
                client_cert.as_deref(),
                client_key.as_deref(),
                cookie_jar,
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

        Some(Command::Init) => init().into(),

        Some(Command::Schema { output }) => schema(output).into(),

        Some(Command::Import { target }) => match target {
            ImportTarget::Curl { command, output } => import_curl(command, output).into(),
        },
    }
}

/// Runs `sendra tui`, in-process — `sendra-tui::run` is a blocking, synchronous
/// full-screen event loop, not itself `async`; calling it from inside this
/// `async fn main` just occupies the current-thread runtime's one thread for
/// as long as the TUI session lasts, exactly the way `sendra tui` occupying
/// the whole process would on its own. Nothing else needs that thread
/// concurrently — the TUI's own HTTP sends and OAuth logins each spin up
/// their own dedicated OS thread and runtime (`run_request::spawn`,
/// `oauth_login::spawn`), independent of this one, the same way they always
/// have.
///
/// A plain `eprintln!` + `ExitCode::FAILURE` on I/O failure, not `Exit`'s
/// codes: those are `run`/`test`'s documented contract with scripts checking
/// `$?` (0/1/3/4), and a TUI session ending — cleanly or via a startup I/O
/// error — is not one of the outcomes that contract describes.
fn run_tui(path: Option<std::path::PathBuf>) -> ExitCode {
    match sendra_tui::run(path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err}");
            ExitCode::FAILURE
        }
    }
}
