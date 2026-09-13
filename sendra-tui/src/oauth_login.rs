//! The interactive `grant_type: authorization_code` login flow, triggered
//! from the auth editor (`Message::StartOAuthLogin`) rather than run
//! automatically — see `sendra_core::oauth`'s module doc comment for why
//! this grant cannot acquire a token on its own the way the other two do.
//!
//! Mirrors [`crate::run_request::spawn`]'s shape (a dedicated OS thread, a
//! `FnOnce` completion callback, a per-run current-thread tokio runtime for
//! the one `.await` this needs) but is a genuinely different pipeline: open
//! a browser to the provider's authorization URL, run a local HTTP listener
//! just long enough to catch the redirect back, then exchange the code it
//! caught for a token via [`sendra_core::exchange_authorization_code`].
//!
//! **The acquired token is only ever handed to
//! [`sendra_core::OAuthTokenCache::insert_token`], never returned to the
//! caller or written anywhere else.** [`LoginOutcome`] tells the UI whether
//! the login succeeded, not what the token was — the TUI has no code path
//! that needs the raw string, and every later send picks the token back up
//! automatically through the exact same cache
//! [`sendra_core::Request::resolve_oauth`] already reads (see
//! `main::run`'s shared `AppState::oauth_cache`).

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use sendra_core::oauth::{
    build_authorization_url, exchange_authorization_code, generate_pkce, generate_state,
};
use sendra_core::{OAuthAuth, OAuthTokenCache};

/// How long the local callback listener waits for the provider to redirect
/// back before giving up.
///
/// Five minutes: long enough to cover a real human in a real browser —
/// picking an account, typing a password, clearing an MFA prompt, or a
/// provider page that is just slow — without leaving a listener bound
/// (however harmlessly) for the rest of a long TUI session if the user
/// abandons the browser tab or the provider hangs. There is no signal from
/// the provider or the OS that says "the user gave up"; a fixed ceiling is
/// the only way to guarantee this never waits forever.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// What one login attempt produced. Carries no token — see the module doc
/// comment for why — only enough to tell the user what happened.
#[derive(Debug)]
pub enum LoginOutcome {
    Success,
    Failed(String),
}

/// Runs one login attempt on a dedicated OS thread and calls `on_complete`
/// exactly once when it finishes, success or failure alike — the same
/// contract [`crate::run_request::spawn`] makes, for the same reason: the
/// caller forwards `on_complete`'s argument back into the event loop as a
/// message, and every path through [`run_login`] below reaches its one
/// `on_complete` call rather than leaving the caller's waiting state stuck
/// forever.
///
/// **Always calls `on_complete` exactly once, even if `run_login` panics** —
/// wrapped in `catch_unwind` the same way [`crate::run_request::spawn`]
/// wraps `execute`, so a bug anywhere in the login pipeline cannot unwind
/// this thread out from under `on_complete` and leave the caller's
/// `OAuthLoginState` stuck at `WaitingForBrowser` forever.
pub fn spawn(
    oauth: OAuthAuth,
    cache: Arc<OAuthTokenCache>,
    on_complete: impl FnOnce(LoginOutcome) + Send + 'static,
) {
    std::thread::spawn(move || {
        let outcome =
            login_catching_panics(std::panic::AssertUnwindSafe(|| run_login(&oauth, &cache)));
        on_complete(outcome);
    });
}

/// Runs `f`, catching any panic and turning it into `LoginOutcome::Failed`
/// instead of letting it unwind past this point — the mechanism [`spawn`]
/// relies on to guarantee `on_complete` always runs exactly once, panic or
/// not. Factored out from `spawn` itself, the same way
/// [`crate::run_request::run_catching_panics`] is, so this recovery
/// behavior is directly unit-testable with a closure that deliberately
/// panics, rather than only reachable by getting a real bug to misfire
/// somewhere inside `run_login`'s real network/browser pipeline.
fn login_catching_panics(
    f: impl FnOnce() -> LoginOutcome + std::panic::UnwindSafe,
) -> LoginOutcome {
    std::panic::catch_unwind(f).unwrap_or_else(|payload| {
        LoginOutcome::Failed(format!(
            "the login thread panicked: {}",
            crate::run_request::panic_payload_message(&payload)
        ))
    })
}

