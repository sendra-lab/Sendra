//! The pipeline both subcommands share — config, environment, file, then send
//! each request — and the two command handlers built on top of it.

use std::path::{Path, PathBuf};

use sendra_core::environment::{find_environment, DEFAULT_ENVIRONMENT_NAME};
use sendra_core::script::{run_post_request, run_pre_request, Scripts};
use sendra_core::{Config, Document, Environment, Request, SendraError};

use crate::exit::{exit_for_run, Exit, Outcome, Summary};
use crate::output::{print_environment_error, print_error, Detail, Format, Reporter};

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
///
/// `json` chooses the rendering and nothing else: the same requests are sent in
/// the same order and the exit code is the one this run earned either way. A
/// failure in [`prepare`] returns before the reporter has anything to say, so
/// `--json` writes nothing at all on stdout in that case — the run never
/// started, and the error is on stderr with the exit code that says so.
pub(crate) async fn run(
    path: &Path,
    name: Option<&str>,
    environment_name: Option<&str>,
    allow_error_status: bool,
    json: bool,
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
    let reporter = &Reporter::new(Format::for_json_flag(json), Detail::Full);
    let outcomes = run_requests(&requests, &environment, reporter, |request| async move {
        send(&request, config, reporter).await
    })
    .await;
    reporter.finish_run();

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
///
/// `json` behaves exactly as it does on `run` — see there — with one addition:
/// the document `test` writes carries the [`Summary`] the terminal output ends
/// with. The counts, and the exit code they produce, are the same numbers in
/// both renderings.
pub(crate) async fn test(path: &Path, environment_name: Option<&str>, json: bool) -> Exit {
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
    let reporter = &Reporter::new(Format::for_json_flag(json), Detail::StatusOnly);
    let outcomes = run_requests(&requests, &environment, reporter, |request| async move {
        send(&request, config, reporter).await
    })
    .await;

    let summary = Summary::of(&outcomes);
    reporter.finish_test(&summary);
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
pub(crate) enum EnvironmentError {
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

/// Substitute and send each of `requests` in file order, reporting each result
/// as it arrives and returning what became of every one of them.
///
/// Everything this says out loud goes through `reporter`, which is what decides
/// whether "reporting" means printing as the run goes or collecting a document
/// for the end of it; see [`Reporter`].
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
///
/// [`worst`]: crate::exit::worst
pub(crate) async fn run_requests<S, F>(
    requests: &[&Request],
    environment: &Environment,
    reporter: &Reporter,
    mut send_one: S,
) -> Vec<Outcome>
where
    S: FnMut(Request) -> F,
    F: std::future::Future<Output = Outcome>,
{
    let mut outcomes = Vec::with_capacity(requests.len());

    for (index, request) in requests.iter().enumerate() {
        // Blank line between results so a multi-request run stays readable.
        // Under `--json` there is no such line, and no stdout to put it on.
        if index > 0 {
            reporter.separate();
        }

        let substituted = environment.apply(request);

        // Announced before the outcome either way, because in a collection run
        // the label is the only thing that says *which* request this is — a
        // "no variable named X" message names the variable, not the request.
        // On success the label describes the request as it will actually be
        // sent (a resolved URL, for a request with no `name`); on failure there
        // is no such request, so it falls back to the label as written.
        reporter.request_started(&substituted.as_ref().unwrap_or(request).label());

        outcomes.push(match substituted {
            Ok(request) => send_one(request).await,
            Err(err) => {
                reporter.request_failed(&err);
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
/// # The whole pipeline for one request, in order
///
/// Substitution has happened; the rest is here, and the order is fixed because
/// it decides what an existing file means:
///
/// 1. **Compile both scripts.** Before anything else, and before the wire: a
///    `post_request` script that does not parse stops the `POST` that would
///    have created an order, rather than being discovered after it. A compile
///    failure is [`Outcome::NoResponse`] — nothing was sent, so it is the same
///    category as a missing variable.
/// 2. **Apply the config.** Done here rather than inside `sendra_core::send`,
///    which is what step 3 requires: a `pre_request` script that *removes* a
///    config-injected header only works if nothing re-merges the config after
///    it. That is what [`sendra_core::send_prepared`] is for.
/// 3. **Run `pre_request`**, the last thing to touch the request. A script that
///    throws, or that leaves the request in a state that cannot be sent, is
///    again [`Outcome::NoResponse`]: there is no response and never will be.
/// 4. **Send.**
/// 5. **Run `post_request`** against the response.
/// 6. **Evaluate assertions** against the same response. Whether a script ran,
///    and what it decided, changes nothing about them — the two mechanisms are
///    independent and neither can see the other.
///
/// # Where the results are printed
///
/// Under the response, with the response, so in a collection run each block of
/// results sits with the thing it is about rather than in a summary at the end
/// that would have to name every request again. `sendra test` adds a summary
/// *as well as* these blocks, not instead of them: the counts say how the run
/// went, and these say which check, in which request, was the reason.
///
/// Both subcommands come through here, so both run scripts and evaluate
/// assertions in exactly the same place. What they differ on — how much of the
/// response is shown, and whether it is shown at all or recorded for `--json` —
/// belongs to the [`Reporter`] they were given.
async fn send(request: &Request, config: &Config, reporter: &Reporter) -> Outcome {
    // Nothing goes over the wire until both scripts are known to parse.
    let scripts = match Scripts::compile(request) {
        Ok(scripts) => scripts,
        Err(err) => {
            reporter.request_failed(&err);
            return Outcome::NoResponse;
        }
    };

    // Config first, then the script, then the wire — see the numbered list
    // above for why this is here and not inside `sendra_core::send`.
    let configured = config.apply(request);
    let prepared = match scripts.pre_request() {
        Some(script) => {
            let (result, output) = run_pre_request(script, &configured);

            // Printed before the verdict is acted on, and whether or not the
            // script succeeded: a script that printed and then threw usually
            // printed the reason.
            reporter.script_output(&output);

            match result {
                Ok(prepared) => prepared,
                Err(err) => {
                    reporter.request_failed(&err);
                    return Outcome::NoResponse;
                }
            }
        }
        None => configured,
    };

    match sendra_core::send_prepared(&prepared, config).await {
        Ok(response) => {
            // No `post_request` block is `None`, which prints nothing — a
            // different thing from a script that ran and was happy. Same rule
            // as the empty assertion report below.
            let script = scripts.post_request().map(|script| {
                let (outcome, output) = run_post_request(script, &response);
                reporter.script_output(&output);
                outcome
            });

            // No `assertions` block is the empty report, which prints nothing:
            // a request written before this feature existed looks exactly as it
            // did before it existed.
            let assertions = prepared
                .assertions
                .as_ref()
                .map(|assertions| assertions.evaluate(&response))
                .unwrap_or_default();

            reporter.responded(&response, script.as_ref(), &assertions);

            Outcome::Responded {
                status: response.status,
                script,
                assertions,
            }
        }
        Err(err) => {
            reporter.request_failed(&err);
            Outcome::NoResponse
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use sendra_core::environment::environment_path;

    use std::collections::BTreeMap;

    use sendra_core::ScriptOutcome;

    use crate::exit::exit_for_status;
    use crate::test_support::{all_passed, reporter, responded};

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
        let outcomes = run_requests(&requests, &environment(), &reporter(), |request| {
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
        let outcomes = run_requests(&requests, &environment(), &reporter(), |_| async {
            responded(404)
        })
        .await;

        assert_eq!(exit_for_run(&outcomes, true), Exit::Failure);
    }

    // --- scripts in the pipeline -----------------------------------------

    /// The one request out of a YAML string, for the tests below.
    fn request(yaml: &str) -> Request {
        Document::from_yaml_str(yaml)
            .expect("the test request should parse")
            .requests()[0]
            .clone()
    }

    #[tokio::test]
    async fn a_pre_request_script_runs_after_the_config_and_wins() {
        // The ordering the issue fixes, tested where it is decided: the config
        // default is in place before the script runs, so the script can see it,
        // change it, and — the case that only works because the CLI applies the
        // config itself rather than letting `sendra_core::send` do it — remove
        // it and have it stay removed.
        let config = Config {
            headers: BTreeMap::from([
                ("X-From-Config".to_string(), "config".to_string()),
                ("X-Doomed".to_string(), "config".to_string()),
            ]),
            ..Config::default()
        };
        let request = request(
            "method: GET\nurl: https://example.com\npre_request: |\n  \
             if request.headers[\"X-From-Config\"] != \"config\" { throw \"config had not been applied\"; }\n  \
             request.headers[\"X-From-Config\"] = \"script\";\n  \
             request.headers.remove(\"X-Doomed\");\n",
        );

        let scripts = Scripts::compile(&request).expect("the script compiles");
        let configured = config.apply(&request);
        let (prepared, _) = run_pre_request(scripts.pre_request().unwrap(), &configured);
        let prepared = prepared.expect("the script runs");

        assert_eq!(
            prepared.headers.get("X-From-Config").map(String::as_str),
            Some("script"),
            "the script runs after the config and wins ties with it"
        );
        assert!(
            !prepared.headers.contains_key("X-Doomed"),
            "a header the script removed must stay removed: {:?}",
            prepared.headers
        );
    }

    #[tokio::test]
    async fn script_source_is_never_substituted() {
        // The confirmed design decision, as a test. `{{marker}}` is defined in
        // the environment and `${MARKER}` is not exported anywhere; both appear
        // in the url and in both scripts. Only the url comes back changed.
        let environment = Environment::from_yaml_str("marker: SUBSTITUTED\n").unwrap();
        let request = request(
            "method: GET\nurl: 'https://example.com/{{marker}}'\n\
             pre_request: |\n  request.headers[\"X\"] = \"{{marker}} ${MARKER}\";\n\
             post_request: |\n  if response.body != \"{{marker}}\" { throw \"{{marker}}\"; }\n",
        );

        let substituted = environment
            .apply(&request)
            .expect("the url's variable resolves");

        // The url was substituted...
        assert_eq!(substituted.url, "https://example.com/SUBSTITUTED");
        // ...and neither script was touched. `${MARKER}` is the case that
        // matters most: nothing exports it, so a script that *was* substituted
        // would have failed the whole request with `EnvVarNotSet` rather than
        // quietly changing meaning.
        assert_eq!(substituted.pre_request, request.pre_request);
        assert_eq!(substituted.post_request, request.post_request);
        assert!(
            substituted
                .pre_request
                .as_deref()
                .unwrap()
                .contains("{{marker}}"),
            "the placeholder must survive verbatim: {:?}",
            substituted.pre_request
        );

        // And the script really does see the braces as ordinary text: running
        // it puts the literal placeholder on the wire, not the variable value.
        let scripts = Scripts::compile(&substituted).expect("the script compiles");
        let (prepared, _) = run_pre_request(scripts.pre_request().unwrap(), &substituted);
        let prepared = prepared.expect("it runs");

        assert_eq!(
            prepared.headers.get("X").map(String::as_str),
            Some("{{marker}} ${MARKER}")
        );
    }

    #[tokio::test]
    async fn a_script_that_does_not_compile_means_the_request_is_never_sent() {
        // A compile error in *either* hook is `NoResponse` — including the
        // `post_request` one, which is the whole point of compiling both up
        // front.
        //
        // The url points at a port nothing listens on, so a request that *did*
        // reach the wire would come back `NoResponse` as well. The extra
        // assertion is what tells the two apart: compilation is what failed, so
        // the request was refused before a socket was ever opened.
        for yaml in [
            "method: POST\nurl: https://127.0.0.1:1/\npre_request: |\n  ) (\n",
            "method: POST\nurl: https://127.0.0.1:1/\npost_request: |\n  ) (\n",
        ] {
            let request = request(yaml);

            assert!(
                matches!(
                    Scripts::compile(&request),
                    Err(SendraError::ScriptParse { .. })
                ),
                "{yaml:?} should not have compiled"
            );

            let outcome = send(&request, &Config::default(), &reporter()).await;
            assert!(
                matches!(outcome, Outcome::NoResponse),
                "{yaml:?} produced {outcome:?}"
            );
        }

        // And that is `Exit::Failure` under `run` and `no_response` under
        // `test`: the same category as a missing variable, for the same reason
        // — nothing went over the wire.
        let request = request("method: POST\nurl: https://127.0.0.1:1/\npre_request: |\n  ) (\n");
        let outcomes = vec![send(&request, &Config::default(), &reporter()).await];

        assert_eq!(exit_for_run(&outcomes, false), Exit::Failure);
        assert_eq!(
            Summary::of(&outcomes),
            Summary {
                total: 1,
                no_response: 1,
                ..Summary::default()
            }
        );
    }

    #[tokio::test]
    async fn a_pre_request_script_that_throws_means_the_request_is_never_sent() {
        let request = request(
            "method: POST\nurl: https://127.0.0.1:1/\npre_request: |\n  throw \"no signing key\";\n",
        );
        let outcome = send(&request, &Config::default(), &reporter()).await;

        assert!(matches!(outcome, Outcome::NoResponse), "{outcome:?}");
        assert_eq!(Summary::of(&[outcome]).no_response, 1);
    }

    #[tokio::test]
    async fn a_post_request_script_and_the_assertions_do_not_see_each_other() {
        // The two mechanisms are independent: the same response, evaluated
        // twice, with the script's verdict changing nothing about the
        // assertions' and vice versa.
        let request = request(
            "method: GET\nurl: https://example.com\n\
             assertions:\n  status: 200\n\
             post_request: |\n  throw \"the script is unhappy\";\n",
        );
        let response = crate::test_support::response(200);

        let scripts = Scripts::compile(&request).expect("the script compiles");
        let (script, _) = run_post_request(scripts.post_request().unwrap(), &response);
        let assertions = request.assertions.as_ref().unwrap().evaluate(&response);

        assert_eq!(
            script,
            ScriptOutcome::Failed {
                message: "the script is unhappy".to_string()
            }
        );
        assert!(
            assertions.passed(),
            "the assertion asked for 200 and got one; the script's verdict is not its business"
        );

        // Together they are one failed request, counted once.
        let outcomes = vec![Outcome::Responded {
            status: 200,
            script: Some(script),
            assertions,
        }];
        assert_eq!(Summary::of(&outcomes).failed, 1);
        assert_eq!(Summary::of(&outcomes).exit(), Exit::TestFailed);
        // And `run` still exits 0: the status was fine, and that is all `run`
        // reads.
        assert_eq!(exit_for_run(&outcomes, false), Exit::Ok);
    }

    #[tokio::test]
    async fn a_substitution_failure_outranks_a_bad_status_from_a_sibling() {
        let document = Document::from_yaml_str(COLLECTION_WITH_A_BROKEN_VARIABLE).unwrap();
        let requests: Vec<&Request> = document.requests().iter().collect();

        // Every sibling answers 500, so the run holds both kinds of failure at
        // once. "Never got a response" is the more serious of the two.
        let outcomes = run_requests(&requests, &environment(), &reporter(), |_| async {
            responded(500)
        })
        .await;

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
        let outcomes = run_requests(&[request], &environment(), &reporter(), |request| {
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
        let outcomes = run_requests(&[request], &environment(), &reporter(), |request| {
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
        let outcomes = run_requests(&requests, &environment(), &reporter(), |request| {
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
            let outcomes = run_requests(&requests, &environment, &reporter(), |request| {
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
        let outcomes = run_requests(&requests, &environment(), &reporter(), |request| {
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
        let outcomes = run_requests(&requests, &environment(), &reporter(), |request| {
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

    #[tokio::test]
    async fn a_substitution_failure_is_counted_and_fails_the_test_run() {
        // Through the real loop, not with a hand-built `NoResponse`: the
        // continue-on-failure model is shared with `run`, so the broken request
        // in the middle must still not stop the two around it, and `test` must
        // count it in the category that has no response in it.
        let document = Document::from_yaml_str(COLLECTION_WITH_A_BROKEN_VARIABLE).unwrap();
        let requests: Vec<&Request> = document.requests().iter().collect();

        let mut sent = Vec::new();
        let outcomes = run_requests(&requests, &environment(), &reporter(), |request| {
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

        let outcomes = run_requests(
            &requests,
            &environment(),
            &reporter(),
            |request| async move {
                if request.url.ends_with("/unreachable") {
                    Outcome::NoResponse
                } else {
                    all_passed(200)
                }
            },
        )
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

        let outcomes = run_requests(&requests, &environment(), &reporter(), |_| async {
            all_passed(200)
        })
        .await;

        assert_eq!(
            Summary::of(&outcomes),
            Summary {
                total: 1,
                passed: 1,
                ..Summary::default()
            }
        );
    }
}
