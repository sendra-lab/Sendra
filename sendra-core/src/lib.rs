//! Core model for Sendra: request/response types, YAML loading, HTTP execution.
//!
//! This crate is deliberately free of CLI concerns (argument parsing, terminal
//! colouring, exit codes). A future `sendra-tui` crate will depend on it
//! directly, so everything here returns typed [`SendraError`] values that a
//! front-end can match on rather than pre-formatted strings.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::FollowRedirects;
use crate::environment::{describe_captured, describe_environment, describe_variables};

pub mod assertions;
pub mod capture;
pub mod config;
pub mod environment;
pub mod script;

pub use assertions::{AssertionKind, AssertionReport, AssertionResult, Assertions};
pub use capture::{CaptureFailure, CaptureReport, CaptureResult, Captures};
pub use config::Config;
pub use environment::Environment;
pub use script::{Hook, Script, ScriptOutcome, ScriptOutput, Scripts};

/// Every way loading or sending a request can fail.
///
/// Typed rather than `anyhow` so front-ends can branch on the variant (e.g. a
/// TUI showing a "file missing" prompt vs. a network retry).
#[derive(Debug, thiserror::Error)]
pub enum SendraError {
    #[error("could not read request file `{path}`")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not parse request file `{path}`")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },

    /// YAML that did not come from a file on disk (string input, tests).
    #[error("could not parse request")]
    ParseStr(#[source] serde_yaml::Error),

    #[error("header `{name}` is not valid: {reason}")]
    InvalidHeader { name: String, reason: String },

    #[error("request to `{url}` failed")]
    Network {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    /// The request did not finish inside the configured timeout.
    ///
    /// Split out of [`Network`](Self::Network) because it is the one network
    /// failure whose cause is a *Sendra setting*. Every other one — DNS,
    /// refused connection, TLS — is a statement about the network or the
    /// server, and the fix is out there; this one says the server was still
    /// working when Sendra stopped waiting, and the fix may well be a line in
    /// `.sendra/config.yaml`. Folded into `Network`, all a user got was
    /// "request to `x` failed / caused by: operation timed out", which never
    /// mentions that Sendra imposed the limit or what it was set to.
    ///
    /// Carries the limit that was actually applied — the resolved
    /// [`Config::timeout`](crate::Config::timeout), not the raw
    /// `timeout_seconds` key, which may not have been set at all — so the
    /// message can name it whether it came from a config file or from
    /// [`DEFAULT_TIMEOUT`](crate::config::DEFAULT_TIMEOUT).
    ///
    /// The whole-request timeout covers connect, send *and* body read, so this
    /// is raised from either half of [`send_prepared`]: a server that accepts
    /// the connection and then dribbles the body out too slowly times out here
    /// exactly like one that never answers at all.
    #[error("request to `{url}` timed out after {}s", .timeout.as_secs_f64())]
    Timeout {
        url: String,
        /// The limit that was exceeded, as applied to the client.
        timeout: Duration,
        /// reqwest's own error, kept so the cause chain still shows where in
        /// the request the clock ran out.
        #[source]
        source: reqwest::Error,
    },

    /// The HTTP client itself could not be built, so nothing was sent and
    /// nothing will be: this is a failure of the run's configuration (a TLS
    /// backend that will not initialise, say), not of one request. Separate
    /// from [`Network`](Self::Network) because there is no URL to name — the
    /// client is built once for the whole run, before any request is looked
    /// at.
    #[error("could not build the HTTP client")]
    Client(#[source] reqwest::Error),

    /// A named request was asked for, but the collection has no such name.
    ///
    /// Carries the names that *are* available so a front-end can list them (or
    /// offer a "did you mean") without re-reading the file.
    #[error("no request named `{name}` in this collection (available: {})", .available.join(", "))]
    RequestNotFound {
        name: String,
        available: Vec<String>,
    },

    /// A name was asked for, but the file holds a single request rather than a
    /// collection, so there is nothing to select from.
    #[error(
        "cannot select request `{name}`: this file defines a single request, not a collection"
    )]
    NotACollection { name: String },

    /// The file parsed as a collection but broke a rule serde cannot express:
    /// `requests` must be non-empty, every request must have a `name`, and
    /// those names must be unique.
    #[error("invalid collection: {reason}")]
    InvalidCollection { reason: String },

    /// A single request broke a rule serde cannot express: at most one of
    /// `body`/`json`/`body_file`/`form`/`multipart` may be set, and each
    /// `multipart` part needs exactly one of `value`/`path`. Raised at parse
    /// time — for a collection, wrapped into [`InvalidCollection`](Self::InvalidCollection)
    /// with which request it was, the same way a duplicate name is.
    #[error("invalid request: {reason}")]
    InvalidRequest { reason: String },

    /// A `body_file` (or a multipart `path`) named a file that could not be
    /// read, or one whose content is not valid UTF-8. Distinct from
    /// [`Io`](Self::Io), which is about the request *file itself* not being
    /// readable — this is about a file the request *references*, resolved
    /// relative to the request file's own directory. See
    /// [`Request::resolve_body`].
    #[error("could not read request body file `{path}`")]
    BodyFileIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A config file was found but could not be read. Separate from [`Io`](Self::Io)
    /// so a front-end can say "your config is broken" rather than "your request
    /// file is broken" — the user did not name this path on the command line
    /// and needs to be told which file to go and fix.
    #[error("could not read config file `{path}`")]
    ConfigIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A config file was read but is not valid: bad YAML, an unknown key, or a
    /// value of the wrong type. Never silently ignored — a config that does not
    /// parse is a config whose settings are not being applied.
    #[error("could not parse config file `{path}`")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },

    /// The working directory could not be read, so the walk-up looking for a
    /// project config has nowhere to start.
    #[error("could not determine the current directory")]
    CurrentDir(#[source] std::io::Error),

    /// An environment file was found but could not be read. Its own variant for
    /// the same reason [`ConfigIo`](Self::ConfigIo) is: the user did not name
    /// this path on the command line, so the error has to say which file to go
    /// and fix.
    #[error("could not read environment file `{path}`")]
    EnvIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// An environment file was read but is not a flat map of string to string:
    /// bad YAML, a nested mapping, or a value that is not a string. Never
    /// ignored — an environment that does not parse is a set of variables that
    /// are not being substituted.
    #[error("could not parse environment file `{path}`")]
    EnvParse {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },

    /// A request referenced `{{name}}` and the active environment has no such
    /// variable.
    ///
    /// Carries the names that *are* defined, and the file they came from, the
    /// way [`RequestNotFound`](Self::RequestNotFound) carries the request names
    /// a collection does have. Raised while the request is being built, so it
    /// happens before anything goes over the wire.
    #[error(
        "no variable named `{name}` in {}{}",
        describe_variables(.environment, .available),
        describe_captured(.captured)
    )]
    VariableNotFound {
        name: String,
        available: Vec<String>,
        /// The environment file the variable was looked for in, or `None` when
        /// no environment file was found at all.
        environment: Option<PathBuf>,
        /// The names captured by earlier requests in this run, which are looked
        /// up alongside the file's own and so belong in the same message.
        ///
        /// Listed separately from `available` rather than merged into it
        /// because they did not come from the file the message names, and a
        /// list that claimed they did would send the reader to edit a file that
        /// has never mentioned them. Empty for a single request, and for every
        /// run of a collection that captures nothing — in which case the
        /// message is exactly the one it has always been.
        captured: Vec<String>,
    },

    /// An environment file value is `${VAR}` and `VAR` is not in the OS
    /// environment.
    ///
    /// Deliberately an error rather than an empty string: silently sending
    /// `Authorization: Bearer ` would turn a missing secret into a puzzling 401
    /// instead of a message naming the variable to export.
    #[error(
        "environment variable `{name}` is not set (referenced by `{variable}` in {})",
        describe_environment(.environment)
    )]
    EnvVarNotSet {
        /// The OS environment variable that is not set.
        name: String,
        /// The environment-file variable whose value referenced it.
        variable: String,
        environment: Option<PathBuf>,
    },

    /// A `pre_request` or `post_request` script does not parse.
    ///
    /// Its own variant, separate from [`ScriptFailed`](Self::ScriptFailed),
    /// because they are different problems for a user to fix — the same reason
    /// config and environment each split IO from Parse. A script that does not
    /// compile is a broken *file*: nothing about the request or the response
    /// could have changed the outcome, and the fix is a syntax error at a
    /// position Rhai names. Both hooks are compiled before the request is sent,
    /// so this is always raised with nothing having gone over the wire.
    #[error("could not compile the `{hook}` script")]
    ScriptParse {
        hook: script::Hook,
        #[source]
        source: rhai::ParseError,
    },

    /// A script compiled, ran, and threw — or hit a runtime error.
    ///
    /// Only ever produced for `pre_request`. A `post_request` script that fails
    /// is a statement about a response that did arrive, so it comes back as
    /// [`ScriptOutcome::Failed`](script::ScriptOutcome::Failed) rather than as
    /// an error; see the note on that type.
    #[error("the `{hook}` script failed: {message}")]
    ScriptFailed { hook: script::Hook, message: String },

    /// A `pre_request` script ran without throwing but left `request` in a
    /// state that is not a request: an unknown field, a value of the wrong
    /// type, or an assignment to the read-only `method`.
    ///
    /// Separate from [`ScriptFailed`](Self::ScriptFailed) because the script
    /// did not fail — it succeeded at doing something Sendra cannot act on, and
    /// the fix is a line of the script rather than whatever it was checking.
    #[error("the `pre_request` script left the request in a state it cannot be sent in: {reason}")]
    ScriptRequest { reason: String },
}

/// HTTP methods Sendra can send. Deliberately a closed set for now — an
/// arbitrary-method escape hatch can be added when something needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
            Method::Head => "HEAD",
            Method::Options => "OPTIONS",
        }
    }
}

