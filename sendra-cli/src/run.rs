//! The pipeline both subcommands share — config, environment, file, then send
//! each request — and the two command handlers built on top of it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sendra_core::config::{find_project_config, global_config_path};
use sendra_core::environment::{find_environment, DEFAULT_ENVIRONMENT_NAME};
use sendra_core::script::{run_post_request, run_pre_request, Scripts};
use sendra_core::{Config, Document, Environment, HttpClient, Request, SendraError};

use crate::cli::OutputMode;
use crate::exit::{exit_for_run, Exit, Outcome, Summary};
use crate::output::{
    print_environment_error, print_error, print_insecure_warning, print_provenance, Format,
    Reporter,
};

/// Everything both subcommands do before the first byte goes out: the config,
/// the HTTP client the whole run sends through, the environment, and the file.
struct Prepared {
    config: Config,
    /// Built once here and borrowed by every send in the run, so a collection
    /// hitting one host reuses its connection instead of handshaking again per
    /// request. See [`sendra_core::build_client`].
    client: HttpClient,
    environment: Environment,
    document: Document,
    /// The project config file this run actually used, or `None` — for
    /// `-v`/`--verbose` alone. Recomputed independently of `config.sources`
    /// (which does not distinguish project from global) via the same
    /// [`find_project_config`] call [`Config::resolve_from`] itself makes,
    /// against the same `start_dir`.
    project_config: Option<PathBuf>,
    /// The global config file this run actually used, or `None` — for
    /// `-v`/`--verbose` alone. See [`project_config`](Self::project_config).
    global_config: Option<PathBuf>,
}

