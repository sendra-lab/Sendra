//! [`Request`]: the on-disk shape of one request, its parsing/validation, and
//! [`Method`]. The three "structured input becomes the final wire form"
//! passes — [`Request::resolve_query`], [`Request::resolve_body`],
//! [`Request::resolve_auth`] — live in [`resolve`], each field's own type
//! lives in [`multipart`]/[`auth`], and the repeated-header (de)serializers
//! live in `headers` (private: only `Request`'s own `#[serde(...)]`
//! attributes need them).

pub mod auth;
pub(crate) mod headers;
pub mod multipart;
pub mod resolve;

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::assertions::Assertions;
use crate::capture::Captures;
use crate::error::SendraError;
use crate::request::auth::Auth;
use crate::request::headers::{deserialize_headers, serialize_headers};
use crate::request::multipart::MultipartPart;

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
/// **Headers are a `Vec` of pairs, not a map** — matching [`Response::headers`](crate::Response::headers)
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
    /// time. By the time a `pre_request` script or [`send_prepared`](crate::send_prepared) sees a
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
    /// module docs on [`assertions`](crate::assertions).
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
    /// characters. See the [`script`](crate::script) module for both decisions and for what
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
    /// the response, never before it. See the [`capture`](crate::capture) module for what a
    /// path may select and for what happens when one does not match.
    ///
    /// **The block is not substituted.** A `{{var}}` in a capture path or name
    /// stays those characters; see [`Environment::apply`](crate::Environment::apply).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<Captures>,

    /// Retry this request on a true failure — no response at all: a DNS,
    /// connection, TLS or timeout error — up to `count` additional attempts,
    /// waiting `delay_ms` (default `0`, no wait) between each.
    ///
    /// ```text
    /// retry:
    ///   count: 2
    ///   delay_ms: 200
    /// ```
    ///
    /// Only a failure to get *any* response triggers a retry. A 4xx/5xx is
    /// still a response — [`send_prepared`](crate::send_prepared) returns it
    /// as `Ok`, not `Err` — so it is never retried by this field; retrying on
    /// a specific status is a separate, more advanced feature this does not
    /// attempt. Simple, fixed backoff: no exponential delay or jitter.
    ///
    /// **Only the final attempt's outcome is reported.** A request that fails
    /// twice and then succeeds is reported as a plain, ordinary success — the
    /// two failed attempts before it are never counted toward `sendra test`'s
    /// summary or either subcommand's exit code, though each retry is logged
    /// to stderr for visibility. A request that exhausts every attempt is
    /// reported as the one failure it always would have been, with no
    /// separate record of the attempts that came before it.
    ///
    /// `None` — no `retry:` key at all — means a request is sent once and
    /// whatever happens is final, exactly as it was before this field
    /// existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryConfig>,
}

/// How many extra times to try a request, and how long to wait between
/// attempts, when it fails to get any response — see [`Request::retry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RetryConfig {
    /// Additional attempts beyond the first — `count: 2` means up to three
    /// attempts total. `0` is accepted and means what writing no `retry:`
    /// block at all already means.
    pub count: u32,
    /// Milliseconds to wait before each retry. `None`/omitted is `0`: retry
    /// immediately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
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
    pub(crate) fn validate(&self) -> Result<(), SendraError> {
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
                retry: None,
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
        assert_eq!(
            capture.entries()["auth_token"],
            crate::capture::CaptureSource::JsonPath("$.token".to_string())
        );
        assert_eq!(
            capture.entries()["user_id"],
            crate::capture::CaptureSource::JsonPath("$.user.id".to_string())
        );
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
        assert_eq!(
            request.capture.unwrap().entries()["v"],
            crate::capture::CaptureSource::JsonPath("nonsense".to_string())
        );
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

    // --- `retry` -----------------------------------------------------------

    #[test]
    fn no_retry_key_at_all_is_none() {
        // The no-op guarantee: a file written before this field existed
        // parses to exactly the same `Request` it always did.
        let request = Request::from_yaml_str("method: GET\nurl: https://example.com\n").unwrap();
        assert_eq!(request.retry, None);
    }

    #[test]
    fn retry_parses_count_and_an_optional_delay() {
        let request = Request::from_yaml_str(
            "method: GET\nurl: https://example.com\nretry:\n  count: 2\n  delay_ms: 250\n",
        )
        .unwrap();

        assert_eq!(
            request.retry,
            Some(RetryConfig {
                count: 2,
                delay_ms: Some(250),
            })
        );
    }

    #[test]
    fn retry_delay_ms_is_optional_and_defaults_to_none() {
        let request =
            Request::from_yaml_str("method: GET\nurl: https://example.com\nretry:\n  count: 3\n")
                .unwrap();

        assert_eq!(
            request.retry,
            Some(RetryConfig {
                count: 3,
                delay_ms: None,
            })
        );
    }

    #[test]
    fn retry_without_count_is_a_parse_error() {
        // `count` has no default: writing `retry:` at all is a statement of
        // intent to retry, and a block that does not say how many times is a
        // broken file rather than "retry zero times".
        let err = Request::from_yaml_str(
            "method: GET\nurl: https://example.com\nretry:\n  delay_ms: 100\n",
        )
        .expect_err("a `retry` block with no `count` must not parse");
        assert!(matches!(err, SendraError::ParseStr(_)), "got {err:?}");
    }

    #[test]
    fn retry_rejects_an_unknown_field() {
        let err = Request::from_yaml_str(
            "method: GET\nurl: https://example.com\nretry:\n  count: 1\n  backoff: exponential\n",
        )
        .expect_err("an unknown `retry` field must not silently parse");
        assert!(matches!(err, SendraError::ParseStr(_)), "got {err:?}");
    }
}