impl From<Method> for reqwest::Method {
    fn from(m: Method) -> Self {
        match m {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Put => reqwest::Method::PUT,
            Method::Patch => reqwest::Method::PATCH,
            Method::Delete => reqwest::Method::DELETE,
            Method::Head => reqwest::Method::HEAD,
            Method::Options => reqwest::Method::OPTIONS,
        }
    }
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single request, as described by one YAML file.
///
/// The on-disk shape is the contract other Sendra features build on:
///
/// ```text
/// name: Get user
/// method: GET
/// url: https://api.example.com/users/1
/// headers:
///   Accept: application/json
/// body: null
/// assertions:
///   status: 200
/// ```
///
/// Everything but `method` and `url` is optional.
///
/// **Headers are a `Vec` of pairs, not a map** — matching [`Response::headers`]
/// and for the same reason: HTTP allows a header name to repeat (multiple
/// `Set-Cookie`-shaped headers, repeated `X-Forwarded-For` values), and a map
/// cannot represent that. Order is preserved exactly as written in the file.
///
/// A standard YAML mapping still cannot have two keys with the same name, so
/// writing a repeated header names it once with a *list* of values instead of
/// a scalar:
///
/// ```text
/// headers:
///   Accept: application/json    # scalar: one header
///   X-Forwarded-For:            # list: one header per entry, in order
///     - 1.2.3.4
///     - 5.6.7.8
/// ```
///
/// Two entries with the same name *and* the same value are accepted rather
/// than rejected: Sendra's stance elsewhere is to reject ambiguity, not
/// redundancy, and a client is allowed to send the same header twice even
/// when doing so is pointless.
///
/// `Eq` is deliberately absent where `PartialEq` is derived: an expected JSON
/// value in an [`Assertions`] block can be a float, and JSON floats are not
/// `Eq`. Nothing keys a map on a request, so the bound was never load-bearing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub method: Method,
    pub url: String,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_headers",
        serialize_with = "serialize_headers"
    )]
    pub headers: Vec<(String, String)>,
    /// Query parameters, merged onto whatever `url` already has and
    /// percent-encoded properly — the alternative to hand-building a query
    /// string inside `url` itself, where a value containing a space, `&`,
    /// `=` or non-ASCII character has to be encoded by hand or the request
    /// silently means something different than intended.
    ///
    /// ```text
    /// url: https://api.example.com/search
    /// query:
    ///   q: coffee & tea      # -> q=coffee+%26+tea
    ///   tag:                 # list: one `tag=` per entry, in order
    ///     - hot
    ///     - iced
    /// ```
    ///
    /// [`Request::resolve_query`] merges this onto `url`'s own query string
    /// (if it has one) with **`query` winning**: a key present in both is
    /// sent only with the value(s) from here, not the URL's. `query` is the
    /// more structured, explicit source, so a key repeated between the two
    /// is far more likely to be a stale copy left in `url` than a
    /// deliberately duplicated value.
    ///
    /// Deserialized the same way [`headers`](Self::headers) is — a value may
    /// be a scalar or, for a repeated key (`?tag=hot&tag=iced`), a list of
    /// scalars — since a repeated query parameter is the same shape of
    /// problem a repeated header already had a good answer for. An unquoted
    /// number or boolean is coerced to its string form rather than rejected,
    /// matching header values.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_headers",
        serialize_with = "serialize_headers"
    )]
    pub query: Vec<(String, String)>,
    /// Raw body, sent verbatim.
    ///
    /// One of five ways to specify a body — `body`, [`json`](Self::json),
    /// [`body_file`](Self::body_file), [`form`](Self::form) or
    /// [`multipart`](Self::multipart) — and a request may set at most one of
    /// them; [`Request::validate`] rejects any other combination at parse
    /// time. By the time a `pre_request` script or [`send_prepared`] sees a
    /// request, whichever of the five was set has already been resolved down
    /// to this field by [`Request::resolve_body`] — see there for exactly
    /// what each one becomes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// A body given as YAML — a mapping, a list, a string, whatever value —
    /// serialized to JSON and sent as `application/json`.
    ///
    /// ```text
    /// json:
    ///   name: ada
    ///   roles: [admin, user]
    /// ```
    ///
    /// [`Request::resolve_body`] sets `Content-Type: application/json` only
    /// when the request has not already set that header itself — an explicit
    /// header always wins. See [`Request::body`] for how this relates to the
    /// other four ways of specifying a body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json: Option<serde_json::Value>,
    /// A body read from a file, verbatim, sent exactly as `body` would be.
    ///
    /// ```text
    /// body_file: ./payload.json
    /// ```
    ///
    /// The path is resolved relative to the *request file's own directory*,
    /// not the process's current working directory — see
    /// [`Request::resolve_body`] for why. Unlike [`json`](Self::json) and
    /// [`form`](Self::form), no `Content-Type` is set automatically: Sendra
    /// cannot know what an arbitrary file contains, so a request using this
    /// field is responsible for its own `headers:` if the server needs one.
    /// The file's content must be valid UTF-8 — see the module docs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_file: Option<String>,
    /// A body given as name/value pairs, URL-encoded and sent as
    /// `application/x-www-form-urlencoded` — the same encoding an HTML form
    /// submission uses.
    ///
    /// ```text
    /// form:
    ///   username: ada
    ///   remember_me: "true"
    /// ```
    ///
    /// A plain YAML mapping cannot repeat a key, so unlike
    /// [`headers`](Self::headers) this has no list form for a repeated field
    /// name — nothing in Sendra has needed one yet. See
    /// [`Request::resolve_body`] for the `Content-Type` rule, which matches
    /// [`json`](Self::json)'s.
    ///
    /// Deserialized the same way [`headers`](Self::headers) is — a repeated
    /// field name is written as a list rather than rejected as a duplicate
    /// key — since a form field repeating (an HTML multi-select, say) is the
    /// same shape of problem a repeated header already had a good answer for.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_headers",
        serialize_with = "serialize_headers"
    )]
    pub form: Vec<(String, String)>,
    /// A body given as named parts, each either inline text or a file, sent
    /// as `multipart/form-data`.
    ///
    /// ```text
    /// multipart:
    ///   - name: description
    ///     value: a photo of my cat
    ///   - name: photo
    ///     path: ./cat.jpg
    /// ```
    ///
    /// Each part is exactly one of a text part (`value`) or a file part
    /// (`path`, resolved the same way [`body_file`](Self::body_file) is) —
    /// [`Request::validate`] rejects a part with both or neither. See the
    /// module docs for why a file part's content must be valid UTF-8 in this
    /// version.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub multipart: Vec<MultipartPart>,
    /// How to authenticate this request: bearer token or basic credentials,
    /// resolved to a plain `Authorization` header before anything else sees
    /// it.
    ///
    /// ```text
    /// auth:
    ///   bearer: {{token}}
    ///
    /// # or
    ///
    /// auth:
    ///   basic:
    ///     user: {{username}}
    ///     pass: {{password}}
    /// ```
    ///
    /// Exactly one of `bearer`/`basic` may be set — [`Request::validate`]
    /// rejects any other combination, the same shape as the five body
    /// fields above. A request may not set `auth` *and* an explicit
    /// `Authorization` header under `headers:`: both trying to control the
    /// same header is far more likely a mistake than a deliberate layering
    /// (unlike, say, a config default header and a request header, where
    /// "the request wins" is a sensible answer), so `validate` rejects the
    /// combination rather than silently picking one.
    ///
    /// [`Request::resolve_auth`] turns this into the `Authorization` header
    /// and clears the field, following the same "scripts see the final
    /// resolved form" precedent as [`Request::resolve_body`]: a
    /// `pre_request` script reads or overrides
    /// `request.headers["Authorization"]` like any other header, with no
    /// separate `request.auth` API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<Auth>,
    /// Declarative checks on the response, evaluated by
    /// [`Assertions::evaluate`] once it arrives.
    ///
    /// `None` — no `assertions:` key at all — is not the same as an empty
    /// block, and both are kept distinct on the way back out to YAML. Neither
    /// changes how the request is sent: assertions are read after the response,
    /// never before it, and they do not decide the process exit code. See the
    /// module docs on [`assertions`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assertions: Option<Assertions>,

    /// A script run against this request just before it is sent, as inline
    /// Rhai source.
    ///
    /// Written as a YAML block scalar, which is what a multiline script needs
    /// and the reason the file format is YAML rather than JSON or TOML:
    ///
    /// ```text
    /// pre_request: |
    ///   request.headers["X-Request-Id"] = "abc-123";
    /// ```
    ///
    /// It runs *after* environment substitution and *after* the config is
    /// applied, as the final mutation step before the wire. **Its own source is
    /// never substituted** — a `{{var}}` inside a script is just those
    /// characters. See the [`script`] module for both decisions and for what
    /// the script can see.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_request: Option<String>,

    /// A script run against the response, before assertions are evaluated.
    ///
    /// ```text
    /// post_request: |
    ///   if response.status != 201 {
    ///     throw "expected 201, got " + response.status;
    ///   }
    /// ```
    ///
    /// `throw` is how it reports a failure. Like an assertion, that failure is
    /// visible in the output and decides `sendra test`'s verdict without
    /// changing `sendra run`'s exit code. Compiled before the request is sent,
    /// so a syntax error here stops the request rather than being discovered
    /// after it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_request: Option<String>,

    /// Values to pull out of the response and hand to the requests after this
    /// one, as variable name to JSON path:
    ///
    /// ```text
    /// capture:
    ///   auth_token: $.token
    ///   user_id: $.user.id
    /// ```
    ///
    /// Each name becomes usable as `{{name}}` in every request *after* this one
    /// in file order, within the same `sendra run` or `sendra test`
    /// invocation — nothing is written to disk and a fresh process starts with
    /// nothing captured.
    ///
    /// `None` — no `capture:` key at all — is kept distinct from an empty
    /// block on the way back out to YAML, the same way an `assertions` block
    /// is. Neither changes how this request is sent: a capture is read after
    /// the response, never before it. See the [`capture`] module for what a
    /// path may select and for what happens when one does not match.
    ///
    /// **The block is not substituted.** A `{{var}}` in a capture path or name
    /// stays those characters; see [`Environment::apply`](crate::Environment::apply).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<Captures>,
}

impl Request {
    /// Parse a request from a YAML string.
    pub fn from_yaml_str(yaml: &str) -> Result<Self, SendraError> {
        let request: Request = serde_yaml::from_str(yaml).map_err(SendraError::ParseStr)?;
        request.validate()?;
        Ok(request)
    }

    /// Read and parse a request from a YAML file on disk.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, SendraError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|source| SendraError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let request: Request = serde_yaml::from_str(&raw).map_err(|source| SendraError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        request.validate()?;
        Ok(request)
    }

    /// Rules the `Deserialize` impl cannot express: at most one of
    /// `body`/`json`/`body_file`/`form`/`multipart` may be set, and each
    /// `multipart` part needs exactly one of `value`/`path`.
    ///
    /// Checked here, at parse time, rather than left for
    /// [`resolve_body`](Self::resolve_body) to discover: a request with two
    /// body sources is a broken file the same way an unnamed request in a
    /// collection is, and both are worth catching before anything is sent
    /// rather than resolved by silently picking one and ignoring the rest.
    fn validate(&self) -> Result<(), SendraError> {
        let invalid = |reason: String| Err(SendraError::InvalidRequest { reason });

        let mut set = Vec::new();
        if self.body.is_some() {
            set.push("body");
        }
        if self.json.is_some() {
            set.push("json");
        }
        if self.body_file.is_some() {
            set.push("body_file");
        }
        if !self.form.is_empty() {
            set.push("form");
        }
        if !self.multipart.is_empty() {
            set.push("multipart");
        }
        if set.len() > 1 {
            return invalid(format!(
                "at most one of `body`, `json`, `body_file`, `form`, `multipart` may be set, but found: {}",
                set.join(", ")
            ));
        }

        for part in &self.multipart {
            match (&part.value, &part.path) {
                (Some(_), Some(_)) => {
                    return invalid(format!(
                        "multipart part `{}` has both `value` and `path`; exactly one is required",
                        part.name
                    ));
                }
                (None, None) => {
                    return invalid(format!(
                        "multipart part `{}` has neither `value` nor `path`; exactly one is required",
                        part.name
                    ));
                }
                _ => {}
            }
        }

        if let Some(auth) = &self.auth {
            let mut set = Vec::new();
            if auth.bearer.is_some() {
                set.push("bearer");
            }
            if auth.basic.is_some() {
                set.push("basic");
            }
            if set.len() != 1 {
                return invalid(format!(
                    "exactly one of `auth.bearer` or `auth.basic` must be set, but found: {}",
                    if set.is_empty() {
                        "neither".to_string()
                    } else {
                        set.join(", ")
                    }
                ));
            }

            if self
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("Authorization"))
            {
                return invalid(
                    "`auth` and an explicit `Authorization` header cannot both be set on the \
                     same request; remove one"
                        .to_string(),
                );
            }
        }

        Ok(())
    }

    /// Merge `query` onto `url`'s own query string, percent-encoded
    /// properly, returning a request whose `url` is the final string that
    /// goes on the wire and whose `query` is empty.
    ///
    /// Called right after environment substitution and before
    /// [`resolve_body`](Self::resolve_body), the config, or a `pre_request`
    /// script ever see the request — the same "structured input becomes the
    /// final wire form before anything else touches it" shape as
    /// `resolve_body`. A `pre_request` script therefore sees `query`
    /// parameters already merged into `request.url`, not a separate map, for
    /// consistency with `resolve_body`'s "scripts see the final resolved
    /// form" precedent.
    ///
    /// A request with an empty `query` is returned with `url` untouched —
    /// not even reparsed — so a `url`-only request behaves exactly as it
    /// always has, including one whose `url` would not itself parse as a
    /// valid [`reqwest::Url`] (which today is only ever caught by `reqwest`
    /// itself, at send time).
    ///
    /// Uses [`reqwest::Url`]'s own query-pair APIs — already a dependency —
    /// rather than string concatenation, so a value containing a space, `&`,
    /// `=` or non-ASCII character is encoded correctly rather than however it
    /// happened to be typed.
    pub fn resolve_query(&self) -> Result<Request, SendraError> {
        let mut resolved = self.clone();
        if self.query.is_empty() {
            return Ok(resolved);
        }

        let mut url =
            reqwest::Url::parse(&self.url).map_err(|source| SendraError::InvalidRequest {
                reason: format!("url `{}` is not valid: {source}", self.url),
            })?;

        // `query` wins: drop any existing pair under a name `query` also
        // sets, then write the URL's surviving pairs back first so a key
        // `query` says nothing about keeps its place ahead of the new ones.
        let overridden: std::collections::HashSet<&str> =
            self.query.iter().map(|(name, _)| name.as_str()).collect();
        let kept: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(name, _)| !overridden.contains(name.as_ref()))
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect();

        let mut pairs = url.query_pairs_mut();
        pairs.clear();
        for (name, value) in &kept {
            pairs.append_pair(name, value);
        }
        for (name, value) in &self.query {
            pairs.append_pair(name, value);
        }
        drop(pairs);

        resolved.url = url.to_string();
        resolved.query = Vec::new();
        Ok(resolved)
    }

    /// Resolve whichever of `body`/`json`/`body_file`/`form`/`multipart` was
    /// set into the final `body` string that goes on the wire, setting
    /// `Content-Type` when the field implies one and the request has not
    /// already set that header itself.
    ///
    /// Called once, after environment substitution and before the config is
    /// applied or a `pre_request` script runs — so both see a plain `body`
    /// string regardless of which field produced it, the same way they
    /// already see a request whose `{{var}}`s have been resolved. `json`,
    /// `body_file`, `form` and `multipart` are cleared on the way out; `body`
    /// is the only body field left on the result.
    ///
    /// `base_dir` is where `body_file` and a multipart part's `path` resolve
    /// relative to: **the directory containing the request's own YAML file**,
    /// not the process's current working directory. A request file is
    /// something a user can run from anywhere — `sendra run
    /// requests/create-user.yaml` from a repository root — and `body_file:
    /// ./payload.json` written inside `create-user.yaml` obviously means the
    /// file beside it, not one resolved against whatever directory the
    /// command happened to be typed from.
    ///
    /// `json`, `form` and `body_file`'s *path* were already substituted by
    /// [`Environment::apply`](crate::Environment::apply) before this runs.
    /// `body_file`'s *file content* is deliberately not substituted — it is
    /// external content Sendra reads, not a value written in the request
    /// file, and substitution has never reached outside the document; see the
    /// [`environment`] module docs.
    ///
    /// File content — for `body_file` and a multipart file part alike — is
    /// read as UTF-8 text; a file that is not valid UTF-8 is
    /// [`SendraError::BodyFileIo`]. Sendra's bodies are text throughout, the
    /// same way a [`Response`]'s is, and true binary uploads are out of scope
    /// for this version.
    pub fn resolve_body(&self, base_dir: &Path) -> Result<Request, SendraError> {
        let mut resolved = self.clone();

        if let Some(value) = &self.json {
            let body = serde_json::to_string(value).expect("a serde_json::Value always serializes");
            resolved.body = Some(body);
            config::insert_if_absent(&mut resolved.headers, "Content-Type", "application/json");
        } else if let Some(path) = &self.body_file {
            resolved.body = Some(read_body_file(base_dir, path)?);
        } else if !self.form.is_empty() {
            let body = serde_urlencoded::to_string(&self.form)
                .expect("a Vec<(String, String)> always encodes as x-www-form-urlencoded pairs");
            resolved.body = Some(body);
            config::insert_if_absent(
                &mut resolved.headers,
                "Content-Type",
                "application/x-www-form-urlencoded",
            );
        } else if !self.multipart.is_empty() {
            let (body, content_type) = encode_multipart(&self.multipart, base_dir)?;
            resolved.body = Some(body);
            config::insert_if_absent(&mut resolved.headers, "Content-Type", &content_type);
        }

        resolved.json = None;
        resolved.body_file = None;
        resolved.form = Vec::new();
        resolved.multipart = Vec::new();

        Ok(resolved)
    }

    /// Resolve `auth` into the `Authorization` header that goes on the wire,
    /// clearing `auth` on the way out.
    ///
    /// Called after [`resolve_body`](Self::resolve_body) and before the
    /// config is applied or a `pre_request` script runs — the same
    /// "structured input becomes the final wire form before anything else
    /// touches it" shape as `resolve_query` and `resolve_body`. A
    /// `pre_request` script therefore sees a plain `Authorization` header
    /// like any other, with no separate `request.auth` API.
    ///
    /// [`Request::validate`] has already rejected a request that sets both
    /// `auth` and an explicit `Authorization` header, so this always adds
    /// the header rather than needing [`config::insert_if_absent`]'s
    /// suppression rule.
    pub fn resolve_auth(&self) -> Result<Request, SendraError> {
        let mut resolved = self.clone();

        if let Some(auth) = &self.auth {
            let value = match (&auth.bearer, &auth.basic) {
                (Some(token), None) => format!("Bearer {token}"),
                (None, Some(basic)) => {
                    let credentials = format!("{}:{}", basic.user, basic.pass);
                    let encoded = base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        credentials,
                    );
                    format!("Basic {encoded}")
                }
                // `validate` already rejected any other combination.
                _ => unreachable!("Request::validate enforces exactly one of bearer/basic"),
            };
            resolved.headers.push(("Authorization".to_string(), value));
        }
        resolved.auth = None;

        Ok(resolved)
    }

    /// Display label: the `name` field if present, else `METHOD url`.
    pub fn label(&self) -> String {
        match &self.name {
            Some(name) => name.clone(),
            None => format!("{} {}", self.method, self.url),
        }
    }

    /// The first header with exactly this name, if any.
    ///
    /// A convenience for callers that know (or only care about) at most one
    /// occurrence; a header that may legitimately repeat should read
    /// `.headers` directly rather than lose every occurrence but the first.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(existing, _)| existing == name)
            .map(|(_, value)| value.as_str())
    }
}