/// Resolve the config, build the client, then resolve the environment and the
/// request file, in that order.
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
/// The client joins them for the same reason and in the same category: it is
/// built from the config, once for the whole run, and a client that cannot be
/// built is not "this request failed" either. Building it here is also what
/// makes the reuse real — one client, borrowed by every send in the
/// invocation, rather than one per request.
///
/// Both subcommands share this because they must: `sendra test` that resolved
/// config differently from `sendra run` would mean a request could pass under
/// one and fail under the other for reasons neither prints.
///
/// `var_overrides` and `timeout_override` are `--var` and `--timeout`: the two
/// CLI overrides that land here rather than in [`send`], because they change
/// *what gets resolved* rather than the already-substituted request `send`
/// sees. `-H`/`--header` is not one of the arguments — see [`send`], where it
/// is applied instead, and the module doc comment for the full precedence
/// chain this and `send` together implement.
///
/// `insecure_override` and `proxy_override` are `--insecure` and `--proxy`:
/// two more CLI overrides that belong here for the same reason `timeout`
/// does, and for no other — both are read only by
/// [`sendra_core::build_client`], never per-request, so both are folded into
/// `config` before the one client this run's every request shares is built,
/// exactly like `timeout_override`. `insecure_override` can only turn
/// `config.insecure` on, never back off, matching `--insecure` being a bare
/// flag with no `--secure` counterpart; `proxy_override`, when present, wins
/// outright, matching `--timeout`.
///
/// `client_cert_override`/`client_key_override` are `--client-cert`/
/// `--client-key`: the same highest-precedence layer, applied the same way
/// and for the same reason — before `build_client` ever sees `config` —
/// but, unlike `insecure_override`/`proxy_override`, resolved *independently*
/// of each other rather than as a pair. Each one, when present, replaces only
/// its own half of `config`'s client certificate, so `--client-cert` on the
/// command line can pair with a `client_cert.key` a config file set (or vice
/// versa) — [`sendra_core::build_client`] is what actually requires both
/// halves to be present together, once every source has had its say, not
/// this function. Resolved relative to the current working directory — see
/// [`Config::client_cert`](sendra_core::Config::client_cert)'s doc comment
/// for why that differs from the config-file form's own resolution rule.
///
/// `cookie_jar_override` is `--cookie-jar`: the same highest-precedence
/// layer as `insecure_override`, folded into `config` the same way and for
/// the same reason — before `build_client` ever sees it. Like
/// `insecure_override`, it can only turn `config.cookie_jar` on, never back
/// off, matching `--cookie-jar` being a bare flag with no
/// `--no-cookie-jar` counterpart.
#[allow(clippy::too_many_arguments)]
fn prepare(
    path: &Path,
    environment_name: Option<&str>,
    var_overrides: &[(String, String)],
    timeout_override: Option<u64>,
    insecure_override: bool,
    proxy_override: Option<&str>,
    client_cert_override: Option<&Path>,
    client_key_override: Option<&Path>,
    cookie_jar_override: bool,
) -> Result<Prepared, Exit> {
    // Computed first, and reused below for both config and the environment,
    // rather than each resolving the working directory on its own: one
    // `current_dir()` call means `-v`/`--verbose`'s report of *which* project
    // config was found is guaranteed to be the same search `Config` itself
    // just ran, not a second call that could in principle see a different
    // directory. The walk-up looking for the environment needs `start_dir`
    // explicitly anyway, because a `--env` that finds nothing has to be able
    // to say *where* it looked; `CurrentDir` is the same error core would
    // have raised for the same reason.
    let start_dir = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(err) => {
            print_error(&SendraError::CurrentDir(err));
            return Err(Exit::Failure);
        }
    };

    // Recomputed independently of `Config::resolve_from`'s own internal
    // search — see `Prepared::project_config` — rather than threading
    // provenance out of core: `find_project_config`/`global_config_path` are
    // already `pub`, and a second, cheap call here keeps `Config` itself
    // free of a field that only `-v` ever reads.
    let project_config = find_project_config(&start_dir);
    let global_config = global_config_path().filter(|path| path.is_file());

    // Resolved once for the whole run: every request in a collection is sent
    // under the same defaults, and a broken config file stops the run instead
    // of failing partway through it.
    let mut config = match Config::resolve_from(&start_dir, global_config.as_deref()) {
        Ok(config) => config,
        Err(err) => {
            print_error(&err);
            return Err(Exit::Failure);
        }
    };

    // `--timeout` wins over both the global and the project config — the
    // highest-precedence layer, applied last, exactly like `--var` below and
    // `-H` in `send`. Overwriting the resolved `Config` here, before
    // `build_client` ever sees it, means the override reaches the one place
    // a timeout actually takes effect without `build_client` needing to know
    // CLI overrides exist at all.
    if let Some(seconds) = timeout_override {
        config.timeout = Duration::from_secs(seconds);
    }

    // `--insecure`/`--proxy`, the same highest-precedence layer as
    // `--timeout` above, applied the same way and for the same reason —
    // before `build_client` ever sees `config`. See the doc comment on this
    // function's parameters.
    if insecure_override {
        config.insecure = true;
    }
    if let Some(url) = proxy_override {
        config.proxy = Some(url.to_string());
    }
    if let Some(path) = client_cert_override {
        config.client_cert = Some(path.to_path_buf());
    }
    if let Some(path) = client_key_override {
        config.client_key = Some(path.to_path_buf());
    }
    if cookie_jar_override {
        config.cookie_jar = true;
    }

    // One client for the whole invocation: every request below sends through
    // this one, so the connection the first request opens is the connection the
    // rest of them use.
    let client = match sendra_core::build_client(&config) {
        Ok(client) => client,
        Err(err) => {
            print_error(&err);
            return Err(Exit::Failure);
        }
    };

    let mut environment = match environment_for(&start_dir, environment_name) {
        Ok(environment) => environment,
        Err(err) => {
            print_environment_error(&err);
            return Err(Exit::Failure);
        }
    };

    // `--var` overrides the environment file's own value for the same name —
    // "most specific wins", the same reasoning `--timeout` above and `-H` in
    // `send` both follow. It is folded directly into `variables` rather than
    // kept as a separate layer, which is a deliberate choice and not just a
    // shortcut: every consumer of `Environment` — substitution, and the
    // shadow check a `capture` block runs — already treats `variables` as
    // "what the active environment defines", and a `--var` value is exactly
    // that for this one invocation. The consequence, stated plainly: a
    // `capture` naming a variable a `--var` already set is refused the same
    // way one naming a file-defined variable is, as
    // `CaptureFailure::Shadowed`. That is not a special case added for
    // `--var` — it falls out of `--var` being indistinguishable, by the time
    // anything downstream looks, from a value the file itself defined.
    for (name, value) in var_overrides {
        environment.variables.insert(name.clone(), value.clone());
    }

    let document = match Document::from_path(path) {
        Ok(document) => document,
        Err(err) => {
            print_error(&err);
            return Err(Exit::Failure);
        }
    };

    Ok(Prepared {
        config,
        client,
        environment,
        document,
        project_config,
        global_config,
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
///
/// `show_captures` reaches only the `--json` document, and only its
/// `capture.values` — see [`Reporter`] and `output::json::CaptureRecord`. It
/// changes nothing about which requests are sent or what they do.
///
/// `headers`, `vars` and `timeout` are `-H`, `--var` and `--timeout` — the
/// highest-precedence layer, applied last, over everything below it. See the
/// module doc comment for the full chain and [`prepare`]/[`send`] for where
/// each one is actually applied.
///
/// `repeat` is `--repeat`: the whole selected run — every request in
/// `requests` — sent again, sequentially, this many times, `1` (the default)
/// being today's single pass. Each pass is its own [`run_requests`] call, so
/// each starts with an empty capture store — see that function's doc comment
/// — and every pass's outcomes are folded into the same `exit_for_run` call
/// at the end, worst-wins across every request in every pass, exactly as
/// `worst` already folds every request within one pass.
///
/// `dry_run` is `--dry-run`: every resolution step still runs — substitution
/// in [`run_requests`], then config/`-H`/`pre_request` in
/// [`prepare_request`] — and the pipeline stops there instead of calling
/// [`send`]. `run`-only; see the module doc comment on why `test` does not
/// offer it. It changes nothing about *which* requests are selected or how
/// they are resolved, only whether the last step — the actual network call —
/// happens, which is why it is threaded through as one more argument to the
/// same loop rather than a separate code path.
///
/// `output` is `-o`/`--output`: `None` (the flag was omitted) keeps `run`'s
/// long-standing default of [`OutputMode::Full`]; `Some(mode)` is the
/// caller's explicit choice. `main` has already refused `-o status` together
/// with `dry_run`, and `output` together with `json`, before this is called —
/// see `reject_output_status_with_dry_run` and `reject_output_with_json`.
///
/// `quiet` is `-q`/`--quiet`. `main` has already folded its `-o none`
/// implication into `output` — see `output`'s own doc above and
/// `reject_quiet_with_output` for the case where an explicit `output`
/// disagreed — so what reaches here is only the half `output` cannot
/// express: suppressing the `→` labels via [`Reporter::with_quiet`].
///
/// `verbose` is `-v`/`--verbose`: `main` has already refused it together
/// with `quiet` — see `reject_verbose_with_quiet` — so the two are never
/// both true here. When true, [`prepare`]'s provenance
/// (`project_config`/`global_config`/`environment.source`) is printed once,
/// via `output::print_provenance`, before the sending loop starts —
/// stderr-only and unaffected by `json`, per its own doc comment in
/// `cli.rs`.
///
/// `insecure` and `proxy` are `--insecure` and `--proxy`, folded into
/// `config` inside [`prepare`] before the client is built — see there for
/// how each wins over its config-file counterpart. Whenever the *resolved*
/// `config.insecure` — from either source, not only the flag — comes back
/// `true`, [`output::print_insecure_warning`] prints once, unconditionally,
/// before the sending loop starts: unlike `verbose`'s provenance just above,
/// this is not gated behind a flag of its own and is not suppressed by
/// `quiet` — see that function's own doc comment for why.
///
/// `client_cert`/`client_key` are `--client-cert`/`--client-key`, folded into
/// `config` inside [`prepare`] the same way `insecure`/`proxy` are — see
/// there. Presenting a client certificate is not a security downgrade the
/// way `--insecure` is, so unlike `insecure` it prints no warning of its own,
/// gated or not.
///
/// `cookie_jar` is `--cookie-jar`, folded into `config` inside [`prepare`]
/// the same way `insecure`/`proxy` are — see there. **Interacts with
/// `repeat`**: when the resolved `config.cookie_jar` is true, the client is
/// rebuilt at the start of every pass after the first, so each pass starts
/// with an empty jar — the same "each repeat is a clean run" rule
/// `run_requests`'s capture store already follows, and the reason it needs
/// stating here at all is that reqwest exposes no way to clear a jar in
/// place; the client that owns it has to be rebuilt instead. When
/// `cookie_jar` is false this changes nothing: the one client [`prepare`]
/// built is reused for every pass, exactly as before this flag existed.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    path: &Path,
    name: Option<&str>,
    environment_name: Option<&str>,
    headers: &[(String, String)],
    vars: &[(String, String)],
    timeout: Option<u64>,
    repeat: u32,
    insecure: bool,
    proxy: Option<&str>,
    client_cert: Option<&Path>,
    client_key: Option<&Path>,
    cookie_jar: bool,
    allow_error_status: bool,
    json: bool,
    show_captures: bool,
    dry_run: bool,
    output: Option<OutputMode>,
    quiet: bool,
    verbose: bool,
) -> Exit {
    let Prepared {
        config,
        client,
        environment,
        document,
        project_config,
        global_config,
    } = match prepare(
        path,
        environment_name,
        vars,
        timeout,
        insecure,
        proxy,
        client_cert,
        client_key,
        cookie_jar,
    ) {
        Ok(prepared) => prepared,
        Err(exit) => return exit,
    };

    if verbose {
        print_provenance(
            project_config.as_deref(),
            global_config.as_deref(),
            environment_name.unwrap_or(DEFAULT_ENVIRONMENT_NAME),
            environment.source.as_deref(),
            vars,
            headers,
        );
    }
    if config.insecure {
        print_insecure_warning();
    }

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
    let mut client = client;
    let reporter = &Reporter::new(
        Format::for_json_flag(json),
        output.unwrap_or(OutputMode::Full),
        show_captures,
    )
    .with_quiet(quiet);

    // `--repeat`: one full sequential pass over `requests` per iteration,
    // each through its own `run_requests` call so its capture store starts
    // empty — see the module doc comment on `run_requests` and `repeat`'s own
    // doc comment in `cli.rs`. `repeat == 1` (the default) is exactly the
    // loop this always was, one pass, with `reporter.set_iteration` clearing
    // the label suffix rather than adding "(iteration 1 of 1)" noise nothing
    // asked for.
    let mut outcomes = Vec::new();
    for iteration in 1..=repeat {
        // See this function's doc comment on `cookie_jar`: rebuilding is the
        // only way to give this pass an empty jar, since reqwest exposes no
        // way to clear one in place. Skipped on the first pass — the client
        // `prepare` just built already has an empty jar — and skipped
        // entirely when the jar is off, so nothing here changes for a run
        // that never asked for `--cookie-jar`.
        if iteration > 1 && config.cookie_jar {
            client = match sendra_core::build_client(config) {
                Ok(client) => client,
                Err(err) => {
                    print_error(&err);
                    return Exit::Failure;
                }
            };
        }
        let client = &client;

        if iteration > 1 {
            reporter.separate();
        }
        reporter.set_iteration(iteration as usize, repeat as usize);
        outcomes.extend(
            run_requests(
                &requests,
                base_dir(path),
                &environment,
                reporter,
                |request, environment| async move {
                    if dry_run {
                        dry_run_one(&request, config, headers, reporter)
                    } else {
                        send(&request, client, config, &environment, headers, reporter).await
                    }
                },
            )
            .await,
        );
    }
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
/// **Takes an optional request name, exactly like `run`.** Omit it and every
/// request in the file is tested, in file order, same as before; name one and
/// only it is tested, with the same `RequestNotFound` error `run` already
/// raises for the equivalent case when the name does not exist.
///
/// `json` behaves exactly as it does on `run` — see there — with one addition:
/// the document `test` writes carries the [`Summary`] the terminal output ends
/// with. The counts, and the exit code they produce, are the same numbers in
/// both renderings.
///
/// `junit` is `--junit <path>`: a JUnit XML report written to that path, in
/// addition to whatever `json` already chose — a `--junit` run with no
/// `--json` still prints the ordinary terminal output, since someone reading
/// a CI log wants both the machine-readable report and something to read
/// when it fails. `run` has no verdict for a JUnit report to carry, so the
/// flag is `test`'s alone; see [`Reporter::with_junit`].
///
/// `output` behaves exactly as it does on `run` — see there — except `None`
/// (the flag was omitted) keeps `test`'s long-standing default of
/// [`OutputMode::Status`] rather than `run`'s [`OutputMode::Full`].
///
/// `quiet` behaves exactly as it does on `run` — see there.
///
/// `verbose` behaves exactly as it does on `run` — see there.
///
/// `repeat` behaves exactly as it does on `run` — see there — with one
/// addition: [`Summary::of`] is called once, over every pass's outcomes
/// concatenated together, so the printed counts and the `--json` `summary`
/// object both cover every request in every pass rather than only the last.
///
/// `insecure` and `proxy` behave exactly as they do on `run` — see there —
/// including the unconditional, `-q`-immune warning whenever the resolved
/// `config.insecure` comes back true.
///
/// `client_cert`/`client_key` behave exactly as they do on `run` — see there.
///
/// `cookie_jar` behaves exactly as it does on `run` — see there, including
/// the per-pass client rebuild under `--repeat`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn test(
    path: &Path,
    name: Option<&str>,
    environment_name: Option<&str>,
    headers: &[(String, String)],
    vars: &[(String, String)],
    timeout: Option<u64>,
    repeat: u32,
    insecure: bool,
    proxy: Option<&str>,
    client_cert: Option<&Path>,
    client_key: Option<&Path>,
    cookie_jar: bool,
    json: bool,
    show_captures: bool,
    junit: Option<PathBuf>,
    output: Option<OutputMode>,
    quiet: bool,
    verbose: bool,
) -> Exit {
    let Prepared {
        config,
        client,
        environment,
        document,
        project_config,
        global_config,
    } = match prepare(
        path,
        environment_name,
        vars,
        timeout,
        insecure,
        proxy,
        client_cert,
        client_key,
        cookie_jar,
    ) {
        Ok(prepared) => prepared,
        Err(exit) => return exit,
    };

    if verbose {
        print_provenance(
            project_config.as_deref(),
            global_config.as_deref(),
            environment_name.unwrap_or(DEFAULT_ENVIRONMENT_NAME),
            environment.source.as_deref(),
            vars,
            headers,
        );
    }
    if config.insecure {
        print_insecure_warning();
    }

    let requests: Vec<&Request> = match name {
        Some(name) => match document.get(name) {
            Ok(request) => vec![request],
            Err(err) => {
                print_error(&err);
                return Exit::Failure;
            }
        },
        // No name: every request in the file, in file order — a
        // single-request file is a run of one.
        None => document.requests().iter().collect(),
    };

    let config = &config;
    let mut client = client;
    let mut reporter = Reporter::new(
        Format::for_json_flag(json),
        output.unwrap_or(OutputMode::Status),
        show_captures,
    )
    .with_quiet(quiet);
    if let Some(path) = junit {
        reporter = reporter.with_junit(path);
    }
    let reporter = &reporter;

    // `--repeat`: see the identical loop, and its doc comment, in `run` —
    // including the per-pass client rebuild that resets the cookie jar.
    let mut outcomes = Vec::new();
    for iteration in 1..=repeat {
        if iteration > 1 && config.cookie_jar {
            client = match sendra_core::build_client(config) {
                Ok(client) => client,
                Err(err) => {
                    print_error(&err);
                    return Exit::Failure;
                }
            };
        }
        let client = &client;

        if iteration > 1 {
            reporter.separate();
        }
        reporter.set_iteration(iteration as usize, repeat as usize);
        outcomes.extend(
            run_requests(
                &requests,
                base_dir(path),
                &environment,
                reporter,
                |request, environment| async move {
                    send(&request, client, config, &environment, headers, reporter).await
                },
            )
            .await,
        );
    }

    let summary = Summary::of(&outcomes);
    reporter.finish_test(&summary);
    summary.exit()
}

/// The directory `body_file` and multipart file paths in requests loaded from
/// `path` resolve relative to: `path`'s own directory, not the process's
/// current working directory.
///
/// A request file is something a user runs from anywhere — `sendra run
/// requests/create-user.yaml` from a repository root — and a `body_file:
/// ./payload.json` written inside `create-user.yaml` means the file beside
/// it, not one resolved against wherever the command was typed. `path` with
/// no parent (a bare filename, or `/`) resolves against `.`, which is the
/// same thing a bare filename already means to `path` itself.
fn base_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
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
/// *this* request could not be completed. A run treats that the same way it
/// treats any other per-request failure — the sibling requests are still
/// sent, every result is still printed, and the aggregate decides the exit
/// code — since there is no reason a variable should be the one failure that
/// also cancels the requests around it. Checking
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
/// directories as arguments instead of reading the real ones. It is handed the
/// environment view as well as the request, because a capture is evaluated
/// against the environment it might collide with; see below.
///
/// # The accumulating store
///
/// This loop owns one `captured` map and grows it as it advances. Before each
/// request it builds a **view** — [`Environment::with_captured`] — from the
/// store *as it stands at that moment*, substitutes against the view, and after
/// the request comes back merges whatever its `capture` block produced into the
/// store for the requests still to come.
///
/// That arrangement, rather than a mutable `Environment` threaded through, is
/// what makes file order real order structurally instead of by convention:
///
/// - Request 3 sees what requests 1 and 2 captured, because the store held them
///   when its view was built.
/// - Request 1 cannot see anything request 2 captured, because there is no
///   `&mut Environment` for a later merge to reach an earlier substitution
///   through — the view request 1 used is a value, already consumed.
/// - The `Environment` this function was handed is never modified at all, so
///   the caller's environment means the same thing after the run as before it.
///
/// A later capture of a name an earlier one already took **overwrites** it, and
/// that is the only thing an ordered store can mean: re-logging in, or reading
/// the next page's cursor, is a real flow, and "the most recent value wins" is
/// exactly what the file order already says. A capture colliding with the
/// *environment file* is the case that is refused instead — see
/// [`CaptureFailure::Shadowed`](sendra_core::CaptureFailure::Shadowed) — because
/// there the two names come from different kinds of source, and letting one win
/// would make the same `{{name}}` mean the file's value before the capturing
/// request and the captured value after it.
///
/// A request whose capture failed contributes nothing, so the downstream
/// `{{name}}` is a `VariableNotFound` naming the variable rather than a request
/// sent with an empty string in it. That downstream failure is a real second
/// failure and the run keeps going through it, exactly as it does for every
/// other per-request problem; the original capture failure is reported at its
/// own request, which is what stops the later one from being the only clue.
///
/// [`worst`]: crate::exit::worst
pub(crate) async fn run_requests<S, F>(
    requests: &[&Request],
    base_dir: &Path,
    environment: &Environment,
    reporter: &Reporter,
    mut send_one: S,
) -> Vec<Outcome>
where
    S: FnMut(Request, Environment) -> F,
    F: std::future::Future<Output = Outcome>,
{
    let mut outcomes = Vec::with_capacity(requests.len());

    // The store. Empty at the start of every invocation: captures do not
    // persist across processes, and nothing here reads or writes a file.
    let mut captured: BTreeMap<String, String> = BTreeMap::new();

    for (index, request) in requests.iter().enumerate() {
        // Blank line between results so a multi-request run stays readable.
        // Under `--json` there is no such line, and no stdout to put it on.
        if index > 0 {
            reporter.separate();
        }

        // Built here, per request, from the store as it stands now — which is
        // the whole of the ordering guarantee. See the note above.
        let environment = environment.with_captured(&captured);
        // Substitution, then — while `{{var}}`s are already resolved but
        // before the config or a `pre_request` script ever sees the request —
        // merging `query` into `url`, resolving whichever of
        // `body`/`json`/`body_file`/`form`/`multipart` was set down to the
        // final `body` string, and resolving `auth` down to a plain
        // `Authorization` header. All failures are the same category: the
        // request could not be built, so there is nothing to send. See
        // `Request::resolve_query`, `Request::resolve_body` and
        // `Request::resolve_auth`.
        let substituted = environment
            .apply(request)
            .and_then(|request| request.resolve_query())
            .and_then(|request| request.resolve_body(base_dir))
            .and_then(|request| request.resolve_auth());

        // Announced before the outcome either way, because in a collection run
        // the label is the only thing that says *which* request this is — a
        // "no variable named X" message names the variable, not the request.
        // On success the label describes the request as it will actually be
        // sent (a resolved URL, for a request with no `name`); on failure there
        // is no such request, so it falls back to the label as written.
        reporter.request_started(&substituted.as_ref().unwrap_or(request).label());

        let outcome = match substituted {
            Ok(request) => send_one(request, environment).await,
            Err(err) => {
                // Never reached the network at all, so there is nothing for
                // `retry` to have widened — one attempt, the same as every
                // other pre-send failure below.
                reporter.request_failed(&err, 1);
                Outcome::NoResponse
            }
        };

        // Merged after the request is over, so nothing it captured could have
        // reached its own substitution.
        if let Outcome::Responded { capture, .. } = &outcome {
            captured.extend(capture.values());
        }

        outcomes.push(outcome);
    }

    outcomes
}

/// Everything done to a request before it is ready for the wire: compile both
/// scripts, apply the config, apply `-H`/`--header` overrides, run
/// `pre_request`. Shared by [`send`] and [`dry_run_one`], which is exactly
/// this and nothing else — `--dry-run` stops precisely where this function
/// already stops, one step before [`sendra_core::send_prepared`].
///
/// The request arrives already substituted, with `query`/`body`/`auth`
/// already resolved — that happened earlier, in [`run_requests`].
///
/// # The order, and why it is fixed
///
/// 1. **Compile both scripts.** Before anything else, and before the wire: a
///    `post_request` script that does not parse stops the `POST` that would
///    have created an order, rather than being discovered after it. A compile
///    failure is `None` — nothing was sent, so it is the same category as a
///    missing variable.
/// 2. **Apply the config.** Done here rather than inside `sendra_core::send`,
///    which is what step 3 requires: a `pre_request` script that *removes* a
///    config-injected header only works if nothing re-merges the config after
///    it. That is what [`sendra_core::send_prepared`] is for.
/// 3. **Apply `-H`/`--header` overrides.** Right after the config, and for the
///    same structural reason: this is the layer nothing above it should be
///    able to re-add over. By this point the request already carries its own
///    `headers:`, whatever config added, and the `Authorization` header a
///    resolved `auth:` produced — so applying `-H` here, replacing any header
///    of the same name (case-insensitively) rather than adding to it, is what
///    makes it the highest-precedence layer the module doc comment promises.
///    A `pre_request` script still runs after this and can still change or
///    remove what `-H` set, the same way it can a config header — `-H` is not
///    given special protection from a script the request file itself wrote.
/// 4. **Run `pre_request`**, the last thing to touch the request. A script that
///    throws, or that leaves the request in a state that cannot be sent, is
///    again `None`: there is no response and never will be.
///
/// Returns the compiled [`Scripts`] alongside the request because [`send`]
/// still needs `post_request` out of them; [`dry_run_one`] just drops that
/// half — a `post_request` script has no response to run against, so it never
/// runs under `--dry-run`, and that is true simply because nothing after this
/// function ever calls it, not because of anything special-cased here.
fn prepare_request(
    request: &Request,
    config: &Config,
    header_overrides: &[(String, String)],
    reporter: &Reporter,
) -> Option<(Request, Scripts)> {
    // Nothing goes over the wire until both scripts are known to parse.
    let scripts = match Scripts::compile(request) {
        Ok(scripts) => scripts,
        Err(err) => {
            reporter.request_failed(&err, 1);
            return None;
        }
    };

    // Config, then `-H`, then the script — see the numbered list above for
    // why each is here and not inside `sendra_core::send`.
    let configured = config.apply(request);
    let configured = apply_header_overrides(configured, header_overrides);
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
                    reporter.request_failed(&err, 1);
                    return None;
                }
            }
        }
        None => configured,
    };

    Some((prepared, scripts))
}

