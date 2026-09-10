//! Executes one request through sendra-core's real send pipeline, off the
//! terminal's event-loop thread.
//!
//! This is the same resolution chain the detail pane already previews
//! (`app::resolve_preview`: `Environment::apply` then
//! `resolve_auth`/`resolve_query`/`resolve_body`), extended with the two
//! steps a preview skips because they cost something a frame draw can't
//! afford — `resolve_oauth` (a real HTTP call to a token endpoint) and the
//! final send itself — and finished with [`sendra_core::send`], the exact
//! function `sendra-cli`'s `send` calls before layering its own retry loop
//! and reporting on top. No HTTP logic and no `resolve_*` call is
//! reimplemented here; this only sequences sendra-core's own functions.
//!
//! On a successful send, the resolved request's own `assertions`/`capture`
//! blocks are evaluated the same way `sendra-cli run`/`test`'s `send` does
//! — `Assertions::evaluate`/`Captures::evaluate`, sendra-core's own
//! machinery, not a parallel check reimplemented here — so a run through
//! the TUI produces the same [`AssertionReport`]/[`CaptureReport`] the CLI
//! would for the identical request.
//!
//! Config is resolved fresh per run, from the process's current directory,
//! the same way sendra-cli's `prepare` resolves it once per invocation —
//! each run here *is* one invocation's worth of work, just triggered by a
//! keypress instead of a command line.

use std::path::PathBuf;

use sendra_core::config::global_config_path;
use sendra_core::{
    AssertionReport, CaptureReport, Config, Environment, OAuthTokenCache, Request, Response,
    SendraError,
};

/// What running a request produced: the response or the error that stopped
/// it — never a TUI-invented summary — plus whatever the request's own
/// `assertions`/`capture` blocks say about it.
///
/// `assertions`/`capture` are always the empty report when `result` is
/// `Err`: nothing was checked or captured, because there is no response to
/// check or capture from. That is the same rule sendra-cli's own `send`
/// follows — see `sendra-cli/src/run.rs` — an empty report there means
/// exactly the same thing an absent `assertions`/`capture` block does
/// (nothing declared), which is why `AppState`/the response panel do not
/// need to tell "not evaluated" apart from "evaluated, nothing declared".
#[derive(Debug)]
pub struct RunOutcome {
    pub result: Result<Response, RunError>,
    pub assertions: AssertionReport,
    pub capture: CaptureReport,
}

/// Every way a run can fail: everything sendra-core itself can report
/// (`Core`), plus the one failure that happens before sendra-core is even
/// reached — the per-run tokio runtime (see [`spawn`]) failing to start.
/// That is not a `sendra_core::SendraError` (nothing in core is involved
/// yet), but it is a real, if rare, failure mode — OS-level thread/resource
/// exhaustion — and belongs in `RunOutcome::result` exactly like any other
/// reason a run did not get a response, rather than behind an `.expect()`
/// that would silently strand the run in `RunState::InFlight` forever (no
/// `RunCompleted` message would ever arrive) or, if the calling thread were
/// ever anything other than the dedicated one `spawn` creates, panic it
/// outright.
#[derive(Debug)]
pub enum RunError {
    Core(SendraError),
    /// `tokio::runtime::Builder::build()` failed for this run's dedicated
    /// runtime — see [`spawn`].
    RuntimeUnavailable(std::io::Error),
    /// The run thread itself panicked somewhere in the pipeline below — a
    /// bug in this crate, not a network/parsing failure. Without
    /// `spawn`'s `catch_unwind`, a panic here would unwind the whole thread
    /// before `on_complete` ever ran, and `RunState` would stay
    /// `InFlight` forever with no `RunCompleted` message ever arriving —
    /// the exact silent-hang shape issue 11 already fixed for other error
    /// paths. Carrying the panic payload through `RunOutcome` the same way
    /// any other run failure travels keeps that guarantee: every run ends
    /// in a `RunCompleted`, even this one.
    Panicked(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Core(error) => write!(f, "{error}"),
            RunError::RuntimeUnavailable(error) => {
                write!(f, "could not start a runtime to send this request: {error}")
            }
            RunError::Panicked(message) => {
                write!(f, "the run thread panicked: {message}")
            }
        }
    }
}

impl From<SendraError> for RunError {
    fn from(error: SendraError) -> Self {
        RunError::Core(error)
    }
}

