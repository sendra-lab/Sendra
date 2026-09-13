//! OAuth token acquisition for `auth.oauth`'s three supported grants —
//! `client_credentials`, `password`, and `authorization_code` — and
//! [`OAuthTokenCache`], the in-run, in-memory cache that lets many requests
//! sharing one `oauth:` config reuse one token instead of re-authenticating
//! per request.
//!
//! **`client_credentials` and `password` acquire automatically.**
//! [`acquire_token`] makes the token request itself, with no human
//! involved — the shape [`crate::Request::resolve_oauth`] calls before every
//! send.
//!
//! **`authorization_code` cannot acquire automatically.** It needs a human
//! to approve access in a browser and a local callback listener to catch
//! the resulting code — a fundamentally different problem from making an
//! HTTP call, and not something a headless send can do on its own.
//! [`acquire_token`] refuses this grant outright (see its doc comment)
//! rather than pretend it can proceed; instead, three building blocks let a
//! front end — sendra-tui, concretely — drive the flow itself:
//! [`generate_pkce`] (a fresh verifier/challenge pair — see [RFC 7636]),
//! [`build_authorization_url`] (the URL to open a browser to), and
//! [`exchange_authorization_code`] (trading the code the callback caught for
//! a token). Once that exchange succeeds, [`OAuthTokenCache::insert_token`]
//! puts the result in the exact same cache [`acquire_token`] reads, so every
//! later request sharing this `oauth:` config — via the ordinary
//! `resolve_oauth` path, completely unchanged — reuses it instead of asking
//! the human to log in again.
//!
//! PKCE is used unconditionally for `authorization_code`, not offered as a
//! config toggle: [RFC 8252 §8.1] (OAuth for native apps) treats it as
//! required for exactly this kind of client, since a desktop/TUI app cannot
//! keep a `client_secret` confidential the way a server-side client can, and
//! is equally exposed to authorization-code interception on the loopback
//! redirect either way.
//!
//! `refresh_token` is not implemented: once expiry-checking exists here, no
//! fast-follow issue is worth its own scope for the marginal request
//! `refresh_token` would save over just re-running the same grant (or, for
//! `authorization_code`, logging in again), so it is deferred rather than
//! treated as a real gap.
//!
//! **No cross-invocation persistence.** [`OAuthTokenCache`] lives only as
//! long as the process that built it — never written to disk — the same
//! conservative stance this project already takes on captured variables and
//! the cookie jar: new statefulness is opt-in and scoped to one run, not
//! silently carried between separate `sendra` invocations. A cached token on
//! disk would need the same protection `${VAR}` passthrough was designed
//! around, for a feature nothing has asked for yet. This holds just as much
//! for a token acquired interactively through `authorization_code`: it lives
//! only in this cache, for this run, and is never written anywhere.
//!
//! [RFC 7636]: https://www.rfc-editor.org/rfc/rfc7636
//! [RFC 8252 §8.1]: https://www.rfc-editor.org/rfc/rfc8252#section-8.1

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::http::client::HttpClient;
use crate::http::send_prepared;
use crate::request::auth::{OAuthAuth, OAuthGrantType};
use crate::request::{Method, Request};
use crate::SendraError;

/// How much earlier than a token's actual `expires_in` it is treated as
/// expired.
///
/// Guards against acquiring a token, caching it, and then racing its own
/// expiry: "the token is still valid" is only ever true at the instant it is
/// checked, and the request it authorizes reaches the wire some — usually
/// small — amount of time later. 30 seconds is generous enough to cover that
/// gap (and ordinary clock skew against the token server) without
/// discarding a meaningful fraction of the lifetime of the short-lived
/// tokens (a minute or two) some servers issue.
const EXPIRY_MARGIN: Duration = Duration::from_secs(30);

/// [`Auth::oauth`](crate::Auth::oauth)'s identity for caching: two requests
/// with the same `token_url`, `client_id`, `grant_type` and `scope` share
/// one token rather than each acquiring their own.
///
/// `scope` is part of the key — not just the three "which client, at which
/// endpoint, under which grant" fields — because a server is free to issue a
/// narrower or differently-scoped token for the same client under a
/// different `scope`; folding two different scopes into one cache entry
/// could hand a request a token that cannot actually do what it asked for.
///
/// `client_secret`/`username`/`password` are deliberately **not** part of
/// the key: they are credentials, not identity. The same
/// `token_url`/`client_id`/`grant_type`/`scope` with a different secret is a
/// configuration error `auth.oauth` has no business caching around, not a
/// second legitimate identity to track.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    token_url: String,
    client_id: String,
    grant_type: OAuthGrantType,
    scope: Option<String>,
}

