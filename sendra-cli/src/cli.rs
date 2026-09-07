//! The command line as clap sees it: the subcommands, their arguments, and the
//! `--help` text those arguments carry.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Parse one `-H`/`--header` value: `Name: value`.
///
/// A clap `value_parser` rather than downstream validation, so a malformed
/// value (`-H "no-colon-here"`) is refused where every other bad argument
/// is — with clap's usual message and exit code 2 — instead of surfacing
/// later as a confusing substitution or request-building failure.
///
/// Both sides are trimmed: the natural way to type this is `"Name: value"`,
/// with the space after the colon that HTTP header folding has always
/// treated as insignificant, and trimming it here means that space never
/// becomes part of the value by accident.
fn parse_header_override(raw: &str) -> Result<(String, String), String> {
    let (name, value) = raw
        .split_once(':')
        .ok_or_else(|| format!("expected `Name: value`, got `{raw}` (no `:` found)"))?;
    let name = name.trim();
    if name.is_empty() {
        return Err(format!("header name is empty in `{raw}`"));
    }
    Ok((name.to_string(), value.trim().to_string()))
}

/// Parse one `--var` value: `name=value`.
///
/// Same reasoning as [`parse_header_override`]: a clap-level parser turns a
/// missing `=` into a clear, immediate CLI error rather than a variable that
/// silently never gets set.
fn parse_var_override(raw: &str) -> Result<(String, String), String> {
    let (name, value) = raw
        .split_once('=')
        .ok_or_else(|| format!("expected `name=value`, got `{raw}` (no `=` found)"))?;
    if name.is_empty() {
        return Err(format!("variable name is empty in `{raw}`"));
    }
    Ok((name.to_string(), value.to_string()))
}

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

        /// Add or override a header for this invocation only, `Name: value`.
        ///
        /// Repeatable. Wins over everything else a header can come from —
        /// config's default headers, the request file's own `headers:`, and
        /// the `Authorization` header an `auth:` block resolves to — because
        /// it is the most specific, most deliberate override available: typed
        /// for this one run, not left in a file for every run after it.
        ///
        /// Passing `-H` more than once for the *same* name (case-insensitive)
        /// replaces the earlier value rather than adding a repeat. That
        /// differs from the request file's own `headers:`, which does allow a
        /// name to repeat — but a name typed twice on one command line reads
        /// as "I meant to change it", the way retyping a shell variable
        /// reassigns it rather than appending to it, not as a deliberate
        /// multi-value header.
        ///
        /// See the README's "Precedence, start to finish" section for where
        /// this sits among every other layer.
        #[arg(short = 'H', long = "header", value_name = "NAME:VALUE", value_parser = parse_header_override)]
        header: Vec<(String, String)>,

        /// Set a substitution variable for this invocation only, `name=value`.
        ///
        /// Repeatable, and usable with or without `--env`. Overrides a value
        /// the active environment file defines for the same name, for the
        /// same "most specific wins" reason `-H` overrides config and request
        /// headers. It is treated as if it were part of the environment file
        /// itself rather than as a separate, higher layer: in particular, a
        /// `capture` block naming the same variable a `--var` already set is
        /// refused exactly as it would be for a name the file defines — see
        /// the README for why letting a capture silently win, or silently
        /// lose, would both be worse than an error.
        #[arg(long = "var", value_name = "NAME=VALUE", value_parser = parse_var_override)]
        var: Vec<(String, String)>,

        /// Override the resolved timeout, in seconds, for this invocation
        /// only.
        ///
        /// Applies to the whole request — connect, send and body read — the
        /// same span `timeout_seconds` in a config file covers. Wins over
        /// both the global and the project config, for the same reason every
        /// other override here does.
        #[arg(long, value_name = "SECONDS")]
        timeout: Option<u64>,

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

        /// Add or override a header for this invocation only, `Name: value`.
        /// Behaves exactly as it does on `run` — see `run --help` for the
        /// full reasoning and the repeated-`-H` rule.
        #[arg(short = 'H', long = "header", value_name = "NAME:VALUE", value_parser = parse_header_override)]
        header: Vec<(String, String)>,

        /// Set a substitution variable for this invocation only, `name=value`.
        /// Behaves exactly as it does on `run` — see `run --help`.
        #[arg(long = "var", value_name = "NAME=VALUE", value_parser = parse_var_override)]
        var: Vec<(String, String)>,

        /// Override the resolved timeout, in seconds, for this invocation
        /// only. Behaves exactly as it does on `run` — see `run --help`.
        #[arg(long, value_name = "SECONDS")]
        timeout: Option<u64>,

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

    /// Scaffold `.sendra/config.yaml` and `.sendra/environments/default.yaml`
    /// in the current directory.
    ///
    /// Both files are written with every known field present but commented
    /// out, showing the shape a config and an environment file can take
    /// without imposing any values of its own. Refuses, without writing
    /// anything, if `.sendra/` already exists — see `sendra init --help` in
    /// the README for the reasoning.
    Init,
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
                header,
                var,
                timeout,
                json,
                show_captures,
                allow_error_status,
            } => {
                assert_eq!(path, PathBuf::from("collection.yaml"));
                assert_eq!(env.as_deref(), Some("staging"));
                assert!(header.is_empty(), "no -H was passed");
                assert!(var.is_empty(), "no --var was passed");
                assert_eq!(timeout, None, "no --timeout was passed");
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

    // --- `-H`/`--header` ----------------------------------------------------

    #[test]
    fn header_is_repeatable_and_offered_by_both_subcommands() {
        let cli = Cli::try_parse_from([
            "sendra",
            "run",
            "req.yaml",
            "-H",
            "X-Trace-Id: abc",
            "--header",
            "Accept: application/json",
        ])
        .expect("`-H`/`--header` are the same flag, and repeat");

        match cli.command {
            Command::Run { header, .. } => assert_eq!(
                header,
                vec![
                    ("X-Trace-Id".to_string(), "abc".to_string()),
                    ("Accept".to_string(), "application/json".to_string()),
                ],
                "both occurrences must reach `main`, in order"
            ),
            _ => panic!("`sendra run` should have parsed as `Command::Run`"),
        }

        let cli = Cli::try_parse_from(["sendra", "test", "req.yaml", "-H", "X-Trace-Id: abc"])
            .expect("`-H` is offered by `test` too");
        assert!(matches!(
            cli.command,
            Command::Test { header, .. } if header == vec![("X-Trace-Id".to_string(), "abc".to_string())]
        ));
    }

    #[test]
    fn header_trims_the_space_after_the_colon_but_not_the_name() {
        let cli = Cli::try_parse_from(["sendra", "run", "req.yaml", "-H", "X-Trace-Id:   abc  "])
            .expect("a trailing/leading space around the value is not an error");
        match cli.command {
            Command::Run { header, .. } => {
                assert_eq!(header, vec![("X-Trace-Id".to_string(), "abc".to_string())]);
            }
            _ => panic!("`sendra run` should have parsed as `Command::Run`"),
        }
    }

    /// `Cli` derives no `Debug`, so `Result::expect_err`/`unwrap_err` (which
    /// both require it on the `Ok` side) cannot be used against
    /// `Cli::try_parse_from` directly; this unwraps by hand instead.
    fn expect_cli_error(args: &[&str]) -> clap::Error {
        match Cli::try_parse_from(args) {
            Ok(_) => panic!("expected a CLI error parsing {args:?}"),
            Err(err) => err,
        }
    }

    #[test]
    fn a_malformed_header_is_a_clear_cli_error() {
        let err = expect_cli_error(&["sendra", "run", "req.yaml", "-H", "no-colon-here"]);
        assert_eq!(
            err.exit_code(),
            2,
            "clap's usage-error exit code, same as any other bad argument"
        );
        assert!(
            err.to_string().contains("no `:` found"),
            "the message should say what is wrong: {err}"
        );

        let err = expect_cli_error(&["sendra", "run", "req.yaml", "-H", ": no name"]);
        assert!(err.to_string().contains("header name is empty"));
    }

    // --- `--var` -------------------------------------------------------------

    #[test]
    fn var_is_repeatable_and_offered_by_both_subcommands() {
        let cli = Cli::try_parse_from([
            "sendra",
            "run",
            "req.yaml",
            "--var",
            "base_url=https://example.com",
            "--var",
            "token=abc123",
        ])
        .expect("`--var` repeats");

        match cli.command {
            Command::Run { var, .. } => assert_eq!(
                var,
                vec![
                    ("base_url".to_string(), "https://example.com".to_string()),
                    ("token".to_string(), "abc123".to_string()),
                ]
            ),
            _ => panic!("`sendra run` should have parsed as `Command::Run`"),
        }

        let cli = Cli::try_parse_from(["sendra", "test", "req.yaml", "--var", "token=abc123"])
            .expect("`--var` is offered by `test` too");
        assert!(matches!(
            cli.command,
            Command::Test { var, .. } if var == vec![("token".to_string(), "abc123".to_string())]
        ));
    }

    #[test]
    fn a_var_value_may_contain_an_equals_sign() {
        // Only the first `=` is the separator — a value that is itself a
        // `key=value` pair (a query string, say) must survive intact.
        let cli = Cli::try_parse_from(["sendra", "run", "req.yaml", "--var", "q=a=b"])
            .expect("only the first `=` splits name from value");
        match cli.command {
            Command::Run { var, .. } => {
                assert_eq!(var, vec![("q".to_string(), "a=b".to_string())]);
            }
            _ => panic!("`sendra run` should have parsed as `Command::Run`"),
        }
    }

    #[test]
    fn a_malformed_var_is_a_clear_cli_error() {
        let err = expect_cli_error(&["sendra", "run", "req.yaml", "--var", "no-equals-here"]);
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("no `=` found"));

        let err = expect_cli_error(&["sendra", "run", "req.yaml", "--var", "=novalue"]);
        assert!(err.to_string().contains("variable name is empty"));
    }

    // --- `--timeout` -----------------------------------------------------

    #[test]
    fn timeout_is_an_optional_number_of_seconds_offered_by_both_subcommands() {
        let cli =
            Cli::try_parse_from(["sendra", "run", "req.yaml"]).expect("`--timeout` is optional");
        assert!(matches!(cli.command, Command::Run { timeout: None, .. }));

        let cli = Cli::try_parse_from(["sendra", "run", "req.yaml", "--timeout", "5"])
            .expect("`--timeout` takes a number of seconds");
        assert!(matches!(
            cli.command,
            Command::Run {
                timeout: Some(5),
                ..
            }
        ));

        let cli = Cli::try_parse_from(["sendra", "test", "req.yaml", "--timeout", "5"])
            .expect("`--timeout` is offered by `test` too");
        assert!(matches!(
            cli.command,
            Command::Test {
                timeout: Some(5),
                ..
            }
        ));

        assert!(
            Cli::try_parse_from(["sendra", "run", "req.yaml", "--timeout", "not-a-number"])
                .is_err(),
            "a non-numeric timeout is a clap-level error"
        );
    }
}