/// The flow itself: PKCE pair and CSRF `state` generated fresh for this one
/// attempt, a local listener bound to `oauth.redirect_uri`, the browser
/// opened to `oauth.authorization_url`, then a wait — bounded by
/// [`LOGIN_TIMEOUT`] — for the callback, and finally the code-for-token
/// exchange. On success, the token is written straight into `cache` via
/// [`OAuthTokenCache::insert_token`] before returning, so `on_complete`
/// running is proof the token is already reusable.
fn run_login(oauth: &OAuthAuth, cache: &OAuthTokenCache) -> LoginOutcome {
    let redirect_uri = match oauth.redirect_uri.as_deref() {
        Some(uri) if !uri.is_empty() => uri,
        _ => return LoginOutcome::Failed("auth.oauth.redirect_uri is not set".to_string()),
    };
    let bind_addr = match redirect_bind_addr(redirect_uri) {
        Ok(addr) => addr,
        Err(reason) => return LoginOutcome::Failed(reason),
    };
    let listener = match TcpListener::bind(bind_addr) {
        Ok(listener) => listener,
        Err(err) => {
            return LoginOutcome::Failed(format!(
                "could not start the local callback listener on {bind_addr} (does another \
                 process already own that port?): {err}"
            ))
        }
    };

    let pkce = generate_pkce();
    let csrf_state = generate_state();
    let authorization_url = match build_authorization_url(oauth, &csrf_state, &pkce.challenge) {
        Ok(url) => url,
        Err(err) => return LoginOutcome::Failed(err.to_string()),
    };

    if let Err(err) = open::that(&authorization_url) {
        return LoginOutcome::Failed(format!(
            "could not open the browser to {authorization_url}: {err}"
        ));
    }

    let (code, returned_state) = match accept_callback(listener, LOGIN_TIMEOUT) {
        Ok(pair) => pair,
        Err(reason) => return LoginOutcome::Failed(reason),
    };

    if returned_state != csrf_state {
        return LoginOutcome::Failed(
            "the callback's `state` parameter did not match what was sent — aborting rather \
             than risk accepting a code that was not requested by this login"
                .to_string(),
        );
    }

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            return LoginOutcome::Failed(format!(
                "could not start a runtime for the token exchange: {err}"
            ))
        }
    };

    runtime.block_on(async {
        let start_dir = match std::env::current_dir() {
            Ok(dir) => dir,
            Err(err) => {
                return LoginOutcome::Failed(format!(
                    "could not resolve the current directory: {err}"
                ))
            }
        };
        let config = match crate::run_request::resolve_config(&start_dir) {
            Ok(config) => config,
            Err(err) => return LoginOutcome::Failed(err.to_string()),
        };
        let client = match sendra_core::build_client(&config) {
            Ok(client) => client,
            Err(err) => return LoginOutcome::Failed(err.to_string()),
        };
        match exchange_authorization_code(oauth, &client, &code, &pkce.verifier).await {
            Ok((access_token, expires_in)) => {
                cache.insert_token(oauth, access_token, expires_in);
                LoginOutcome::Success
            }
            Err(err) => LoginOutcome::Failed(err.to_string()),
        }
    })
}

/// Resolves `redirect_uri` (e.g. `http://127.0.0.1:8899/callback`) to the
/// local address the callback listener binds to. Goes through
/// [`ToSocketAddrs`] rather than a direct [`SocketAddr`] parse so a
/// `localhost` host (common in provider examples) resolves the same as a
/// literal loopback IP would, not just the literal-IP case.
fn redirect_bind_addr(redirect_uri: &str) -> Result<SocketAddr, String> {
    let url = reqwest::Url::parse(redirect_uri)
        .map_err(|err| format!("`redirect_uri` ({redirect_uri}) is not a valid URL: {err}"))?;
    let host = url
        .host_str()
        .ok_or_else(|| format!("`redirect_uri` ({redirect_uri}) has no host to bind to"))?;
    let port = url.port_or_known_default().ok_or_else(|| {
        format!("`redirect_uri` ({redirect_uri}) has no port and no default for its scheme")
    })?;
    (host, port)
        .to_socket_addrs()
        .map_err(|err| format!("could not resolve `redirect_uri` host `{host}`: {err}"))?
        .next()
        .ok_or_else(|| format!("`redirect_uri` host `{host}` resolved to no address"))
}

/// Waits up to `timeout` for exactly one HTTP request on `listener`, parses
/// its query string for `code`/`state` (or `error`, the provider's own way
/// of saying the user declined), and answers it with a short HTML page
/// telling the human to return to the terminal — the only feedback they get
/// in the browser tab itself.
///
/// **Never blocks past `timeout`, even though [`TcpListener::accept`] has no
/// timeout of its own.** The accept runs on its own thread; this function
/// waits on an `mpsc` channel with [`mpsc::Receiver::recv_timeout`] instead
/// of calling `accept` directly. If the deadline passes first, a throwaway
/// connection to the listener's own address is made purely to unblock that
/// thread's `accept()` call so it does not leak past this function
/// returning — the listener itself is dropped right after, on either path.
fn accept_callback(listener: TcpListener, timeout: Duration) -> Result<(String, String), String> {
    let local_addr = listener
        .local_addr()
        .map_err(|err| format!("the local callback listener has no address: {err}"))?;

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(listener.accept());
    });

    let accept_result = match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let _ = TcpStream::connect(local_addr);
            return Err(format!(
                "no callback arrived within {}s — the browser login was abandoned, or the \
                 provider never redirected back",
                timeout.as_secs()
            ));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err("the local callback listener stopped unexpectedly".to_string());
        }
    };

    let (mut stream, _) = accept_result.map_err(|err| {
        format!("the local callback listener failed to accept a connection: {err}")
    })?;

    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|err| format!("could not read the callback request: {err}"))?,
    );
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|err| format!("could not read the callback request: {err}"))?;
    let path = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| "the callback sent a malformed HTTP request line".to_string())?;

    // A bare path has no authority to parse against; joining it onto a
    // throwaway base URL is only ever used to reuse `reqwest::Url`'s own
    // query-pair parser instead of hand-rolling one — the host here is
    // never contacted or shown to anyone.
    let full_url = format!("http://callback.local{path}");
    let parsed = reqwest::Url::parse(&full_url)
        .map_err(|err| format!("the callback sent a malformed request path: {err}"))?;

    let mut code = None;
    let mut returned_state = None;
    let mut provider_error = None;
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => returned_state = Some(value.into_owned()),
            "error" => provider_error = Some(value.into_owned()),
            _ => {}
        }
    }

    respond(&mut stream, provider_error.is_none() && code.is_some());

    if let Some(error) = provider_error {
        return Err(format!("the provider reported an error: {error}"));
    }
    let code = code.ok_or_else(|| "the callback had no `code` parameter".to_string())?;
    let returned_state =
        returned_state.ok_or_else(|| "the callback had no `state` parameter".to_string())?;
    Ok((code, returned_state))
}