/// One part of a [`Request::multipart`] body: either inline text (`value`) or
/// a file (`path`), never both and never neither — enforced by
/// [`Request::validate`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultipartPart {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// [`Request::auth`]: exactly one of `bearer` or `basic`, enforced by
/// [`Request::validate`].
///
/// This is also the shape a default `auth:` at the environment/config level
/// is expected to reuse unchanged, and the shape a future `api_key` variant
/// is expected to join as a third mutually-exclusive case — so kept as its
/// own type rather than inlined onto `Request`, the same way `MultipartPart`
/// is its own type rather than an inline tuple.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    /// Sets `Authorization: Bearer <bearer>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer: Option<String>,
    /// Sets `Authorization: Basic <base64(user:pass)>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basic: Option<BasicAuth>,
}

/// [`Auth::basic`]'s credentials, base64-encoded as `user:pass` by
/// [`Request::resolve_auth`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BasicAuth {
    pub user: String,
    pub pass: String,
}

/// Read a `body_file` (or a multipart file part) relative to `base_dir`, as
/// UTF-8 text.
///
/// A non-UTF-8 file surfaces as [`SendraError::BodyFileIo`] wrapping an
/// `InvalidData` error, matching what `std::fs::read_to_string` itself
/// returns for the same failure, rather than a silent lossy conversion —
/// unlike a *response* body, which Sendra has never promised to send
/// unmodified.
fn read_body_file(base_dir: &Path, path: &str) -> Result<String, SendraError> {
    let full_path = base_dir.join(path);
    std::fs::read_to_string(&full_path).map_err(|source| SendraError::BodyFileIo {
        path: full_path,
        source,
    })
}

/// Encode a `multipart` body by hand, as `multipart/form-data` text, and
/// return it along with the `Content-Type` (boundary included) it implies.
///
/// Not built with `reqwest::multipart::Form`: that type holds arbitrary
/// bytes and cannot be cloned, `PartialEq`d or serialized, none of which
/// [`Request`] can give up — it is `Clone`, `PartialEq`, `Serialize` and
/// `Deserialize` throughout, including in the config/substitution/scripting
/// pipeline a multipart request passes through like any other. Writing the
/// format directly keeps the whole body a `String`, consistent with
/// [`resolve_body`](Request::resolve_body)'s UTF-8-text rule for
/// `body_file`.
///
/// The boundary is derived from the current time, which is unique enough
/// per-request for a boundary's actual job: a delimiter unlikely to occur
/// inside any part's own content, not a cryptographic guarantee.
fn encode_multipart(
    parts: &[MultipartPart],
    base_dir: &Path,
) -> Result<(String, String), SendraError> {
    let boundary = format!(
        "----sendra-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );

    let mut body = String::new();
    for part in parts {
        body.push_str("--");
        body.push_str(&boundary);
        body.push_str("\r\n");
        match (&part.value, &part.path) {
            (Some(value), None) => {
                body.push_str(&format!(
                    "Content-Disposition: form-data; name=\"{}\"\r\n\r\n",
                    part.name
                ));
                body.push_str(value);
            }
            (None, Some(path)) => {
                let filename = Path::new(path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(path);
                body.push_str(&format!(
                    "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n\r\n",
                    part.name, filename
                ));
                body.push_str(&read_body_file(base_dir, path)?);
            }
            // Ruled out by `Request::validate` before `resolve_body` is ever
            // reached; kept exhaustive rather than `unreachable!()` so a
            // future caller of `encode_multipart` that skips validation gets
            // an empty part instead of a panic.
            (Some(_), Some(_)) | (None, None) => {}
        }
        body.push_str("\r\n");
    }
    body.push_str("--");
    body.push_str(&boundary);
    body.push_str("--\r\n");

    let content_type = format!("multipart/form-data; boundary={boundary}");
    Ok((body, content_type))
}

/// Deserialize a `headers:` mapping into ordered `(name, value)` pairs.
///
/// A standard YAML mapping cannot have two keys with the same name, so a
/// value may be either a scalar (one header) or a sequence of scalars (one
/// header per entry, expanded in list order) — see the shape documented on
/// [`Request::headers`]. Order among distinct names is preserved exactly as
/// the underlying `MapAccess` yields it, which for `serde_yaml` is document
/// order.
fn deserialize_headers<'de, D>(deserializer: D) -> Result<Vec<(String, String)>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{MapAccess, SeqAccess, Visitor};

    enum HeaderValue {
        Single(String),
        Multiple(Vec<String>),
    }

    // Hand-written rather than `#[serde(untagged)]`, which reports every
    // mistake as "data did not match any variant of untagged enum
    // HeaderValue". This way a number where a value belongs is serde's own
    // "invalid type: integer `5`, expected a string or a list of strings",
    // naming what was found and what was wanted.
    impl<'de> Deserialize<'de> for HeaderValue {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            struct HeaderValueVisitor;

            impl<'de> Visitor<'de> for HeaderValueVisitor {
                type Value = HeaderValue;

                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str("a string or a list of strings")
                }

                fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                    Ok(HeaderValue::Single(value.to_string()))
                }

                // An unquoted `X-Api-Version: 2` read as the header value "2"
                // back when this field was a `BTreeMap<String, String>`, since
                // that is what serde_yaml does for a plain scalar asked for as
                // a string. Kept, so the type change does not quietly start
                // rejecting files that have always worked.
                fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
                    Ok(HeaderValue::Single(value.to_string()))
                }

                fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                    Ok(HeaderValue::Single(value.to_string()))
                }

                fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                    Ok(HeaderValue::Single(value.to_string()))
                }

                fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
                    Ok(HeaderValue::Single(value.to_string()))
                }

                fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
                where
                    A: SeqAccess<'de>,
                {
                    let mut values = Vec::new();
                    while let Some(value) = seq.next_element::<String>()? {
                        values.push(value);
                    }
                    Ok(HeaderValue::Multiple(values))
                }
            }

            deserializer.deserialize_any(HeaderValueVisitor)
        }
    }

    struct HeadersVisitor;

    impl<'de> Visitor<'de> for HeadersVisitor {
        type Value = Vec<(String, String)>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a map of header name to a string or list of strings")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut headers = Vec::new();
            while let Some((name, value)) = map.next_entry::<String, HeaderValue>()? {
                match value {
                    HeaderValue::Single(value) => headers.push((name, value)),
                    HeaderValue::Multiple(values) => {
                        headers.extend(values.into_iter().map(|value| (name.clone(), value)));
                    }
                }
            }
            Ok(headers)
        }
    }

    deserializer.deserialize_map(HeadersVisitor)
}

/// Serialize ordered `(name, value)` pairs back into a `headers:` mapping.
///
/// The inverse of [`deserialize_headers`]: a name that occurs once is written
/// as a scalar, one that occurs more than once is grouped under that name as
/// a list, in the order its values first and subsequently appear. Grouping
/// means two occurrences of the same name that were *not* adjacent in the
/// original `Vec` come back out adjacent — the only place this round trip is
/// lossy, and unobservable in practice since nothing else in Sendra cares
/// where among same-named headers a value sits.
fn serialize_headers<S>(headers: &[(String, String)], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeMap;

    let mut order: Vec<&str> = Vec::new();
    let mut grouped: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    for (name, value) in headers {
        let values = grouped.entry(name.as_str()).or_default();
        if values.is_empty() {
            order.push(name.as_str());
        }
        values.push(value.as_str());
    }

    let mut map = serializer.serialize_map(Some(order.len()))?;
    for name in order {
        let values = &grouped[name];
        if values.len() == 1 {
            map.serialize_entry(name, values[0])?;
        } else {
            map.serialize_entry(name, values)?;
        }
    }
    map.end()
}

/// A named group of requests living in one YAML file.
///
/// ```text
/// name: Example API        # optional, a label for the collection as a whole
/// requests:
///   - name: List users     # required inside a collection: it is the selector
///     method: GET
///     url: https://api.example.com/users
///   - name: Create user
///     method: POST
///     url: https://api.example.com/users
///     body: '{"name": "ada"}'
/// ```
///
/// `requests` is a *list*, not a map of name-to-request, for two reasons.
/// First, each entry is then exactly a single-request file: a request can be
/// lifted into a collection (or pulled back out into its own file) verbatim,
/// with its `name` staying a field instead of becoming a key. There is one
/// request shape in Sendra, not two. Second, a list preserves file order,
/// which is the order `sendra run <file>` sends them in; the map types serde
/// reaches for either sort the entries (`BTreeMap`) or need a dependency
/// (`IndexMap`) to avoid it. Lookup by name is then a linear scan, which costs
/// nothing at the sizes a hand-written collection reaches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Collection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub requests: Vec<Request>,
}

impl Collection {
    /// Look a request up by its `name`.
    ///
    /// Errors with [`SendraError::RequestNotFound`], which carries the names
    /// that do exist, rather than returning a bare `Option` — a missing name
    /// is a user-facing mistake worth a good message everywhere it happens.
    pub fn get(&self, name: &str) -> Result<&Request, SendraError> {
        self.requests
            .iter()
            .find(|request| request.name.as_deref() == Some(name))
            .ok_or_else(|| SendraError::RequestNotFound {
                name: name.to_string(),
                available: self.names(),
            })
    }

    /// The name of every request, in file order.
    pub fn names(&self) -> Vec<String> {
        self.requests
            .iter()
            .filter_map(|request| request.name.clone())
            .collect()
    }

    /// Rules the `Deserialize` impl cannot express: at least one request,
    /// every request named, no name used twice.
    ///
    /// `name` stays `Option` on [`Request`] because a standalone request file
    /// genuinely does not need one, so the requirement is enforced here, at
    /// parse time — a collection that cannot be addressed by name is a broken
    /// file, and finding that out before the first request goes over the wire
    /// beats finding out halfway through a run.
    fn validate(&self) -> Result<(), SendraError> {
        let invalid = |reason: String| Err(SendraError::InvalidCollection { reason });

        if self.requests.is_empty() {
            return invalid("`requests` is empty".to_string());
        }

        let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
        for (index, request) in self.requests.iter().enumerate() {
            let Some(name) = request.name.as_deref() else {
                return invalid(format!(
                    "request {} ({}) has no `name`; every request in a collection needs one to be selectable",
                    index + 1,
                    request.label()
                ));
            };
            if let Some(first) = seen.insert(name, index + 1) {
                return invalid(format!(
                    "two requests are named `{name}` (numbers {first} and {}); names must be unique",
                    index + 1
                ));
            }
            // Wrapped into `InvalidCollection`, with which request it was,
            // the same way the duplicate-name error above is — a standalone
            // request file raises `InvalidRequest` directly, but inside a
            // collection this is still a fact about *the file*, so it gets
            // the file-level error with request-level context added.
            if let Err(SendraError::InvalidRequest { reason }) = request.validate() {
                return invalid(format!("request {} ({name}): {reason}", index + 1));
            }
        }

        Ok(())
    }
}

