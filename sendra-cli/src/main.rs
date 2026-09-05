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
use sendra_core::{AssertionReport, Config, Document, Environment, Request, Response, SendraError};

/// Sendra's exit-code convention, in one place, for every subcommand.
///
/// ```text
/// code  run  test  meaning
///
/// 0      ·    ·    nothing went wrong. For `run`: every request was sent and
///                  no response was an error status (or the user opted out
///                  with --allow-error-status). For `test`: every request got
///                  a response, and every assertion any of them declared,
///                  passed.
/// 1      ·    ·    some request never got a response: file missing or
///                  malformed, no such request name, `--env` naming an
///                  environment with no file behind it, a `{{variable}}` or
///                  `${VAR}` with no value, invalid header, DNS/TLS/connection
///                  failure.
/// 2      ·    ·    bad command-line usage — clap exits with this itself.
/// 3      ·         `run` only: every request got a response, but at least one
///                  was 4xx/5xx.
/// 4           ·    `test` only: every request got a response, but at least one
///                  had a failing assertion.
/// ```
///
/// A collection run sends many requests under one exit code, so these are
/// aggregates; [`worst`] is where they combine, over [`Outcome`]s produced by
/// the loop both subcommands share.
///
/// **One enum for both subcommands, not one each.** `sendra run` and
/// `sendra test` answer different questions, but they answer them to the same
/// shell, and a number that means one thing under `run` and another under
/// `test` is a trap for anyone writing `case $? in` around either. So the codes
/// are globally unique across the binary: `1` means "never got a response"
/// whichever command produced it, and the two commands' *own* verdicts get
/// their own numbers — `3` for `run`'s bad status, `4` for `test`'s failed
/// assertion. Reusing `3` for a failed assertion was the alternative, and it
/// would have made the same number mean "the server said 500" in one command
/// and "the server said exactly what you asked for, and it was wrong" in the
/// other.
///
/// Every exit path in the binary returns one of these variants rather than
/// calling `std::process::exit` inline, so adding a code later means adding a
/// row here and nothing else. Codes 5 and up stay free.
///
/// **`test` never returns 3, and `run` never returns 4.** `run` does not read
/// assertions when deciding what to return — see [`exit_for_response`], which
/// is the single place that decision lives — so a `run` that prints "1 failed"
/// still exits `0`. That asymmetry is deliberate and permanent, not a stage on
/// the way to unifying them: wiring assertions into `run`'s exit code would
/// silently change what every existing `sendra run x && deploy.sh` means the
/// moment an `assertions` block is added to a file, and `sendra test` exists
/// precisely so that nobody has to.
///
/// Symmetrically, `test` does not read raw status: a request that declared no
/// assertions and came back `404` exits `0` under `test`. See [`Summary`] for
/// why, and for the three-way split — passed, failed, no assertions — that
/// makes it visible rather than silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    Ok = 0,
    Failure = 1,
    // 2 belongs to clap; see the table above.
    ErrorStatus = 3,
    TestFailed = 4,
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

/// The outcome `sendra run` reports for a request that came back, assertions
/// included — which is to say, assertions *excluded*.
///
/// This exists rather than calling [`exit_for_status`] directly at the call
/// site so that "assertions do not affect `run`'s exit code" is a stated
/// decision with a test on it, in one place, instead of an absence nobody can
/// point at. [`Summary::exit`] is the other half of the pair: the same report,
/// read rather than discarded, for the command whose job is to read it.
fn exit_for_response(status: u16, assertions: &AssertionReport, allow_error_status: bool) -> Exit {
    // Read and deliberately discarded: see above, and the `Exit` table.
    let _ = assertions;
    exit_for_status(status, allow_error_status)
}

/// Fold the outcomes of several requests into the single code the process can
/// return. The worst outcome wins, ranked
/// `Ok` < `ErrorStatus` < `TestFailed` < `Failure`.
///
/// "Worst wins" rather than "the last request wins": the exit code should
/// answer "did anything go wrong?", and tying it to the last request would make
/// it depend on the order the file happens to list requests in, so reordering a
/// collection could change whether a script proceeds. This keeps
/// `sendra run collection.yaml && deploy.sh` meaning for a collection what it
/// means for a single request — exit 0 is a promise that nothing in the run
/// failed.
///
/// `Failure` outranks both middle tiers because "never got a response" is the
/// bigger problem: a 404 is an answer, a DNS failure is not. A run reports the
/// most serious thing that happened, not the most recent.
///
/// `ErrorStatus` and `TestFailed` cannot meet today — one is produced only by
/// `run` and the other only by `test`, per the [`Exit`] table — so their
/// relative order is a convention rather than an observable. It is set the way
/// it is because if they ever did meet, the explicit failed expectation is the
/// more informative answer than the status nobody wrote down.
fn worst(a: Exit, b: Exit) -> Exit {
    // Rank by severity, not by the exit numbers: 3 (ErrorStatus) and 4
    // (TestFailed) are the milder outcomes, so the numeric order is the wrong
    // order.
    fn severity(exit: Exit) -> u8 {
        match exit {
            Exit::Ok => 0,
            Exit::ErrorStatus => 1,
            Exit::TestFailed => 2,
            Exit::Failure => 3,
        }
    }

    if severity(b) > severity(a) {
        b
    } else {
        a
    }
}

/// What became of one request in a run.
///
/// `run` and `test` disagree about what to print and about what to return, but
/// not about what happened, so [`run_requests`] produces these and each
/// subcommand folds them its own way — [`exit_for_run`] for `run`, [`Summary`]
/// for `test`. Keeping the shared loop's output a fact rather than an exit code
/// is what let the two commands share it at all: `test` needs to know *why* a
/// request contributed a failure, and an `Exit` has already thrown that away.
#[derive(Debug)]
enum Outcome {
    /// The request never got a response: a `{{variable}}` with nothing behind
    /// it, an invalid header, a refused connection. There is no status and no
    /// assertion report, because neither of them exists without a response.
    NoResponse,

