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
//! Config is resolved fresh per run, from the process's current directory,
//! the same way sendra-cli's `prepare` resolves it once per invocation —
//! each run here *is* one invocation's worth of work, just triggered by a
//! keypress instead of a command line.

use std::path::PathBuf;

use sendra_core::config::global_config_path;
use sendra_core::{Config, Environment, OAuthTokenCache, Request, Response, SendraError};

/// What running a request produced: a real response, or the real
/// `sendra-core` error that stopped it — never a TUI-invented summary.
pub type RunOutcome = Result<Response, SendraError>;

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
/// pay for), then the actual send via [`sendra_core::send`].
async fn execute(request: Request, environment: Environment, base_dir: PathBuf) -> RunOutcome {
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

    sendra_core::send(&resolved, &client, &config).await
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

        let response = outcome.expect("a reachable server must yield a real response");
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
            outcome.is_err(),
            "an unreachable host must produce a clear failure, not a fabricated response"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "a refused connection must fail fast, not hang: took {elapsed:?}"
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

        let response = outcome.expect("httpbin.org should be reachable");
        eprintln!("status: {} {}", response.status, response.status_text);
        for (name, value) in &response.headers {
            eprintln!("{name}: {value}");
        }
        eprintln!();
        eprintln!("{}", response.body);
    }
}
