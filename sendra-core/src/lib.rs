//! Core model for Sendra: request/response types, YAML loading, HTTP execution.
//!
//! This crate is deliberately free of CLI concerns (argument parsing, terminal
//! colouring, exit codes). A future `sendra-tui` crate will depend on it
//! directly, so everything here returns typed [`SendraError`] values that a
//! front-end can match on rather than pre-formatted strings.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

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
/// The HTTP client [`send`] and [`send_prepared`] send through, re-exported so a
/// front-end can build one with [`build_client`] and pass it around without
/// taking a direct dependency on reqwest.
pub use reqwest::Client as HttpClient;
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
/// Everything but `method` and `url` is optional. Headers are a `BTreeMap` so
/// iteration order is deterministic across runs.
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Raw body, sent verbatim. Structured/multipart bodies come later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
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
        serde_yaml::from_str(yaml).map_err(SendraError::ParseStr)
    }

    /// Read and parse a request from a YAML file on disk.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, SendraError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|source| SendraError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        serde_yaml::from_str(&raw).map_err(|source| SendraError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Display label: the `name` field if present, else `METHOD url`.
    pub fn label(&self) -> String {
        match &self.name {
            Some(name) => name.clone(),
            None => format!("{} {}", self.method, self.url),
        }
    }
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
            Ok(Document::Single(serde_yaml::from_str(yaml).map_err(&wrap)?))
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
    pub body: String,
    pub elapsed: Duration,
}

impl Response {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
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
/// particular one. The config decides one thing here — the timeout — and
/// nothing else about the client is configurable in v1; reqwest's own pool
/// defaults are what a command-line tool wants.
///
/// Fails only when reqwest cannot construct a client at all (a TLS backend that
/// will not initialise, say), which is fatal to the whole run and so is
/// [`SendraError::Client`] rather than a per-request network error.
pub fn build_client(config: &Config) -> Result<HttpClient, SendraError> {
    // reqwest has no timeout of its own by default, so an unresponsive server
    // would hang the process indefinitely; the config always supplies one.
    HttpClient::builder()
        .timeout(config.timeout)
        .build()
        .map_err(SendraError::Client)
}

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
        headers.insert(header_name, header_value);
    }

    let network_err = |source: reqwest::Error| SendraError::Network {
        url: request.url.clone(),
        source,
    };

    let mut builder = client
        .request(request.method.into(), &request.url)
        .headers(headers);
    if let Some(body) = &request.body {
        builder = builder.body(body.clone());
    }

    let started = Instant::now();
    let response = builder.send().await.map_err(network_err)?;

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
    let bytes = response.bytes().await.map_err(network_err)?;
    let elapsed = started.elapsed();

    Ok(Response {
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or("").to_owned(),
        headers: header_pairs,
        // Lossy: a body can legitimately be binary, and this crate hands back a
        // printable String for now. Binary-safe bodies are a later concern.
        body: String::from_utf8_lossy(&bytes).into_owned(),
        elapsed,
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

        let mut expected_headers = BTreeMap::new();
        expected_headers.insert("Accept".to_string(), "application/json".to_string());

        assert_eq!(
            request,
            Request {
                name: Some("Get user".to_string()),
                method: Method::Get,
                url: "https://api.example.com/users/1".to_string(),
                headers: expected_headers,
                body: None,
                assertions: None,
                pre_request: None,
                post_request: None,
                capture: None,
            }
        );
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
            headers: BTreeMap::from([("bad header".to_string(), "x".to_string())]),
            body: None,
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
            headers: BTreeMap::new(),
            body: None,
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
            headers: BTreeMap::new(),
            body: None,
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
}