    /// A response came back, and the request's assertions — if it declared any
    /// — were evaluated against it.
    ///
    /// The report is empty when the file declared none, which is a third thing
    /// from "passed" and from "failed"; see [`Summary`].
    Responded {
        status: u16,
        assertions: AssertionReport,
    },
}

/// `run`'s verdict on one outcome. Assertions are carried through and ignored;
/// see [`exit_for_response`].
fn exit_for_outcome(outcome: &Outcome, allow_error_status: bool) -> Exit {
    match outcome {
        Outcome::NoResponse => Exit::Failure,
        Outcome::Responded { status, assertions } => {
            exit_for_response(*status, assertions, allow_error_status)
        }
    }
}

/// Fold a whole `run` into the one code the process returns.
fn exit_for_run(outcomes: &[Outcome], allow_error_status: bool) -> Exit {
    outcomes.iter().fold(Exit::Ok, |exit, outcome| {
        worst(exit, exit_for_outcome(outcome, allow_error_status))
    })
}

/// The counts `sendra test` prints at the end of a run, and the exit code it
/// derives from them.
///
/// **Five numbers, and the middle three are separate categories on purpose.**
/// A request that declared no assertions is not a pass and not a failure: it is
/// a request nobody said anything about. Folding it into `passed` would make a
/// collection with no assertions anywhere report a perfect green run, which is
/// the single most misleading thing a test command can do; folding it into
/// `failed` would make adding a request to a collection break the build until
/// somebody wrote expectations for it. Counting it on its own line says the
/// true thing — "these ran, and nothing was checked" — and leaves what to do
/// about it to the person reading.
///
/// **What fails the run.** `failed` and `no_response`, and nothing else:
///
/// - A request whose assertions did not all hold is the whole point of the
///   command. [`Exit::TestFailed`].
/// - A request that never got a response cannot have its assertions evaluated,
///   so a run containing one cannot honestly say the expectations held. It is
///   [`Exit::Failure`], the same code `run` gives it, because it is the same
///   event: the tool could not do its job, as against the API failing to meet
///   expectations. In CI those two want different handling — one is "fix your
///   test setup", the other is "fix your API" — which is exactly why they get
///   different numbers instead of one generic non-zero.
///
/// **`without_assertions` does not fail the run, whatever the status was.**
/// This is the debatable one, so: a request with no `assertions` block that
/// comes back `404` exits `0` under `sendra test`. The command's contract is
/// that the *file* says what it expects and `test` reports whether it got it.
/// Failing on a bare 404 means asserting something the file never wrote down —
/// inventing an expectation on the author's behalf — which is the same class of
/// mistake as a silently-ignored assertion typo, only inverted. Sendra already
/// refuses to guess anywhere else in its schema, and the check is one line to
/// write when it is wanted:
///
/// ```yaml
/// assertions:
///   status: 200
/// ```
///
/// It also keeps `test` from having two independent verdicts that can
/// disagree — "assertions passed but the status was bad" has no sensible
/// single answer — and it leaves a real use intact: a request that is in the
/// collection to *reach* an endpoint (a login, a setup call) rather than to be
/// checked. The raw-status question already has a command that answers it, and
/// answers it well: `sendra run`, exit `3`. Nothing is lost by `test` declining
/// to answer it a second time with a different number.
///
/// The safeguard against that decision hiding a problem is the summary itself:
/// `without_assertions` is printed, so a run whose expectations were never
/// written is visibly not the same thing as a run that passed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Summary {
    /// Every request the run attempted. Always the sum of the four below.
    total: usize,
    /// Got a response, declared at least one assertion, and all of them held.
    passed: usize,
    /// Got a response, declared assertions, and at least one did not hold.
    failed: usize,
    /// Got a response and declared no assertions at all.
    without_assertions: usize,
    /// Never got a response, so there was nothing to evaluate against.
    no_response: usize,
}

impl Summary {
    /// Classify each outcome into exactly one of the four categories.
    fn of(outcomes: &[Outcome]) -> Self {
        let mut summary = Summary {
            total: outcomes.len(),
            ..Summary::default()
        };

        for outcome in outcomes {
            match outcome {
                Outcome::NoResponse => summary.no_response += 1,
                // An empty report is a request that declared nothing, whether
                // it had no `assertions` key or an empty one. Either way there
                // is nothing to have passed.
                Outcome::Responded { assertions, .. } if assertions.is_empty() => {
                    summary.without_assertions += 1
                }
                Outcome::Responded { assertions, .. } if assertions.passed() => summary.passed += 1,
                Outcome::Responded { .. } => summary.failed += 1,
            }
        }

        summary
    }

    /// The code the process returns. Worst-wins over the two failing
    /// categories, through the same [`worst`] every other aggregate uses, so
    /// the ordering lives in one place.
    fn exit(&self) -> Exit {
        let mut exit = Exit::Ok;

        if self.failed > 0 {
            exit = worst(exit, Exit::TestFailed);
        }
        if self.no_response > 0 {
            exit = worst(exit, Exit::Failure);
        }

        exit
    }
}