/// What one Sendra YAML file can hold: a single request, or a collection.
///
/// The two shapes are told apart by **the presence of a top-level `requests`
/// key**. A mapping with `requests` is a [`Collection`]; anything else is
/// parsed as a single [`Request`]. The discriminator is in the file itself, so
/// no new extension and no CLI flag are needed, and it cannot be ambiguous:
/// [`Request`] rejects unknown top-level keys, so a single-request file could
/// never have carried a `requests` key to begin with.
///
/// Detection is a separate pass over the YAML rather than a
/// `#[serde(untagged)]` enum on purpose. An untagged enum collapses every
/// failure into "data did not match any variant" with no position; picking the
/// target first and then deserializing the original text keeps serde's real
/// error message, line and column included.
///
/// The `Single` variant is not boxed, though it is several times the size of
/// `Collection`. A `Document` is built once per invocation and read from where
/// it sits — the requests are borrowed out of it, never moved through it — so
/// the indirection would buy nothing and would cost every caller a deref to
/// reach a request that is right there.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Document {
    Single(Request),
    Collection(Collection),
}

impl Document {
    /// Parse a request or a collection from a YAML string.
    pub fn from_yaml_str(yaml: &str) -> Result<Self, SendraError> {
        Self::parse(yaml, SendraError::ParseStr)
    }

    /// Read and parse a request or a collection from a YAML file on disk.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, SendraError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|source| SendraError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&raw, |source| SendraError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Shared body of the two constructors; `wrap` supplies the error variant
    /// that says where the YAML came from.
    fn parse(
        yaml: &str,
        wrap: impl Fn(serde_yaml::Error) -> SendraError,
    ) -> Result<Self, SendraError> {
        // First pass: shape detection only. Cheap, and it means the second
        // pass parses the original text and so reports real positions.
        let probe: serde_yaml::Value = serde_yaml::from_str(yaml).map_err(&wrap)?;
        let is_collection = probe
            .as_mapping()
            .is_some_and(|mapping| mapping.contains_key("requests"));

        if is_collection {
            let collection: Collection = serde_yaml::from_str(yaml).map_err(&wrap)?;
            collection.validate()?;
            Ok(Document::Collection(collection))
        } else {
            let request: Request = serde_yaml::from_str(yaml).map_err(&wrap)?;
            request.validate()?;
            Ok(Document::Single(request))
        }
    }

    /// Every request the document holds, in file order — one for a single
    /// request, all of them for a collection. This is what `sendra run <file>`
    /// with no name sends.
    pub fn requests(&self) -> &[Request] {
        match self {
            Document::Single(request) => std::slice::from_ref(request),
            Document::Collection(collection) => &collection.requests,
        }
    }

    /// Look up one request by name.
    ///
    /// Asking a single-request file for a name is its own error rather than a
    /// "not found": the file has no names to choose between, and saying so is
    /// more useful than listing an empty set.
    pub fn get(&self, name: &str) -> Result<&Request, SendraError> {
        match self {
            Document::Single(_) => Err(SendraError::NotACollection {
                name: name.to_string(),
            }),
            Document::Collection(collection) => collection.get(name),
        }
    }
}

/// The result of sending a [`Request`].
///
/// Headers are a `Vec` of pairs rather than a map: HTTP allows repeats
/// (`set-cookie`) and wire order is worth preserving for display.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    /// The response body, decoded from the bytes on the wire **lossily**:
    /// any byte sequence that is not valid UTF-8 is replaced with U+FFFD
    /// (`\u{fffd}`, the replacement character) rather than erroring.
    ///
    /// This is a deliberate contract, not an accident of the type. A body can
    /// legitimately be a PNG or a protobuf, and a tool whose job is to show
    /// you what came back should show you *something* rather than refuse the
    /// whole response over its encoding — the status, the headers and the
    /// elapsed time are all still true and all still worth seeing. So an
    /// invalid body is never an error.
    ///
    /// The cost is that it is **not round-trippable**: `body.as_bytes()` is
    /// not what the server sent, and the original bytes cannot be recovered
    /// from here. Everything downstream that reads this — assertions,
    /// captures, scripts, `--json` output — is reading the replaced text, so
    /// a `body_contains` against a binary payload is comparing against U+FFFD
    /// and will not match. Binary-safe bodies (keeping the raw bytes
    /// alongside, and telling the user when a substitution happened) are a
    /// later concern; today the substitution is silent.
    pub body: String,
    pub elapsed: Duration,
    /// Every redirect hop that led to this response, oldest first: empty when
    /// the request was answered directly, when [`FollowRedirects::Disabled`]
    /// left a 3xx response as this one, or when only one hop's worth of
    /// following happened and it landed here without an intermediate stop.
    ///
    /// Each entry is the status of the response that redirected, and the
    /// `Location` it pointed at (resolved to an absolute URL) — the same two
    /// facts a `curl -v` trace would show for that hop. This response's own
    /// status and headers are not repeated here.
    pub redirects: Vec<RedirectHop>,
}

impl Response {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// One hop of a redirect chain, recorded on the way to a [`Response`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedirectHop {
    /// The status of the response that redirected — `301`, `302`, and so on.
    pub status: u16,
    /// Where it pointed: the `Location` header, resolved against the URL that
    /// received it, as an absolute URL.
    pub location: String,
}

/// Redirect hops recorded during the request currently in flight through a
/// given [`HttpClient`].
///
/// A `reqwest::redirect::Policy` closure has no way to hand its caller
/// anything back directly — it only decides follow/stop/error — so this is
/// the side channel: the policy pushes a hop here as it sees each one, and
/// [`send_prepared`] drains it right after that request finishes. The client
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

