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

use crate::environment::{describe_environment, describe_variables};

pub mod assertions;
pub mod config;
pub mod environment;

pub use assertions::{AssertionKind, AssertionReport, AssertionResult, Assertions};
pub use config::Config;
pub use environment::Environment;

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
    #[error("no variable named `{name}` in {}", describe_variables(.environment, .available))]
    VariableNotFound {
        name: String,
        available: Vec<String>,
        /// The environment file the variable was looked for in, or `None` when
        /// no environment file was found at all.
        environment: Option<PathBuf>,
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

/// Send `request` under `config` and collect the full response.
///
/// The elapsed time covers connect, send and body read — i.e. what a user
/// waits for, not just time-to-first-byte.
///
/// `config` is a parameter rather than something resolved in here, and is not
/// optional, so that there is exactly one way to send a request and it is the
/// one that applies configuration. Callers with nothing to apply pass
/// [`Config::default`], which is the same defaults resolution falls back to. It
/// contributes two things: default headers, merged by [`Config::apply`] with
/// the request winning ties, and the timeout the client is built with.
pub async fn send(request: &Request, config: &Config) -> Result<Response, SendraError> {
    // Everything below works from the merged request, so a config header is
    // validated and sent exactly like one written in the file.
    let request = &config.apply(request);

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

    // reqwest has no timeout of its own by default, so an unresponsive server
    // would hang the process indefinitely; the config always supplies one.
    let client = reqwest::Client::builder()
        .timeout(config.timeout)
        .build()
        .map_err(network_err)?;

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
        };
        let err = send(&request, &Config::default())
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
        };
        let config = Config {
            headers: BTreeMap::from([("bad header".to_string(), "x".to_string())]),
            ..Config::default()
        };
        let err = send(&request, &config)
            .await
            .expect_err("invalid header must error");
        assert!(
            matches!(err, SendraError::InvalidHeader { .. }),
            "got {err:?}"
        );
    }
}
