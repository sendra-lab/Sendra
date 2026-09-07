//! [`Collection`] and [`Document`]: a named group of requests in one YAML
//! file, and the two shapes a Sendra file can hold.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::SendraError;
use crate::request::Request;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::Method;

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
            "richer-assertions.yaml",
            "test-collection.yaml",
            "scripted-request.yaml",
            "capture-chain.yaml",
            "capture-header-status.yaml",
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
}