/// Resolve one request through every step short of the network call, and
/// report the result instead of sending it — `--dry-run`'s whole
/// contribution to the pipeline.
///
/// Everything before this is unchanged: substitution and `query`/`body`/`auth`
/// resolution already happened in [`run_requests`], and [`prepare_request`]
/// runs the config, `-H` and `pre_request` exactly as [`send`] does. A
/// resolution failure at any of those steps is reported the same way it would
/// be without the flag, via [`Outcome::NoResponse`] — `--dry-run` changes
/// nothing about what counts as a failure, only what happens after success.
fn dry_run_one(
    request: &Request,
    config: &Config,
    header_overrides: &[(String, String)],
    reporter: &Reporter,
) -> Outcome {
    match prepare_request(request, config, header_overrides, reporter) {
        Some((prepared, _scripts)) => {
            reporter.dry_run(&prepared);
            Outcome::DryRun
        }
        None => Outcome::NoResponse,
    }
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
/// # The pipeline for one request, in order
///
/// Everything up to and including `pre_request` is [`prepare_request`], shared
/// with `--dry-run`'s [`dry_run_one`]. What is left here is the half that only
/// runs once something is actually going over the wire:
///
/// 5. **Send — and, on a true failure (no response at all), retry.** A
///    request's `retry` block (see [`Request::retry`](sendra_core::Request::retry))
///    allows up to `count` further attempts, each after `delay_ms` of fixed
///    delay, all sequentially through the one shared client. Only the last
///    attempt's result reaches the steps below; every earlier failure is
///    announced via [`Reporter::retrying`] and then discarded.
/// 6. **Run `post_request`** against the response.
/// 7. **Evaluate assertions** against the same response. Whether a script ran,
///    and what it decided, changes nothing about them — the two mechanisms are
///    independent and neither can see the other.
/// 8. **Evaluate the `capture` block** against the same response, last, because
///    it is the only step that is about the requests *after* this one rather
///    than about this one. Independent of the two above in both directions: a
///    failed assertion does not stop a capture, and a failed capture does not
///    fail an assertion. `environment` is read here and nowhere else in this
///    function — a capture whose name the environment already defines is
///    refused rather than allowed to shadow it, which is a fact about the pair
///    and so has to be decided where both are in hand.
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
async fn send(
    request: &Request,
    client: &HttpClient,
    config: &Config,
    environment: &Environment,
    header_overrides: &[(String, String)],
    reporter: &Reporter,
) -> Outcome {
    let Some((prepared, scripts)) = prepare_request(request, config, header_overrides, reporter)
    else {
        return Outcome::NoResponse;
    };

    // The run's one client, not a new one: see [`prepare`]. `retry` widens
    // this from one send to up to `count + 1`, all through that same client,
    // one after another — never concurrently, which is what lets the
    // client's redirect log stay a single shared `Arc<Mutex<...>>` with no
    // change here at all. See `Request::retry` for the reporting rule this
    // implements: every attempt but the last is discarded once one succeeds,
    // and only announced via `reporter.retrying` for visibility.
    let attempts = prepared
        .retry
        .as_ref()
        .map_or(1, |retry| retry.count as usize + 1);
    let delay_ms = prepared
        .retry
        .as_ref()
        .and_then(|retry| retry.delay_ms)
        .unwrap_or(0);

    let mut attempt = 1;
    let result = loop {
        match sendra_core::send_prepared(&prepared, client).await {
            Ok(response) => break Ok(response),
            Err(err) if attempt < attempts => {
                reporter.retrying(attempt, attempts, &err);
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                attempt += 1;
            }
            Err(err) => break Err(err),
        }
    };

    match result {
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

            // No `capture` block is the empty report, which prints nothing —
            // the same rule as the assertion report above.
            let capture = prepared
                .capture
                .as_ref()
                .map(|capture| capture.evaluate(&response, environment))
                .unwrap_or_default();

            // `attempt` is the loop variable above, left at whichever attempt
            // this response came back on — 1 when `retry` was never
            // configured or never needed, up to `attempts` when every retry
            // was used. See `Reporter::responded`'s doc comment for how this
            // number reaches `--json`/`--junit`.
            reporter.responded(&response, script.as_ref(), &assertions, &capture, attempt);

            Outcome::Responded {
                status: response.status,
                script,
                assertions,
                capture,
            }
        }
        Err(err) => {
            // Same `attempt` reading as above: here it is the total number of
            // attempts made before giving up, i.e. `attempts` itself.
            reporter.request_failed(&err, attempt);
            Outcome::NoResponse
        }
    }
}