impl CacheKey {
    fn from(auth: &OAuthAuth) -> Self {
        Self {
            token_url: auth.token_url.clone(),
            client_id: auth.client_id.clone(),
            grant_type: auth.grant_type,
            scope: auth.scope.clone(),
        }
    }
}

/// One acquired-or-failed OAuth token, keyed by [`CacheKey`].
enum CacheEntry {
    Token {
        access_token: String,
        /// `None` when the token response omitted `expires_in` — see
        /// [`acquire_token`]'s doc comment for why that is treated as "does
        /// not expire for this run" rather than guessed at.
        expires_at: Option<Instant>,
    },
    /// A remembered acquisition failure for this exact config, so a second
    /// request sharing a broken `oauth:` config fails immediately rather
    /// than hitting an endpoint that already refused it. See
    /// [`OAuthTokenCache`]'s doc comment for the tradeoff this makes.
    Failed(String),
}

/// The in-run OAuth token cache: acquired (or failed) tokens, keyed by
/// [`CacheKey`], shared by every request in one `sendra` invocation that
/// resolves the same `oauth:` config.
///
/// **Scoped to one run, never persisted.** See the module doc comment.
/// Built once per invocation — in `sendra-cli`, alongside the one
/// [`HttpClient`] the whole run shares — and, under `--repeat`, deliberately
/// **outlives every pass** rather than being rebuilt per iteration: a
/// `--repeat 5` run is still one invocation, and the waste this cache exists
/// to eliminate — a token request on every single call — would otherwise
/// resurface once per pass instead of once for the run. This is the same
/// reasoning that keeps the shared `HttpClient` across passes; it is the
/// per-*pass* capture store, and (when `--cookie-jar` is set) the client's
/// cookie jar, that are deliberately reset instead, because captures and
/// cookies are meant to model one fresh run each time, while
/// re-authenticating every pass is exactly the waste this cache exists to
/// eliminate.
///
/// **A failed acquisition is cached too, and is not retried.** Once a given
/// `oauth:` config has failed once in this run, every later request sharing
/// it fails immediately with the same reason rather than hitting the token
/// endpoint again. A broken config (bad credentials, a typo'd `token_url`,
/// an endpoint that is genuinely down) is not going to fix itself between
/// one request and the next *within the same invocation* — retrying it for
/// every request in a large collection would only hammer an endpoint that
/// has already said no, and would queue the run's other, unrelated failures
/// behind a string of repeated timeouts. The cost is that a config which
/// failed on a one-off transient blip (a dropped connection, a token server
/// mid-restart) stays failed for the rest of this run — but the fix there is
/// simply running `sendra` again, which is cheap, whereas there is no cheap
/// way to walk back having hammered a struggling endpoint once per request
/// instead of once.
pub struct OAuthTokenCache {
    entries: Mutex<HashMap<CacheKey, CacheEntry>>,
}