/// Runs `request` against `environment` on a dedicated OS thread, each with
/// its own current-thread tokio runtime built just for this one send — see
/// the module doc comment and the `tokio` dependency's comment in
/// `Cargo.toml` for why current-thread and why per-run rather than shared.
///
/// `on_complete` is called from that thread once the run finishes, success or
/// failure alike; the caller is expected to forward its argument back into
/// the event loop as a `Message::RunCompleted`, exactly like a crossterm
/// event would be.
///
/// **Always calls `on_complete` exactly once, even if the pipeline panics.**
/// The whole run — runtime build, `block_on`, everything — is wrapped in
/// `catch_unwind` so a bug anywhere in `execute` cannot unwind this thread
/// out from under `on_complete` and leave the caller's `RunState` stuck at
/// `InFlight` with no `RunCompleted` ever coming (see [`RunError::Panicked`]).
/// A caught panic still runs the process's global panic hook first — that
/// hook (`main::install_panic_hook`) only restores the terminal for a panic
/// on the *main* thread, specifically so a panic caught and recovered here,
/// on this background thread, never tears down a terminal session that
/// never actually crashed.
pub fn spawn(
    request: Request,
    environment: Environment,
    base_dir: PathBuf,
    on_complete: impl FnOnce(RunOutcome) + Send + 'static,
) {
    std::thread::spawn(move || {
        let outcome = run_catching_panics(move || {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(execute(request, environment, base_dir)),
                // See `RunError::RuntimeUnavailable`: reported through the
                // same `RunOutcome` a network failure would be, not
                // panicked — the run ends up in `RunState::Completed(Err(_))`,
                // exactly as reachable and exactly as recoverable as any
                // other failed run.
                Err(error) => RunOutcome {
                    result: Err(RunError::RuntimeUnavailable(error)),
                    assertions: AssertionReport::default(),
                    capture: CaptureReport::default(),
                },
            }
        });
        on_complete(outcome);
    });
}

/// Runs `f`, catching any panic and turning it into `RunOutcome { result:
/// Err(RunError::Panicked(_)), .. }` instead of letting it unwind past this
/// point — the mechanism [`spawn`] relies on to guarantee `on_complete`
/// always runs exactly once, panic or not. Factored out from `spawn` itself
/// so this recovery behavior is directly unit-testable with a closure that
/// deliberately panics, rather than only reachable by getting a real bug to
/// misfire somewhere inside `execute`'s real HTTP pipeline.
fn run_catching_panics(f: impl FnOnce() -> RunOutcome + std::panic::UnwindSafe) -> RunOutcome {
    std::panic::catch_unwind(f).unwrap_or_else(|payload| RunOutcome {
        result: Err(RunError::Panicked(panic_payload_message(&payload))),
        assertions: AssertionReport::default(),
        capture: CaptureReport::default(),
    })
}

/// The panic message from a `catch_unwind` payload, for the two shapes
/// `panic!`/`.unwrap()`/`.expect()` actually produce (`&str` for a string
/// literal message, `String` for a formatted one) — anything else (a panic
/// with a non-string payload, rare in practice) falls back to a fixed,
/// still-honest message rather than a blank one.
///
/// Takes `&Box<dyn Any + Send>` and derefs it explicitly (`&*payload`)
/// rather than a plain `&(dyn Any + Send)` parameter relying on the call
/// site's implicit `&payload` deref-coercion — the two are not
/// interchangeable here: the implicit coercion at the `unwrap_or_else` call
/// site was observed to produce a reference `downcast_ref` silently never
/// matches against either `&str` or `String`, even for a payload that
/// genuinely is one, while the explicit `&*payload` deref downcasts
/// correctly. Verified with a minimal repro outside this crate before
/// settling on this signature, not assumed.
fn panic_payload_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    let payload: &(dyn std::any::Any + Send) = &**payload;
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// The pipeline itself: config, then substitution/auth/query/body
/// resolution (`environment.apply`, `resolve_oauth`, `resolve_auth`,
/// `resolve_query`, `resolve_body` — the same chain `resolve_preview` in
/// `app.rs` runs, plus `resolve_oauth`, which a preview has no reason to
/// pay for), then the actual send via [`sendra_core::send`], then — only on
/// a successful send — assertions and captures against the response that
/// came back.
async fn execute(request: Request, environment: Environment, base_dir: PathBuf) -> RunOutcome {
    match execute_inner(request, environment, base_dir).await {
        Ok((response, assertions, capture)) => RunOutcome {
            result: Ok(response),
            assertions,
            capture,
        },
        Err(err) => RunOutcome {
            result: Err(err),
            assertions: AssertionReport::default(),
            capture: CaptureReport::default(),
        },
    }
}