/// Apply `-H`/`--header` overrides to `request`, returning the request as it
/// will be sent.
///
/// For each override, in the order given on the command line, every header of
/// the same name (**compared case-insensitively**, matching
/// [`Config::apply`]) is dropped and the override's value is pushed in its
/// place. Two consequences fall out of doing it this way, both deliberate:
///
/// - It replaces rather than adds, so it overrides — not merges with — a
///   header the request, the config, or a resolved `auth:` already set,
///   including a repeated header the request file wrote more than once. That
///   is what makes `-H` a genuine override rather than one more source a
///   header can repeat from.
/// - Applying overrides in order means passing the same name to `-H` more
///   than once keeps only the *last* value: the second override drops the
///   header the first one just added before pushing its own. A repeated `-H`
///   reads as "I meant to change it", not as a deliberate multi-value header,
///   which is the opposite of what a repeated `headers:` entry in a request
///   file means — that one is authored once and meant to stay a list.
fn apply_header_overrides(mut request: Request, overrides: &[(String, String)]) -> Request {
    for (name, value) in overrides {
        request
            .headers
            .retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
        request.headers.push((name.clone(), value.clone()));
    }
    request
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use sendra_core::{AssertionReport, CaptureReport, ScriptOutcome};

    use crate::exit::exit_for_status;
    use crate::test_support::{all_passed, client, dry_run_reporter, reporter, responded};

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

    /// The one request out of a YAML string, for the tests below.
    fn request(yaml: &str) -> Request {
        Document::from_yaml_str(yaml)
            .expect("the test request should parse")
            .requests()[0]
            .clone()
    }

    mod substitution {
        use super::*;

        #[tokio::test]
        async fn a_broken_variable_does_not_stop_the_requests_after_it() {
            let document = Document::from_yaml_str(COLLECTION_WITH_A_BROKEN_VARIABLE).unwrap();
            let requests: Vec<&Request> = document.requests().iter().collect();

            // Stands in for the network: records what it was handed and reports a
            // clean response, so the substitution is the only failure in the run.
            let mut sent = Vec::new();
            let outcomes = run_requests(
                &requests,
                Path::new("."),
                &environment(),
                &reporter(),
                |request, _| {
                    sent.push(request.url.clone());
                    async { responded(200) }
                },
            )
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
            let outcomes = run_requests(
                &requests,
                Path::new("."),
                &environment(),
                &reporter(),
                |_, _| async { responded(404) },
            )
            .await;

            assert_eq!(exit_for_run(&outcomes, true), Exit::Failure);
        }

        #[tokio::test]
        async fn a_substitution_failure_outranks_a_bad_status_from_a_sibling() {
            let document = Document::from_yaml_str(COLLECTION_WITH_A_BROKEN_VARIABLE).unwrap();
            let requests: Vec<&Request> = document.requests().iter().collect();

            // Every sibling answers 500, so the run holds both kinds of failure at
            // once. "Never got a response" is the more serious of the two.
            let outcomes = run_requests(
                &requests,
                Path::new("."),
                &environment(),
                &reporter(),
                |_, _| async { responded(500) },
            )
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
            let outcomes = run_requests(
                &[request],
                Path::new("."),
                &environment(),
                &reporter(),
                |request, _| {
                    sent.push(request.url.clone());
                    async { responded(200) }
                },
            )
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
            let outcomes = run_requests(
                &[request],
                Path::new("."),
                &environment(),
                &reporter(),
                |request, _| {
                    sent.push(request.url.clone());
                    async { responded(200) }
                },
            )
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
            let outcomes = run_requests(
                &requests,
                Path::new("."),
                &environment(),
                &reporter(),
                |request, _| {
                    sent.push(request.url.clone());
                    async { responded(200) }
                },
            )
            .await;

            assert_eq!(
                sent,
                vec!["https://example.com/first", "https://example.com/second"]
            );
            assert_eq!(exit_for_run(&outcomes, false), Exit::Ok);
        }
    }

    mod scripts {
        use super::*;

        #[tokio::test]
        async fn a_pre_request_script_sees_the_resolved_body_regardless_of_which_field_set_it() {
            // The design decision, tested end to end minus the socket: by the
            // time a `pre_request` script runs, `resolve_body`
            // (called inside `run_requests`, before this closure) has already
            // turned `json:` into the plain `body` string a script has always
            // seen. The script here checks that string and mutates it, exactly
            // as it would for a request that had written `body:` by hand.
            let document = Document::from_yaml_str(
                "method: POST\nurl: https://example.com\n\
                 json:\n  id: 1\n\
                 pre_request: |\n  \
                 if request.body != \"{\\\"id\\\":1}\" { throw \"unexpected body: \" + request.body; }\n  \
                 request.body = request.body + \"!\";\n",
            )
            .unwrap();
            let requests: Vec<&Request> = document.requests().iter().collect();

            let mut seen_body = None;
            let outcomes = run_requests(
                &requests,
                Path::new("."),
                &environment(),
                &reporter(),
                |request, _| {
                    // `json` is already `None` here — resolved away before this
                    // closure runs, the same way `send` in the real pipeline
                    // only ever sees a request past that point.
                    assert!(
                        request.json.is_none(),
                        "`json` must already be resolved by the time a script can run"
                    );
                    let scripts = Scripts::compile(&request).expect("the script compiles");
                    let (prepared, _) = run_pre_request(scripts.pre_request().unwrap(), &request);
                    let prepared = prepared.expect("the script runs and does not throw");
                    seen_body = Some(prepared.body.clone());
                    async { responded(200) }
                },
            )
            .await;

            assert_eq!(exit_for_run(&outcomes, false), Exit::Ok);
            assert_eq!(seen_body, Some(Some("{\"id\":1}!".to_string())));
        }

        #[tokio::test]
        async fn a_pre_request_script_runs_after_the_config_and_wins() {
            // The ordering, tested where it is decided: the config default is
            // in place before the script runs, so the script can see it,
            // change it, and — the case that only works because the CLI
            // applies the config itself rather than letting
            // `sendra_core::send` do it — remove it and have it stay removed.
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
                prepared.header("X-From-Config"),
                Some("script"),
                "the script runs after the config and wins ties with it"
            );
            assert!(
                prepared.header("X-Doomed").is_none(),
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

            assert_eq!(prepared.header("X"), Some("{{marker}} ${MARKER}"));
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

                let outcome = send(
                    &request,
                    &client(),
                    &Config::default(),
                    &Environment::default(),
                    &[],
                    &reporter(),
                )
                .await;
                assert!(
                    matches!(outcome, Outcome::NoResponse),
                    "{yaml:?} produced {outcome:?}"
                );
            }

            // And that is `Exit::Failure` under `run` and `no_response` under
            // `test`: the same category as a missing variable, for the same reason
            // — nothing went over the wire.
            let request =
                request("method: POST\nurl: https://127.0.0.1:1/\npre_request: |\n  ) (\n");
            let outcomes = vec![
                send(
                    &request,
                    &client(),
                    &Config::default(),
                    &Environment::default(),
                    &[],
                    &reporter(),
                )
                .await,
            ];

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
            let outcome = send(
                &request,
                &client(),
                &Config::default(),
                &Environment::default(),
                &[],
                &reporter(),
            )
            .await;

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
                capture: CaptureReport::default(),
            }];
            assert_eq!(Summary::of(&outcomes).failed, 1);
            assert_eq!(Summary::of(&outcomes).exit(), Exit::TestFailed);
            // And `run` still exits 0: the status was fine, and that is all `run`
            // reads.
            assert_eq!(exit_for_run(&outcomes, false), Exit::Ok);
        }
    }

    mod environment_selection {
        use super::*;

        use sendra_core::environment::environment_path;

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
            // The decision this test pins: an explicit name is an assertion
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
                let outcomes = run_requests(
                    &requests,
                    Path::new("."),
                    &environment,
                    &reporter(),
                    |request, _| {
                        sent.push(request.url.clone());
                        async { responded(200) }
                    },
                )
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
    }

    mod summaries {
        use super::*;

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
            let outcomes = run_requests(
                &requests,
                Path::new("."),
                &environment(),
                &reporter(),
                |request, _| {
                    assert!(
                        request.assertions.is_none(),
                        "substitution must not invent a block"
                    );
                    sent.push(request.url.clone());
                    async { responded(200) }
                },
            )
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
            let outcomes = run_requests(
                &requests,
                Path::new("."),
                &environment(),
                &reporter(),
                |request, _| {
                    seen.push(request.assertions.clone());
                    async { responded(200) }
                },
            )
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
            let outcomes = run_requests(
                &requests,
                Path::new("."),
                &environment(),
                &reporter(),
                |request, _| {
                    sent.push(request.url.clone());
                    async { all_passed(200) }
                },
            )
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
                Path::new("."),
                &environment(),
                &reporter(),
                |request, _| async move {
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

            let outcomes = run_requests(
                &requests,
                Path::new("."),
                &environment(),
                &reporter(),
                |_, _| async { all_passed(200) },
            )
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

        #[tokio::test]
        async fn selecting_one_request_by_name_produces_a_summary_of_one() {
            // `test`'s name selection is `document.get`, the same call `run`
            // already makes — this pins the summary/exit behaviour that falls
            // out of feeding just that one request into the same `run_requests`
            // and `Summary` machinery every other `test` run uses.
            let document = Document::from_yaml_str(COLLECTION_WITH_A_BROKEN_VARIABLE).unwrap();
            let request = document.get("Third").expect("`Third` is in the collection");

            let outcomes = run_requests(
                &[request],
                Path::new("."),
                &environment(),
                &reporter(),
                |_, _| async { all_passed(200) },
            )
            .await;

            assert_eq!(
                Summary::of(&outcomes),
                Summary {
                    total: 1,
                    passed: 1,
                    ..Summary::default()
                },
                "a named single request is a test run of one, exactly like an \
                 unnamed single-request file"
            );
        }

        #[test]
        fn naming_a_request_that_does_not_exist_is_the_same_error_run_uses() {
            // `test` reuses `run`'s `document.get`, so a nonexistent name must
            // fail the same way here as it does for `run` — see
            // `sendra_core::Document::get`'s own tests for the error's shape.
            let document = Document::from_yaml_str(COLLECTION_WITH_A_BROKEN_VARIABLE).unwrap();

            let err = document
                .get("Nonexistent")
                .expect_err("no request has this name");
            assert!(
                matches!(err, SendraError::RequestNotFound { ref name, .. } if name == "Nonexistent"),
                "expected RequestNotFound, got {err:?}"
            );
        }
    }

    mod captures {
        use super::*;

        /// A login that captures a token, then a request that uses it. The second
        /// request also captures, so "request 1 does not see request 2" has
        /// something to be false about.
        const CHAIN: &str = "\
requests:
  - name: Log in
    method: POST
    url: '{{base_url}}/login'
    capture:
      auth_token: $.token
  - name: Whoami
    method: GET
    url: '{{base_url}}/me'
    headers:
      Authorization: 'Bearer {{auth_token}}'
    capture:
      user_id: $.user.id
  - name: Profile
    method: GET
    url: '{{base_url}}/users/{{user_id}}'
";

        /// A response whose body is `body`, for a fake `send_one`.
        fn json_body(body: &'static str) -> sendra_core::Response {
            sendra_core::Response {
                status: 200,
                status_text: "OK".to_string(),
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: body.to_string(),
                elapsed: std::time::Duration::from_millis(1),
                redirects: Vec::new(),
            }
        }

        /// The outcome of a request that came back with `body` and had its own
        /// `capture` block evaluated against it — what the real `send` does at
        /// step 7, without a socket.
        fn captured_from(
            request: &Request,
            environment: &Environment,
            body: &'static str,
        ) -> Outcome {
            let response = json_body(body);
            Outcome::Responded {
                status: response.status,
                script: None,
                assertions: AssertionReport::default(),
                capture: request
                    .capture
                    .as_ref()
                    .map(|capture| capture.evaluate(&response, environment))
                    .unwrap_or_default(),
            }
        }

        #[tokio::test]
        async fn a_value_captured_by_one_request_resolves_in_the_next() {
            let document = Document::from_yaml_str(CHAIN).unwrap();
            let requests: Vec<&Request> = document.requests().iter().collect();

            let mut sent: Vec<(String, Option<String>)> = Vec::new();
            let outcomes = run_requests(
                &requests,
                Path::new("."),
                &environment(),
                &reporter(),
                |request, environment| {
                    // Each request answers with the body the next one reads from.
                    let body = if request.url.ends_with("/login") {
                        r#"{"token": "abc123"}"#
                    } else {
                        r#"{"user": {"id": 42}}"#
                    };
                    let outcome = captured_from(&request, &environment, body);
                    sent.push((
                        request.url.clone(),
                        request.header("Authorization").map(str::to_string),
                    ));
                    async move { outcome }
                },
            )
            .await;

            assert_eq!(
                sent,
                vec![
                    ("https://example.com/login".to_string(), None),
                    (
                        "https://example.com/me".to_string(),
                        Some("Bearer abc123".to_string()),
                    ),
                    // Captured by request 2, resolved in request 3's URL: the
                    // store carries forward, not just one step.
                    ("https://example.com/users/42".to_string(), None),
                ]
            );
            assert!(
                outcomes.iter().all(|outcome| matches!(
                    outcome,
                    Outcome::Responded { capture, .. } if capture.passed()
                )),
                "every capture in the chain should have produced a value"
            );
            // No response status was an error and nothing failed a check.
            assert_eq!(exit_for_run(&outcomes, false), Exit::Ok);
            assert_eq!(Summary::of(&outcomes).exit(), Exit::Ok);
        }

        #[tokio::test]
        async fn a_request_cannot_see_what_a_later_request_captures() {
            // File order is real order. Request 1 references `user_id`, which
            // request 2 captures — so it must fail, and the failure must be
            // `VariableNotFound` rather than a request sent with an empty string.
            let document = Document::from_yaml_str(
                "\
requests:
  - name: Too early
    method: GET
    url: '{{base_url}}/users/{{user_id}}'
  - name: Captures it
    method: GET
    url: '{{base_url}}/me'
    capture:
      user_id: $.user.id
",
            )
            .unwrap();
            let requests: Vec<&Request> = document.requests().iter().collect();

            let mut sent = Vec::new();
            let outcomes = run_requests(
                &requests,
                Path::new("."),
                &environment(),
                &reporter(),
                |request, environment| {
                    let outcome = captured_from(&request, &environment, r#"{"user": {"id": 42}}"#);
                    sent.push(request.url.clone());
                    async move { outcome }
                },
            )
            .await;

            // The first request was never sent; the second still was.
            assert_eq!(sent, vec!["https://example.com/me".to_string()]);
            assert!(
                matches!(outcomes[0], Outcome::NoResponse),
                "{:?}",
                outcomes[0]
            );
            // And the run reports it: a reference with nothing behind it is the
            // same category as a refused connection, whichever direction it points.
            assert_eq!(exit_for_run(&outcomes, false), Exit::Failure);
        }

        #[tokio::test]
        async fn the_store_starts_empty_on_every_invocation() {
            // No persistence across processes, and none across calls either: two
            // runs over the same requests behave identically, so a captured value
            // cannot leak from one into the next.
            let document = Document::from_yaml_str(
                "requests:\n  - name: Uses it\n    method: GET\n    url: '{{auth_token}}'\n",
            )
            .unwrap();
            let requests: Vec<&Request> = document.requests().iter().collect();

            for _ in 0..2 {
                let outcomes = run_requests(
                    &requests,
                    Path::new("."),
                    &environment(),
                    &reporter(),
                    |_, _| async { responded(200) },
                )
                .await;
                assert!(matches!(outcomes[0], Outcome::NoResponse));
            }
        }

        #[tokio::test]
        async fn a_later_capture_of_the_same_name_overwrites_the_earlier_one() {
            // Re-logging in, or reading the next page's cursor: the most recent
            // value wins, which is the only thing an ordered store can mean.
            let document = Document::from_yaml_str(
                "\
requests:
  - name: First login
    method: POST
    url: '{{base_url}}/login'
    capture:
      auth_token: $.token
  - name: Second login
    method: POST
    url: '{{base_url}}/login'
    capture:
      auth_token: $.token
  - name: Uses it
    method: GET
    url: '{{base_url}}/me?t={{auth_token}}'
",
            )
            .unwrap();
            let requests: Vec<&Request> = document.requests().iter().collect();

            let mut nth = 0;
            let mut sent = Vec::new();
            let outcomes = run_requests(
                &requests,
                Path::new("."),
                &environment(),
                &reporter(),
                |request, environment| {
                    nth += 1;
                    let body = if nth == 1 {
                        r#"{"token": "first"}"#
                    } else {
                        r#"{"token": "second"}"#
                    };
                    let outcome = captured_from(&request, &environment, body);
                    sent.push(request.url.clone());
                    async move { outcome }
                },
            )
            .await;

            assert_eq!(sent[2], "https://example.com/me?t=second");
            assert_eq!(exit_for_run(&outcomes, false), Exit::Ok);
        }

        #[tokio::test]
        async fn a_capture_that_collides_with_the_environment_is_refused_and_the_file_value_stands()
        {
            // The precedence decision, end to end. `base_url` is in the
            // environment file; a request that tries to capture over it fails, and
            // the request after it still sees the file's value.
            let document = Document::from_yaml_str(
                "\
requests:
  - name: Tries to shadow
    method: GET
    url: '{{base_url}}/login'
    capture:
      base_url: $.token
  - name: After
    method: GET
    url: '{{base_url}}/me'
",
            )
            .unwrap();
            let requests: Vec<&Request> = document.requests().iter().collect();

            let mut sent = Vec::new();
            let outcomes = run_requests(
                &requests,
                Path::new("."),
                &environment(),
                &reporter(),
                |request, environment| {
                    let outcome =
                        captured_from(&request, &environment, r#"{"token": "https://evil.test"}"#);
                    sent.push(request.url.clone());
                    async move { outcome }
                },
            )
            .await;

            let Outcome::Responded { capture, .. } = &outcomes[0] else {
                panic!("the first request came back: {:?}", outcomes[0]);
            };
            assert!(!capture.passed(), "the collision must be reported");
            assert!(
                matches!(
                    capture.results()[0].failure(),
                    Some(sendra_core::CaptureFailure::Shadowed { .. })
                ),
                "got {:?}",
                capture.results()[0].failure()
            );

            // Neither value silently won: the environment's `base_url` still
            // resolved for the request after it.
            assert_eq!(sent[1], "https://example.com/me");

            // Reported as a failed check, not as a request that never happened.
            assert_eq!(
                Summary::of(&outcomes),
                Summary {
                    total: 2,
                    failed: 1,
                    without_assertions: 1,
                    ..Summary::default()
                }
            );
            assert_eq!(Summary::of(&outcomes).exit(), Exit::TestFailed);
            // ...and `run`'s answer is unchanged: both requests came back fine.
            assert_eq!(exit_for_run(&outcomes, false), Exit::Ok);
        }

        #[tokio::test]
        async fn a_capture_that_matches_nothing_fails_its_own_request_and_the_run_continues() {
            // The categorisation decision, end to end: the capture failure is
            // attributed to the request that declared it, the requests around it
            // are still sent, and the downstream request that needed the variable
            // fails on its own terms rather than being pre-empted.
            let document = Document::from_yaml_str(
                "\
requests:
  - name: Captures nothing
    method: POST
    url: '{{base_url}}/login'
    capture:
      auth_token: $.token
  - name: Needs it
    method: GET
    url: '{{base_url}}/me?t={{auth_token}}'
  - name: Independent
    method: GET
    url: '{{base_url}}/health'
",
            )
            .unwrap();
            let requests: Vec<&Request> = document.requests().iter().collect();

            let mut sent = Vec::new();
            let outcomes = run_requests(
                &requests,
                Path::new("."),
                &environment(),
                &reporter(),
                |request, environment| {
                    // The login answers 200 with a body that has no `token` in it.
                    let outcome = captured_from(&request, &environment, r#"{"error": "nope"}"#);
                    sent.push(request.url.clone());
                    async move { outcome }
                },
            )
            .await;

            // The capturing request was sent and came back; the one needing the
            // variable was not; the one after it was.
            assert_eq!(
                sent,
                vec![
                    "https://example.com/login".to_string(),
                    "https://example.com/health".to_string(),
                ]
            );

            let Outcome::Responded { capture, .. } = &outcomes[0] else {
                panic!("the login came back: {:?}", outcomes[0]);
            };
            assert_eq!(
                capture.results()[0].failure(),
                Some(&sendra_core::CaptureFailure::NoMatch)
            );
            assert!(matches!(outcomes[1], Outcome::NoResponse));

            // `test` counts the capture failure as a failed check and the
            // compounding one as a request that never happened...
            assert_eq!(
                Summary::of(&outcomes),
                Summary {
                    total: 3,
                    failed: 1,
                    without_assertions: 1,
                    no_response: 1,
                    ..Summary::default()
                }
            );
            // ...and "never got a response" outranks it, so the run reports 1.
            assert_eq!(Summary::of(&outcomes).exit(), Exit::Failure);
            // `run` reads only the statuses, and the one request that failed to be
            // built is the reason it is not 0.
            assert_eq!(exit_for_run(&outcomes, false), Exit::Failure);
        }

        #[tokio::test]
        async fn a_capture_and_the_checks_beside_it_do_not_see_each_other() {
            // Independent in both directions: a failed assertion does not stop the
            // capture, and a failed capture does not fail the assertion.
            let request = request(
                "method: GET\nurl: https://example.com\n\
                 assertions:\n  status: 500\n\
                 capture:\n  token: $.token\n",
            );
            let response = json_body(r#"{"token": "abc123"}"#);

            let assertions = request.assertions.as_ref().unwrap().evaluate(&response);
            let capture = request
                .capture
                .as_ref()
                .unwrap()
                .evaluate(&response, &Environment::default());

            assert!(!assertions.passed(), "the response was 200, not 500");
            assert!(
                capture.passed(),
                "a failed assertion does not stop the capture beside it"
            );
            assert_eq!(capture.values()["token"], "abc123");

            // One request, counted once, whichever of the two was the reason.
            let outcomes = vec![Outcome::Responded {
                status: 200,
                script: None,
                assertions,
                capture,
            }];
            assert_eq!(Summary::of(&outcomes).failed, 1);
        }

        #[tokio::test]
        async fn a_successful_capture_is_not_a_check_and_does_not_make_a_request_pass() {
            // The asymmetry stated on `Summary`: a login that captures a token and
            // asserts nothing was still not checked.
            let request =
                request("method: POST\nurl: https://example.com\ncapture:\n  t: $.token\n");
            let capture = request
                .capture
                .as_ref()
                .unwrap()
                .evaluate(&json_body(r#"{"token": "abc"}"#), &Environment::default());
            assert!(capture.passed());

            let outcomes = vec![Outcome::Responded {
                status: 200,
                script: None,
                assertions: AssertionReport::default(),
                capture,
            }];
            assert_eq!(
                Summary::of(&outcomes),
                Summary {
                    total: 1,
                    without_assertions: 1,
                    ..Summary::default()
                },
                "a capture is a dependency of the run, not an expectation about the response"
            );
            assert_eq!(Summary::of(&outcomes).exit(), Exit::Ok);
        }

        #[tokio::test]
        async fn a_request_with_no_capture_block_behaves_exactly_as_it_did_before() {
            let request = request("method: GET\nurl: https://example.com\n");
            assert_eq!(request.capture, None);

            let outcomes = vec![Outcome::Responded {
                status: 200,
                script: None,
                assertions: AssertionReport::default(),
                capture: CaptureReport::default(),
            }];
            assert_eq!(
                Summary::of(&outcomes),
                Summary {
                    total: 1,
                    without_assertions: 1,
                    ..Summary::default()
                }
            );
            assert_eq!(exit_for_run(&outcomes, false), Exit::Ok);
        }
    }

    mod dry_run {
        use super::*;

        #[tokio::test]
        async fn dry_run_never_calls_send_and_reports_dry_run() {
            // `dry_run_one` takes the same request `send` would, but a socket
            // pointed at a port nothing listens on: if this reached the wire, the
            // outcome would be `NoResponse` (a refused connection), not
            // `DryRun`.
            let request =
                request("method: GET\nurl: https://127.0.0.1:1/\nheaders:\n  X-Test: value\n");

            let outcome = dry_run_one(&request, &Config::default(), &[], &dry_run_reporter());

            assert!(matches!(outcome, Outcome::DryRun), "{outcome:?}");
            assert_eq!(exit_for_run(&[outcome], false), Exit::Ok);
        }

        #[test]
        fn dry_run_applies_config_then_header_overrides_then_pre_request() {
            // The same ordering `send` guarantees, exercised through
            // `prepare_request` directly: config first, `-H` next (replacing a
            // config header rather than adding to it), and the script last,
            // still able to see and change what came before it.
            let config = Config {
                headers: BTreeMap::from([("X-From-Config".to_string(), "config".to_string())]),
                ..Config::default()
            };
            let request = request(
                "method: GET\nurl: https://example.com\npre_request: |\n  \
                 if request.headers[\"X-From-Config\"] != \"overridden\" { throw \"override missing\"; }\n  \
                 request.headers[\"X-From-Config\"] = \"script\";\n",
            );

            let (prepared, _scripts) = prepare_request(
                &request,
                &config,
                &[("X-From-Config".to_string(), "overridden".to_string())],
                &reporter(),
            )
            .expect("resolution succeeds");

            assert_eq!(prepared.header("X-From-Config"), Some("script"));
        }

        #[tokio::test]
        async fn a_resolution_failure_under_dry_run_is_no_response_not_dry_run() {
            // A `pre_request` script that throws never gets as far as
            // `Outcome::DryRun` — the same `NoResponse` category a compile
            // failure or a missing variable already gets, with or without the
            // flag.
            let request = request(
                "method: GET\nurl: https://example.com\npre_request: |\n  throw \"no signing key\";\n",
            );

            let outcome = dry_run_one(&request, &Config::default(), &[], &dry_run_reporter());

            assert!(matches!(outcome, Outcome::NoResponse), "{outcome:?}");
            assert_eq!(exit_for_run(&[outcome], false), Exit::Failure);
        }

        #[tokio::test]
        async fn dry_run_never_runs_post_request() {
            // There is no response for `post_request` to see, and the pipeline
            // structure is what makes that true rather than a special case:
            // `dry_run_one` never reads `scripts.post_request()` at all. Pinned
            // here by a script that would fail the test if it ever ran.
            let request = request(
                "method: GET\nurl: https://example.com\n\
                 post_request: |\n  throw \"post_request must never run under --dry-run\";\n",
            );

            let outcome = dry_run_one(&request, &Config::default(), &[], &dry_run_reporter());

            assert!(matches!(outcome, Outcome::DryRun), "{outcome:?}");
        }

        #[tokio::test]
        async fn a_collection_dry_run_resolves_every_request_and_all_succeed() {
            let document = Document::from_yaml_str(
                "requests:\n  \
                   - name: First\n    method: GET\n    url: '{{base_url}}/first'\n  \
                   - name: Second\n    method: GET\n    url: '{{base_url}}/second'\n",
            )
            .unwrap();
            let requests: Vec<&Request> = document.requests().iter().collect();

            let config = &Config::default();
            let outcomes = run_requests(
                &requests,
                Path::new("."),
                &environment(),
                &dry_run_reporter(),
                |request, _| async move { dry_run_one(&request, config, &[], &dry_run_reporter()) },
            )
            .await;

            assert_eq!(outcomes.len(), 2);
            assert!(
                outcomes
                    .iter()
                    .all(|outcome| matches!(outcome, Outcome::DryRun)),
                "{outcomes:?}"
            );
            assert_eq!(exit_for_run(&outcomes, false), Exit::Ok);
        }

        #[tokio::test]
        async fn a_broken_request_in_a_dry_run_collection_does_not_stop_its_siblings() {
            let document = Document::from_yaml_str(COLLECTION_WITH_A_BROKEN_VARIABLE).unwrap();
            let requests: Vec<&Request> = document.requests().iter().collect();

            let config = &Config::default();
            let outcomes = run_requests(
                &requests,
                Path::new("."),
                &environment(),
                &dry_run_reporter(),
                |request, _| async move { dry_run_one(&request, config, &[], &dry_run_reporter()) },
            )
            .await;

            assert!(matches!(outcomes[0], Outcome::DryRun));
            assert!(matches!(outcomes[1], Outcome::NoResponse));
            assert!(matches!(outcomes[2], Outcome::DryRun));
            assert_eq!(exit_for_run(&outcomes, false), Exit::Failure);
        }
    }
}
