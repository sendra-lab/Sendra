//! What Sendra says when something goes wrong: the `error:` and `hint:` lines
//! every failure is printed through, and the one clap error raised in place of
//! output.

use std::io::IsTerminal;
use std::path::Path;

use owo_colors::{OwoColorize, Stream};
use sendra_core::environment::environment_path;
use sendra_core::SendraError;

use crate::cli::Cli;
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

/// The red `error:` line every failure starts with.
pub(super) fn print_error_line(message: impl std::fmt::Display) {
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