impl OAuthTokenCache {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for OAuthTokenCache {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for OAuthTokenCache {
    /// Deliberately never prints a cached access token — only how many
    /// entries exist. `sendra-tui`'s `AppState` derives `Debug` (used only
    /// for `assert_eq!`/panic messages in its own tests, never logged), and
    /// this cache is one of its fields; a token leaking into a debug print
    /// anywhere would defeat the whole "never persisted, never written
    /// anywhere but this in-memory cache" guarantee the module doc comment
    /// makes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self
            .entries
            .lock()
            .map(|entries| entries.len())
            .unwrap_or(0);
        f.debug_struct("OAuthTokenCache")
            .field("entries", &count)
            .finish()
    }
}

impl OAuthTokenCache {
    /// Records a token acquired outside the ordinary [`acquire_token`] path —
    /// the one case that needs this is an `authorization_code` login
    /// completed interactively (see the module doc comment), whose caller
    /// exchanged a code for a token itself via [`exchange_authorization_code`]
    /// and now wants every later request sharing this exact `oauth:` config
    /// to reuse it instead of asking the human to log in again.
    ///
    /// Keyed and stored exactly like a token [`acquire_token`] acquired
    /// itself — same [`CacheKey`], same expiry-margin handling — so from
    /// `acquire_token`'s perspective afterward there is no difference
    /// between a token it fetched and one handed to it this way.
    pub fn insert_token(&self, auth: &OAuthAuth, access_token: String, expires_in: Option<u64>) {
        let key = CacheKey::from(auth);
        let expires_at = expires_in.map(|secs| Instant::now() + Duration::from_secs(secs));
        let mut entries = self
            .entries
            .lock()
            .expect("the cache mutex is never held across a panic");
        entries.insert(
            key,
            CacheEntry::Token {
                access_token,
                expires_at,
            },
        );
    }
}

/// A token endpoint's JSON response — only the fields Sendra reads. Every
/// other field a server includes (`refresh_token`, `id_token`, `token_type`,
/// ...) is ignored: v1 implements neither `refresh_token` nor OIDC ID
/// tokens, and the bearer form this becomes needs nothing else. See the
/// module doc comment.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// Acquire (or reuse a cached) access token for `auth`.
///
/// [`crate::Request::resolve_oauth`] is the only caller, and hands the plain
/// bearer token string this returns to the exact same code path
/// [`crate::Request::resolve_auth`] already uses for `auth.bearer`.
///
/// **`grant_type: authorization_code` never reaches the token endpoint from
/// here.** A cache hit is served exactly like any other grant's, since a
/// token acquired interactively via [`exchange_authorization_code`] and
/// stored with [`OAuthTokenCache::insert_token`] is indistinguishable from
/// one acquired automatically once cached. On a cache miss, though, there is
/// no `code` to send and no way to obtain one without a browser and a human
/// — see the module doc comment — so this returns
/// [`SendraError::OAuthAcquisition`] naming that directly, instead of
/// attempting a token request that could not possibly succeed.
///
/// Reuses [`crate::http::send_prepared`] to make the token request itself —
/// through the same shared [`HttpClient`] every other request in the run
/// sends through, so a token acquisition is not a second, unrelated HTTP
/// stack, and inherits the same timeout, proxy and TLS settings. The
/// request sent is `POST <token_url>` with
/// `Content-Type: application/x-www-form-urlencoded` and a body of
/// `grant_type`, `client_id`, `client_secret`, and — for
/// `grant_type: password` — `username`/`password`, plus `scope` when set:
/// the standard shape an OAuth 2.0 token request takes (RFC 6749 §4.3.2,
/// §4.4.2).
///
/// A response outside 2xx, or a 2xx body with no `access_token`, is
/// [`SendraError::OAuthAcquisition`], naming `token_url` and why. So is a
/// network/timeout failure reaching the endpoint at all —
/// `send_prepared`'s own [`SendraError::Network`]/[`SendraError::Timeout`],
/// re-described here rather than passed through directly, so every
/// acquisition failure a caller can match on is the one variant regardless
/// of which of these three things went wrong.
///
/// **`expires_in` omitted by the server** is treated as "this token does not
/// expire for the rest of this run" — no expiry is recorded, and the cached
/// token is reused until the process exits — rather than guessed at with an
/// arbitrary default lifetime. A server that does not say when a token
/// expires has given no basis for picking one duration over another, and a
/// wrong guess is bad in both directions: too short reacquires (and
/// re-spends any rate limit) needlessly, too long risks sending an
/// already-invalid token. `client_credentials` tokens in particular are
/// commonly long-lived or effectively static per client, which is the
/// ordinary case this default fits.
///
/// See [`OAuthTokenCache`] for the cache key, the expiry margin, and the
/// retry-vs-fail-fast decision for a config that has already failed once in
/// this run.
pub async fn acquire_token(
    auth: &OAuthAuth,
    client: &HttpClient,
    cache: &OAuthTokenCache,
) -> Result<String, SendraError> {
    let key = CacheKey::from(auth);

    {
        let entries = cache
            .entries
            .lock()
            .expect("the cache mutex is never held across a panic");
        match entries.get(&key) {
            Some(CacheEntry::Token {
                access_token,
                expires_at,
            }) => {
                let still_valid = match expires_at {
                    Some(expires_at) => Instant::now() + EXPIRY_MARGIN < *expires_at,
                    None => true,
                };
                if still_valid {
                    return Ok(access_token.clone());
                }
                // Expired (or within the margin of expiring): fall through
                // and acquire a fresh one below, using the same
                // cache-and-reuse logic as a first-time acquisition.
            }
            Some(CacheEntry::Failed(reason)) => {
                return Err(SendraError::OAuthAcquisition {
                    token_url: auth.token_url.clone(),
                    reason: reason.clone(),
                });
            }
            None => {}
        }
        // Lock dropped here, before the `.await` below — never held across
        // one.
    }

    if auth.grant_type == OAuthGrantType::AuthorizationCode {
        let reason = "grant_type: authorization_code requires an interactive browser login — \
                       trigger it from the auth editor, then re-run this request"
            .to_string();
        let mut entries = cache
            .entries
            .lock()
            .expect("the cache mutex is never held across a panic");
        entries.insert(key, CacheEntry::Failed(reason.clone()));
        return Err(SendraError::OAuthAcquisition {
            token_url: auth.token_url.clone(),
            reason,
        });
    }

    match acquire_fresh(auth, client).await {
        Ok((access_token, expires_in)) => {
            let expires_at = expires_in.map(|secs| Instant::now() + Duration::from_secs(secs));
            let mut entries = cache
                .entries
                .lock()
                .expect("the cache mutex is never held across a panic");
            entries.insert(
                key,
                CacheEntry::Token {
                    access_token: access_token.clone(),
                    expires_at,
                },
            );
            Ok(access_token)
        }
        Err(reason) => {
            let mut entries = cache
                .entries
                .lock()
                .expect("the cache mutex is never held across a panic");
            entries.insert(key, CacheEntry::Failed(reason.clone()));
            Err(SendraError::OAuthAcquisition {
                token_url: auth.token_url.clone(),
                reason,
            })
        }
    }
}

/// The actual token request, with no cache involved — [`acquire_token`]'s
/// only caller, split out so the cache-locking there stays free of the
/// request-building and response-parsing detail.
async fn acquire_fresh(
    auth: &OAuthAuth,
    client: &HttpClient,
) -> Result<(String, Option<u64>), String> {
    let mut form: Vec<(String, String)> = vec![
        (
            "grant_type".to_string(),
            auth.grant_type.as_str().to_string(),
        ),
        ("client_id".to_string(), auth.client_id.clone()),
    ];
    if let Some(client_secret) = &auth.client_secret {
        form.push(("client_secret".to_string(), client_secret.clone()));
    }
    if let Some(scope) = &auth.scope {
        form.push(("scope".to_string(), scope.clone()));
    }
    if auth.grant_type == OAuthGrantType::Password {
        form.push((
            "username".to_string(),
            auth.username.clone().unwrap_or_default(),
        ));
        form.push((
            "password".to_string(),
            auth.password.clone().unwrap_or_default(),
        ));
    }
    post_token_form(auth, client, form).await
}

/// A code exchanged for [`exchange_authorization_code`], obtained interactively
/// (see the module doc comment) rather than from `auth.oauth` itself, so it
/// cannot travel through [`OAuthAuth`] the way every other grant's fields do.
///
/// Trades `code` (plus the PKCE `code_verifier` matching the `code_challenge`
/// [`build_authorization_url`] sent) for a token, via the standard
/// `authorization_code` token request (RFC 6749 §4.1.3): `POST
/// auth.token_url` with `grant_type=authorization_code`, `code`,
/// `redirect_uri`, `client_id`, `code_verifier`, and `client_secret` when
/// `auth` has one — reusing [`send_prepared`] through the same shared
/// [`HttpClient`] every other request in the run sends through, exactly like
/// [`acquire_fresh`] does for the other two grants.
///
/// This is a standalone entry point, not folded into [`acquire_token`]: it is
/// driven by a front end that already has a `code` in hand from its own
/// browser/callback-listener flow, not by the automatic "acquire whatever
/// this `oauth:` config needs" path every other grant uses. Once this
/// succeeds, the caller is expected to hand the result to
/// [`OAuthTokenCache::insert_token`] so later automatic resolution reuses it
/// — see the module doc comment.
///
/// Errors exactly like [`acquire_token`] does: a non-2xx response or an
/// unparseable body is [`SendraError::OAuthAcquisition`] naming
/// `auth.token_url` and why; so is a network/timeout failure reaching it.
pub async fn exchange_authorization_code(
    auth: &OAuthAuth,
    client: &HttpClient,
    code: &str,
    code_verifier: &str,
) -> Result<(String, Option<u64>), SendraError> {
    let redirect_uri = auth.redirect_uri.clone().unwrap_or_default();
    let mut form: Vec<(String, String)> = vec![
        (
            "grant_type".to_string(),
            OAuthGrantType::AuthorizationCode.as_str().to_string(),
        ),
        ("code".to_string(), code.to_string()),
        ("redirect_uri".to_string(), redirect_uri),
        ("client_id".to_string(), auth.client_id.clone()),
        ("code_verifier".to_string(), code_verifier.to_string()),
    ];
    if let Some(client_secret) = &auth.client_secret {
        form.push(("client_secret".to_string(), client_secret.clone()));
    }

    post_token_form(auth, client, form)
        .await
        .map_err(|reason| SendraError::OAuthAcquisition {
            token_url: auth.token_url.clone(),
            reason,
        })
}

/// The wire mechanics shared by [`acquire_fresh`] and
/// [`exchange_authorization_code`]: `POST auth.token_url` with `form` as
/// `application/x-www-form-urlencoded`, then parse the response as
/// [`TokenResponse`] — everything past "which fields does this grant send"
/// is identical between every grant, so it lives here once.
async fn post_token_form(
    auth: &OAuthAuth,
    client: &HttpClient,
    form: Vec<(String, String)>,
) -> Result<(String, Option<u64>), String> {
    let body = serde_urlencoded::to_string(&form)
        .expect("a Vec<(String, String)> always encodes as x-www-form-urlencoded pairs");

    let request = Request {
        name: None,
        method: Method::Post,
        url: auth.token_url.clone(),
        headers: vec![(
            "Content-Type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        )],
        query: Vec::new(),
        body: Some(body),
        json: None,
        body_file: None,
        form: Vec::new(),
        multipart: Vec::new(),
        auth: None,
        assertions: None,
        pre_request: None,
        post_request: None,
        capture: None,
        retry: None,
    };

    let response = send_prepared(&request, client)
        .await
        .map_err(|err| err.to_string())?;

    if !(200..300).contains(&response.status) {
        return Err(format!(
            "token endpoint responded {} {}: {}",
            response.status,
            response.status_text,
            truncate(&response.body)
        ));
    }

    let parsed: TokenResponse = serde_json::from_str(&response.body).map_err(|err| {
        format!(
            "could not parse the token response as JSON: {err} (body: {})",
            truncate(&response.body)
        )
    })?;

    Ok((parsed.access_token, parsed.expires_in))
}

/// A fresh PKCE verifier/challenge pair for one `authorization_code` login
/// attempt (RFC 7636) — see the module doc comment for why this is used
/// unconditionally rather than offered as config.
///
/// `verifier` is 32 bytes (256 bits) of randomness — two
/// [`uuid::Uuid::new_v4`] values concatenated, reusing the dependency this
/// workspace already pulls in for `uuid()` rather than adding `rand` for the
/// entropy source alone — base64url-encoded without padding, which RFC
/// 7636's `code_verifier` charset (unreserved URL characters) accepts
/// directly and yields exactly 43 characters, the shortest length the RFC
/// allows. `challenge` is `BASE64URL-ENCODE(SHA256(verifier))`, the `S256`
/// method [`build_authorization_url`] declares — the method RFC 7636 §4.2
/// requires clients to use "if the client is capable of doing so", which a
/// TUI, unlike some constrained embedded clients, always is.
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

pub fn generate_pkce() -> PkcePair {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    PkcePair {
        verifier,
        challenge,
    }
}

/// A fresh CSRF `state` value for one `authorization_code` login attempt
/// (RFC 6749 §10.12) — checked against the callback's own `state` parameter
/// by whichever front end ran [`build_authorization_url`], not by anything
/// in this module, since holding onto the value being checked against is
/// specific to how that front end tracks a pending login.
pub fn generate_state() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The URL to open a browser to for one `authorization_code` login attempt:
/// `auth.authorization_url` with `response_type=code`, `client_id`,
/// `redirect_uri`, `code_challenge`/`code_challenge_method=S256` (from
/// [`generate_pkce`]), `state` (from [`generate_state`]), and `scope` when
/// `auth` has one, appended as query parameters — the standard shape an
/// OAuth 2.0 authorization request takes (RFC 6749 §4.1.1) plus PKCE's two
/// parameters (RFC 7636 §4.3).
///
/// Uses [`reqwest::Url`]'s query-pair API — already a dependency, and the
/// same one [`crate::Request::resolve_query`] uses — rather than string
/// concatenation, so `redirect_uri` and `scope` are percent-encoded
/// correctly regardless of what characters they contain.
///
/// Errors with [`SendraError::OAuthAuthorizationUrl`] if
/// `auth.authorization_url` does not parse as a URL — the one failure mode
/// possible before any browser or network is involved.
pub fn build_authorization_url(
    auth: &OAuthAuth,
    state: &str,
    code_challenge: &str,
) -> Result<String, SendraError> {
    let authorization_url = auth.authorization_url.clone().unwrap_or_default();
    let mut url = reqwest::Url::parse(&authorization_url).map_err(|source| {
        SendraError::OAuthAuthorizationUrl {
            authorization_url: authorization_url.clone(),
            reason: source.to_string(),
        }
    })?;

    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("response_type", "code");
        pairs.append_pair("client_id", &auth.client_id);
        pairs.append_pair(
            "redirect_uri",
            auth.redirect_uri.as_deref().unwrap_or_default(),
        );
        pairs.append_pair("code_challenge", code_challenge);
        pairs.append_pair("code_challenge_method", "S256");
        pairs.append_pair("state", state);
        if let Some(scope) = &auth.scope {
            pairs.append_pair("scope", scope);
        }
    }

    Ok(url.into())
}

/// Keeps an acquisition-failure message from embedding an entire
/// (possibly huge, possibly HTML) response body.
fn truncate(body: &str) -> String {
    const MAX_CHARS: usize = 200;
    if body.chars().count() <= MAX_CHARS {
        body.to_string()
    } else {
        format!("{}...", body.chars().take(MAX_CHARS).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::http::client::build_client;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn oauth_auth(token_url: &str) -> OAuthAuth {
        OAuthAuth {
            grant_type: OAuthGrantType::ClientCredentials,
            token_url: token_url.to_string(),
            client_id: "client-id".to_string(),
            client_secret: Some("client-secret".to_string()),
            scope: None,
            username: None,
            password: None,
            authorization_url: None,
            redirect_uri: None,
        }
    }

    fn client() -> HttpClient {
        build_client(&Config::default()).expect("a client builds")
    }

    /// A token endpoint that answers every request on `/token` with the
    /// same fixed raw HTTP response and counts how many times it was hit —
    /// hand-rolled over a blocking `TcpListener`, the same pattern
    /// `crate::test_support` uses, since what these tests need to observe
    /// (whether the endpoint was hit a second time at all) is below the
    /// level a mock-server crate would add anything over.
    struct TokenServer {
        addr: SocketAddr,
        hits: Arc<AtomicUsize>,
    }

    impl TokenServer {
        fn start(response: Vec<u8>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
            let addr = listener.local_addr().expect("the listener has an address");
            let hits = Arc::new(AtomicUsize::new(0));

            let counted = hits.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    let mut writer = stream.try_clone().expect("the socket clones");
                    let mut reader = BufReader::new(stream);

                    let mut request_line = String::new();
                    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                        continue;
                    }
                    let mut content_length = 0usize;
                    loop {
                        let mut header = String::new();
                        match reader.read_line(&mut header) {
                            Ok(0) | Err(_) => break,
                            Ok(_) if header == "\r\n" => break,
                            Ok(_) => {
                                if let Some((name, value)) = header.split_once(':') {
                                    if name.trim().eq_ignore_ascii_case("content-length") {
                                        content_length = value.trim().parse().unwrap_or(0);
                                    }
                                }
                            }
                        }
                    }
                    let mut body = vec![0u8; content_length];
                    if content_length > 0 {
                        use std::io::Read;
                        let _ = reader.read_exact(&mut body);
                    }

                    counted.fetch_add(1, Ordering::SeqCst);
                    if writer.write_all(&response).is_err() {
                        continue;
                    }
                    let _ = writer.flush();
                }
            });

            Self { addr, hits }
        }

        fn token_url(&self) -> String {
            format!("http://{}/token", self.addr)
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }

    fn token_response(body: &'static str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[tokio::test]
    async fn client_credentials_acquires_a_token() {
        let server = TokenServer::start(token_response(
            r#"{"access_token": "abc123", "token_type": "Bearer"}"#,
        ));
        let auth = oauth_auth(&server.token_url());
        let client = client();
        let cache = OAuthTokenCache::new();

        let token = acquire_token(&auth, &client, &cache)
            .await
            .expect("the mock token endpoint answers");
        assert_eq!(token, "abc123");
    }

    #[tokio::test]
    async fn password_grant_acquires_a_token() {
        let server = TokenServer::start(token_response(r#"{"access_token": "pwd-token"}"#));
        let auth = OAuthAuth {
            grant_type: OAuthGrantType::Password,
            username: Some("ada".to_string()),
            password: Some("s3cr3t".to_string()),
            ..oauth_auth(&server.token_url())
        };
        let client = client();
        let cache = OAuthTokenCache::new();

        let token = acquire_token(&auth, &client, &cache)
            .await
            .expect("the mock token endpoint answers");
        assert_eq!(token, "pwd-token");
    }

    #[tokio::test]
    async fn a_token_is_reused_across_requests_sharing_the_same_config() {
        let server = TokenServer::start(token_response(r#"{"access_token": "shared"}"#));
        let auth = oauth_auth(&server.token_url());
        let client = client();
        let cache = OAuthTokenCache::new();

        for _ in 0..3 {
            let token = acquire_token(&auth, &client, &cache)
                .await
                .expect("acquires or reuses successfully");
            assert_eq!(token, "shared");
        }

        assert_eq!(
            server.hits(),
            1,
            "three requests through one config must acquire exactly one token"
        );
    }

    #[tokio::test]
    async fn a_token_with_no_expires_in_is_reused_indefinitely() {
        let server = TokenServer::start(token_response(r#"{"access_token": "no-expiry"}"#));
        let auth = oauth_auth(&server.token_url());
        let client = client();
        let cache = OAuthTokenCache::new();

        for _ in 0..5 {
            acquire_token(&auth, &client, &cache)
                .await
                .expect("acquires or reuses successfully");
        }

        assert_eq!(
            server.hits(),
            1,
            "omitting `expires_in` must be treated as not expiring for this run, not \
             reacquired on every call"
        );
    }

    #[tokio::test]
    async fn an_expired_cached_token_triggers_reacquisition() {
        let server = TokenServer::start(token_response(
            r#"{"access_token": "still-first", "expires_in": 0}"#,
        ));
        let auth = oauth_auth(&server.token_url());
        let client = client();
        let cache = OAuthTokenCache::new();

        acquire_token(&auth, &client, &cache)
            .await
            .expect("the first acquisition succeeds");
        // `expires_in: 0` is already inside the expiry margin at the moment
        // it is cached, so this second call must reacquire rather than
        // reuse — visible as a second hit on the endpoint, not just an
        // equal token value (the server always answers the same body).
        acquire_token(&auth, &client, &cache)
            .await
            .expect("reacquisition against the same, still-up server succeeds");

        assert_eq!(
            server.hits(),
            2,
            "an expired cached token must trigger a fresh acquisition"
        );
    }

    #[tokio::test]
    async fn a_non_2xx_token_response_is_a_typed_acquisition_error() {
        let server = TokenServer::start(
            b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 20\r\n\r\n{\"error\":\"denied\"}\r\n"
                .to_vec(),
        );
        let auth = oauth_auth(&server.token_url());
        let client = client();
        let cache = OAuthTokenCache::new();

        let err = acquire_token(&auth, &client, &cache)
            .await
            .expect_err("a 401 must not be treated as success");
        match err {
            SendraError::OAuthAcquisition { reason, .. } => {
                assert!(reason.contains("401"), "got {reason}");
            }
            other => panic!("expected OAuthAcquisition, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_malformed_token_response_is_a_typed_acquisition_error() {
        let server = TokenServer::start(token_response("not json"));
        let auth = oauth_auth(&server.token_url());
        let client = client();
        let cache = OAuthTokenCache::new();

        let err = acquire_token(&auth, &client, &cache)
            .await
            .expect_err("a non-JSON body must not be treated as success");
        assert!(matches!(err, SendraError::OAuthAcquisition { .. }));
    }

    #[tokio::test]
    async fn a_failed_acquisition_is_remembered_and_not_retried() {
        let server = TokenServer::start(token_response("not json"));
        let auth = oauth_auth(&server.token_url());
        let client = client();
        let cache = OAuthTokenCache::new();

        assert!(acquire_token(&auth, &client, &cache).await.is_err());
        assert!(acquire_token(&auth, &client, &cache).await.is_err());

        assert_eq!(
            server.hits(),
            1,
            "a config that already failed once in this run must not be retried against \
             the endpoint for a second request"
        );
    }

    #[tokio::test]
    async fn two_different_scopes_are_cached_separately() {
        let server = TokenServer::start(token_response(r#"{"access_token": "tok"}"#));
        let base = oauth_auth(&server.token_url());
        let client = client();
        let cache = OAuthTokenCache::new();

        let scoped_a = OAuthAuth {
            scope: Some("read".to_string()),
            ..base.clone()
        };
        let scoped_b = OAuthAuth {
            scope: Some("write".to_string()),
            ..base
        };

        acquire_token(&scoped_a, &client, &cache)
            .await
            .expect("acquires");
        acquire_token(&scoped_b, &client, &cache)
            .await
            .expect("acquires");

        assert_eq!(
            server.hits(),
            2,
            "two different scopes must not share one cache entry"
        );
    }

    // --- authorization_code: cannot auto-acquire ---------------------------

    fn authorization_code_auth(token_url: &str) -> OAuthAuth {
        OAuthAuth {
            grant_type: OAuthGrantType::AuthorizationCode,
            token_url: token_url.to_string(),
            client_id: "client-id".to_string(),
            client_secret: None,
            scope: None,
            username: None,
            password: None,
            authorization_url: Some("https://auth.example.com/authorize".to_string()),
            redirect_uri: Some("http://127.0.0.1:8899/callback".to_string()),
        }
    }

    #[tokio::test]
    async fn authorization_code_never_reaches_the_token_endpoint_via_acquire_token() {
        let server = TokenServer::start(token_response(r#"{"access_token": "unused"}"#));
        let auth = authorization_code_auth(&server.token_url());
        let client = client();
        let cache = OAuthTokenCache::new();

        let err = acquire_token(&auth, &client, &cache)
            .await
            .expect_err("no code is available for an automatic acquisition");
        match err {
            SendraError::OAuthAcquisition { reason, .. } => {
                assert!(reason.contains("interactive"), "got {reason}");
            }
            other => panic!("expected OAuthAcquisition, got {other:?}"),
        }
        assert_eq!(
            server.hits(),
            0,
            "acquire_token must never hit the token endpoint for authorization_code — there is \
             no code to send"
        );
    }

    #[tokio::test]
    async fn a_token_inserted_via_insert_token_is_served_by_acquire_token_afterward() {
        let server = TokenServer::start(token_response(r#"{"access_token": "unused"}"#));
        let auth = authorization_code_auth(&server.token_url());
        let cache = OAuthTokenCache::new();

        // Simulates the interactive login flow's own conclusion: exchange
        // happened elsewhere (`exchange_authorization_code`), and the result
        // is written straight into the cache — never through `acquire_fresh`.
        cache.insert_token(&auth, "interactively-acquired".to_string(), Some(3600));

        let client = client();
        let token = acquire_token(&auth, &client, &cache)
            .await
            .expect("a token inserted via insert_token must be served like any other");
        assert_eq!(token, "interactively-acquired");
        assert_eq!(
            server.hits(),
            0,
            "serving an inserted token must never itself hit the token endpoint"
        );
    }

    // --- PKCE / authorization URL / code exchange ---------------------------

    #[test]
    fn generate_pkce_produces_a_verifier_and_a_matching_s256_challenge() {
        let pkce = generate_pkce();

        assert_eq!(
            pkce.verifier.len(),
            43,
            "32 bytes base64url-no-pad encodes to exactly 43 characters"
        );
        assert!(
            pkce.verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "the verifier must use only RFC 7636's unreserved base64url characters: {}",
            pkce.verifier
        );

        let expected_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected_challenge);
    }

    #[test]
    fn generate_pkce_never_repeats_across_calls() {
        let a = generate_pkce();
        let b = generate_pkce();
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.challenge, b.challenge);
    }

    #[test]
    fn generate_state_never_repeats_across_calls() {
        assert_ne!(generate_state(), generate_state());
    }

    #[test]
    fn build_authorization_url_includes_every_required_parameter() {
        let mut auth = authorization_code_auth("https://auth.example.com/token");
        auth.scope = Some("read write".to_string());

        let url = build_authorization_url(&auth, "csrf-state", "the-challenge")
            .expect("a valid authorization_url must build");
        let parsed = reqwest::Url::parse(&url).expect("the result is itself a valid URL");

        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            pairs.get("client_id").map(String::as_str),
            Some("client-id")
        );
        assert_eq!(
            pairs.get("redirect_uri").map(String::as_str),
            Some("http://127.0.0.1:8899/callback")
        );
        assert_eq!(
            pairs.get("code_challenge").map(String::as_str),
            Some("the-challenge")
        );
        assert_eq!(
            pairs.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(pairs.get("state").map(String::as_str), Some("csrf-state"));
        assert_eq!(pairs.get("scope").map(String::as_str), Some("read write"));
    }

    #[test]
    fn build_authorization_url_rejects_an_unparseable_authorization_url() {
        let mut auth = authorization_code_auth("https://auth.example.com/token");
        auth.authorization_url = Some("not a url".to_string());

        let err = build_authorization_url(&auth, "state", "challenge").expect_err(
            "an unparseable authorization_url must be rejected before any network call",
        );
        assert!(matches!(err, SendraError::OAuthAuthorizationUrl { .. }));
    }

    #[tokio::test]
    async fn exchange_authorization_code_acquires_a_token() {
        let server = TokenServer::start(token_response(r#"{"access_token": "exchanged"}"#));
        let auth = authorization_code_auth(&server.token_url());
        let client = client();

        let (token, expires_in) =
            exchange_authorization_code(&auth, &client, "the-code", "the-verifier")
                .await
                .expect("the mock token endpoint answers");
        assert_eq!(token, "exchanged");
        assert_eq!(expires_in, None);
    }

    #[tokio::test]
    async fn exchange_authorization_code_surfaces_a_non_2xx_response_as_a_typed_error() {
        let server = TokenServer::start(
            b"HTTP/1.1 400 Bad Request\r\nContent-Length: 20\r\n\r\n{\"error\":\"invalid\"}\r\n"
                .to_vec(),
        );
        let auth = authorization_code_auth(&server.token_url());
        let client = client();

        let err = exchange_authorization_code(&auth, &client, "the-code", "the-verifier")
            .await
            .expect_err("a 400 must not be treated as success");
        match err {
            SendraError::OAuthAcquisition { reason, .. } => {
                assert!(reason.contains("400"), "got {reason}");
            }
            other => panic!("expected OAuthAcquisition, got {other:?}"),
        }
    }
}
