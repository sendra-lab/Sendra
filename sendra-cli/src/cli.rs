//! The command line as clap sees it: the subcommands, their arguments, and the
//! `--help` text those arguments carry.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "sendra",
    version,
    about = "Terminal-native HTTP client — send requests defined in YAML files."
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
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

        /// Name of the environment to substitute `{{variable}}` values from.
        ///
        /// `--env staging` loads `.sendra/environments/staging.yaml`, found by
        /// walking up from the directory you are in. Omit it and the
        /// environment named `default` is loaded if there is one, or no
        /// environment at all if there is not. Naming an environment that has
        /// no file is an error: the run stops rather than quietly sending
        /// against variables you did not ask for.
        // The reasoning behind those two answers lives on `environment_for`;
        // this doc comment is what `--help` prints, so it stays user-facing.
        #[arg(long, value_name = "NAME")]
        env: Option<String>,

        /// Exit 0 even when a response status is 4xx or 5xx.
        ///
        /// Responses are printed either way; this only changes the exit code,
        /// for inspecting an error response without failing the surrounding
        /// script.
        #[arg(long)]
        allow_error_status: bool,

        /// Print one JSON object describing the whole run, instead of the
        /// human-readable output.
        ///
        /// `{"requests": [...]}`, one entry per request in file order, each
        /// carrying its response — status, headers, body, elapsed time — or the
        /// error that stopped it, and its assertion results. Nothing else goes
        /// to stdout in this mode: the `→` labels and every error message stay
        /// on stderr, so `sendra run req.yaml --json > out.json` leaves a file
        /// `jq` can read. Exit codes are exactly the same either way.
        #[arg(long)]
        json: bool,

        /// Show captured values verbatim in `--json` output instead of
        /// redacting them.
        ///
        /// A `capture` block often pulls an auth token or other sensitive
        /// value out of a response, and `--json` is the format that ends up
        /// piped into a CI log — a more structured, more attractive target
        /// than the same value sitting inside an escaped response body.
        /// `capture.values` entries are redacted by default; this flag opts
        /// back into the original behaviour. `capture.failures` is never
        /// affected: it names variables and paths, not the values captured
        /// from them. Meaningless without `--json`; accepted either way.
        #[arg(long)]
        show_captures: bool,
    },

    /// Run every request in a YAML file and pass or fail on its assertions.
    ///
    /// Sends the same requests `run` sends, under the same config and the same
    /// environment, and prints the same per-request assertion results — then a
    /// summary across the whole run, and an exit code decided by the
    /// assertions rather than by the response statuses. See `sendra help run`
    /// for the shared parts.
    Test {
        /// Path to the request or collection file.
        ///
        /// A single-request file and a collection are both accepted, and a
        /// collection runs every request in it, in file order. There is no
        /// name argument: `test`'s answer is a verdict over the whole file.
        path: PathBuf,

        /// Name of the environment to substitute `{{variable}}` values from.
        ///
        /// Behaves exactly as it does on `run`: `--env staging` loads
        /// `.sendra/environments/staging.yaml`, found by walking up from the
        /// directory you are in; omitting it loads `default` if there is one;
        /// naming an environment that has no file is an error.
        #[arg(long, value_name = "NAME")]
        env: Option<String>,

        /// Print one JSON object describing the whole run, instead of the
        /// human-readable output.
        ///
        /// The same document `sendra run --json` writes — `requests`, in file
        /// order, each with its full response and assertion results — plus a
        /// `summary` object holding the counts the terminal output ends with.
        /// Note that `requests` carries whole responses here, headers and body
        /// included, where the terminal output shows only a status line: that
        /// is a decision about what is readable on a screen, and a program
        /// reading the output has no such problem.
        #[arg(long)]
        json: bool,

        /// Show captured values verbatim in `--json` output instead of
        /// redacting them. See `run --help` for the reasoning; the same
        /// default and the same flag apply here.
        #[arg(long)]
        show_captures: bool,

        /// Accepted only so that passing it can be refused with an
        /// explanation. Hidden from `--help`, rejected in `main`.
        #[arg(long, hide = true)]
        allow_error_status: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- `--allow-error-status` has no meaning under `test` ---------------

    #[test]
    fn the_cli_definition_is_internally_consistent() {
        use clap::CommandFactory;

        // clap's own check that the two subcommands' arguments are well-formed
        // — cheap, and it catches a duplicated long name or a bad default the
        // moment it is written rather than the first time someone runs the
        // command.
        Cli::command().debug_assert();
    }

    #[test]
    fn test_accepts_allow_error_status_only_so_that_it_can_be_refused() {
        // Not defining the flag at all would also reject it, with clap's
        // generic "unexpected argument". It is defined and hidden so that the
        // refusal can say *why* it does not apply — see
        // `reject_allow_error_status`.
        let cli = Cli::try_parse_from(["sendra", "test", "req.yaml", "--allow-error-status"])
            .expect("the flag must parse, so `main` can refuse it with an explanation");

        match cli.command {
            Command::Test {
                allow_error_status, ..
            } => assert!(
                allow_error_status,
                "the flag must reach `main` to be refused"
            ),
            _ => panic!("`sendra test` should have parsed as `Command::Test`"),
        }
    }

    #[test]
    fn allow_error_status_is_advertised_by_run_and_hidden_by_test() {
        use clap::CommandFactory;

        let mut cli = Cli::command();

        let run_help = cli
            .find_subcommand_mut("run")
            .expect("`run` is a subcommand")
            .render_help()
            .to_string();
        assert!(
            run_help.contains("--allow-error-status"),
            "`run` still offers the flag"
        );

        let test_help = cli
            .find_subcommand_mut("test")
            .expect("`test` is a subcommand")
            .render_help()
            .to_string();
        assert!(
            !test_help.contains("--allow-error-status"),
            "`test` must not offer a flag it refuses: {test_help}"
        );
    }

    #[test]
    fn test_takes_a_path_and_an_env_and_no_request_name() {
        let cli = Cli::try_parse_from(["sendra", "test", "collection.yaml", "--env", "staging"])
            .expect("path and --env are the whole surface");

        match cli.command {
            Command::Test {
                path,
                env,
                json,
                show_captures,
                allow_error_status,
            } => {
                assert_eq!(path, PathBuf::from("collection.yaml"));
                assert_eq!(env.as_deref(), Some("staging"));
                assert!(!json, "the human output is what you get without --json");
                assert!(!show_captures, "captures are redacted by default");
                assert!(!allow_error_status);
            }
            _ => panic!("`sendra test` should have parsed as `Command::Test`"),
        }

        // Whether the flag was passed is all `main` needs from it; what it
        // then means is `Reporter`'s.
        let cli = Cli::try_parse_from(["sendra", "test", "collection.yaml", "--json"])
            .expect("`--json` is offered by `test` as well as by `run`");
        assert!(
            matches!(cli.command, Command::Test { json: true, .. }),
            "`--json` must reach `main`"
        );

        // A second positional is `run`'s, not `test`'s: a verdict over one
        // hand-picked request is a different thing, and is not offered rather
        // than being offered and ignored.
        assert!(
            Cli::try_parse_from(["sendra", "test", "collection.yaml", "One request"]).is_err(),
            "`test` takes no request name"
        );
    }

    // --- `--show-captures` -------------------------------------------------

    #[test]
    fn show_captures_defaults_to_false_and_is_offered_by_both_subcommands() {
        let cli =
            Cli::try_parse_from(["sendra", "run", "req.yaml"]).expect("`run` takes just a path");
        assert!(
            matches!(
                cli.command,
                Command::Run {
                    show_captures: false,
                    ..
                }
            ),
            "captures are redacted by default under `run`"
        );

        let cli = Cli::try_parse_from(["sendra", "run", "req.yaml", "--show-captures"])
            .expect("`--show-captures` is offered by `run`");
        assert!(matches!(
            cli.command,
            Command::Run {
                show_captures: true,
                ..
            }
        ));

        let cli = Cli::try_parse_from(["sendra", "test", "req.yaml", "--show-captures"])
            .expect("`--show-captures` is offered by `test`");
        assert!(matches!(
            cli.command,
            Command::Test {
                show_captures: true,
                ..
            }
        ));
    }
}
