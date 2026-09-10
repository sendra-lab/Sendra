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

/// What running a request produced: the response or the real `sendra-core`
/// error that stopped it — never a TUI-invented summary — plus whatever the
/// request's own `assertions`/`capture` blocks say about it.
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
    pub result: Result<Response, SendraError>,
    pub assertions: AssertionReport,
    pub capture: CaptureReport,
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
pub fn spawn(
    request: Request,
    environment: Environment,
    base_dir: PathBuf,
    on_complete: impl FnOnce(RunOutcome) + Send + 'static,
) {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to start a tokio runtime for the request");
        on_complete(runtime.block_on(execute(request, environment, base_dir)));
    });
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
) -> Result<(Response, AssertionReport, CaptureReport), SendraError> {
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