/// How much of a response to print.
///
/// The two subcommands print the same *assertion* block — issue 6's format,
/// unchanged, because a second way to render a passed check would be a second
/// thing to learn — and differ only in how much of the response they put above
/// it. `run` exists to show you what came back, so it shows all of it. `test`
/// answers a yes/no question about a whole collection, and burying that answer
/// under four JSON bodies would make the summary the hardest line to find in
/// its own output; it prints the status line, which is one line, carries the
/// timing, and says which response the checks below it are about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Detail {
    /// Status line, headers and body.
    Full,
    /// The status line alone.
    StatusOnly,
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

        Command::Test {
            path,
            env,
            allow_error_status,
        } => {
            if allow_error_status {
                reject_allow_error_status();
            }
            test(&path, env.as_deref()).await.into()
        }
    }
}

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
fn reject_allow_error_status() -> ! {
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

/// Everything both subcommands do before the first byte goes out: the config,
/// the environment, and the file.
struct Prepared {
    config: Config,
    environment: Environment,
    document: Document,
}

/// Resolve the config, the environment and the request file, in that order.
///
/// Returns `Err(Exit::Failure)` — having already printed the error — because
/// there is nothing for the caller to add: these three failures are fatal to
/// the whole run in both subcommands, and both report them the same way.
///
/// **Config and the environment are resolved once, before anything is read or
/// sent.** Those two failures stop the run, and belong in a different category
/// from anything a request can do: a config or environment file that does not
/// parse is not "this request failed", it is "the settings this whole run was
/// going to use are unreadable", and sending some requests under half-applied
/// defaults would be worse than sending none. `--env` naming an environment
/// that does not exist joins them, for the reason given on
/// [`environment_for`].
///
/// Both subcommands share this because they must: `sendra test` that resolved
/// config differently from `sendra run` would mean a request could pass under
/// one and fail under the other for reasons neither prints.
fn prepare(path: &Path, environment_name: Option<&str>) -> Result<Prepared, Exit> {
    // Resolved once for the whole run: every request in a collection is sent
    // under the same defaults, and a broken config file stops the run instead
    // of failing partway through it.
    let config = match Config::resolve() {
        Ok(config) => config,
        Err(err) => {
            print_error(&err);
            return Err(Exit::Failure);
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
            return Err(Exit::Failure);
        }
    };

    let environment = match environment_for(&start_dir, environment_name) {
        Ok(environment) => environment,
        Err(err) => {
            print_environment_error(&err);
            return Err(Exit::Failure);
        }
    };

    let document = match Document::from_path(path) {
        Ok(document) => document,
        Err(err) => {
            print_error(&err);
            return Err(Exit::Failure);
        }
    };

    Ok(Prepared {
        config,
        environment,
        document,
    })
}

/// Load `path` and send either the one request named, or all of them.
///
/// Returns an exit code rather than a `Result` because a collection run can
/// half-succeed: one request failing does not stop the rest, so there is no
/// single error to propagate. Every outcome is printed as it happens and folded
/// into the code with [`exit_for_run`].
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
/// The setup this shares with [`test`] lives in [`prepare`]; the sending loop
/// they share is [`run_requests`]. What is left here is the two things `run`
/// does that `test` does not: selecting one request by name, and reading raw
/// statuses to produce an exit code.
async fn run(
    path: &Path,
    name: Option<&str>,
    environment_name: Option<&str>,
    allow_error_status: bool,
) -> Exit {
    let Prepared {
        config,
        environment,
        document,
    } = match prepare(path, environment_name) {
        Ok(prepared) => prepared,
        Err(exit) => return exit,
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
    let outcomes = run_requests(&requests, &environment, |request| async move {
        send(&request, config, Detail::Full).await
    })
    .await;

    exit_for_run(&outcomes, allow_error_status)
}

/// Load `path`, send every request in it, and pass or fail on the assertions.
///
/// The sending half is exactly the one `run` uses: same [`prepare`], same
/// [`run_requests`], same substitution, same rule that one request failing does
/// not stop the rest, same assertion evaluation in [`send`]. The two commands
/// diverge only after the outcomes are in — `run` folds them by status through
/// [`exit_for_run`], `test` counts them into a [`Summary`] — which is why the
/// shared code is the whole pipeline rather than an abstraction invented to
/// hold two similar things together.
///
/// **No name argument, unlike `run`.** `run <file> <name>` exists to send one
/// request out of a collection and look at it; `test` produces a verdict over a
/// file, and a verdict over one hand-picked request out of a collection is a
/// different, narrower thing that nothing has yet asked for. It can be added
/// later without changing anything here.
async fn test(path: &Path, environment_name: Option<&str>) -> Exit {
    let Prepared {
        config,
        environment,
        document,
    } = match prepare(path, environment_name) {
        Ok(prepared) => prepared,
        Err(exit) => return exit,
    };

    // Every request in the file, in file order — a single-request file is a
    // run of one.
    let requests: Vec<&Request> = document.requests().iter().collect();

    let config = &config;
    let outcomes = run_requests(&requests, &environment, |request| async move {
        send(&request, config, Detail::StatusOnly).await
    })
    .await;

    let summary = Summary::of(&outcomes);
    print_summary(&summary);
    summary.exit()
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

/// Substitute and send each of `requests` in file order, printing each result
/// as it arrives and returning what became of every one of them.
///
/// Returns [`Outcome`]s rather than an exit code because its two callers want
/// different answers out of the same run: `run` folds them by status, `test`
/// counts them by assertion. An `Exit` per request would have already discarded
/// the distinction `test` is built on — a `Failure` cannot say whether it was a
/// refused connection or a failed check.
///
/// **Substitution happens here, per request, not as a pass over the batch
/// first.** A `{{var}}` with nothing behind it, or a `${VAR}` that is not
/// exported, is exactly the same category of problem as a refused connection:
/// *this* request could not be completed. Issue 2 settled what a run does with
/// that — the sibling requests are still sent, every result is still printed,
/// and the aggregate decides the exit code — and there is no reason a variable
/// should be the one failure that also cancels the requests around it. Checking
/// the whole collection up front would additionally mean the file's *last*
/// request could stop the first one from ever being sent, which is the kind of
/// order-dependence [`worst`] exists to keep out of the exit code.
///
/// A substitution failure is [`Outcome::NoResponse`], the same as a DNS or TLS
/// failure, and both commands treat it the same way for the same reason: there
/// is no response, so there is no status for `--allow-error-status` to forgive
/// and nothing for an assertion to be evaluated against.
///
/// `send_one` is a parameter rather than a direct call to [`send`] so that this
/// loop — which is the whole of "one request failing does not stop the rest" —
/// can be tested without a network, the way config resolution takes its
/// directories as arguments instead of reading the real ones.
async fn run_requests<S, F>(
    requests: &[&Request],
    environment: &Environment,
    mut send_one: S,
) -> Vec<Outcome>
where
    S: FnMut(Request) -> F,
    F: std::future::Future<Output = Outcome>,
{
    let mut outcomes = Vec::with_capacity(requests.len());

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

        outcomes.push(match substituted {
            Ok(request) => send_one(request).await,
            Err(err) => {
                print_error(&err);
                Outcome::NoResponse
            }
        });
    }

    outcomes
}

/// Send one request, print whatever came back, and report what happened.
///
/// A failure is printed and returned rather than propagated: in a collection
/// run the requests after this one still deserve to be sent, and the user still
/// deserves to see them.
///
/// The request arrives already substituted, and the `→` label has already been
/// printed by [`run_requests`].
///
/// Assertions are evaluated here, against the response this request got, and
/// printed under it — so in a collection run each block of results sits with
/// the response it is about, rather than in a summary at the end that would
/// have to name every request again. `sendra test` adds a summary *as well as*
/// these blocks, not instead of them: the counts say how the run went, and
/// these say which check, in which request, was the reason.
///
/// Both subcommands come through here, so both evaluate assertions in exactly
/// the same place, from exactly the same [`Assertions::evaluate`]. `detail` is
/// the only thing they differ on; see [`Detail`].
async fn send(request: &Request, config: &Config, detail: Detail) -> Outcome {
    match sendra_core::send(request, config).await {
        Ok(response) => {
            match detail {
                Detail::Full => print_response(&response),
                Detail::StatusOnly => print_status_line(&response),
            }

            // No `assertions` block is the empty report, which prints nothing:
            // a request written before this feature existed looks exactly as it
            // did before it existed.
            let assertions = request
                .assertions
                .as_ref()
                .map(|assertions| assertions.evaluate(&response))
                .unwrap_or_default();

            if assertions.is_empty() && detail == Detail::StatusOnly {
                // `run` says nothing here, and must keep saying nothing. Under
                // `test` the silence is the problem: the summary is about to
                // count this request as one of N "without assertions", and
                // without a marker there is nothing to match that number
                // against. One dimmed line, no symbol, so it reads as an
                // absence rather than as a result.
                print_no_assertions();
            } else {
                print_assertions(&assertions);
            }

            Outcome::Responded {
                status: response.status,
                assertions,
            }
        }
        Err(err) => {
            print_error(&err);
            Outcome::NoResponse
        }
    }
}

/// The one line every response gets, whichever subcommand asked for it:
/// `200 OK  412 ms`.
///
/// Split out of [`print_response`] so that `sendra test`, which prints no
/// headers and no body, still says which response the assertions under it are
/// about — and says it in the same words and the same colours, rather than in a
/// second rendering of the same fact.
fn print_status_line(response: &Response) {
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
}

fn print_response(response: &Response) {
    print_status_line(response);

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

/// Print one request's assertion results under its response.
///
/// ```text
/// assertions
///   ✓ status is 200
///   ✓ header `content-type` is `application/json`
///   ✗ body contains `widget` — not found in the 429-byte body
///   ✗ `$.json.count` is 3 — got 2
///   2 passed, 2 failed
/// ```
///
/// Green for a `✓` and for the pass count, red for a `✗`, its detail and the
/// fail count.
///
/// An empty report prints nothing at all, so a request with no `assertions`
/// block produces byte-for-byte the output it did before assertions existed.
///
/// This goes to stdout, with the response rather than with the `→` label on
/// stderr: an assertion result is a statement *about the response*, produced
/// only because the file asked for it, and belongs next to the thing it
/// describes. The indented block under a dimmed `assertions` heading is what
/// keeps it apart from the raw body above it — a body can end in anything,
/// including a line that looks like a checkmark, so the separation is carried
/// by a blank line and a heading rather than by the symbols alone.
///
/// The wording of each line comes from core, so a future TUI reports the same
/// failure in the same words. Only the symbols, colour and layout are decided
/// here.
fn print_assertions(report: &AssertionReport) {
    if report.is_empty() {
        return;
    }

    println!();
    println!(
        "{}",
        "assertions".if_supports_color(Stream::Stdout, |t| t.dimmed())
    );

    for result in report.results() {
        match &result.failure {
            None => println!(
                "  {} {}",
                "✓".if_supports_color(Stream::Stdout, |t| t.green()),
                result.expectation
            ),
            // The detail is on the same line as the expectation it belongs to:
            // "what I asked for" and "what I got" are one sentence, and reading
            // them together is the whole reason the detail exists.
            Some(detail) => println!(
                "  {} {} {} {}",
                "✗".if_supports_color(Stream::Stdout, |t| t.red()),
                result.expectation,
                "—".if_supports_color(Stream::Stdout, |t| t.dimmed()),
                detail.if_supports_color(Stream::Stdout, |t| t.red())
            ),
        }
    }

    // A count line even for a single assertion: it is the one line worth
    // grepping for, and a summary that appears only sometimes is worse to
    // script against than one that is always there.
    //
    // Each half is coloured like the symbol it counts — green for the passes,
    // red for the failures — rather than the line taking one colour from
    // whether anything failed. Colouring the whole line red made "4 passed"
    // read as bad news.
    let passed = format!("{} passed", report.passed_count())
        .if_supports_color(Stream::Stdout, |t| t.green())
        .to_string();

    if report.passed() {
        println!("  {passed}");
    } else {
        let failed = format!("{} failed", report.failed_count())
            .if_supports_color(Stream::Stdout, |t| t.red())
            .to_string();
        println!("  {passed}, {failed}");
    }
}

/// The `sendra test` counterpart to [`print_assertions`] for a request that
/// declared none:
///
/// ```text
/// no assertions
/// ```
///
/// In the same position an `assertions` heading would occupy, dimmed and with
/// no `✓`/`✗` under it, because it is the absence of results rather than a
/// result. It exists so the summary's `without assertions` count has something
/// to point at: without it, the only way to find which request was uncovered
/// would be to notice which one printed nothing.
///
/// `sendra run` does not print it. A request with no assertions has always
/// produced byte-for-byte the output it produced before assertions existed, and
/// that stays true.
fn print_no_assertions() {
    println!();
    println!(
        "{}",
        "no assertions".if_supports_color(Stream::Stdout, |t| t.dimmed())
    );
}

/// Print the counts `sendra test` ends a run with.
///
/// ```text
/// summary
///   5 requests: 2 passed, 1 failed, 1 without assertions, 1 no response
/// ```
///
/// The total and the passes are always shown; the other three appear only when
/// they are not zero, so a clean run reads `3 requests: 3 passed` and nothing
/// competes with it. That follows [`print_assertions`], which prints
/// `4 passed` and adds `, 2 failed` only when there is something to add — one
/// rule, applied at both levels, rather than a per-request line that hides its
/// zeroes under a summary line that spells them out.
///
/// Each count is coloured like what it counts: green for passes, red for the
/// two that fail the run, dimmed for the one that does not. `without
/// assertions` being dimmed rather than yellow is deliberate — it is not a
/// warning, it is a fact about what the file asked for.
///
/// Under the same dimmed heading style as the per-request `assertions` blocks,
/// and for the same reason: a response body can end in anything, and the
/// heading plus the blank line is what keeps the summary from reading as the
/// tail of whatever printed above it.
fn print_summary(summary: &Summary) {
    println!();
    println!(
        "{}",
        "summary".if_supports_color(Stream::Stdout, |t| t.dimmed())
    );

    let mut counts = vec![format!("{} passed", summary.passed)
        .if_supports_color(Stream::Stdout, |t| t.green())
        .to_string()];

    if summary.failed > 0 {
        counts.push(
            format!("{} failed", summary.failed)
                .if_supports_color(Stream::Stdout, |t| t.red())
                .to_string(),
        );
    }
    if summary.without_assertions > 0 {
        counts.push(
            format!("{} without assertions", summary.without_assertions)
                .if_supports_color(Stream::Stdout, |t| t.dimmed())
                .to_string(),
        );
    }
    if summary.no_response > 0 {
        counts.push(
            format!("{} no response", summary.no_response)
                .if_supports_color(Stream::Stdout, |t| t.red())
                .to_string(),
        );
    }

    println!(
        "  {} {}: {}",
        summary.total,
        if summary.total == 1 {
            "request"
        } else {
            "requests"
        },
        counts.join(", ")
    );
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
        for a in [Exit::Ok, Exit::ErrorStatus, Exit::TestFailed, Exit::Failure] {
            for b in [Exit::Ok, Exit::ErrorStatus, Exit::TestFailed, Exit::Failure] {
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

    /// The outcome of a request that came back with `status` and declared no
    /// assertions: what a fake `send_one` hands back when the response itself
    /// is not what the test is about.
    fn responded(status: u16) -> Outcome {
        Outcome::Responded {
            status,
            assertions: AssertionReport::default(),
        }
    }

    /// The outcome of a request that came back with `status` carrying an
    /// already-evaluated assertion report.
    fn checked(status: u16, assertions: AssertionReport) -> Outcome {
        Outcome::Responded { status, assertions }
    }

    #[tokio::test]
    async fn a_broken_variable_does_not_stop_the_requests_after_it() {
        let document = Document::from_yaml_str(COLLECTION_WITH_A_BROKEN_VARIABLE).unwrap();
        let requests: Vec<&Request> = document.requests().iter().collect();

        // Stands in for the network: records what it was handed and reports a
        // clean response, so the substitution is the only failure in the run.
        let mut sent = Vec::new();
        let outcomes = run_requests(&requests, &environment(), |request| {
            sent.push(request.url.clone());
            async { responded(200) }
        })
        .await;
        let exit = exit_for_run(&outcomes, false);

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
        let outcomes = run_requests(&requests, &environment(), |_| async { responded(404) }).await;

        assert_eq!(exit_for_run(&outcomes, true), Exit::Failure);
    }

    #[tokio::test]
    async fn a_substitution_failure_outranks_a_bad_status_from_a_sibling() {
        let document = Document::from_yaml_str(COLLECTION_WITH_A_BROKEN_VARIABLE).unwrap();
        let requests: Vec<&Request> = document.requests().iter().collect();

        // Every sibling answers 500, so the run holds both kinds of failure at
        // once. "Never got a response" is the more serious of the two.
        let outcomes = run_requests(&requests, &environment(), |_| async { responded(500) }).await;

        assert_eq!(exit_for_run(&outcomes, false), Exit::Failure);
    }

    #[tokio::test]
    async fn selecting_one_request_by_name_is_unaffected_by_a_broken_sibling() {
        // The named path was already scoped to the one request selected, and
        // stays that way: `Broken` needing a variable nothing defines has no
        // bearing on running `Third`.
        let document = Document::from_yaml_str(COLLECTION_WITH_A_BROKEN_VARIABLE).unwrap();
        let request = document.get("Third").expect("`Third` is in the collection");

        let mut sent = Vec::new();
        let outcomes = run_requests(&[request], &environment(), |request| {
            sent.push(request.url.clone());
            async { responded(200) }
        })
        .await;

        assert_eq!(sent, vec!["https://example.com/third"]);
        assert_eq!(exit_for_run(&outcomes, false), Exit::Ok);
    }

    #[tokio::test]
    async fn selecting_the_broken_request_by_name_fails_on_its_own() {
        let document = Document::from_yaml_str(COLLECTION_WITH_A_BROKEN_VARIABLE).unwrap();
        let request = document
            .get("Broken")
            .expect("`Broken` is in the collection");

        let mut sent = Vec::new();
        let outcomes = run_requests(&[request], &environment(), |request| {
            sent.push(request.url.clone());
            async { responded(200) }
        })
        .await;

        assert!(sent.is_empty(), "nothing should have been sent");
        assert_eq!(exit_for_run(&outcomes, false), Exit::Failure);
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
        let outcomes = run_requests(&requests, &environment(), |request| {
            sent.push(request.url.clone());
            async { responded(200) }
        })
        .await;

        assert_eq!(
            sent,
            vec!["https://example.com/first", "https://example.com/second"]
        );
        assert_eq!(exit_for_run(&outcomes, false), Exit::Ok);
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
            let outcomes = run_requests(&requests, &environment, |request| {
                sent.push(request.url.clone());
                async { responded(200) }
            })
            .await;
            assert_eq!(exit_for_run(&outcomes, false), Exit::Ok);
        }

        assert_eq!(
            sent,
            vec![
                "https://staging.example/health",
                "https://prod.example/health"
            ]
        );
    }

    // --- assertions do not touch the exit code ---------------------------
    //
    // The non-goal of the issue that added assertions, tested rather than
    // assumed. `sendra run` reports what came back; `sendra test` will be the
    // command that passes or fails on expectations.

    /// A response to hand [`exit_for_response`]. Built by hand: none of these
    /// tests need a socket, and the field values other than `status` never
    /// enter into the decision.
    fn response(status: u16) -> Response {
        Response {
            status,
            status_text: "Test".to_string(),
            headers: vec![("content-type".to_string(), "text/plain".to_string())],
            body: "body".to_string(),
            elapsed: std::time::Duration::from_millis(1),
        }
    }

    /// The `assertions` block of a request file, parsed the way a real run
    /// parses it — through `Document`, rather than by reaching for a YAML
    /// dependency this crate does not otherwise need.
    fn assertions_from(yaml: &str) -> sendra_core::Assertions {
        Document::from_yaml_str(yaml)
            .expect("the test request should parse")
            .requests()[0]
            .assertions
            .clone()
            .expect("the test request has an assertions block")
    }

    /// A report in which everything that could fail, did.
    fn a_failing_report(status: u16) -> AssertionReport {
        let report = assertions_from(
            "\
method: GET
url: https://example.com
assertions:
  status: 599
  headers:
    x-nope: whatever
  body_contains: definitely-not-in-the-body
  json:
    $.nope: 1
",
        )
        .evaluate(&response(status));

        assert_eq!(report.failed_count(), 4, "all four should have failed");
        report
    }

    #[test]
    fn failing_assertions_do_not_change_the_exit_code_of_a_successful_response() {
        // The intentional, temporary asymmetry: four failed assertions printed,
        // exit 0 all the same.
        let exit = exit_for_response(200, &a_failing_report(200), false);
        assert_eq!(exit, Exit::Ok);
        assert_eq!(exit as u8, 0);
    }

    #[test]
    fn failing_assertions_do_not_change_the_exit_code_of_an_error_response() {
        // Nor do they promote a 404 to something else, or rescue it: the status
        // is still the only thing being read.
        assert_eq!(
            exit_for_response(404, &a_failing_report(404), false),
            Exit::ErrorStatus
        );
        assert_eq!(
            exit_for_response(404, &a_failing_report(404), true),
            Exit::Ok,
            "--allow-error-status still forgives the status, and nothing else"
        );
    }

    #[test]
    fn passing_assertions_do_not_rescue_an_error_status_either() {
        let report =
            assertions_from("method: GET\nurl: https://example.com\nassertions:\n  status: 500\n")
                .evaluate(&response(500));
        assert!(
            report.passed(),
            "the assertion asked for exactly this status"
        );

        assert_eq!(
            exit_for_response(500, &report, false),
            Exit::ErrorStatus,
            "a 500 the file expected is still a 500"
        );
    }

    #[test]
    fn the_exit_code_is_the_same_with_and_without_an_assertions_block() {
        // The no-op guarantee, at the level that decides the process's answer.
        for status in [200, 301, 404, 500] {
            for allow in [false, true] {
                assert_eq!(
                    exit_for_response(status, &AssertionReport::default(), allow),
                    exit_for_response(status, &a_failing_report(status), allow),
                    "assertions changed the exit code for {status} (allow_error_status={allow})"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_request_with_no_assertions_block_parses_and_runs_unchanged() {
        // A file written before assertions existed: it still parses, still
        // carries no assertions, and still runs to exit 0.
        let document =
            Document::from_yaml_str("name: Plain\nmethod: GET\nurl: '{{base_url}}/plain'\n")
                .unwrap();
        let requests: Vec<&Request> = document.requests().iter().collect();
        assert!(requests[0].assertions.is_none());

        let mut sent = Vec::new();
        let outcomes = run_requests(&requests, &environment(), |request| {
            assert!(
                request.assertions.is_none(),
                "substitution must not invent a block"
            );
            sent.push(request.url.clone());
            async { responded(200) }
        })
        .await;

        assert_eq!(sent, vec!["https://example.com/plain"]);
        assert_eq!(exit_for_run(&outcomes, false), Exit::Ok);
    }

    #[tokio::test]
    async fn an_assertions_block_reaches_the_send_step_substituted() {
        // End to end minus the socket: the block survives the run loop, with
        // its values resolved against the environment.
        let document = Document::from_yaml_str(
            "\
name: Checked
method: GET
url: '{{base_url}}/thing'
assertions:
  status: 200
  body_contains: '{{base_url}}'
",
        )
        .unwrap();
        let requests: Vec<&Request> = document.requests().iter().collect();

        let mut seen = Vec::new();
        let outcomes = run_requests(&requests, &environment(), |request| {
            seen.push(request.assertions.clone());
            async { responded(200) }
        })
        .await;

        assert_eq!(exit_for_run(&outcomes, false), Exit::Ok);
        let assertions = seen.pop().flatten().expect("the block reached `send`");
        assert_eq!(assertions.status, Some(200));
        assert_eq!(
            assertions.body_contains.as_deref(),
            Some("https://example.com"),
            "assertion values are substituted like everything else"
        );
    }

    #[test]
    fn exit_codes_match_the_documented_convention() {
        // The numbers are the contract with anyone scripting sendra, so pin
        // them here rather than only asserting on the variants.
        assert_eq!(Exit::Ok as u8, 0);
        assert_eq!(Exit::Failure as u8, 1);
        assert_eq!(Exit::ErrorStatus as u8, 3);
        assert_eq!(Exit::TestFailed as u8, 4);
    }

    // --- `sendra test`: the summary, and the exit code it comes from ------
    //
    // The command's whole contract is in `Summary`: which of the four
    // categories each request lands in, and which of them make the run fail.
    // These test that against outcomes built by hand, and — where the point is
    // that a request that never got a response is not special-cased anywhere —
    // through the real `run_requests` loop.

    /// An outcome that came back with `status` and declared one assertion,
    /// which held.
    fn all_passed(status: u16) -> Outcome {
        let report = assertions_from(&format!(
            "method: GET\nurl: https://example.com\nassertions:\n  status: {status}\n"
        ))
        .evaluate(&response(status));

        assert!(report.passed(), "the assertion asked for exactly {status}");
        checked(status, report)
    }

    /// An outcome that came back with `status` and declared assertions, none of
    /// which held.
    fn some_failed(status: u16) -> Outcome {
        checked(status, a_failing_report(status))
    }

    #[test]
    fn a_mixed_collection_counts_each_category_separately() {
        // One of each of the three things a response can be, so no two
        // categories can be collapsed without this noticing.
        let outcomes = vec![all_passed(200), some_failed(200), responded(200)];

        assert_eq!(
            Summary::of(&outcomes),
            Summary {
                total: 3,
                passed: 1,
                failed: 1,
                without_assertions: 1,
                no_response: 0,
            }
        );
        assert_eq!(Summary::of(&outcomes).exit(), Exit::TestFailed);
        assert_eq!(Exit::TestFailed as u8, 4);
    }

    #[test]
    fn a_collection_where_everything_passes_exits_zero() {
        let outcomes = vec![all_passed(200), all_passed(201), all_passed(204)];

        assert_eq!(
            Summary::of(&outcomes),
            Summary {
                total: 3,
                passed: 3,
                ..Summary::default()
            }
        );
        assert_eq!(Summary::of(&outcomes).exit(), Exit::Ok);
    }

    #[test]
    fn a_request_with_no_assertions_is_neither_a_pass_nor_a_failure() {
        // The third category, on its own: three requests that came back fine
        // and were never checked. Nothing failed, so the run exits 0 — and
        // nothing passed either, so the summary cannot be read as three green
        // checks.
        let outcomes = vec![responded(200), responded(200), responded(200)];
        let summary = Summary::of(&outcomes);

        assert_eq!(
            summary,
            Summary {
                total: 3,
                without_assertions: 3,
                ..Summary::default()
            }
        );
        assert_eq!(summary.passed, 0, "an unchecked request is not a pass");
        assert_eq!(summary.failed, 0, "nor is it a failure");
        assert_eq!(summary.exit(), Exit::Ok);
    }

    #[test]
    fn an_empty_assertions_block_counts_as_no_assertions_at_all() {
        // `assertions: {}` is a block that asserts nothing, and is the same
        // thing to this command as having written no block: an empty report
        // either way. See `Assertions::is_empty` in core.
        let assertions = assertions_from("method: GET\nurl: https://example.com\nassertions: {}\n");
        assert!(assertions.is_empty());

        let outcomes = vec![checked(200, assertions.evaluate(&response(200)))];

        assert_eq!(
            Summary::of(&outcomes),
            Summary {
                total: 1,
                without_assertions: 1,
                ..Summary::default()
            }
        );
    }

    #[test]
    fn a_bad_status_with_no_assertions_does_not_fail_a_test_run() {
        // The debatable decision, pinned. A request that declared nothing and
        // came back 404 or 500 exits 0 under `test`: the file said nothing
        // about the status, so `test` says nothing about it either. See
        // `Summary` for the reasoning.
        let outcomes = vec![responded(404), responded(500)];
        let summary = Summary::of(&outcomes);

        assert_eq!(
            summary,
            Summary {
                total: 2,
                without_assertions: 2,
                ..Summary::default()
            }
        );
        assert_eq!(
            summary.exit(),
            Exit::Ok,
            "an unasserted status must not fail a test run"
        );

        // And the contrast that makes it a decision rather than an oversight:
        // the very same run, under `sendra run`, still exits 3. The raw-status
        // question has a command that answers it; `test` declining to answer it
        // a second time loses nothing.
        assert_eq!(exit_for_run(&outcomes, false), Exit::ErrorStatus);
    }

    #[test]
    fn an_asserted_bad_status_behaves_exactly_as_written() {
        // The corollary of the rule above: the status is not ignored, it is
        // only ever read through an assertion. Asserting `status: 404` and
        // getting one is a pass; asserting `status: 200` and getting a 404 is
        // a failure. Both under the same command that shrugs at an unasserted
        // 404.
        assert_eq!(Summary::of(&[all_passed(404)]).exit(), Exit::Ok);

        let wrong =
            assertions_from("method: GET\nurl: https://example.com\nassertions:\n  status: 200\n")
                .evaluate(&response(404));
        assert!(!wrong.passed());
        assert_eq!(Summary::of(&[checked(404, wrong)]).exit(), Exit::TestFailed);
    }

    #[test]
    fn a_failed_assertion_on_a_perfectly_good_status_still_fails_the_run() {
        // The other half of "status is not the input": a 200 does not rescue a
        // check that did not hold.
        let outcomes = vec![some_failed(200)];

        assert_eq!(
            Summary::of(&outcomes),
            Summary {
                total: 1,
                failed: 1,
                ..Summary::default()
            }
        );
        assert_eq!(Summary::of(&outcomes).exit(), Exit::TestFailed);
    }

    #[test]
    fn passing_assertions_on_an_error_status_pass_the_test_run() {
        // A request that expects a 500 and gets one has met its expectations.
        // `sendra run` would still exit 3 on the same response, and that is the
        // difference between the two commands stated as a test rather than as a
        // paragraph.
        let outcomes = vec![all_passed(500)];

        assert_eq!(Summary::of(&outcomes).exit(), Exit::Ok);
        assert_eq!(exit_for_run(&outcomes, false), Exit::ErrorStatus);
    }

    #[test]
    fn a_request_that_never_got_a_response_fails_the_run() {
        // No response means no assertions could be evaluated, so the run cannot
        // claim its expectations held — whatever the requests around it did.
        let outcomes = vec![all_passed(200), Outcome::NoResponse, all_passed(200)];
        let summary = Summary::of(&outcomes);

        assert_eq!(
            summary,
            Summary {
                total: 3,
                passed: 2,
                no_response: 1,
                ..Summary::default()
            }
        );
        assert_eq!(summary.exit(), Exit::Failure);
        assert_eq!(Exit::Failure as u8, 1);
    }

    #[test]
    fn never_got_a_response_outranks_a_failed_assertion() {
        // Both are failures and both are non-zero; the code says which kind,
        // and "the tool could not do its job" is the more serious of the two —
        // the same ranking `run` uses.
        let outcomes = vec![some_failed(200), Outcome::NoResponse];
        assert_eq!(Summary::of(&outcomes).exit(), Exit::Failure);

        // And it does not depend on which came first.
        let outcomes = vec![Outcome::NoResponse, some_failed(200)];
        assert_eq!(Summary::of(&outcomes).exit(), Exit::Failure);
    }

    #[test]
    fn every_outcome_lands_in_exactly_one_category() {
        // The four counts are a partition, not four overlapping questions, so
        // the printed line always adds up.
        let outcomes = vec![
            all_passed(200),
            some_failed(200),
            responded(200),
            responded(404),
            Outcome::NoResponse,
            all_passed(500),
        ];
        let summary = Summary::of(&outcomes);

        assert_eq!(summary.total, outcomes.len());
        assert_eq!(
            summary.passed + summary.failed + summary.without_assertions + summary.no_response,
            summary.total,
            "the categories must partition the run: {summary:?}"
        );
    }

    #[test]
    fn the_summary_does_not_depend_on_the_order_of_the_requests() {
        // Same reasoning as `worst`: reordering a collection must not change
        // whether a script proceeds.
        let forwards = Summary::of(&[all_passed(200), some_failed(200), responded(200)]);
        let backwards = Summary::of(&[responded(200), some_failed(200), all_passed(200)]);

        assert_eq!(forwards, backwards);
        assert_eq!(forwards.exit(), backwards.exit());
    }

    #[test]
    fn a_test_run_never_returns_the_code_that_belongs_to_run() {
        // `3` is `run`'s answer to a question `test` does not ask. Over every
        // shape of summary the classifier can produce, `test` returns one of
        // exactly three codes.
        for passed in 0..2 {
            for failed in 0..2 {
                for without_assertions in 0..2 {
                    for no_response in 0..2 {
                        let summary = Summary {
                            total: passed + failed + without_assertions + no_response,
                            passed,
                            failed,
                            without_assertions,
                            no_response,
                        };

                        assert!(
                            matches!(summary.exit(), Exit::Ok | Exit::TestFailed | Exit::Failure),
                            "{summary:?} produced {:?}",
                            summary.exit()
                        );
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn a_substitution_failure_is_counted_and_fails_the_test_run() {
        // Through the real loop, not with a hand-built `NoResponse`: the
        // continue-on-failure model is shared with `run`, so the broken request
        // in the middle must still not stop the two around it, and `test` must
        // count it in the category that has no response in it.
        let document = Document::from_yaml_str(COLLECTION_WITH_A_BROKEN_VARIABLE).unwrap();
        let requests: Vec<&Request> = document.requests().iter().collect();

        let mut sent = Vec::new();
        let outcomes = run_requests(&requests, &environment(), |request| {
            sent.push(request.url.clone());
            async { all_passed(200) }
        })
        .await;

        assert_eq!(
            sent,
            vec!["https://example.com/first", "https://example.com/third"],
            "the siblings of a broken request are still sent under `test`"
        );

        let summary = Summary::of(&outcomes);
        assert_eq!(
            summary,
            Summary {
                total: 3,
                passed: 2,
                no_response: 1,
                ..Summary::default()
            }
        );
        assert_eq!(summary.exit(), Exit::Failure);
    }

    #[tokio::test]
    async fn a_connection_failure_is_counted_the_same_way_a_substitution_failure_is() {
        // `send` returns `NoResponse` for a refused connection, a DNS failure
        // and a TLS failure alike; this is that path, with the network stubbed
        // out. Same category, same exit code, for the same reason: there is no
        // response to check anything against.
        let document = Document::from_yaml_str(
            "\
requests:
  - name: Fine
    method: GET
    url: '{{base_url}}/fine'
  - name: Unreachable
    method: GET
    url: '{{base_url}}/unreachable'
",
        )
        .unwrap();
        let requests: Vec<&Request> = document.requests().iter().collect();

        let outcomes = run_requests(&requests, &environment(), |request| async move {
            if request.url.ends_with("/unreachable") {
                Outcome::NoResponse
            } else {
                all_passed(200)
            }
        })
        .await;

        let summary = Summary::of(&outcomes);
        assert_eq!(
            summary,
            Summary {
                total: 2,
                passed: 1,
                no_response: 1,
                ..Summary::default()
            }
        );
        assert_eq!(summary.exit(), Exit::Failure);
    }

    #[tokio::test]
    async fn a_single_request_file_is_a_test_run_of_one() {
        // `test` takes the same two shapes `run` does, through the same
        // `Document`, so a file with no `requests` key is a collection of one
        // as far as the summary is concerned.
        let document = Document::from_yaml_str(
            "\
name: Solo
method: GET
url: '{{base_url}}/solo'
assertions:
  status: 200
",
        )
        .unwrap();
        let requests: Vec<&Request> = document.requests().iter().collect();
        assert_eq!(requests.len(), 1);

        let outcomes = run_requests(&requests, &environment(), |_| async { all_passed(200) }).await;

        assert_eq!(
            Summary::of(&outcomes),
            Summary {
                total: 1,
                passed: 1,
                ..Summary::default()
            }
        );
    }

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
