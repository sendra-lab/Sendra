//! Sendra's command-line front-end.
//!
//! Everything here is presentation: argument parsing, terminal output and exit
//! codes. The request model and HTTP execution live in `sendra-core`.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use owo_colors::{OwoColorize, Stream};
use sendra_core::environment::{environment_path, find_environment, DEFAULT_ENVIRONMENT_NAME};
use sendra_core::{Config, Document, Environment, Request, Response, SendraError};

/// Sendra's exit-code convention, in one place.
///
/// ```text
/// 0  ok          every request sent, and no response was an error status (or
///                the user opted out of status-based failure with
///                --allow-error-status)
/// 1  failure     some request never got a response: file missing or malformed,
///                no such request name, `--env` naming an environment with no
///                file behind it, a `{{variable}}` or `${VAR}` with no value,
///                invalid header, DNS/TLS/connection failure
/// 2  (reserved)  bad command-line usage — clap exits with this itself
/// 3  status      every request got a response, but at least one was 4xx/5xx
/// ```
///
/// A collection run sends many requests under one exit code, so these are
/// aggregates; [`worst`] is where they combine.
///
/// Codes 4 and up are deliberately free: `sendra test` will need its own
/// outcome (assertions failed) that is neither "could not send" nor "one bad
/// status". Every exit path in the binary returns one of these variants rather
/// than calling `std::process::exit` inline, so adding a code later means
/// adding a row here and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    Ok = 0,
    Failure = 1,
    // 2 belongs to clap; see the table above.
    ErrorStatus = 3,
}

impl From<Exit> for ExitCode {
    fn from(exit: Exit) -> Self {
        ExitCode::from(exit as u8)
    }
}

/// Decide the exit code for a response that came back.
///
/// 1xx/2xx/3xx are "the server answered and did not object"; 4xx and 5xx are
/// failures unless the caller opted out. Anything at or above 400 counts,
/// including non-standard 6xx codes — a status we do not recognise is not a
/// status we should report as success.
///
/// Takes a bare `u16` rather than a `&Response` so the decision stays pure and
/// testable without constructing a response or touching the network.
fn exit_for_status(status: u16, allow_error_status: bool) -> Exit {
    if allow_error_status || status < 400 {
        Exit::Ok
    } else {
        Exit::ErrorStatus
    }
}

/// Fold the outcomes of several requests into the single code the process can
/// return. The worst outcome wins, ranked `Ok` < `ErrorStatus` < `Failure`.
///
/// "Worst wins" rather than "the last request wins": the exit code should
/// answer "did anything go wrong?", and tying it to the last request would make
/// it depend on the order the file happens to list requests in, so reordering a
/// collection could change whether a script proceeds. This keeps
/// `sendra run collection.yaml && deploy.sh` meaning for a collection what it
/// means for a single request — exit 0 is a promise that nothing in the run
/// failed.
///
/// `Failure` outranks `ErrorStatus` because "never got a response" is the
/// bigger problem: a 404 is an answer, a DNS failure is not. A run reports the
/// most serious thing that happened, not the most recent.
fn worst(a: Exit, b: Exit) -> Exit {
    // Rank by severity, not by the exit numbers: 3 (ErrorStatus) is the milder
    // outcome of the two failures, so the numeric order is the wrong order.
    fn severity(exit: Exit) -> u8 {
        match exit {
            Exit::Ok => 0,
            Exit::ErrorStatus => 1,
            Exit::Failure => 2,
        }
    }

    if severity(b) > severity(a) {
        b
    } else {
        a
    }
}

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
            allow_error_status,
        } => run(
            &path,
            request.as_deref(),
            env.as_deref(),
            allow_error_status,
        )
        .await
        .into(),
    }
}

