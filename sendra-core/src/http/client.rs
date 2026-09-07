//! [`HttpClient`] and [`build_client`]: the connection-pooled client
//! [`crate::send`]/[`crate::send_prepared`] send through, and its redirect
//! bookkeeping.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::{Config, FollowRedirects};
use crate::error::SendraError;
use crate::http::response::RedirectHop;

/// Redirect hops recorded during the request currently in flight through a
/// given [`HttpClient`].
///
/// A `reqwest::redirect::Policy` closure has no way to hand its caller
/// anything back directly — it only decides follow/stop/error — so this is
/// the side channel: the policy pushes a hop here as it sees each one, and
/// [`send_prepared`](crate::send_prepared) drains it right after that request finishes. The client
/// is built once per run and reused by every request in it (see
/// [`build_client`]), so the log is cleared at the *start* of each send
/// rather than trusted to be empty — nothing else empties it, and requests in
/// a run are sent one at a time, never concurrently, so there is never more
/// than one request's hops in it at once.
///
/// **This assumes strictly sequential sends through one [`HttpClient`].**
/// There is exactly one log per client, shared by every request that client
/// ever sends, and a hop is attributed to "whatever is currently between the
/// clear in `send_prepared` and the drain right after it" — not to any
/// particular request. Two requests sent concurrently through the same
/// client would race on that log and could easily come back with each
/// other's redirect hops, or a merged chain that belongs to neither. Nothing
/// today does that — `run_requests` in `sendra-cli` awaits each request
/// before starting the next — but if a future feature sends requests from
/// one client in parallel (a `--repeat`/retry feature that fires several at
/// once, say, or any other parallel send path), this mechanism has to change
/// with it: most likely one log per in-flight request rather than one per
/// client, or a channel instead of a shared `Vec`.
type RedirectLog = Arc<Mutex<Vec<RedirectHop>>>;

/// The HTTP client [`send`](crate::send) and [`send_prepared`](crate::send_prepared) send through.
///
/// A thin wrapper around `reqwest::Client` rather than a re-export of it, so
/// that the redirect chain a request's `reqwest::redirect::Policy` observes
/// has somewhere to be recorded and read back — see [`RedirectLog`]. A
/// front-end builds one with [`build_client`] and passes it around without
/// taking a direct dependency on reqwest.
pub struct HttpClient {
    pub(super) inner: reqwest::Client,
    /// See [`RedirectLog`] — in particular, its note on why this only works
    /// as long as sends through this client stay sequential.
    pub(super) redirects: RedirectLog,
    /// The whole-request timeout this client was built with, kept so that
    /// [`SendraError::Timeout`] can name the limit it hit.
    ///
    /// Here rather than passed back down through [`send_prepared`](crate::send_prepared) because
    /// this is where the limit *is*: reqwest keeps its own copy inside
    /// `inner` and will not hand it back, and `send_prepared` deliberately
    /// takes no `&Config` (see its doc comment). The client enforces the
    /// timeout, so the client is what remembers it.
    pub(super) timeout: Duration,
}

/// Build the HTTP client a run sends every one of its requests through.
///
/// **Once per run, not once per request.** A `reqwest::Client` owns the
/// connection pool: the TLS session, the kept-alive TCP connection and the
/// resolved DNS for a host all live in it, and all of it is thrown away with
/// the client. Building one per request means a collection of twenty requests
/// against one API pays twenty TLS handshakes to send twenty requests, which is
/// most of the wall clock for a run that does nothing else. Built once and
/// borrowed by every send, the second request onwards reuses the connection the
/// first opened.
///
/// It is a function taking a `&Config` rather than a method on `Config`
/// because a client is not configuration: it holds sockets, it is cheap to
/// clone and expensive to rebuild, and it belongs to a *run*, whereas the
/// config it is built from is a resolved set of values that outlives any
/// particular one. The config decides two things here — the timeout and the
/// redirect policy — and nothing else about the client is configurable in v1;
/// reqwest's own pool defaults are what a command-line tool wants.
///
/// Fails only when reqwest cannot construct a client at all (a TLS backend that
/// will not initialise, say), which is fatal to the whole run and so is
/// [`SendraError::Client`] rather than a per-request network error.
pub fn build_client(config: &Config) -> Result<HttpClient, SendraError> {
    let redirects: RedirectLog = Arc::new(Mutex::new(Vec::new()));

    let policy = match config.redirects {
        // `Policy::none()` hands the 3xx response straight back rather than
        // erroring: a redirect with following disabled is a normal,
        // inspectable response, not a failure. Our custom policy below is
        // never consulted in this case, so nothing is logged — which is
        // exactly right, since there is no chain to show.
        FollowRedirects::Disabled => reqwest::redirect::Policy::none(),

        // Custom rather than `Policy::limited(max)`, because `limited` has no
        // way to tell us what it saw: every attempt is a hop this crate wants
        // to show, whether or not it ends up being followed.
        FollowRedirects::Follow(max) => {
            let log = redirects.clone();
            reqwest::redirect::Policy::custom(move |attempt: reqwest::redirect::Attempt| {
                log.lock().unwrap().push(RedirectHop {
                    status: attempt.status().as_u16(),
                    location: attempt.url().to_string(),
                });

                // `previous()` does not count the attempt now being decided,
                // so this matches `Policy::limited`'s own rule: `max` hops are
                // allowed, and the one that would make it `max + 1` errors.
                if attempt.previous().len() as u32 >= max {
                    attempt.error(TooManyRedirects { max })
                } else {
                    attempt.follow()
                }
            })
        }
    };

    // reqwest has no timeout of its own by default, so an unresponsive server
    // would hang the process indefinitely; the config always supplies one.
    let inner = reqwest::Client::builder()
        .timeout(config.timeout)
        .redirect(policy)
        .build()
        .map_err(SendraError::Client)?;

    Ok(HttpClient {
        inner,
        redirects,
        timeout: config.timeout,
    })
}

/// Raised by the custom redirect policy in [`build_client`] when a chain runs
/// past the configured maximum.
///
/// **Exceeding the limit is an error, the same as reqwest's own default
/// behaviour today.** A response was never short of one — the chain simply
/// did not resolve within the hops the config allows — so there is no single
/// "last response reached" that would not misrepresent what happened, the way
/// there would be for a hop that landed on a plain 3xx with redirects turned
/// off entirely. This reaches the caller as [`SendraError::Network`], wrapping
/// reqwest's own redirect error, exactly like a DNS or TLS failure.
#[derive(Debug)]
struct TooManyRedirects {
    max: u32,
}

impl std::fmt::Display for TooManyRedirects {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "exceeded the configured maximum of {} redirect(s)",
            self.max
        )
    }
}

impl std::error::Error for TooManyRedirects {}
