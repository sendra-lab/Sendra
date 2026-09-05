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
                allow_error_status,
            } => {
                assert_eq!(path, PathBuf::from("collection.yaml"));
                assert_eq!(env.as_deref(), Some("staging"));
                assert!(!allow_error_status);
            }
            _ => panic!("`sendra test` should have parsed as `Command::Test`"),
        }

        // A second positional is `run`'s, not `test`'s: a verdict over one
        // hand-picked request is a different thing, and is not offered rather
        // than being offered and ignored.
        assert!(
            Cli::try_parse_from(["sendra", "test", "collection.yaml", "One request"]).is_err(),
            "`test` takes no request name"
        );
    }
}
