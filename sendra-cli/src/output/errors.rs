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
///
/// A hint may run to several lines — [`REQUEST_FILE_HINT`] carries a worked
/// example — so every line after the first is indented to sit under the text
/// of the first rather than starting back at column zero. Blank lines stay
/// blank rather than becoming a row of trailing spaces.
fn print_hint(message: impl std::fmt::Display) {
    if !std::io::stderr().is_terminal() {
        return;
    }

    let label = "hint:".if_supports_color(Stream::Stderr, |t| t.dimmed());
    eprintln!("{}", render_hint(&label.to_string(), &message.to_string()));
}

/// The width of the `  hint: ` label, which every line after the first is
/// indented by so that the whole hint reads as one block.
const HINT_INDENT: &str = "        ";

/// Lay a hint out under its label.
///
/// Pure, and separate from [`print_hint`], because printing is gated on stderr
/// being a terminal and a test harness is not one — so this is the only part
/// of a multi-line hint's layout a test can actually see. `label` arrives
/// already styled, since its colour is not this function's business.
fn render_hint(label: &str, message: &str) -> String {
    let mut lines = message.lines();
    let mut out = format!("  {label} {}", lines.next().unwrap_or_default());

    for line in lines {
        out.push('\n');
        // A blank line stays blank rather than becoming a row of trailing
        // spaces.
        if !line.is_empty() {
            out.push_str(HINT_INDENT);
            out.push_str(line);
        }
    }

    out
}

/// The hint under a request file that could not be read.
///
/// It inlines a whole request rather than naming a file to go and look at.
/// The obvious thing to point at — `examples/get-request.yaml` — exists only
/// in a clone of the source repository, so for anyone who installed a
/// released binary it named a file that was never on their disk. A hint that
/// sends its reader hunting for something that was never shipped is worse
/// than no hint at all, and a link to hosted docs would be the same mistake
/// today, there being no docs site yet.
///
/// So the example *is* the hint: two keys, which is everything a request
/// cannot do without, and nothing that needs a second thing open to read.
/// Indented by two so that [`print_hint`]'s own continuation indent nests it
/// under the sentence introducing it.
const REQUEST_FILE_HINT: &str = "\
check the path. A request file is YAML, and needs only two keys:

  method: GET
  url: https://api.example.com/users/1";

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
    match err {
        SendraError::Io { .. } => print_hint(REQUEST_FILE_HINT),
        // The whole reason `Timeout` is its own variant: this is the one
        // network failure whose fix might be a Sendra setting rather than
        // something out on the network, and the error line has just named a
        // limit the user may not know they can change.
        SendraError::Timeout { .. } => {
            print_hint("raise `timeout_seconds` in .sendra/config.yaml if the server is just slow")
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use sendra_core::{Method, Request};

    #[test]
    fn the_request_file_hint_teaches_a_file_sendra_can_actually_read() {
        // The hint inlines a worked example precisely because there is no file
        // to point at, which moves the risk: nothing else in the build reads
        // this YAML, so a typo in it would ship as advice that does not load.
        // Pull the indented lines back out and put them through the real
        // parser, the same one `sendra run` would.
        let example: String = REQUEST_FILE_HINT
            .lines()
            .filter_map(|line| line.strip_prefix("  "))
            .map(|line| format!("{line}\n"))
            .collect();
        assert!(
            !example.is_empty(),
            "the hint must carry a worked example, not just prose"
        );

        let request = Request::from_yaml_str(&example)
            .expect("the example printed in the hint must parse as a request");
        assert_eq!(request.method, Method::Get);
        assert_eq!(request.url, "https://api.example.com/users/1");
    }

    #[test]
    fn the_request_file_hint_renders_as_one_indented_block() {
        // What the reader actually sees. Worth pinning because a worked
        // example is only useful if it arrives laid out — continuation lines
        // starting back at column zero would read as separate output rather
        // than as part of the hint.
        assert_eq!(
            render_hint("hint:", REQUEST_FILE_HINT),
            "  hint: check the path. A request file is YAML, and needs only two keys:\n\
             \n\
             \x20         method: GET\n\
             \x20         url: https://api.example.com/users/1"
        );
    }

    #[test]
    fn a_single_line_hint_is_unchanged_by_the_multi_line_layout() {
        // The other two hints are one-liners and must stay exactly as they
        // were before hints learned to wrap.
        assert_eq!(
            render_hint("hint:", "create that file, or omit --env"),
            "  hint: create that file, or omit --env"
        );
    }

    #[test]
    fn no_hint_sends_the_reader_to_a_file_that_ships_only_with_the_source() {
        // The bug this replaced: a hint naming `examples/get-request.yaml`,
        // which is in the repository and in no released binary. Anything a
        // hint names has to be either something Sendra resolves at runtime and
        // has just printed, or something the reader creates themselves.
        assert!(
            !REQUEST_FILE_HINT.contains("examples/"),
            "the request-file hint must stand on its own: {REQUEST_FILE_HINT}"
        );
    }
}