/// The HTTP client [`send`] and [`send_prepared`] send through.
///
/// A thin wrapper around `reqwest::Client` rather than a re-export of it, so
/// that the redirect chain a request's `reqwest::redirect::Policy` observes
/// has somewhere to be recorded and read back — see [`RedirectLog`]. A
/// front-end builds one with [`build_client`] and passes it around without
/// taking a direct dependency on reqwest.
pub struct HttpClient {
    inner: reqwest::Client,
    /// See [`RedirectLog`] — in particular, its note on why this only works
    /// as long as sends through this client stay sequential.
    redirects: RedirectLog,
    /// The whole-request timeout this client was built with, kept so that
    /// [`SendraError::Timeout`] can name the limit it hit.
    ///
    /// Here rather than passed back down through [`send_prepared`] because
    /// this is where the limit *is*: reqwest keeps its own copy inside
    /// `inner` and will not hand it back, and `send_prepared` deliberately
    /// takes no `&Config` (see its doc comment). The client enforces the
    /// timeout, so the client is what remembers it.
    timeout: Duration,
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

/// Send `request` under `config` and collect the full response.
///
/// The elapsed time covers connect, send and body read — i.e. what a user
/// waits for, not just time-to-first-byte.
///
/// `config` is a parameter rather than something resolved in here, and is not
/// optional, so that a caller cannot send a request without deciding what
/// configuration applies to it. Callers with nothing to apply pass
/// [`Config::default`], which is the same defaults resolution falls back to. It
/// contributes one thing here — default headers, merged by [`Config::apply`]
/// with the request winning ties. The other thing it decides, the timeout, was
/// applied when `client` was built; see [`build_client`].
///
/// `client` is borrowed rather than built here so that a run sending more than
/// one request sends them all down the same connection pool. See
/// [`build_client`] for what that is worth and where the client should come
/// from.
///
/// This is the whole pipeline in one call, for a caller that has no reason to
/// step between the two halves. A caller that does — one running a
/// `pre_request` script, which by definition is the *last* thing to touch the
/// request — applies the config itself and calls [`send_prepared`]. That is the
/// only reason the seam exists; see there.
pub async fn send(
    request: &Request,
    client: &HttpClient,
    config: &Config,
) -> Result<Response, SendraError> {
    // Everything below works from the merged request, so a config header is
    // validated and sent exactly like one written in the file.
    send_prepared(&config.apply(request), client).await
}

/// Send a request that is already exactly what should go over the wire.
///
/// Identical to [`send`] except that [`Config::apply`] is the caller's job and
/// has already happened. There is no `&Config` here at all: the only thing this
/// half ever read from it was the timeout, and that now lives in the `client`
/// it is handed.
///
/// It exists because of `pre_request`. The ordering the scripting feature is
/// built on puts the script strictly after the config and strictly before the
/// wire, and a script's most obvious use — *removing* a header the config
/// injected — only works if nothing re-merges the config afterwards. So the
/// seam has to be somewhere, and here it is named, and says in its own
/// signature that configuration is not its problem because it has already been
/// handled.
///
/// Prefer [`send`] unless there is something to do in between.
pub async fn send_prepared(
    request: &Request,
    client: &HttpClient,
) -> Result<Response, SendraError> {
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in &request.headers {
        let header_name = reqwest::header::HeaderName::try_from(name.as_str()).map_err(|e| {
            SendraError::InvalidHeader {
                name: name.clone(),
                reason: e.to_string(),
            }
        })?;
        let header_value = reqwest::header::HeaderValue::try_from(value.as_str()).map_err(|e| {
            SendraError::InvalidHeader {
                name: name.clone(),
                reason: e.to_string(),
            }
        })?;
        // `append`, not `insert`: `insert` replaces any existing value under
        // that name, which would silently drop every occurrence but the last
        // of a header this crate now allows to repeat.
        headers.append(header_name, header_value);
    }

    // Every failure below comes back as a `reqwest::Error`, and exactly one
    // kind of it is worth its own variant: the timeout, because it is the
    // only one Sendra itself caused. See `SendraError::Timeout`.
    let send_err = |source: reqwest::Error| {
        if source.is_timeout() {
            SendraError::Timeout {
                url: request.url.clone(),
                timeout: client.timeout,
                source,
            }
        } else {
            SendraError::Network {
                url: request.url.clone(),
                source,
            }
        }
    };

    let mut builder = client
        .inner
        .request(request.method.into(), &request.url)
        .headers(headers);
    if let Some(body) = &request.body {
        builder = builder.body(body.clone());
    }

    // Cleared here rather than trusted to already be empty — see
    // `RedirectLog`. This assumes `send_prepared` calls through one
    // `HttpClient` never overlap; a concurrent send through the same client
    // would race on this log and misattribute hops between requests. See
    // `RedirectLog`'s doc comment before changing that.
    client.redirects.lock().unwrap().clear();

    let started = Instant::now();
    let response = builder.send().await.map_err(send_err)?;
    let redirects = std::mem::take(&mut *client.redirects.lock().unwrap());

    let status = response.status();
    let header_pairs = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value
                    .to_str()
                    .unwrap_or("<non-utf8 header value>")
                    .to_owned(),
            )
        })
        .collect();
    let bytes = response.bytes().await.map_err(send_err)?;
    let elapsed = started.elapsed();

    Ok(Response {
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or("").to_owned(),
        headers: header_pairs,
        // Lossy by contract, and explicitly so: `.bytes()` then
        // `from_utf8_lossy`, rather than reqwest's `.text()`, which reaches
        // the same result by a route that reads like an accident. See the
        // note on `Response::body`.
        body: String::from_utf8_lossy(&bytes).into_owned(),
        elapsed,
        redirects,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_valid_request() {
        let yaml = "\
name: Get user
method: GET
url: https://api.example.com/users/1
headers:
  Accept: application/json
body: null
";
        let request = Request::from_yaml_str(yaml).expect("valid yaml should parse");

        let expected_headers = vec![("Accept".to_string(), "application/json".to_string())];

        assert_eq!(
            request,
            Request {
                name: Some("Get user".to_string()),
                method: Method::Get,
                url: "https://api.example.com/users/1".to_string(),
                headers: expected_headers,
                query: Vec::new(),
                body: None,
                json: None,
                body_file: None,
                form: Vec::new(),
                multipart: Vec::new(),
                auth: None,
                assertions: None,
                pre_request: None,
                post_request: None,
                capture: None,
            }
        );
    }

    #[test]
    fn a_header_value_that_is_a_list_expands_to_one_header_per_entry() {
        let request = Request::from_yaml_str(
            "\
method: GET
url: https://example.com
headers:
  Accept: application/json
  X-Forwarded-For:
    - 1.2.3.4
    - 5.6.7.8
",
        )
        .expect("a list-valued header is part of the file contract");

        assert_eq!(
            request.headers,
            vec![
                ("Accept".to_string(), "application/json".to_string()),
                ("X-Forwarded-For".to_string(), "1.2.3.4".to_string()),
                ("X-Forwarded-For".to_string(), "5.6.7.8".to_string()),
            ]
        );
    }

    #[test]
    fn two_identical_headers_are_kept_not_rejected() {
        // Redundant, but not ambiguous: Sendra rejects ambiguity elsewhere, not
        // a user's explicit (if pointless) choice to repeat a value.
        let request = Request::from_yaml_str(
            "\
method: GET
url: https://example.com
headers:
  X-Tag:
    - same
    - same
",
        )
        .expect("identical repeated headers are allowed, not an error");

        assert_eq!(
            request.headers,
            vec![
                ("X-Tag".to_string(), "same".to_string()),
                ("X-Tag".to_string(), "same".to_string()),
            ]
        );
    }

    #[test]
    fn an_unquoted_scalar_header_value_is_still_read_as_a_string() {
        // What the field did when it was a `BTreeMap<String, String>`: a plain
        // scalar is the header value spelled out. The type change must not
        // start rejecting `X-Api-Version: 2`.
        let request = Request::from_yaml_str(
            "\
method: GET
url: https://example.com
headers:
  X-Api-Version: 2
  X-Enabled: true
",
        )
        .expect("an unquoted scalar is a header value, as it always was");

        assert_eq!(request.header("X-Api-Version"), Some("2"));
        assert_eq!(request.header("X-Enabled"), Some("true"));
    }

    #[test]
    fn a_header_value_that_is_neither_a_scalar_nor_a_list_says_so() {
        let err = Request::from_yaml_str(
            "\
method: GET
url: https://example.com
headers:
  X:
    nested: map
",
        )
        .expect_err("a nested map is not a header value");

        let message = std::error::Error::source(&err)
            .expect("the serde error is the source")
            .to_string();
        assert!(
            message.contains("expected a string or a list of strings"),
            "the message should name the shape a header value may take: {message}"
        );
    }

    #[test]
    fn repeated_headers_round_trip_through_yaml() {
        let request = Request::from_yaml_str(
            "\
method: GET
url: https://example.com
headers:
  X-Forwarded-For:
    - 1.2.3.4
    - 5.6.7.8
",
        )
        .unwrap();

        let yaml = serde_yaml::to_string(&request).expect("a repeated header serialises");
        let round_tripped = Request::from_yaml_str(&yaml).expect("and reparses");
        assert_eq!(round_tripped.headers, request.headers, "got {yaml}");
    }

    #[test]
    fn parses_a_minimal_request() {
        let request = Request::from_yaml_str("method: POST\nurl: https://example.com\n")
            .expect("method + url is enough");
        assert_eq!(request.method, Method::Post);
        assert!(request.headers.is_empty());
        assert_eq!(request.body, None);
        assert_eq!(
            request.assertions, None,
            "a file written before assertions existed still parses to no assertions"
        );
        assert_eq!(request.label(), "POST https://example.com");
    }

    #[test]
    fn parses_a_request_with_an_assertions_block() {
        // The whole on-disk shape at once; what each entry *means* is tested in
        // the `assertions` module, this is the file contract.
        let request = Request::from_yaml_str(
            "\
method: GET
url: https://api.example.com/users/1
assertions:
  status: 200
  headers:
    content-type: application/json
    x-request-id:
  body_contains: ada
  json:
    $.user.id: 42
",
        )
        .expect("an assertions block is part of the request shape");

        let assertions = request.assertions.expect("the block parsed");
        assert_eq!(assertions.status, Some(200));
        assert_eq!(
            assertions.headers.get("content-type"),
            Some(&Some("application/json".to_string()))
        );
        // A key with no value is presence-only, not a missing entry.
        assert_eq!(assertions.headers.get("x-request-id"), Some(&None));
        assert_eq!(assertions.body_contains.as_deref(), Some("ada"));
        assert_eq!(assertions.json["$.user.id"], serde_json::json!(42));
    }

    #[test]
    fn an_empty_assertions_block_is_kept_distinct_from_no_block_at_all() {
        // `assertions: {}` asserts nothing, which is what an absent block does
        // too — but the file said something, and round-tripping it should not
        // silently rewrite it into a different file.
        let empty =
            Request::from_yaml_str("method: GET\nurl: https://example.com\nassertions: {}\n")
                .unwrap();
        assert_eq!(empty.assertions, Some(Assertions::default()));
        assert!(empty.assertions.as_ref().unwrap().is_empty());

        // A null block is the absent one: `assertions:` with nothing under it
        // is a key the author has not filled in yet.
        let null =
            Request::from_yaml_str("method: GET\nurl: https://example.com\nassertions:\n").unwrap();
        assert_eq!(null.assertions, None);
    }

    #[test]
    fn a_request_with_no_assertions_serialises_without_the_key() {
        // The round trip other Sendra features build on: nothing that did not
        // write an `assertions` block gets one back.
        let request = Request::from_yaml_str("method: GET\nurl: https://example.com\n").unwrap();
        let yaml = serde_yaml::to_string(&request).expect("a request serialises");
        assert!(!yaml.contains("assertions"), "got {yaml}");
    }

    #[test]
    fn parses_a_request_with_a_capture_block() {
        let request = Request::from_yaml_str(
            "method: POST
url: https://api.example.com/login
capture:
  auth_token: $.token
  user_id: $.user.id
",
        )
        .expect("a capture block is part of the request shape");

        let capture = request.capture.expect("the block parsed");
        assert_eq!(capture.variables(), vec!["auth_token", "user_id"]);
        assert_eq!(capture.entries()["auth_token"], "$.token");
        assert_eq!(capture.entries()["user_id"], "$.user.id");
    }

    #[test]
    fn an_empty_capture_block_is_kept_distinct_from_no_block_at_all() {
        // Same rule as `assertions`: the file said something, and a round trip
        // should not silently rewrite it into a different file.
        let empty = Request::from_yaml_str(
            "method: GET
url: https://example.com
capture: {}
",
        )
        .unwrap();
        assert!(empty.capture.as_ref().unwrap().is_empty());

        let null = Request::from_yaml_str(
            "method: GET
url: https://example.com
capture:
",
        )
        .unwrap();
        assert_eq!(null.capture, None);
    }

    #[test]
    fn a_request_with_no_capture_block_serialises_without_the_key() {
        let request = Request::from_yaml_str(
            "method: GET
url: https://example.com
",
        )
        .unwrap();
        let yaml = serde_yaml::to_string(&request).expect("a request serialises");
        assert!(!yaml.contains("capture"), "got {yaml}");
    }

    #[test]
    fn a_capture_path_is_not_validated_when_the_file_is_loaded() {
        // Deliberate, and the same call `assertions` makes: loading a request
        // file must never depend on the path grammar of the JSON path crate,
        // or a stricter release would start rejecting files that used to load.
        // A broken path is reported against the response instead.
        let request = Request::from_yaml_str(
            "method: GET
url: https://example.com
capture:
  v: nonsense
",
        )
        .expect("the file loads");
        assert_eq!(request.capture.unwrap().entries()["v"], "nonsense");
    }

    #[test]
    fn malformed_yaml_is_a_parse_error_not_a_panic() {
        // Unclosed flow sequence: not valid YAML at all.
        let err = Request::from_yaml_str("method: [GET\nurl: https://example.com\n")
            .expect_err("malformed yaml must not parse");
        assert!(matches!(err, SendraError::ParseStr(_)), "got {err:?}");
    }

    #[test]
    fn unknown_method_is_a_parse_error() {
        let err = Request::from_yaml_str("method: TELEPORT\nurl: https://example.com\n")
            .expect_err("unknown method must not parse");
        assert!(matches!(err, SendraError::ParseStr(_)), "got {err:?}");
    }

    #[test]
    fn missing_file_is_an_io_error_carrying_the_path() {
        let err = Request::from_path("does/not/exist.yaml").expect_err("missing file must error");
        match err {
            SendraError::Io { path, .. } => assert_eq!(path, Path::new("does/not/exist.yaml")),
            other => panic!("expected Io, got {other:?}"),
        }
    }

    /// Three requests, in a deliberately non-alphabetical order so the
    /// file-order assertions below mean something.
    const COLLECTION: &str = "\
name: Example API
requests:
  - name: Zeta
    method: GET
    url: https://api.example.com/zeta
    headers:
      Accept: application/json
  - name: Alpha
    method: POST
    url: https://api.example.com/alpha
    body: '{}'
  - name: Middle
    method: DELETE
    url: https://api.example.com/middle
";

    #[test]
    fn parses_a_collection_and_keeps_file_order() {
        let document = Document::from_yaml_str(COLLECTION).expect("valid collection should parse");

        let Document::Collection(collection) = &document else {
            panic!("a top-level `requests` key means a collection, got {document:?}");
        };
        assert_eq!(collection.name.as_deref(), Some("Example API"));
        // File order, not alphabetical: the run order is the author's order.
        assert_eq!(collection.names(), vec!["Zeta", "Alpha", "Middle"]);
        assert_eq!(collection.requests[1].method, Method::Post);
        assert_eq!(collection.requests[1].body.as_deref(), Some("{}"));
    }

    #[test]
    fn a_file_without_a_requests_key_is_still_a_single_request() {
        let document =
            Document::from_yaml_str("name: Get user\nmethod: GET\nurl: https://example.com\n")
                .expect("the existing single-request shape must keep parsing");

        match document {
            Document::Single(request) => assert_eq!(request.label(), "Get user"),
            other => panic!("expected Single, got {other:?}"),
        }
    }

    #[test]
    fn a_single_request_runs_as_a_one_element_document() {
        let document = Document::from_yaml_str("method: GET\nurl: https://example.com\n").unwrap();
        assert_eq!(document.requests().len(), 1);
        assert_eq!(document.requests()[0].url, "https://example.com");
    }

    #[test]
    fn collection_requests_are_returned_in_file_order() {
        let document = Document::from_yaml_str(COLLECTION).unwrap();
        let urls: Vec<&str> = document
            .requests()
            .iter()
            .map(|request| request.url.as_str())
            .collect();
        assert_eq!(
            urls,
            vec![
                "https://api.example.com/zeta",
                "https://api.example.com/alpha",
                "https://api.example.com/middle",
            ]
        );
    }

    #[test]
    fn looks_a_request_up_by_name() {
        let document = Document::from_yaml_str(COLLECTION).unwrap();
        let request = document.get("Alpha").expect("`Alpha` is in the collection");
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.url, "https://api.example.com/alpha");
    }

    #[test]
    fn an_unknown_name_is_a_typed_error_listing_what_is_available() {
        let document = Document::from_yaml_str(COLLECTION).unwrap();
        let err = document
            .get("Beta")
            .expect_err("`Beta` is not in the collection");

        match err {
            SendraError::RequestNotFound { name, available } => {
                assert_eq!(name, "Beta");
                assert_eq!(available, vec!["Zeta", "Alpha", "Middle"]);
            }
            other => panic!("expected RequestNotFound, got {other:?}"),
        }
        // The message is what a user actually sees, so pin it too.
        let message = document.get("Beta").unwrap_err().to_string();
        assert!(message.contains("Zeta, Alpha, Middle"), "got {message}");
    }

    #[test]
    fn asking_a_single_request_file_for_a_name_says_so() {
        let document = Document::from_yaml_str("method: GET\nurl: https://example.com\n").unwrap();
        let err = document.get("Alpha").expect_err("no names to select from");
        assert!(
            matches!(err, SendraError::NotACollection { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn a_request_in_a_collection_must_be_named() {
        let err =
            Document::from_yaml_str("requests:\n  - method: GET\n    url: https://example.com\n")
                .expect_err("an unnamed request cannot be selected, so it is rejected");
        assert!(
            matches!(err, SendraError::InvalidCollection { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn duplicate_names_in_a_collection_are_rejected() {
        let yaml = "\
requests:
  - name: Same
    method: GET
    url: https://example.com/a
  - name: Same
    method: GET
    url: https://example.com/b
";
        let err = Document::from_yaml_str(yaml).expect_err("duplicate names are ambiguous");
        match err {
            SendraError::InvalidCollection { reason } => {
                assert!(reason.contains("Same"), "got {reason}")
            }
            other => panic!("expected InvalidCollection, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_collection_is_rejected() {
        let err = Document::from_yaml_str("requests: []\n").expect_err("nothing to run");
        assert!(
            matches!(err, SendraError::InvalidCollection { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn unknown_keys_in_a_collection_are_rejected() {
        let yaml = "\
requests:
  - name: One
    method: GET
    url: https://example.com
enviroment: staging
";
        let err = Document::from_yaml_str(yaml).expect_err("a typo must not be silently ignored");
        assert!(matches!(err, SendraError::ParseStr(_)), "got {err:?}");
    }

    #[test]
    fn the_shipped_example_files_parse() {
        // The examples are documentation; a broken one is a broken doc.
        for name in [
            "get-request.yaml",
            "post-request.yaml",
            "collection.yaml",
            "mixed-status-collection.yaml",
            // Parses like any other request file: the `{{...}}` in it is a
            // string value, and substitution is a separate pass afterwards.
            "environment-request.yaml",
            "assertions.yaml",
            "test-collection.yaml",
            "scripted-request.yaml",
            "capture-chain.yaml",
            "repeated-headers.yaml",
            "structured-bodies.yaml",
            "query-params.yaml",
            "auth.yaml",
        ] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("examples")
                .join(name);
            Document::from_path(&path).unwrap_or_else(|e| panic!("{name} should parse: {e}"));
        }
    }

    #[test]
    fn missing_collection_file_is_an_io_error_carrying_the_path() {
        let err = Document::from_path("does/not/exist.yaml").expect_err("missing file must error");
        match err {
            SendraError::Io { path, .. } => assert_eq!(path, Path::new("does/not/exist.yaml")),
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_header_name_is_reported_before_any_network_call() {
        let request = Request {
            name: None,
            method: Method::Get,
            // Port 1 on localhost: if we ever got as far as connecting, this
            // would surface as a Network error instead, which the assert catches.
            url: "http://127.0.0.1:1/".to_string(),
            headers: vec![("bad header".to_string(), "x".to_string())],
            query: Vec::new(),
            body: None,
            json: None,
            body_file: None,
            form: Vec::new(),
            multipart: Vec::new(),
            auth: None,
            assertions: None,
            pre_request: None,
            post_request: None,
            capture: None,
        };
        let config = Config::default();
        let client = build_client(&config).expect("a client builds");
        let err = send(&request, &client, &config)
            .await
            .expect_err("invalid header must error");
        assert!(
            matches!(err, SendraError::InvalidHeader { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn an_invalid_header_from_the_config_is_reported_the_same_way() {
        // A config default is merged in before validation, so a bad header name
        // in `.sendra/config.yaml` fails as loudly as one in a request file
        // rather than being dropped on the way to the wire.
        let request = Request {
            name: None,
            method: Method::Get,
            url: "http://127.0.0.1:1/".to_string(),
            headers: Vec::new(),
            query: Vec::new(),
            body: None,
            json: None,
            body_file: None,
            form: Vec::new(),
            multipart: Vec::new(),
            auth: None,
            assertions: None,
            pre_request: None,
            post_request: None,
            capture: None,
        };
        let config = Config {
            headers: BTreeMap::from([("bad header".to_string(), "x".to_string())]),
            ..Config::default()
        };
        let client = build_client(&config).expect("a client builds");
        let err = send(&request, &client, &config)
            .await
            .expect_err("invalid header must error");
        assert!(
            matches!(err, SendraError::InvalidHeader { .. }),
            "got {err:?}"
        );
    }
    /// A server that counts the TCP connections it is asked to accept, so a
    /// test can tell "sent twice down one connection" from "connected twice".
    ///
    /// Deliberately hand-rolled over a blocking `TcpListener` on its own
    /// thread rather than pulled in as a mock-server dependency: what is being
    /// observed here is below HTTP — whether a *socket* was opened — and the
    /// whole protocol these tests need is "read a request, write a response,
    /// keep the connection open", which is shorter than the configuration of a
    /// library that does more.
    struct CountingServer {
        addr: std::net::SocketAddr,
        connections: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CountingServer {
        /// Start on an ephemeral loopback port and serve until the test ends.
        ///
        /// The thread is left running when the test finishes; it dies with the
        /// process, which is the whole lifetime a test binary has.
        fn start() -> Self {
            use std::io::{BufRead, BufReader, Write};
            use std::sync::atomic::{AtomicUsize, Ordering};
            use std::sync::Arc;

            let listener =
                std::net::TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
            let addr = listener.local_addr().expect("the listener has an address");
            let connections = Arc::new(AtomicUsize::new(0));
            let requests = Arc::new(AtomicUsize::new(0));

            let (server_connections, server_requests) = (connections.clone(), requests.clone());
            std::thread::spawn(move || {
                // One connection at a time, which is all a sendra run ever
                // opens: requests go out in file order, one after the other.
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    server_connections.fetch_add(1, Ordering::SeqCst);

                    let mut writer = stream.try_clone().expect("the socket clones");
                    let mut reader = BufReader::new(stream);

                    // Keep reading requests off this connection until the
                    // client hangs up: a client that is reusing the connection
                    // sends its next request here rather than reconnecting.
                    loop {
                        let mut line = String::new();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                        // Drain the headers; these requests carry no body.
                        loop {
                            let mut header = String::new();
                            match reader.read_line(&mut header) {
                                Ok(0) | Err(_) => return,
                                Ok(_) if header == "\r\n" => break,
                                Ok(_) => {}
                            }
                        }

                        // Counted before the response is written, so a client
                        // that has read its last response has necessarily been
                        // counted by the time the test looks.
                        server_requests.fetch_add(1, Ordering::SeqCst);
                        if writer
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                            .is_err()
                        {
                            break;
                        }
                        let _ = writer.flush();
                    }
                }
            });

            Self {
                addr,
                connections,
                requests,
            }
        }

        fn url(&self) -> String {
            format!("http://{}/", self.addr)
        }

        fn connections(&self) -> usize {
            self.connections.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn requests(&self) -> usize {
            self.requests.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// A GET with nothing on it but a URL — what a collection of requests
    /// against one host looks like once everything else is stripped away.
    fn get(url: &str) -> Request {
        Request {
            name: None,
            method: Method::Get,
            url: url.to_string(),
            headers: Vec::new(),
            query: Vec::new(),
            body: None,
            json: None,
            body_file: None,
            form: Vec::new(),
            multipart: Vec::new(),
            auth: None,
            assertions: None,
            pre_request: None,
            post_request: None,
            capture: None,
        }
    }

    #[tokio::test]
    async fn one_client_sends_every_request_down_one_connection() {
        // The point of `build_client` being per-run rather than per-request,
        // stated as an observation a server can make: three requests, one
        // handshake.
        let server = CountingServer::start();
        let config = Config::default();
        let client = build_client(&config).expect("a client builds");

        for _ in 0..3 {
            let response = send(&get(&server.url()), &client, &config)
                .await
                .expect("the mock server answers");
            assert_eq!(response.status, 200);
        }

        assert_eq!(server.requests(), 3, "all three requests were served");
        assert_eq!(
            server.connections(),
            1,
            "three requests through one client must reuse one connection"
        );
    }

    #[tokio::test]
    async fn a_client_per_request_opens_a_connection_per_request() {
        // The counterpart, and the reason the test above is worth anything: it
        // is what the code did before the client was hoisted out of
        // `send_prepared`, and it is what the counter looks like when a client
        // is *not* reused. Without this, a server that closed connections on
        // its own would make the assertion above pass for the wrong reason.
        let server = CountingServer::start();
        let config = Config::default();

        for _ in 0..3 {
            let client = build_client(&config).expect("a client builds");
            let response = send(&get(&server.url()), &client, &config)
                .await
                .expect("the mock server answers");
            assert_eq!(response.status, 200);
        }

        assert_eq!(server.requests(), 3, "all three requests were served");
        assert_eq!(
            server.connections(),
            3,
            "a fresh client per request cannot reuse anything"
        );
    }

    #[tokio::test]
    async fn a_gzip_encoded_response_is_decompressed_before_reaching_response_body() {
        // Many APIs compress their response regardless of what the client
        // negotiated; without the "gzip" feature enabled on the client, this
        // response body would be handed to `Response.body` as raw compressed
        // bytes rather than the JSON text they hold.
        use std::io::Write;

        let body = b"{\"hello\":\"world\"}";
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(body).expect("gzip encodes into memory");
        let compressed = encoder.finish().expect("gzip stream finalises");

        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
        let addr = listener.local_addr().expect("the listener has an address");
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};

            if let Ok(stream) = listener.accept().map(|(s, _)| s) {
                let mut writer = stream.try_clone().expect("the socket clones");
                let mut reader = BufReader::new(stream);

                let mut line = String::new();
                reader.read_line(&mut line).expect("a request line arrives");
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).expect("headers keep coming");
                    if header == "\r\n" {
                        break;
                    }
                }

                writer
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
                            compressed.len()
                        )
                        .as_bytes(),
                    )
                    .expect("status line and headers write");
                writer
                    .write_all(&compressed)
                    .expect("the compressed body writes");
                writer.flush().expect("the response flushes");
            }
        });

        let config = Config::default();
        let client = build_client(&config).expect("a client builds");
        let response = send(&get(&format!("http://{addr}/")), &client, &config)
            .await
            .expect("the mock server answers");

        assert_eq!(response.status, 200);
        assert_eq!(
            response.body, "{\"hello\":\"world\"}",
            "the body must be the decompressed text, not the raw gzip bytes"
        );
    }

    #[tokio::test]
    async fn a_repeated_header_actually_goes_out_twice_on_the_wire() {
        // Confirms the bytes a real server receives, not just that
        // `Request.headers` holds two entries: `send_prepared` has to use
        // `HeaderMap::append` rather than `insert`, or the second value would
        // silently replace the first before anything hits a socket.
        use std::io::{BufRead, BufReader, Write};

        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
        let addr = listener.local_addr().expect("the listener has an address");
        let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let seen_in_thread = seen.clone();
        std::thread::spawn(move || {
            if let Ok(stream) = listener.accept().map(|(s, _)| s) {
                let mut writer = stream.try_clone().expect("the socket clones");
                let mut reader = BufReader::new(stream);

                let mut line = String::new();
                reader.read_line(&mut line).expect("a request line arrives");
                loop {
                    let mut header = String::new();
                    match reader.read_line(&mut header) {
                        Ok(0) | Err(_) => return,
                        Ok(_) if header == "\r\n" => break,
                        Ok(_) => seen_in_thread
                            .lock()
                            .unwrap()
                            .push(header.trim_end().to_string()),
                    }
                }

                writer
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .expect("status line and headers write");
                writer.flush().expect("the response flushes");
            }
        });

        let request = Request {
            headers: vec![
                ("X-Forwarded-For".to_string(), "1.2.3.4".to_string()),
                ("X-Forwarded-For".to_string(), "5.6.7.8".to_string()),
            ],
            ..get(&format!("http://{addr}/"))
        };
        let config = Config::default();
        let client = build_client(&config).expect("a client builds");
        let response = send(&request, &client, &config)
            .await
            .expect("the mock server answers");
        assert_eq!(response.status, 200);

        let lines = seen.lock().unwrap().clone();
        let matching: Vec<&String> = lines
            .iter()
            .filter(|line| line.to_ascii_lowercase().starts_with("x-forwarded-for:"))
            .collect();
        assert_eq!(
            matching.len(),
            2,
            "both values should have gone out as two separate header lines, got {lines:?}"
        );
        assert!(matching.iter().any(|l| l.contains("1.2.3.4")));
        assert!(matching.iter().any(|l| l.contains("5.6.7.8")));
    }

    // --- timeouts ----------------------------------------------------------

    /// Where a slow server stops, relative to the response it owes.
    ///
    /// The configured timeout is a *whole-request* one — connect, send and
    /// body read — and `send_prepared` can therefore fail at either of two
    /// awaits. These are those two places, so both are exercised rather than
    /// assumed equivalent.
    enum Stall {
        /// Read the request and then say nothing at all: what an overloaded
        /// server that has not started work yet looks like.
        BeforeResponding,
        /// Send the status line and a `Content-Length` promising a body, then
        /// stop without sending it: headers arrive, `bytes()` never finishes.
        MidBody,
    }

    /// A server that reads a request and then goes quiet for `delay`.
    ///
    /// Hand-rolled over a blocking `TcpListener` on its own thread, like every
    /// other mock server in this file. A mock-server crate would be a new
    /// dependency for a behaviour that is one `thread::sleep` inside the
    /// pattern already here — and what these tests need is a server that does
    /// *not* obey HTTP's usual rhythm, which is the case a library built
    /// around stubbing well-formed exchanges is least suited to.
    ///
    /// The sleep is on a std thread, not a tokio task, so it blocks nothing
    /// the client under test is running on.
    fn start_stalling_server(stall: Stall, delay: Duration) -> std::net::SocketAddr {
        use std::io::{BufRead, BufReader, Write};

        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
        let addr = listener.local_addr().expect("the listener has an address");

        std::thread::spawn(move || {
            let Ok(stream) = listener.accept().map(|(s, _)| s) else {
                return;
            };
            let mut writer = stream.try_clone().expect("the socket clones");
            let mut reader = BufReader::new(stream);

            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                return;
            }
            loop {
                let mut header = String::new();
                match reader.read_line(&mut header) {
                    Ok(0) | Err(_) => return,
                    Ok(_) if header == "\r\n" => break,
                    Ok(_) => {}
                }
            }

            if matches!(stall, Stall::MidBody) {
                // A body is promised and never sent, so the client is left
                // waiting inside the body read rather than inside the send.
                let _ = writer.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 11\r\n\r\n",
                );
                let _ = writer.flush();
            }

            // Long enough that a test seeing a timeout has necessarily seen
            // the client's clock rather than the server's.
            std::thread::sleep(delay);
            let _ = writer.write_all(b"too late");
        });

        addr
    }

    /// Comfortably longer than any timeout these tests configure: the server
    /// is still holding the connection when the assertions run.
    const STALL: Duration = Duration::from_secs(30);

    #[tokio::test]
    async fn a_server_slower_than_the_timeout_fails_with_a_timeout_error() {
        // The timeout has only ever been checked as a resolved `Config` value.
        // This is it applied: a server that never answers, and a client that
        // stops waiting on its own.
        let addr = start_stalling_server(Stall::BeforeResponding, STALL);
        let config = Config {
            timeout: Duration::from_millis(300),
            ..Config::default()
        };
        let client = build_client(&config).expect("a client builds");
        let url = format!("http://{addr}/");

        let started = Instant::now();
        let err = send(&get(&url), &client, &config)
            .await
            .expect_err("a server that never answers must not hang the run");
        let waited = started.elapsed();

        match &err {
            SendraError::Timeout {
                url: got, timeout, ..
            } => {
                assert_eq!(got, &url);
                assert_eq!(
                    *timeout,
                    Duration::from_millis(300),
                    "the error must name the limit that was actually applied"
                );
            }
            other => panic!("expected a timeout, got {other:?}"),
        }

        // The message a user sees, rather than only the variant a front-end
        // matches on: "failed" alone would not tell them a setting caused it.
        assert_eq!(
            err.to_string(),
            format!("request to `{url}` timed out after 0.3s")
        );

        // The clock that fired was the client's, not the server's: the server
        // is still asleep, and has another twenty-nine-odd seconds to go.
        assert!(
            waited < STALL / 2,
            "gave up after {waited:?}, which is not the configured 300ms"
        );
    }

    #[tokio::test]
    async fn the_timeout_covers_the_body_read_not_just_the_response_headers() {
        // The config calls this a whole-request timeout, so a server that
        // sends its headers promptly and then stalls forever mid-body has to
        // be caught too — a different await in `send_prepared`, and one that
        // would quietly return `Network` if only the first were classified.
        let addr = start_stalling_server(Stall::MidBody, STALL);
        let config = Config {
            timeout: Duration::from_millis(300),
            ..Config::default()
        };
        let client = build_client(&config).expect("a client builds");

        let started = Instant::now();
        let err = send(&get(&format!("http://{addr}/")), &client, &config)
            .await
            .expect_err("a body that never arrives must time out like a response that never does");
        let waited = started.elapsed();

        assert!(
            matches!(err, SendraError::Timeout { .. }),
            "a stall after the headers is still a timeout, got {err:?}"
        );
        assert!(waited < STALL / 2, "gave up after {waited:?}");
    }

    #[tokio::test]
    async fn a_timeout_from_a_config_file_is_the_one_that_is_enforced() {
        // The half config-resolution tests cannot reach: that the number
        // written in `.sendra/config.yaml` is the number the socket obeys.
        // Resolved from a real file on disk, exactly as a run would, then put
        // against a server that never answers.
        let temp = tempfile::tempdir().expect("a temp dir");
        let project_dir = temp.path().join(".sendra");
        std::fs::create_dir_all(&project_dir).expect("the project dir is created");
        std::fs::write(project_dir.join("config.yaml"), "timeout_seconds: 1\n")
            .expect("the config file writes");

        let config = Config::resolve_from(temp.path(), None).expect("the config resolves");
        assert_eq!(config.timeout, Duration::from_secs(1), "the file was read");

        let addr = start_stalling_server(Stall::BeforeResponding, STALL);
        let client = build_client(&config).expect("a client builds");

        let started = Instant::now();
        let err = send(&get(&format!("http://{addr}/")), &client, &config)
            .await
            .expect_err("the configured second must run out");
        let waited = started.elapsed();

        match err {
            SendraError::Timeout { timeout, .. } => assert_eq!(timeout, Duration::from_secs(1)),
            other => panic!("expected a timeout, got {other:?}"),
        }
        assert!(
            waited >= Duration::from_millis(900),
            "gave up after {waited:?}, sooner than the second the file asked for"
        );
        assert!(waited < STALL / 2, "gave up after {waited:?}");
    }

    #[tokio::test]
    async fn a_connection_failure_is_still_a_network_error_not_a_timeout() {
        // The counterpart that makes the variant above worth having: if every
        // failed send came back as `Timeout`, the split would say nothing. A
        // port with nothing behind it refuses immediately, so this is a
        // connection failure and cannot be a slow one.
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
        let addr = listener.local_addr().expect("the listener has an address");
        drop(listener);

        let config = Config {
            timeout: Duration::from_secs(30),
            ..Config::default()
        };
        let client = build_client(&config).expect("a client builds");
        let err = send(&get(&format!("http://{addr}/")), &client, &config)
            .await
            .expect_err("nothing is listening on that port");

        assert!(
            matches!(err, SendraError::Network { .. }),
            "a refused connection is a fact about the network, not about the timeout, got {err:?}"
        );
    }

    // --- non-UTF-8 response bodies -----------------------------------------

    /// A raw `200` whose body is exactly `body`, byte for byte.
    ///
    /// Separate from [`ok_response`] because that one takes a `&str` and so
    /// cannot express a body that is not text — which is the entire subject
    /// of the two tests below.
    fn ok_bytes(content_type: &str, body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    #[tokio::test]
    async fn invalid_utf8_in_a_body_is_replaced_rather_than_erroring() {
        // `Response.body` is a `String`, so bytes that are not UTF-8 have to
        // go somewhere. They are replaced, and this pins exactly what with:
        // U+FFFD per invalid sequence, the surrounding text untouched, and no
        // error — see the contract on `Response::body`.
        //
        // 0xFF and 0xFE cannot begin a UTF-8 sequence at all, and 0xE2 0x28 is
        // a truncated three-byte sequence: the shape a body cut off at the
        // wrong boundary actually has.
        let body = b"ok \xff\xfe then \xe2\x28 end";
        let addr = start_route_server(vec![("/", ok_bytes("text/plain", body))]);

        let config = Config::default();
        let client = build_client(&config).expect("a client builds");
        let response = send(&get(&format!("http://{addr}/")), &client, &config)
            .await
            .expect("an undecodable body is not a failed request");

        assert_eq!(response.status, 200, "the response itself is fine");
        assert_eq!(
            response.body, "ok \u{fffd}\u{fffd} then \u{fffd}( end",
            "each invalid sequence becomes one replacement character, and the \
             valid text around it survives unchanged"
        );
    }

    #[tokio::test]
    async fn a_wholly_binary_body_comes_back_as_a_response_not_an_error() {
        // The everyday case: an endpoint that answers with an image. Status,
        // headers and elapsed time are all still true and worth showing, so
        // the response comes back rather than the request failing over its
        // body's encoding.
        //
        // A PNG signature, whose second byte (0x50, 'P') is deliberately
        // printable — proof the substitution is per invalid sequence and not a
        // blanket rewrite of the whole body.
        let body: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let addr = start_route_server(vec![("/", ok_bytes("image/png", body))]);

        let config = Config::default();
        let client = build_client(&config).expect("a client builds");
        let response = send(&get(&format!("http://{addr}/")), &client, &config)
            .await
            .expect("a binary body is not a failed request");

        assert_eq!(response.status, 200);
        assert_eq!(
            response
                .headers
                .iter()
                .find(|(name, _)| name == "content-type")
                .map(|(_, value)| value.as_str()),
            Some("image/png"),
            "everything but the body is unaffected"
        );
        assert_eq!(response.body, "\u{fffd}PNG\r\n\u{1a}\n");

        // Stated as a test rather than only as a doc comment, because it is
        // the part that bites: what comes back is not what was sent, and no
        // caller can recover the original bytes from here.
        assert_ne!(
            response.body.as_bytes(),
            body,
            "the conversion is lossy, and `Response.body` is not round-trippable"
        );
    }

    // --- redirect handling -------------------------------------------------

    /// A server that answers a fixed table of `path -> raw HTTP response`,
    /// over as many requests on one connection as the client cares to send —
    /// which is what following a redirect chain to the same host looks like
    /// on the wire. Unmatched paths 404, so a route the test forgot to wire up
    /// fails loudly instead of hanging.
    fn start_route_server(routes: Vec<(&'static str, Vec<u8>)>) -> std::net::SocketAddr {
        use std::io::{BufRead, BufReader, Write};

        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
        let addr = listener.local_addr().expect("the listener has an address");

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let mut writer = stream.try_clone().expect("the socket clones");
                let mut reader = BufReader::new(stream);

                loop {
                    let mut request_line = String::new();
                    match reader.read_line(&mut request_line) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                    let path = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_string();

                    loop {
                        let mut header = String::new();
                        match reader.read_line(&mut header) {
                            Ok(0) | Err(_) => return,
                            Ok(_) if header == "\r\n" => break,
                            Ok(_) => {}
                        }
                    }

                    let response = routes
                        .iter()
                        .find(|(route, _)| *route == path)
                        .map(|(_, body)| body.clone())
                        .unwrap_or_else(|| {
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()
                        });

                    if writer.write_all(&response).is_err() {
                        return;
                    }
                    let _ = writer.flush();
                }
            }
        });

        addr
    }

    /// A raw `301 Moved Permanently` pointing at `location`, keep-alive so the
    /// client's next request in the chain arrives on the same connection.
    fn redirect_response(status: u16, reason: &str, location: &str) -> Vec<u8> {
        format!("HTTP/1.1 {status} {reason}\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n")
            .into_bytes()
    }

    fn ok_response(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[tokio::test]
    async fn a_redirect_is_followed_and_the_chain_is_captured_on_the_final_response() {
        let addr = start_route_server(vec![
            (
                "/start",
                redirect_response(301, "Moved Permanently", "/next"),
            ),
            ("/next", redirect_response(302, "Found", "/end")),
            ("/end", ok_response("done")),
        ]);

        let config = Config::default();
        let client = build_client(&config).expect("a client builds");
        let response = send(&get(&format!("http://{addr}/start")), &client, &config)
            .await
            .expect("the chain resolves");

        // The final response is what Sendra reports as *the* response...
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "done");

        // ...and the chain that got there is captured alongside it, oldest
        // hop first, each carrying the status that redirected and the
        // location it pointed at, resolved to an absolute URL.
        assert_eq!(
            response.redirects,
            vec![
                RedirectHop {
                    status: 301,
                    location: format!("http://{addr}/next"),
                },
                RedirectHop {
                    status: 302,
                    location: format!("http://{addr}/end"),
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_request_with_no_redirect_reports_an_empty_chain() {
        // The overwhelmingly common case: nothing about an ordinary response
        // should look any different from before this feature existed.
        let addr = start_route_server(vec![("/", ok_response("hello"))]);

        let config = Config::default();
        let client = build_client(&config).expect("a client builds");
        let response = send(&get(&format!("http://{addr}/")), &client, &config)
            .await
            .expect("a plain response");

        assert_eq!(response.status, 200);
        assert!(response.redirects.is_empty());
    }

    #[tokio::test]
    async fn disabling_redirects_reports_the_3xx_response_itself_not_an_error() {
        let addr = start_route_server(vec![
            (
                "/start",
                redirect_response(301, "Moved Permanently", "/end"),
            ),
            ("/end", ok_response("done")),
        ]);

        let config = Config {
            redirects: config::FollowRedirects::Disabled,
            ..Config::default()
        };
        let client = build_client(&config).expect("a client builds");
        let response = send(&get(&format!("http://{addr}/start")), &client, &config)
            .await
            .expect("a 3xx is a normal, inspectable response");

        // The redirect itself is what came back — status, Location header and
        // all — not the response at the far end of it.
        assert_eq!(response.status, 301);
        assert_eq!(
            response
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("location"))
                .map(|(_, value)| value.as_str()),
            Some("/end")
        );
        // No chain: this response is not the result of following anything.
        assert!(response.redirects.is_empty());
    }

    #[tokio::test]
    async fn a_chain_longer_than_the_configured_maximum_is_an_error() {
        // Three hops to reach `/end`; a maximum of one allows the first and
        // must refuse the second.
        let addr = start_route_server(vec![
            ("/start", redirect_response(301, "Moved Permanently", "/a")),
            ("/a", redirect_response(302, "Found", "/b")),
            ("/b", redirect_response(303, "See Other", "/end")),
            ("/end", ok_response("done")),
        ]);

        let config = Config {
            redirects: config::FollowRedirects::Follow(1),
            ..Config::default()
        };
        let client = build_client(&config).expect("a client builds");
        let err = send(&get(&format!("http://{addr}/start")), &client, &config)
            .await
            .expect_err("a chain past the configured maximum must not resolve to a response");

        match err {
            SendraError::Network { source, .. } => {
                let message = source.to_string();
                assert!(
                    message.contains("redirect") || std::error::Error::source(&source).is_some(),
                    "expected a redirect-shaped error, got {message}"
                );
            }
            other => panic!("expected Network, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_custom_maximum_higher_than_the_chain_still_resolves() {
        // The other side of the same setting: a maximum generous enough for
        // the chain still reaches the end and still reports every hop.
        let addr = start_route_server(vec![
            ("/start", redirect_response(301, "Moved Permanently", "/a")),
            ("/a", redirect_response(302, "Found", "/end")),
            ("/end", ok_response("done")),
        ]);

        let config = Config {
            redirects: config::FollowRedirects::Follow(5),
            ..Config::default()
        };
        let client = build_client(&config).expect("a client builds");
        let response = send(&get(&format!("http://{addr}/start")), &client, &config)
            .await
            .expect("two hops is well within a maximum of five");

        assert_eq!(response.status, 200);
        assert_eq!(response.redirects.len(), 2);
    }

    #[tokio::test]
    async fn each_request_through_a_reused_client_reports_only_its_own_chain() {
        // The client — and its redirect log — is built once per run and
        // reused by every request; a chain from an earlier request must not
        // bleed into a later one that had none of its own.
        let addr = start_route_server(vec![
            (
                "/redirected",
                redirect_response(301, "Moved Permanently", "/plain"),
            ),
            ("/plain", ok_response("done")),
        ]);

        let config = Config::default();
        let client = build_client(&config).expect("a client builds");

        let redirected = send(&get(&format!("http://{addr}/redirected")), &client, &config)
            .await
            .expect("the redirect resolves");
        assert_eq!(redirected.redirects.len(), 1);

        let plain = send(&get(&format!("http://{addr}/plain")), &client, &config)
            .await
            .expect("a direct hit on the same client");
        assert!(
            plain.redirects.is_empty(),
            "the previous request's chain must not leak into this one"
        );
    }

    // --- structured bodies: json, body_file, form, multipart ---------------

    /// A minimal request whose only body field is set from `field: value`
    /// (already valid YAML for every shape these tests need — a scalar, a
    /// block, a sequence).
    fn request_with(field_and_value: &str) -> Request {
        Request::from_yaml_str(&format!(
            "method: POST\nurl: https://example.com\n{field_and_value}\n"
        ))
        .expect("the test request should parse")
    }

    #[test]
    fn a_json_body_is_serialized_and_gets_the_default_content_type() {
        let request = request_with("json:\n  name: ada\n  roles: [admin, user]\n");
        let resolved = request
            .resolve_body(Path::new("."))
            .expect("no file to read");

        let sent: serde_json::Value =
            serde_json::from_str(resolved.body.as_deref().expect("a body was produced"))
                .expect("the body is valid json");
        assert_eq!(
            sent,
            serde_json::json!({"name": "ada", "roles": ["admin", "user"]})
        );
        assert_eq!(resolved.header("Content-Type"), Some("application/json"));
        // The structured field is gone from the resolved request: the only
        // body field left is the plain string a script or `send_prepared`
        // reads.
        assert!(resolved.json.is_none());
    }

    #[test]
    fn a_json_bodys_explicit_content_type_is_not_clobbered() {
        let request = request_with(
            "headers:\n  Content-Type: application/vnd.example+json\njson:\n  ok: true\n",
        );
        let resolved = request
            .resolve_body(Path::new("."))
            .expect("no file to read");

        assert_eq!(
            resolved.header("Content-Type"),
            Some("application/vnd.example+json"),
            "an explicit content-type header must win over the automatic one"
        );
    }

    #[test]
    fn body_file_reads_relative_to_the_request_files_directory_not_the_cwd() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("payload.json"), r#"{"id":1}"#).unwrap();

        let request = request_with("body_file: ./payload.json\n");
        let resolved = request
            .resolve_body(dir.path())
            .expect("the file is beside the (hypothetical) request file");

        assert_eq!(resolved.body.as_deref(), Some(r#"{"id":1}"#));
        // `body_file` sets no content-type: Sendra cannot know what an
        // arbitrary file holds, so the request's own `headers:` is
        // responsible.
        assert!(resolved.header("Content-Type").is_none());

        // And resolving against a directory that does *not* hold the file —
        // standing in for the process's cwd — fails, which is the point of
        // the whole test: the path is relative to something specific, not
        // wherever `sendra` happened to be run from.
        let elsewhere = tempfile::tempdir().unwrap();
        assert!(matches!(
            request.resolve_body(elsewhere.path()),
            Err(SendraError::BodyFileIo { .. })
        ));
    }

    #[test]
    fn a_non_utf8_body_file_is_a_typed_error_not_a_silent_corruption() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("payload.bin"), [0xff, 0xfe, 0x00, 0xff]).unwrap();

        let request = request_with("body_file: ./payload.bin\n");
        assert!(matches!(
            request.resolve_body(dir.path()),
            Err(SendraError::BodyFileIo { .. })
        ));
    }

    #[test]
    fn a_form_body_is_url_encoded_and_gets_the_default_content_type() {
        let request = request_with("form:\n  username: ada lovelace\n  remember_me: \"true\"\n");
        let resolved = request
            .resolve_body(Path::new("."))
            .expect("no file to read");

        assert_eq!(
            resolved.body.as_deref(),
            Some("username=ada+lovelace&remember_me=true")
        );
        assert_eq!(
            resolved.header("Content-Type"),
            Some("application/x-www-form-urlencoded")
        );
        assert!(resolved.form.is_empty());
    }

    #[test]
    fn a_multipart_body_encodes_a_text_part_and_a_file_part() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cat.txt"), "meow").unwrap();

        let request = request_with(
            "multipart:\n  \
             - name: description\n    value: a photo of my cat\n  \
             - name: photo\n    path: ./cat.txt\n",
        );
        let resolved = request
            .resolve_body(dir.path())
            .expect("the file part reads fine");

        let content_type = resolved
            .header("Content-Type")
            .expect("multipart sets its own content-type")
            .to_string();
        assert!(
            content_type.starts_with("multipart/form-data; boundary="),
            "got {content_type}"
        );
        let boundary = content_type
            .strip_prefix("multipart/form-data; boundary=")
            .unwrap();

        let body = resolved.body.expect("a body was produced");
        assert!(body.contains(&format!("--{boundary}\r\n")));
        assert!(body.contains(
            "Content-Disposition: form-data; name=\"description\"\r\n\r\na photo of my cat"
        ));
        assert!(body.contains(
            "Content-Disposition: form-data; name=\"photo\"; filename=\"cat.txt\"\r\n\r\nmeow"
        ));
        assert!(body.trim_end().ends_with(&format!("--{boundary}--")));
        assert!(resolved.multipart.is_empty());
    }

    #[test]
    fn a_multipart_part_with_both_value_and_path_is_rejected_at_parse_time() {
        let err = Request::from_yaml_str(
            "method: POST\nurl: https://example.com\n\
             multipart:\n  - name: photo\n    value: x\n    path: ./cat.jpg\n",
        )
        .expect_err("a part cannot be both text and a file");
        assert!(
            matches!(&err, SendraError::InvalidRequest { reason } if reason.contains("both `value` and `path`")),
            "got {err:?}"
        );
    }

    #[test]
    fn a_multipart_part_with_neither_value_nor_path_is_rejected_at_parse_time() {
        let err = Request::from_yaml_str(
            "method: POST\nurl: https://example.com\nmultipart:\n  - name: photo\n",
        )
        .expect_err("a part needs exactly one of value/path");
        assert!(
            matches!(&err, SendraError::InvalidRequest { reason } if reason.contains("neither `value` nor `path`")),
            "got {err:?}"
        );
    }

    #[test]
    fn a_request_naming_two_body_fields_is_rejected_at_parse_time() {
        let err = Request::from_yaml_str(
            "method: POST\nurl: https://example.com\nbody: '{}'\njson:\n  a: 1\n",
        )
        .expect_err("body and json together must be rejected");
        assert!(
            matches!(&err, SendraError::InvalidRequest { reason } if reason.contains("body") && reason.contains("json")),
            "got {err:?}"
        );
    }

    #[test]
    fn two_body_fields_inside_a_collection_are_rejected_with_the_requests_context() {
        let yaml = "\
requests:
  - name: Broken
    method: POST
    url: https://example.com
    form:
      a: '1'
    body_file: ./x.json
";
        let err = Document::from_yaml_str(yaml).expect_err("must be rejected");
        match err {
            SendraError::InvalidCollection { reason } => {
                assert!(reason.contains("Broken"), "got {reason}");
                assert!(reason.contains("form"), "got {reason}");
                assert!(reason.contains("body_file"), "got {reason}");
            }
            other => panic!("expected InvalidCollection, got {other:?}"),
        }
    }

    #[test]
    fn a_plain_body_still_parses_and_resolves_unchanged() {
        // The non-goal, pinned: a file written before this feature existed
        // still works exactly as it did.
        let request = request_with("body: '{\"name\": \"ada\"}'\n");
        let resolved = request
            .resolve_body(Path::new("."))
            .expect("nothing to read");
        assert_eq!(resolved.body.as_deref(), Some(r#"{"name": "ada"}"#));
        assert!(resolved.header("Content-Type").is_none());
    }

    #[test]
    fn a_request_with_no_body_field_at_all_resolves_to_no_body() {
        let request = request_with("");
        let resolved = request
            .resolve_body(Path::new("."))
            .expect("nothing to resolve");
        assert!(resolved.body.is_none());
    }

    // --- query: as a map with real percent-encoding -------------------------

    fn get_with(field_and_value: &str) -> Request {
        Request::from_yaml_str(&format!(
            "method: GET\nurl: https://example.com/search\n{field_and_value}\n"
        ))
        .expect("the test request should parse")
    }

    #[test]
    fn a_request_with_no_query_field_leaves_the_url_untouched() {
        // The non-goal, pinned: a url-only request is not even reparsed.
        let request = get_with("");
        let resolved = request.resolve_query().expect("nothing to resolve");
        assert_eq!(resolved.url, "https://example.com/search");
        assert!(resolved.query.is_empty());
    }

    #[test]
    fn a_query_map_merges_onto_a_url_with_no_existing_query_string() {
        let request = get_with("query:\n  a: '1'\n  b: '2'\n");
        let resolved = request.resolve_query().expect("resolves");
        assert_eq!(resolved.url, "https://example.com/search?a=1&b=2");
        assert!(resolved.query.is_empty(), "cleared after resolution");
    }

    #[test]
    fn a_query_map_is_appended_onto_a_url_that_already_has_a_query_string() {
        let request = Request::from_yaml_str(
            "method: GET\nurl: https://example.com/search?existing=1\nquery:\n  new: '2'\n",
        )
        .unwrap();
        let resolved = request.resolve_query().expect("resolves");
        assert_eq!(resolved.url, "https://example.com/search?existing=1&new=2");
    }

    #[test]
    fn a_key_in_both_the_url_and_the_query_map_is_decided_by_the_query_map() {
        // `query:` wins: the URL's own `a=from-url` is dropped, not sent
        // alongside `a=from-query`.
        let request = Request::from_yaml_str(
            "method: GET\nurl: https://example.com/search?a=from-url&b=kept\nquery:\n  a: from-query\n",
        )
        .unwrap();
        let resolved = request.resolve_query().expect("resolves");
        let url = reqwest::Url::parse(&resolved.url).unwrap();
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("b".to_string(), "kept".to_string()),
                ("a".to_string(), "from-query".to_string()),
            ],
            "got {pairs:?}"
        );
    }

    #[test]
    fn special_characters_are_percent_encoded_not_concatenated() {
        let request = get_with("query:\n  q: 'coffee & tea, café'\n");
        let resolved = request.resolve_query().expect("resolves");

        // Read back through `Url` rather than asserting on the exact encoded
        // string: what matters is that the server sees the value that was
        // written, not which of several valid encodings was chosen.
        let url = reqwest::Url::parse(&resolved.url).unwrap();
        let (_, value) = url
            .query_pairs()
            .find(|(name, _)| name == "q")
            .expect("q was sent");
        assert_eq!(value, "coffee & tea, café");
        // And the raw query string actually is encoded, not the literal text
        // with a space and a non-ASCII character sitting in it.
        assert!(!resolved.url.contains(' '));
        assert!(resolved.url.is_ascii());
    }

    #[test]
    fn a_repeated_query_key_is_written_as_a_list() {
        let request = get_with("query:\n  tag:\n    - hot\n    - iced\n");
        let resolved = request.resolve_query().expect("resolves");
        let url = reqwest::Url::parse(&resolved.url).unwrap();
        let tags: Vec<String> = url
            .query_pairs()
            .filter(|(name, _)| name == "tag")
            .map(|(_, value)| value.into_owned())
            .collect();
        assert_eq!(tags, vec!["hot".to_string(), "iced".to_string()]);
    }

    #[test]
    fn an_unquoted_number_query_value_is_coerced_to_its_string_form() {
        let request = get_with("query:\n  limit: 10\n");
        let resolved = request.resolve_query().expect("resolves");
        assert_eq!(resolved.url, "https://example.com/search?limit=10");
    }

    #[test]
    fn environment_substitution_reaches_query_values_and_list_entries() {
        let request =
            get_with("query:\n  tenant: '{{tenant}}'\n  tag:\n    - '{{tenant}}'\n    - iced\n");
        let environment = crate::Environment::from_yaml_str("tenant: acme\n").unwrap();
        let substituted = environment.apply(&request).expect("tenant is set");
        assert_eq!(
            substituted.query,
            vec![
                ("tenant".to_string(), "acme".to_string()),
                ("tag".to_string(), "acme".to_string()),
                ("tag".to_string(), "iced".to_string()),
            ]
        );
    }

    // --- auth: bearer and basic ---------------------------------------------

    #[test]
    fn auth_bearer_resolves_to_a_bearer_authorization_header() {
        let request = request_with("auth:\n  bearer: my-token\n");
        let resolved = request.resolve_auth().expect("resolves");
        assert_eq!(resolved.header("Authorization"), Some("Bearer my-token"));
        assert!(resolved.auth.is_none());
    }

    #[test]
    fn auth_basic_resolves_to_a_base64_encoded_authorization_header() {
        let request = request_with("auth:\n  basic:\n    user: ada\n    pass: s3cr3t\n");
        let resolved = request.resolve_auth().expect("resolves");
        // base64("ada:s3cr3t")
        assert_eq!(
            resolved.header("Authorization"),
            Some("Basic YWRhOnMzY3IzdA==")
        );
        assert!(resolved.auth.is_none());
    }

    #[test]
    fn a_request_with_no_auth_field_resolves_to_no_authorization_header() {
        let request = request_with("");
        let resolved = request.resolve_auth().expect("nothing to resolve");
        assert!(resolved.header("Authorization").is_none());
    }

    #[test]
    fn auth_naming_both_bearer_and_basic_is_rejected_at_parse_time() {
        let err = Request::from_yaml_str(
            "method: GET\nurl: https://example.com\nauth:\n  bearer: x\n  basic:\n    user: a\n    pass: b\n",
        )
        .expect_err("bearer and basic together must be rejected");
        assert!(
            matches!(&err, SendraError::InvalidRequest { reason } if reason.contains("bearer") && reason.contains("basic")),
            "got {err:?}"
        );
    }

    #[test]
    fn auth_naming_neither_bearer_nor_basic_is_rejected_at_parse_time() {
        let err = Request::from_yaml_str("method: GET\nurl: https://example.com\nauth: {}\n")
            .expect_err("an empty auth block must be rejected");
        assert!(
            matches!(&err, SendraError::InvalidRequest { reason } if reason.contains("bearer") && reason.contains("basic")),
            "got {err:?}"
        );
    }

    #[test]
    fn auth_alongside_an_explicit_authorization_header_is_rejected_at_parse_time() {
        let err = Request::from_yaml_str(
            "method: GET\nurl: https://example.com\nheaders:\n  Authorization: Bearer hand-written\nauth:\n  bearer: x\n",
        )
        .expect_err("auth and an explicit Authorization header together must be rejected");
        assert!(
            matches!(&err, SendraError::InvalidRequest { reason } if reason.contains("Authorization")),
            "got {err:?}"
        );
    }

    #[test]
    fn the_authorization_collision_check_is_case_insensitive() {
        let err = Request::from_yaml_str(
            "method: GET\nurl: https://example.com\nheaders:\n  authorization: Bearer hand-written\nauth:\n  bearer: x\n",
        )
        .expect_err("a differently-cased Authorization header must still collide");
        assert!(matches!(&err, SendraError::InvalidRequest { .. }));
    }

    #[test]
    fn environment_substitution_reaches_bearer_and_basic_values() {
        let request =
            request_with("auth:\n  basic:\n    user: '{{username}}'\n    pass: '{{password}}'\n");
        let environment =
            crate::Environment::from_yaml_str("username: ada\npassword: s3cr3t\n").unwrap();
        let substituted = environment.apply(&request).expect("both are set");
        let auth = substituted.auth.expect("auth survives substitution");
        let basic = auth.basic.expect("basic survives substitution");
        assert_eq!(basic.user, "ada");
        assert_eq!(basic.pass, "s3cr3t");
    }
}
