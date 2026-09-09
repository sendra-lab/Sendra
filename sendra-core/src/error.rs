//! Every way loading or sending a request can fail: [`SendraError`].

use std::path::PathBuf;
use std::time::Duration;

use crate::environment::{describe_captured, describe_environment, describe_variables};
use crate::script;

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
    /// is raised from either half of [`send_prepared`](crate::send_prepared): a
    /// server that accepts the connection and then dribbles the body out too
    /// slowly times out here exactly like one that never answers at all.
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
    /// [`Request::resolve_body`](crate::Request::resolve_body).
    #[error("could not read request body file `{path}`")]
    BodyFileIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A `client_cert`/`client_key` path (from either config file, or
    /// `--client-cert`/`--client-key`) named a file that could not be read.
    /// Distinct from [`Client`](Self::Client), which wraps only a
    /// `reqwest::Error`: reading the file happens before reqwest is ever
    /// involved, and the message needs to say which path was the problem.
    #[error("could not read client certificate file `{path}`")]
    ClientCertIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Only one of `client_cert`/`client_key` — from config, `--client-cert`/
    /// `--client-key`, or a mix of both — resolved to a path. A client
    /// certificate and its private key are only meaningful as a pair; sending
    /// half of one silently would be worse than refusing to build the client
    /// at all.
    #[error("client_cert/client_key must both be set, but only the {which} was")]
    ClientCertIncomplete { which: &'static str },

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

    /// An environment file parsed as valid YAML, but its `auth:` block broke
    /// a rule `serde` cannot express: exactly one of `bearer`/`basic`/
    /// `api_key` may be set — the same rule
    /// [`Request::validate`](crate::Request::validate) enforces for a
    /// request's own `auth:` block, reused here since an environment's
    /// `auth:` is the exact same [`Auth`](crate::Auth) shape. Raised at
    /// parse time, from [`Environment::from_yaml_str`](crate::Environment::from_yaml_str)/
    /// [`from_path`](crate::Environment::from_path) — a collision between an
    /// environment's default `auth:` and a request's own header/query is a
    /// different failure, folded into [`InvalidRequest`](Self::InvalidRequest)
    /// instead, since it can only be discovered once a specific request is
    /// being substituted against this environment.
    #[error("invalid environment ({}): {reason}", describe_environment(.path))]
    InvalidEnvironment {
        path: Option<PathBuf>,
        reason: String,
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
    /// [`ScriptOutcome::Failed`](crate::script::ScriptOutcome::Failed) rather than as
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

    /// An `auth.oauth` token acquisition failed: bad credentials, an
    /// unreachable or non-2xx token endpoint, or a response with no
    /// `access_token`.
    ///
    /// Raised lazily, only when a request whose `auth.oauth` needs a token is
    /// about to run — [`Request::resolve_oauth`](crate::Request::resolve_oauth),
    /// called just before [`Request::resolve_auth`](crate::Request::resolve_auth)
    /// — rather than up front for the whole run, since it happens per-request
    /// the same way a substitution failure does. A request whose acquisition
    /// fails is a per-request failure with no response, the same category
    /// `VariableNotFound` already is; the siblings around it, using other
    /// auth or none at all, are unaffected. See [`crate::oauth`] for the
    /// in-run cache this reads and writes, and for why a failure for a given
    /// `oauth:` config is remembered rather than retried for every later
    /// request that shares it.
    #[error("could not acquire an OAuth token from `{token_url}`: {reason}")]
    OAuthAcquisition { token_url: String, reason: String },
}