/// Load `path` and send either the one request named, or all of them.
///
/// Returns an exit code rather than a `Result` because a collection run can
/// half-succeed: one request failing does not stop the rest, so there is no
/// single error to propagate. Every outcome is printed as it happens and folded
/// into the code with [`worst`].
///
/// The order of the two passes over a request is fixed and matters:
/// **environment substitution first, then config**. Substitution belongs to the
/// request file — `{{base_url}}` is something its author wrote — while config
/// headers are tool-wide defaults that know nothing about which environment is
/// active. Running substitution first also means [`Config::apply`] compares
/// header names against the names that will actually be sent: a request header
/// written as `{{prefix}}-Auth` would otherwise never be recognised as the same
/// header as a config `X-Auth`, and both would go out.
///
/// The consequence, stated plainly: **config headers are not templated.** A
/// `{{var}}` in `.sendra/config.yaml` is sent verbatim. That is the honest
/// reading of the ordering — config is applied after substitution has finished —
/// and it is the conservative one, since a config is resolved without reference
/// to any environment and applies to every request in every project directory
/// beneath it. Templating config is a decision to make on its own, not a side
/// effect of this one.
///
/// Config and the environment are both resolved here, once, before anything is
/// read or sent. Those two failures *do* stop the run, and belong in a different
/// category from anything a request can do: a config or environment file that
/// does not parse is not "this request failed", it is "the settings this whole
/// run was going to use are unreadable", and sending some requests under
/// half-applied defaults would be worse than sending none. `--env` naming an
/// environment that does not exist joins them, for the reason given on
/// [`environment_for`].
async fn run(
    path: &Path,
    name: Option<&str>,
    environment_name: Option<&str>,
    allow_error_status: bool,
) -> Exit {
    // Resolved once for the whole run, before anything is read or sent: every
    // request in a collection is sent under the same defaults, and a broken
    // config file stops the run instead of failing partway through it.
    let config = match Config::resolve() {
        Ok(config) => config,
        Err(err) => {
            print_error(&err);
            return Exit::Failure;
        }
    };

    // The walk-up looking for the environment starts here rather than inside
    // `Environment::resolve`, because a `--env` that finds nothing has to be
    // able to say *where* it looked. `CurrentDir` is the same error core would
    // have raised for the same reason.
    let start_dir = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(err) => {
            print_error(&SendraError::CurrentDir(err));
            return Exit::Failure;
        }
    };

    let environment = match environment_for(&start_dir, environment_name) {
        Ok(environment) => environment,
        Err(err) => {
            print_environment_error(&err);
            return Exit::Failure;
        }
    };

    let document = match Document::from_path(path) {
        Ok(document) => document,
        Err(err) => {
            print_error(&err);
            return Exit::Failure;
        }
    };

    let requests: Vec<&Request> = match name {
        Some(name) => match document.get(name) {
            Ok(request) => vec![request],
            Err(err) => {
                print_error(&err);
                return Exit::Failure;
            }
        },
        // No name: a single-request file yields its one request, a collection
        // yields all of them, in file order.
        None => document.requests().iter().collect(),
    };

    let config = &config;
    run_requests(&requests, &environment, |request| async move {
        send(&request, config, allow_error_status).await
    })
    .await
}

/// Why the run could not get the environment it was going to send against.
///
/// A CLI-local type rather than a new [`SendraError`] variant, because
/// `NotFound` is not a fact about a file — it is a fact about the *command
/// line*, and `sendra-core` never sees the command line. Core's job is
/// "environment `x` resolved to this file, or to nothing"; deciding that
/// "nothing" is fatal because the user typed the name themselves is a
/// front-end decision, and a `sendra-tui` that offers a picker instead of a
/// flag would never raise it.
#[derive(Debug)]
enum EnvironmentError {
    /// `--env <name>` was given and no `.sendra/environments/<name>.yaml`
    /// exists anywhere up the tree from where sendra was run.
    NotFound {
        name: String,
        searched_from: PathBuf,
    },

    /// An environment file was found but could not be read or parsed. Core's
    /// error, passed through unchanged.
    Unreadable(SendraError),
}