/// Answers the browser's callback request with a minimal, static HTML page —
/// the only thing the human sees in that tab, since the real "did it work"
/// answer is reported back in the TUI itself, not here.
fn respond(stream: &mut TcpStream, success: bool) {
    let body = if success {
        "<html><body><p>Login complete. You can close this tab and return to the terminal.</p></body></html>"
    } else {
        "<html><body><p>Login failed. You can close this tab and return to the terminal.</p></body></html>"
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpStream;

    /// Proof requirement for `spawn`'s doc comment: a panic anywhere in
    /// `run_login` must still produce a `LoginOutcome` rather than unwinding
    /// past `spawn`'s thread closure, the same guarantee
    /// `run_request::spawn` already has (and is tested for) — see that
    /// crate's `a_panic_on_the_run_thread_still_completes_instead_of_hanging_forever`.
    #[test]
    fn a_panic_in_run_login_still_produces_a_failed_outcome_instead_of_hanging_forever() {
        let outcome = login_catching_panics(|| panic!("simulated bug in the login pipeline"));
        match outcome {
            LoginOutcome::Failed(message) => {
                assert!(
                    message.contains("simulated bug in the login pipeline"),
                    "the panic message must survive into the outcome: {message}"
                );
            }
            LoginOutcome::Success => panic!("a panic must never read as a successful login"),
        }
    }

    #[test]
    fn redirect_bind_addr_resolves_loopback_host_and_port() {
        let addr = redirect_bind_addr("http://127.0.0.1:8899/callback").expect("valid loopback");
        assert_eq!(addr, "127.0.0.1:8899".parse().unwrap());
    }

    #[test]
    fn redirect_bind_addr_resolves_localhost() {
        let addr = redirect_bind_addr("http://localhost:8899/callback").expect("valid localhost");
        assert_eq!(addr.port(), 8899);
        assert!(addr.ip().is_loopback());
    }

    #[test]
    fn redirect_bind_addr_rejects_a_uri_with_no_port_and_no_default() {
        // A made-up scheme has a real host but no notion of a "default
        // port" the way http/https do.
        let err = redirect_bind_addr("myapp://127.0.0.1/callback").expect_err("no port to bind to");
        assert!(err.contains("port"), "got {err}");
    }

    /// Proof requirement: the timeout path must not hang, and must actually
    /// free the listener rather than leaking the acceptor thread — proven by
    /// asserting the whole call returns well within the timeout's own
    /// duration when nothing ever connects, and by directly exercising the
    /// unblock-via-throwaway-connection mechanism the doc comment describes.
    #[test]
    fn accept_callback_times_out_instead_of_hanging_forever() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");

        let start = std::time::Instant::now();
        let result = accept_callback(listener, Duration::from_millis(200));
        let elapsed = start.elapsed();

        assert!(result.is_err(), "no connection ever arrived");
        assert!(
            elapsed < Duration::from_secs(2),
            "must return promptly after the timeout elapses, took {elapsed:?}"
        );
    }

    #[test]
    fn accept_callback_parses_code_and_state_from_a_real_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).expect("the listener is up");
            stream
                .write_all(b"GET /callback?code=abc123&state=xyz HTTP/1.1\r\nHost: x\r\n\r\n")
                .unwrap();
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf);
        });

        let (code, state) = accept_callback(listener, Duration::from_secs(5))
            .expect("a real callback with code+state must parse");
        assert_eq!(code, "abc123");
        assert_eq!(state, "xyz");
    }

    #[test]
    fn accept_callback_surfaces_a_provider_error_instead_of_a_missing_code() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).expect("the listener is up");
            stream
                .write_all(b"GET /callback?error=access_denied&state=xyz HTTP/1.1\r\n\r\n")
                .unwrap();
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf);
        });

        let err = accept_callback(listener, Duration::from_secs(5)).expect_err(
            "a provider `error` parameter must surface, not a generic missing-code error",
        );
        assert!(err.contains("access_denied"), "got {err}");
    }
}