async fn execute_inner(
    request: Request,
    environment: Environment,
    base_dir: PathBuf,
) -> Result<(Response, AssertionReport, CaptureReport), RunError> {
    let start_dir = std::env::current_dir().map_err(SendraError::CurrentDir)?;
    let global_config = global_config_path().filter(|path| path.is_file());
    let config = Config::resolve_from(&start_dir, global_config.as_deref())?;
    let client = sendra_core::build_client(&config)?;
    let oauth_cache = OAuthTokenCache::new();

    let resolved = environment
        .apply(&request)?
        .resolve_oauth(&client, &oauth_cache)
        .await?
        .resolve_auth()?
        .resolve_query()?
        .resolve_body(&base_dir)?;

    let response = sendra_core::send(&resolved, &client, &config).await?;

    // Evaluated against the same `resolved` request `send` just sent, and
    // the same `environment` value the substitution above applied — exactly
    // the pairing `sendra-cli`'s own `send` evaluates against, so a
    // `capture` colliding with an environment-defined name is refused here
    // the same way it would be from the CLI. `resolved.assertions`/
    // `resolved.capture` are the request's own declared blocks, untouched
    // by substitution or auth/query/body resolution — those only ever
    // change `url`/`headers`/`body`/`auth`/`query`, never these two fields.
    let assertions = resolved
        .assertions
        .as_ref()
        .map(|assertions| assertions.evaluate(&response))
        .unwrap_or_default();
    let capture = resolved
        .capture
        .as_ref()
        .map(|capture| capture.evaluate(&response, &environment))
        .unwrap_or_default();

    Ok((response, assertions, capture))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use sendra_core::Document;

    /// A server that accepts exactly one connection and answers a fixed
    /// `200`, the same minimal hand-rolled pattern sendra-cli's own
    /// end-to-end tests use (see `sendra-cli/tests/cli_overrides.rs`) —
    /// proof this reaches a real socket rather than stopping short of one.
    fn start_ok_server() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
        let addr = listener.local_addr().expect("the listener has an address");

        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        });

        addr
    }

    fn request(yaml: &str) -> Request {
        Document::from_yaml_str(yaml)
            .expect("valid test YAML")
            .requests()[0]
            .clone()
    }

    /// Runs `spawn` and blocks the test thread for its result, so these
    /// tests can assert on the outcome directly instead of driving an event
    /// loop — `spawn`'s whole contract is "call `on_complete` exactly once",
    /// which an `mpsc` channel proves as directly here as it will from
    /// `main`'s real loop.
    fn run_and_wait(request: Request) -> RunOutcome {
        let (tx, rx) = mpsc::channel();
        spawn(
            request,
            Environment::default(),
            PathBuf::from("."),
            move |result| {
                let _ = tx.send(result);
            },
        );
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the run must complete, not hang")
    }

    // --- clean-exit audit: a panicking run thread must not hang ----------

    /// The exact mechanism `spawn` relies on (see its doc comment),
    /// exercised directly with a closure that deliberately panics — proof
    /// that a bug anywhere in the real pipeline `spawn` wraps this way
    /// becomes a real `RunError::Panicked`, carrying the real panic
    /// message, rather than unwinding past `on_complete` and leaving the
    /// caller's `RunState` stuck at `InFlight` forever.
    #[test]
    fn run_catching_panics_converts_a_panic_into_a_run_error_instead_of_unwinding() {
        let outcome = run_catching_panics(|| panic!("simulated bug in the pipeline"));

        match outcome.result {
            Err(RunError::Panicked(message)) => {
                assert!(
                    message.contains("simulated bug in the pipeline"),
                    "the real panic message must be preserved, got: {message}"
                );
            }
            other => panic!("expected Err(RunError::Panicked(_)), got {other:?}"),
        }
        assert!(
            outcome.assertions.is_empty() && outcome.capture.is_empty(),
            "a panicked run checked/captured nothing, same as any other failed run"
        );
    }

    /// The full `spawn`-shaped scenario end to end: a real OS thread whose
    /// work panics. This is what issue 13's audit calls for directly — "a
    /// bug inside run_request.rs" on the run thread — proven not to hang by
    /// blocking on the very same `mpsc` completion signal `main`'s real loop
    /// waits on, with a timeout that fails the test outright if it ever
    /// does hang instead of asserting a negative.
    #[test]
    fn a_panic_on_the_run_thread_still_completes_instead_of_hanging_forever() {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let outcome = run_catching_panics(|| panic!("boom from the run thread"));
            let _ = tx.send(outcome);
        });

        let outcome = rx.recv_timeout(Duration::from_secs(5)).expect(
            "a panicking run thread must still send a completion, exactly like a \
             successful or a network-failed one would — never hang silently",
        );

        assert!(
            matches!(outcome.result, Err(RunError::Panicked(_))),
            "the panic must surface as a real, inspectable RunError"
        );
    }

    /// Quitting while a run is still in flight drops `main::run`'s own
    /// `run_tx`/`run_rx` — the receiver this `on_complete` closure's cloned
    /// `Sender` eventually sends into. `mpsc::Sender::send` on a channel
    /// whose only `Receiver` has already been dropped returns `Err`, never
    /// panics (the std library's own documented behavior) — this proves
    /// that holds for `spawn`'s specific `on_complete` pattern
    /// (`let _ = tx.send(result);`, exactly as `main.rs` writes it) rather
    /// than assuming it from the docs alone: the run thread must still run
    /// to completion and return normally with the receiver already gone.
    #[test]
    fn sending_the_completed_run_to_an_already_dropped_receiver_does_not_panic() {
        use std::sync::{Arc, Condvar, Mutex};

        let addr = start_ok_server();
        let req = request(&format!("method: GET\nurl: http://{addr}/\n"));

        let (tx, rx) = mpsc::channel::<RunOutcome>();
        drop(rx); // The app quit; nothing is listening anymore.

        let finished = Arc::new((Mutex::new(false), Condvar::new()));
        let finished_writer = Arc::clone(&finished);
        spawn(
            req,
            Environment::default(),
            PathBuf::from("."),
            move |result| {
                // The exact pattern `main::run` uses: discard the Result,
                // never unwrap it. If this panicked instead, the run thread
                // would die silently and `finished` would never flip.
                let _ = tx.send(result);
                let (lock, condvar) = &*finished_writer;
                *lock.lock().expect("lock is not poisoned") = true;
                condvar.notify_one();
            },
        );

        let (lock, condvar) = &*finished;
        let guard = lock.lock().expect("lock is not poisoned");
        let (guard, wait_result) = condvar
            .wait_timeout_while(guard, Duration::from_secs(10), |finished| !*finished)
            .expect("lock is not poisoned");
        assert!(
            *guard && !wait_result.timed_out(),
            "the run thread must complete normally (not hang or die silently) even \
             when sending its result to an already-dropped receiver"
        );
    }

    #[test]
    fn running_a_request_against_a_reachable_host_yields_a_real_response() {
        let addr = start_ok_server();
        let request = request(&format!("method: GET\nurl: http://{addr}/\n"));

        let outcome = run_and_wait(request);

        let response = outcome
            .result
            .expect("a reachable server must yield a real response");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "ok");
    }

    #[test]
    fn running_a_request_against_an_unreachable_host_fails_cleanly_and_quickly() {
        // Port 1 is a real, low, almost-never-listened-on port: connecting
        // to it on localhost gets an immediate ECONNREFUSED rather than a
        // silent timeout, so a broken implementation that hung here would
        // fail this test in seconds rather than however long a real
        // `--timeout` would take.
        let request = request("method: GET\nurl: http://127.0.0.1:1/\n");

        let start = Instant::now();
        let outcome = run_and_wait(request);
        let elapsed = start.elapsed();

        assert!(
            outcome.result.is_err(),
            "an unreachable host must produce a clear failure, not a fabricated response"
        );
        assert!(
            outcome.assertions.is_empty(),
            "there is no response to have checked anything against"
        );
        assert!(
            outcome.capture.is_empty(),
            "there is no response to have captured anything from"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "a refused connection must fail fast, not hang: took {elapsed:?}"
        );
    }

    #[test]
    fn a_request_with_no_assertions_or_capture_yields_empty_reports() {
        let addr = start_ok_server();
        let request = request(&format!("method: GET\nurl: http://{addr}/\n"));

        let outcome = run_and_wait(request);

        assert!(outcome.result.is_ok());
        assert!(
            outcome.assertions.is_empty(),
            "a request with no `assertions:` block must report an empty, not a failing, result"
        );
        assert!(
            outcome.capture.is_empty(),
            "a request with no `capture:` block must report an empty, not a failing, result"
        );
    }

    #[test]
    fn assertions_are_evaluated_against_the_real_response() {
        let addr = start_ok_server();
        let request = request(&format!(
            "method: GET\nurl: http://{addr}/\nassertions:\n  status: 200\n  status_in: [404]\n"
        ));

        let outcome = run_and_wait(request);

        assert!(outcome.result.is_ok());
        assert_eq!(outcome.assertions.len(), 2);
        assert_eq!(
            outcome.assertions.passed_count(),
            1,
            "the real server answered 200, so `status: 200` must pass and `status_in: [404]` must fail"
        );
        assert_eq!(outcome.assertions.failed_count(), 1);
    }

    #[test]
    fn captures_are_evaluated_against_the_real_response() {
        // The hand-rolled server always answers a fixed body ("ok") that is
        // not JSON, so a header capture — which does not touch the body at
        // all — is what proves this reaches the real response rather than
        // an empty stand-in.
        let addr = start_ok_server();
        let request = request(&format!(
            "method: GET\nurl: http://{addr}/\ncapture:\n  length: {{header: Content-Length}}\n"
        ));

        let outcome = run_and_wait(request);

        assert!(outcome.result.is_ok());
        assert_eq!(
            outcome.capture.values().get("length").map(String::as_str),
            Some("2")
        );
    }

    /// Manual parity check against `sendra run` on the exact same request —
    /// `#[ignore]`d because it needs real internet access, not something a
    /// normal `cargo test` run should depend on. Run explicitly with
    /// `cargo test -p sendra-tui -- --ignored --nocapture` and compare the
    /// printed status/headers/body against `sendra run req.yaml` where
    /// `req.yaml` is `method: GET\nurl: https://httpbin.org/json\n` — the
    /// same request this test sends through the same pipeline `spawn` uses.
    #[test]
    #[ignore = "hits the real network (httpbin.org); run explicitly for a parity check against `sendra run`"]
    fn parity_check_against_sendra_run_httpbin_json() {
        let request = request("method: GET\nurl: https://httpbin.org/json\n");

        let outcome = run_and_wait(request);

        let response = outcome.result.expect("httpbin.org should be reachable");
        eprintln!("status: {} {}", response.status, response.status_text);
        for (name, value) in &response.headers {
            eprintln!("{name}: {value}");
        }
        eprintln!();
        eprintln!("{}", response.body);
    }

    /// Manual parity check for assertions/captures: the same request run
    /// above `sendra run req.yaml`, where `req.yaml` is:
    ///
    /// ```yaml
    /// name: MixedAssertions
    /// method: GET
    /// url: https://httpbin.org/json
    /// assertions:
    ///   status: 200
    ///   status_in: [404, 500]
    ///   headers:
    ///     content-type: application/json
    /// capture:
    ///   server_header: {header: server}
    /// ```
    ///
    /// `sendra run` on this file prints:
    ///
    /// ```text
    /// assertions
    ///   ✓ status is 200
    ///   ✗ status is one of [404, 500] — got 200
    ///   ✓ header `content-type` is `application/json`
    ///   2 passed, 1 failed
    ///
    /// capture
    ///   ✓ server_header from `header `server``
    /// ```
    ///
    /// which is exactly what this test's `eprintln!`s should match.
    #[test]
    #[ignore = "hits the real network (httpbin.org); run explicitly for a parity check against `sendra run`"]
    fn parity_check_mixed_assertions_and_capture() {
        let request = request(
            "name: MixedAssertions\nmethod: GET\nurl: https://httpbin.org/json\n\
             assertions:\n  status: 200\n  status_in: [404, 500]\n  \
             headers:\n    content-type: application/json\n\
             capture:\n  server_header: {header: server}\n",
        );

        let outcome = run_and_wait(request);

        assert!(outcome.result.is_ok(), "httpbin.org should be reachable");

        eprintln!("assertions");
        for result in outcome.assertions.results() {
            match &result.failure {
                None => eprintln!("  ✓ {}", result.expectation),
                Some(detail) => eprintln!("  ✗ {} — {detail}", result.expectation),
            }
        }
        eprintln!(
            "  {} passed, {} failed",
            outcome.assertions.passed_count(),
            outcome.assertions.failed_count()
        );
        eprintln!();
        eprintln!("capture");
        for result in outcome.capture.results() {
            let from = format!("{} from `{}`", result.variable, result.path);
            match result.failure() {
                None => eprintln!("  ✓ {from}"),
                Some(failure) => eprintln!("  ✗ {from} — {failure}"),
            }
        }

        assert_eq!(outcome.assertions.passed_count(), 2);
        assert_eq!(outcome.assertions.failed_count(), 1);
        assert!(outcome.capture.results()[0].passed());
    }
}