/// Load the environment this run substitutes from: the one `--env` named, or
/// `default` when the flag was omitted.
///
/// Two decisions live here, and they are deliberately *not* the same decision.
///
/// **Omitting `--env` keeps the pre-flag behaviour.** The name falls back to
/// [`DEFAULT_ENVIRONMENT_NAME`], and a project with no such file gets the empty
/// environment rather than an error. That is the rule environments shipped
/// with, and it has to stay: a request file with no `{{...}}` in it does not
/// need an environment, and most projects have no `.sendra/` at all. Making the
/// flag mandatory — or mandatory-when-a-`{{var}}`-appears — would either break
/// every existing invocation or make whether a flag is required depend on the
/// contents of a file the user has not opened.
///
/// **Naming an environment that does not exist is an error.** This is the one
/// place this function departs from "a missing environment file is the empty
/// environment", and the difference is not the file, it is the sentence the
/// user typed. Omitting `--env` asks for a default; `--env staging` asserts
/// that `staging` exists. Sendra already answers a failed assertion of exactly
/// this shape with an error and not a shrug: `sendra run collection.yaml Nope`
/// is [`RequestNotFound`](SendraError::RequestNotFound), while omitting the
/// name runs everything. Same pattern, same answer.
///
/// The alternative — treating `--env stagng` as the empty environment — fails
/// in the two ways that matter. If the request has `{{base_url}}` in it, the
/// error names the *variable*, sending the reader to look for a typo in their
/// request file when the typo is on their command line. If the request has no
/// variables at all, there is no error: the run succeeds, exit 0, having
/// ignored the flag entirely. A flag that can be silently ignored is worse than
/// one that is occasionally strict, and "you asked to run against staging and I
/// did not run against staging" should never be something the user has to
/// notice for themselves.
///
/// Takes `start_dir` rather than reading the working directory, so the search
/// is testable against a temporary tree — the same arrangement `Config` and
/// `Environment` use in core.
fn environment_for(
    start_dir: &Path,
    requested: Option<&str>,
) -> Result<Environment, EnvironmentError> {
    match requested {
        Some(name) => match find_environment(start_dir, name) {
            Some(path) => Environment::from_path(path).map_err(EnvironmentError::Unreadable),
            None => Err(EnvironmentError::NotFound {
                name: name.to_string(),
                searched_from: start_dir.to_path_buf(),
            }),
        },
        // No flag: core's rule, unchanged — nearest `default.yaml` wins, and no
        // file at all is the empty environment.
        None => Environment::resolve_from(start_dir, DEFAULT_ENVIRONMENT_NAME)
            .map_err(EnvironmentError::Unreadable),
    }
}

/// Substitute and send each of `requests` in file order, printing every outcome
/// and folding them into the one code the process returns.
///
/// **Substitution happens here, per request, not as a pass over the batch
/// first.** A `{{var}}` with nothing behind it, or a `${VAR}` that is not
/// exported, is exactly the same category of problem as a refused connection:
/// *this* request could not be completed. Issue 2 settled what a run does with
/// that — the sibling requests are still sent, every result is still printed,
/// and [`worst`] decides the exit code — and there is no reason a variable
/// should be the one failure that also cancels the requests around it. Checking
/// the whole collection up front would additionally mean the file's *last*
/// request could stop the first one from ever being sent, which is the kind of
/// order-dependence [`worst`] exists to keep out of the exit code.
///
/// A substitution failure is `Failure`, not `ErrorStatus`, so
/// `--allow-error-status` does not suppress it: that flag suppresses a *status*,
/// and a request that was never built has no status to forgive. It sits in the
/// same tier as a DNS or connection failure, which the flag does not suppress
/// either.
///
/// `send_one` is a parameter rather than a direct call to [`send`] so that this
/// loop — which is the whole of "one request failing does not stop the rest" —
/// can be tested without a network, the way config resolution takes its
/// directories as arguments instead of reading the real ones.
async fn run_requests<S, F>(
    requests: &[&Request],
    environment: &Environment,
    mut send_one: S,
) -> Exit
where
    S: FnMut(Request) -> F,
    F: std::future::Future<Output = Exit>,
{
    let mut exit = Exit::Ok;

    for (index, request) in requests.iter().enumerate() {
        // Blank line between results so a multi-request run stays readable.
        if index > 0 {
            println!();
        }

        let substituted = environment.apply(request);

        // Announced before the outcome either way, because in a collection run
        // the label is the only thing that says *which* request this is — a
        // "no variable named X" message names the variable, not the request.
        // On success the label describes the request as it will actually be
        // sent (a resolved URL, for a request with no `name`); on failure there
        // is no such request, so it falls back to the label as written.
        eprintln!(
            "{} {}",
            "→".if_supports_color(Stream::Stderr, |t| t.dimmed()),
            substituted
                .as_ref()
                .unwrap_or(request)
                .label()
                .if_supports_color(Stream::Stderr, |t| t.bold())
        );

        let outcome = match substituted {
            Ok(request) => send_one(request).await,
            Err(err) => {
                print_error(&err);
                Exit::Failure
            }
        };

        exit = worst(exit, outcome);
    }

    exit
}

