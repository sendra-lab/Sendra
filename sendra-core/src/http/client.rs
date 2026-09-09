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
/// particular one. The config decides six things here — the timeout, the
/// redirect policy, whether TLS certificates are verified, which proxy (if
/// any) requests go through, which client certificate (if any) to present
/// for mutual TLS, and whether cookies received are stored and resent
/// automatically — and nothing else about the client is configurable in v1;
/// reqwest's own pool defaults are what a command-line tool wants.
///
/// **Cookies are opt-in.** [`Config::cookie_jar`] defaults to `false`,
/// matching curl's own default of not persisting cookies across requests
/// unless `-c`/`-b` is passed. When enabled, this hands the client
/// reqwest's own in-memory jar (`ClientBuilder::cookie_store(true)`) rather
/// than a jar Sendra owns: there is no persistence to disk and nothing
/// beyond one invocation to manage, so reqwest's default implementation is
/// exactly what is needed. A request whose `headers:` already sets `Cookie`
/// is left alone — reqwest only fills in the jar's `Cookie` header when the
/// request does not already carry one, confirmed by reading reqwest's own
/// `CookieService` rather than assumed, so an explicit `Cookie:` header
/// always wins outright rather than merging with the jar; Sendra raises no
/// conflict for this the way it
/// does for `auth:` plus an explicit `Authorization` header, since the two
/// are not the same field the way `auth:` resolves *into* `Authorization` —
/// the jar operates beneath any one request's headers, at the client's own
/// connection machinery. Cookies received in response to that request are
/// still stored in the jar regardless of the request's own `Cookie` header,
/// so a later request with no explicit header of its own picks them up.
///
/// **The jar sees every hop of a redirect chain, not just the final
/// response.** reqwest layers its cookie handling *underneath* its
/// redirect-following — each hop of a chain is a separate request/response
/// pair the jar's `CookieService` processes on its own, confirmed by
/// reading reqwest's source rather than assumed — so a `Set-Cookie` on an
/// intermediate hop is stored just as reliably as one on the final
/// response, and is even available to *later* hops in the same chain. This
/// is a genuine advantage over `capture`'s manual `Set-Cookie` capture,
/// which can only see the final response's headers once redirects have
/// been followed — see the module doc comment on
/// [`crate::capture`] for that limitation. For a login flow that redirects
/// through an intermediate hop before setting its session cookie, the jar
/// is the only one of the two that can pick it up.
///
/// Fails when reqwest cannot construct a client at all (a TLS backend that
/// will not initialise, say), when [`Config::proxy`] does not parse as a URL
/// reqwest accepts, or when the client certificate cannot be built — either
/// because `client_cert`/`client_key` names a file that cannot be read
/// ([`SendraError::ClientCertIo`]), only one of the pair is set
/// ([`SendraError::ClientCertIncomplete`]), or the files read do not form a
/// valid identity ([`SendraError::Client`]) — all fatal to the whole run: a
/// malformed `proxy:`/`--proxy` value, or an unusable client certificate,
/// means no request in this run could ever have gone anywhere, same as a
/// client reqwest itself refuses to build.
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
    let mut builder = reqwest::Client::builder()
        .timeout(config.timeout)
        .redirect(policy)
        // Unconditional rather than only-when-true: `false` is exactly
        // reqwest's own default (verify), so this changes nothing for the
        // overwhelming majority of runs and there is no third state to
        // handle.
        .danger_accept_invalid_certs(config.insecure)
        // Unconditional for the same reason: `false` is reqwest's own
        // default (no cookie store), so a run that never asked for
        // `cookie_jar`/`--cookie-jar` builds exactly the client it always
        // has. `cookie_store(true)` hands the client reqwest's own
        // in-memory `Jar` — see this function's doc comment for why that,
        // rather than a jar Sendra owns, is the right implementation here.
        .cookie_store(config.cookie_jar);

    if let Some(url) = &config.proxy {
        // `no_proxy()` first: it turns off reqwest's automatic detection of
        // the system `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` environment
        // variables without touching a proxy added explicitly afterwards —
        // see the reasoning on `Config::proxy`. An explicit proxy is meant to
        // be authoritative for the run, not one more candidate layered on
        // top of whatever the environment happens to say.
        let proxy = reqwest::Proxy::all(url).map_err(SendraError::Client)?;
        builder = builder.no_proxy().proxy(proxy);
    }
    // No `proxy:`/`--proxy`: say nothing, and reqwest's own default —
    // reading `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` from the environment —
    // applies, matching what curl and every other common HTTP tool already
    // do without being asked.

    if let Some(identity) = client_identity(config)? {
        builder = builder.identity(identity);
    }

    let inner = builder.build().map_err(SendraError::Client)?;

    Ok(HttpClient {
        inner,
        redirects,
        timeout: config.timeout,
    })
}

/// Build the client certificate identity for mutual TLS, or `None` when
/// `config` sets neither `client_cert` nor `client_key`.
///
/// Reads both files and concatenates them into one buffer — certificate PEM,
/// then key PEM — because reqwest's `rustls-tls` backend exposes exactly one
/// identity constructor, [`reqwest::Identity::from_pem`], and it wants both
/// halves in a single buffer rather than as two arguments. (The two-argument
/// and PKCS#12 constructors exist on `reqwest::Identity` but are gated behind
/// the `native-tls` feature, which this workspace does not enable — see
/// [`Config::client_cert`]'s doc comment.) A file that cannot be read is
/// [`SendraError::ClientCertIo`], naming the path, before reqwest ever sees
/// it; a buffer reqwest cannot parse as a valid identity is
/// [`SendraError::Client`], the same variant every other client-construction
/// failure here uses.
///
/// Exactly one of `client_cert`/`client_key` being set is refused as
/// [`SendraError::ClientCertIncomplete`] — see that variant's doc comment for
/// why this is checked here rather than earlier: CLI overrides for one half
/// and a config value for the other are a valid combination, and both are
/// folded into `config` before this ever runs.
fn client_identity(config: &Config) -> Result<Option<reqwest::Identity>, SendraError> {
    match (&config.client_cert, &config.client_key) {
        (Some(cert_path), Some(key_path)) => {
            let mut pem = std::fs::read(cert_path).map_err(|source| SendraError::ClientCertIo {
                path: cert_path.clone(),
                source,
            })?;
            let key = std::fs::read(key_path).map_err(|source| SendraError::ClientCertIo {
                path: key_path.clone(),
                source,
            })?;
            pem.push(b'\n');
            pem.extend_from_slice(&key);

            reqwest::Identity::from_pem(&pem)
                .map(Some)
                .map_err(SendraError::Client)
        }
        (Some(_), None) => Err(SendraError::ClientCertIncomplete { which: "cert" }),
        (None, Some(_)) => Err(SendraError::ClientCertIncomplete { which: "key" }),
        (None, None) => Ok(None),
    }
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
