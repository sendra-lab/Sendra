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
/// ```
///
/// `name`, `headers` and `body` are optional. Headers are a `BTreeMap` so
/// iteration order is deterministic across runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// Send `request` and collect the full response.
///
/// The elapsed time covers connect, send and body read — i.e. what a user
/// waits for, not just time-to-first-byte.
pub async fn send(request: &Request) -> Result<Response, SendraError> {
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

    let client = reqwest::Client::builder().build().map_err(network_err)?;

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
        assert_eq!(request.label(), "POST https://example.com");
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
        };
        let err = send(&request).await.expect_err("invalid header must error");
        assert!(
            matches!(err, SendraError::InvalidHeader { .. }),
            "got {err:?}"
        );
    }
}