/// Send one request, print whatever came back, and report its outcome.
///
/// A failure is printed and returned rather than propagated: in a collection
/// run the requests after this one still deserve to be sent, and the user still
/// deserves to see them.
///
/// The request arrives already substituted, and the `→` label has already been
/// printed by [`run_requests`].
async fn send(request: &Request, config: &Config, allow_error_status: bool) -> Exit {
    match sendra_core::send(request, config).await {
        Ok(response) => {
            print_response(&response);
            exit_for_status(response.status, allow_error_status)
        }
        Err(err) => {
            print_error(&err);
            Exit::Failure
        }
    }
}

fn print_response(response: &Response) {
    let status_line = format!("{} {}", response.status, response.status_text);
    let status_line = status_line.trim_end().to_string();

    // Green for 2xx, red otherwise — a 404 is a completed run but a failed
    // request, and the colour is what says so at a glance.
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

/// The red `error:` line every failure starts with.
fn print_error_line(message: impl std::fmt::Display) {
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

fn print_environment_error(err: &EnvironmentError) {
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

fn print_error(err: &SendraError) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_and_redirect_statuses_exit_zero() {
        for status in [100, 200, 201, 204, 301, 302, 304, 399] {
            assert_eq!(
                exit_for_status(status, false),
                Exit::Ok,
                "{status} should not fail the run"
            );
        }
    }

    #[test]
    fn client_and_server_error_statuses_exit_non_zero() {
        for status in [400, 401, 404, 418, 500, 503] {
            assert_eq!(
                exit_for_status(status, false),
                Exit::ErrorStatus,
                "{status} should fail the run"
            );
        }
    }

    #[test]
    fn allow_error_status_forces_zero_for_every_status() {
        for status in [200, 301, 404, 500] {
            assert_eq!(
                exit_for_status(status, true),
                Exit::Ok,
                "--allow-error-status should keep {status} at exit 0"
            );
        }
    }

    /// The exit code a collection run produces: fold each request's outcome in,
    /// in order, the way `run` does.
    fn aggregate(outcomes: impl IntoIterator<Item = Exit>) -> Exit {
        outcomes.into_iter().fold(Exit::Ok, worst)
    }

    #[test]
    fn an_all_ok_collection_run_exits_zero() {
        assert_eq!(aggregate([Exit::Ok, Exit::Ok, Exit::Ok]), Exit::Ok);
    }

    #[test]
    fn one_error_status_anywhere_fails_the_whole_collection_run() {
        // Whether the 4xx is first, last or in the middle must not matter.
        for outcomes in [
            [Exit::ErrorStatus, Exit::Ok, Exit::Ok],
            [Exit::Ok, Exit::ErrorStatus, Exit::Ok],
            [Exit::Ok, Exit::Ok, Exit::ErrorStatus],
        ] {
            assert_eq!(
                aggregate(outcomes),
                Exit::ErrorStatus,
                "a 4xx/5xx anywhere in {outcomes:?} should fail the run"
            );
        }
    }

    #[test]
    fn a_mixed_status_collection_reports_the_error_status_not_the_last_one() {
        // examples/mixed-status-collection.yaml: 200, then 404, then 500.
        let outcomes = [200, 404, 500].map(|status| exit_for_status(status, false));
        assert_eq!(aggregate(outcomes), Exit::ErrorStatus);

        // A 200 last would be just as much of a failed run.
        let outcomes = [404, 500, 200].map(|status| exit_for_status(status, false));
        assert_eq!(aggregate(outcomes), Exit::ErrorStatus);
    }

    #[test]
    fn allow_error_status_keeps_a_mixed_collection_at_zero() {
        let outcomes = [200, 404, 500].map(|status| exit_for_status(status, true));
        assert_eq!(aggregate(outcomes), Exit::Ok);
    }

    #[test]
    fn a_request_that_never_got_a_response_outranks_a_bad_status() {
        // "could not send" is the more serious outcome, whichever order the two
        // happen in, because a status at least means the server answered.
        assert_eq!(aggregate([Exit::ErrorStatus, Exit::Failure]), Exit::Failure);
        assert_eq!(aggregate([Exit::Failure, Exit::ErrorStatus]), Exit::Failure);
    }

    #[test]
    fn worst_is_order_independent() {
        // Every pair, both ways round: aggregation must not depend on file
        // order, which is the whole point of not using "last request wins".
        for a in [Exit::Ok, Exit::ErrorStatus, Exit::Failure] {
            for b in [Exit::Ok, Exit::ErrorStatus, Exit::Failure] {
                assert_eq!(
                    worst(a, b),
                    worst(b, a),
                    "worst({a:?}, {b:?}) is asymmetric"
                );
            }
        }
    }

    /// Three requests, the middle one referencing a variable that does not
    /// exist. The broken request is in the middle so "the run carried on" and
    /// "the run stopped" cannot look the same.
    const COLLECTION_WITH_A_BROKEN_VARIABLE: &str = "\
requests:
  - name: First
    method: GET
    url: '{{base_url}}/first'
  - name: Broken
    method: GET
    url: '{{nope}}/broken'
  - name: Third
    method: GET
    url: '{{base_url}}/third'
";

    /// An environment defining `base_url` and nothing else, so `{{nope}}` above
    /// has nothing behind it.
    fn environment() -> Environment {
        Environment::from_yaml_str("base_url: https://example.com\n").unwrap()
    }

    #[tokio::test]
    async fn a_broken_variable_does_not_stop_the_requests_after_it() {
        let document = Document::from_yaml_str(COLLECTION_WITH_A_BROKEN_VARIABLE).unwrap();
        let requests: Vec<&Request> = document.requests().iter().collect();

        // Stands in for the network: records what it was handed and reports a
        // clean response, so the substitution is the only failure in the run.
        let mut sent = Vec::new();
        let exit = run_requests(&requests, &environment(), |request| {
            sent.push(request.url.clone());
            async { Exit::Ok }
        })
        .await;

        // The requests either side of the broken one were still sent, with
        // their variables resolved — a request that cannot be built is that
        // request's problem, not the run's.
        assert_eq!(
            sent,
            vec!["https://example.com/first", "https://example.com/third"]
        );
        // And the broken one was not sent at all: no half-substituted URL goes
        // over the wire.
        assert_eq!(sent.len(), 2, "the broken request must not have been sent");
        // Worst-wins, so the run reports the failure rather than the two
        // successes around it.
        assert_eq!(exit, Exit::Failure);
        assert_ne!(exit as u8, 0, "the run must not exit 0");
    }

    #[tokio::test]
    async fn allow_error_status_does_not_suppress_a_substitution_failure() {
        let document = Document::from_yaml_str(COLLECTION_WITH_A_BROKEN_VARIABLE).unwrap();
        let requests: Vec<&Request> = document.requests().iter().collect();

        // The flag makes even a 404 a non-failure...
        assert_eq!(exit_for_status(404, true), Exit::Ok);

        // ...but it suppresses a *status*, and a request that was never built
        // has no status to forgive. Same treatment as a connection failure,
        // which the flag does not suppress either.
        let exit = run_requests(&requests, &environment(), |_| async {
            exit_for_status(404, true)
        })
        .await;

        assert_eq!(exit, Exit::Failure);
    }

    #[tokio::test]
    async fn a_substitution_failure_outranks_a_bad_status_from_a_sibling() {
        let document = Document::from_yaml_str(COLLECTION_WITH_A_BROKEN_VARIABLE).unwrap();
        let requests: Vec<&Request> = document.requests().iter().collect();

        // Every sibling answers 500, so the run holds both kinds of failure at
        // once. "Never got a response" is the more serious of the two.
        let exit = run_requests(&requests, &environment(), |_| async {
            exit_for_status(500, false)
        })
        .await;

        assert_eq!(exit, Exit::Failure);
    }

    #[tokio::test]
    async fn selecting_one_request_by_name_is_unaffected_by_a_broken_sibling() {
        // The named path was already scoped to the one request selected, and
        // stays that way: `Broken` needing a variable nothing defines has no
        // bearing on running `Third`.
        let document = Document::from_yaml_str(COLLECTION_WITH_A_BROKEN_VARIABLE).unwrap();
        let request = document.get("Third").expect("`Third` is in the collection");

        let mut sent = Vec::new();
        let exit = run_requests(&[request], &environment(), |request| {
            sent.push(request.url.clone());
            async { Exit::Ok }
        })
        .await;

        assert_eq!(sent, vec!["https://example.com/third"]);
        assert_eq!(exit, Exit::Ok);
    }

    #[tokio::test]
    async fn selecting_the_broken_request_by_name_fails_on_its_own() {
        let document = Document::from_yaml_str(COLLECTION_WITH_A_BROKEN_VARIABLE).unwrap();
        let request = document
            .get("Broken")
            .expect("`Broken` is in the collection");

        let mut sent = Vec::new();
        let exit = run_requests(&[request], &environment(), |request| {
            sent.push(request.url.clone());
            async { Exit::Ok }
        })
        .await;

        assert!(sent.is_empty(), "nothing should have been sent");
        assert_eq!(exit, Exit::Failure);
    }

    #[tokio::test]
    async fn a_run_with_nothing_broken_still_exits_zero() {
        // The no-op case: substitution moving into the loop must not change
        // what a perfectly ordinary collection run does.
        let yaml = "\
requests:
  - name: First
    method: GET
    url: '{{base_url}}/first'
  - name: Second
    method: GET
    url: '{{base_url}}/second'
";
        let document = Document::from_yaml_str(yaml).unwrap();
        let requests: Vec<&Request> = document.requests().iter().collect();

        let mut sent = Vec::new();
        let exit = run_requests(&requests, &environment(), |request| {
            sent.push(request.url.clone());
            async { Exit::Ok }
        })
        .await;

        assert_eq!(
            sent,
            vec!["https://example.com/first", "https://example.com/second"]
        );
        assert_eq!(exit, Exit::Ok);
    }

    // --- which environment `--env` selects -------------------------------
    //
    // Built against real directory trees rather than by mocking the lookup:
    // the walk-up is the behaviour under test, and `environment_for` takes its
    // starting directory precisely so these can run without touching the
    // process's working directory.

    /// Write `.sendra/environments/<name>.yaml` under `root`.
    fn write_environment(root: &Path, name: &str, body: &str) {
        let path = environment_path(root, name);
        std::fs::create_dir_all(path.parent().expect("has a parent")).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn omitting_env_loads_the_environment_named_default() {
        let project = tempfile::tempdir().unwrap();
        write_environment(
            project.path(),
            "default",
            "base_url: https://default.example\n",
        );
        write_environment(
            project.path(),
            "staging",
            "base_url: https://staging.example\n",
        );

        let environment = environment_for(project.path(), None).expect("default.yaml is there");

        assert_eq!(
            environment.variables.get("base_url").map(String::as_str),
            Some("https://default.example"),
            "no --env must keep loading `default`, as it did before the flag"
        );
    }

    #[test]
    fn omitting_env_with_no_default_file_is_the_empty_environment_not_an_error() {
        // A project with no `.sendra/` at all: the overwhelmingly common case,
        // and the reason omitting the flag can never be an error.
        let project = tempfile::tempdir().unwrap();

        let environment = environment_for(project.path(), None)
            .unwrap_or_else(|_| panic!("a project with no environments must still run"));

        assert!(environment.is_empty());
    }

    #[test]
    fn naming_an_environment_loads_that_one_and_not_another() {
        let project = tempfile::tempdir().unwrap();
        write_environment(
            project.path(),
            "default",
            "base_url: https://default.example\n",
        );
        write_environment(
            project.path(),
            "staging",
            "base_url: https://staging.example\n",
        );
        write_environment(project.path(), "prod", "base_url: https://prod.example\n");

        for (name, expected) in [
            ("staging", "https://staging.example"),
            ("prod", "https://prod.example"),
        ] {
            let environment =
                environment_for(project.path(), Some(name)).expect("the file is there");
            assert_eq!(
                environment.variables.get("base_url").map(String::as_str),
                Some(expected),
                "--env {name} loaded the wrong file"
            );
        }
    }

    #[test]
    fn a_named_environment_is_found_by_walking_up_from_a_subdirectory() {
        // Same rule as config and as `default`: an environment at the
        // repository root applies from anywhere inside the repository.
        let project = tempfile::tempdir().unwrap();
        write_environment(
            project.path(),
            "staging",
            "base_url: https://staging.example\n",
        );

        let nested = project.path().join("crates").join("api").join("tests");
        std::fs::create_dir_all(&nested).unwrap();

        let environment = environment_for(&nested, Some("staging")).expect("found up the tree");

        assert_eq!(
            environment.variables.get("base_url").map(String::as_str),
            Some("https://staging.example")
        );
    }

    #[test]
    fn naming_an_environment_that_does_not_exist_is_an_error() {
        // The decision this issue turns on: an explicit name is an assertion
        // that the environment exists, so a typo fails loudly instead of
        // silently running against no variables at all. See `environment_for`.
        let project = tempfile::tempdir().unwrap();
        write_environment(
            project.path(),
            "staging",
            "base_url: https://staging.example\n",
        );

        let Err(err) = environment_for(project.path(), Some("stagng")) else {
            panic!("a mistyped --env must not be silently ignored");
        };

        match err {
            EnvironmentError::NotFound { name, .. } => assert_eq!(name, "stagng"),
            EnvironmentError::Unreadable(err) => panic!("wrong error: {err}"),
        }
    }

    #[test]
    fn naming_default_explicitly_errors_where_omitting_env_would_not() {
        // The asymmetry, pinned: same missing file, two different answers,
        // because the difference is what the user asked for and not what is on
        // disk. If this ever collapses into one behaviour it should be because
        // someone changed it on purpose.
        let project = tempfile::tempdir().unwrap();

        assert!(
            environment_for(project.path(), None).is_ok(),
            "omitting --env falls back to the empty environment"
        );
        assert!(
            environment_for(project.path(), Some("default")).is_err(),
            "`--env default` names a file that is not there"
        );
    }

    #[test]
    fn a_named_environment_that_does_not_parse_is_still_a_core_error() {
        // Finding the file and failing to read it is core's error, not the
        // flag's, and must not be flattened into "no such environment".
        let project = tempfile::tempdir().unwrap();
        write_environment(project.path(), "staging", "base_url: [not, a, string]\n");

        let Err(err) = environment_for(project.path(), Some("staging")) else {
            panic!("a malformed environment file is an error");
        };

        assert!(
            matches!(
                err,
                EnvironmentError::Unreadable(SendraError::EnvParse { .. })
            ),
            "a malformed file must keep its own error"
        );
    }

    #[tokio::test]
    async fn staging_and_prod_put_different_urls_on_the_wire() {
        // The acceptance criterion, end to end minus the socket: one request
        // file, two `--env` values, two different resolved URLs.
        let project = tempfile::tempdir().unwrap();
        write_environment(
            project.path(),
            "staging",
            "base_url: https://staging.example\n",
        );
        write_environment(project.path(), "prod", "base_url: https://prod.example\n");

        let document =
            Document::from_yaml_str("name: Health\nmethod: GET\nurl: '{{base_url}}/health'\n")
                .unwrap();
        let requests: Vec<&Request> = document.requests().iter().collect();

        let mut sent = Vec::new();
        for name in ["staging", "prod"] {
            let environment = environment_for(project.path(), Some(name)).expect("both exist");
            let exit = run_requests(&requests, &environment, |request| {
                sent.push(request.url.clone());
                async { Exit::Ok }
            })
            .await;
            assert_eq!(exit, Exit::Ok);
        }

        assert_eq!(
            sent,
            vec![
                "https://staging.example/health",
                "https://prod.example/health"
            ]
        );
    }

    #[test]
    fn exit_codes_match_the_documented_convention() {
        // The numbers are the contract with anyone scripting sendra, so pin
        // them here rather than only asserting on the variants.
        assert_eq!(Exit::Ok as u8, 0);
        assert_eq!(Exit::Failure as u8, 1);
        assert_eq!(Exit::ErrorStatus as u8, 3);
    }
}
