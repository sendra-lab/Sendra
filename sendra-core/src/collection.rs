//! [`Collection`] and [`Document`]: a named group of requests in one YAML
//! file, and the two shapes a Sendra file can hold.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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

    /// Every rule `Deserialize` cannot express, checked directly rather than
    /// only ever at parse time: a single request's own `Request::validate`
    /// (at most one body source, `auth` exclusivity, ...), or, for a
    /// collection, `Collection::validate` (non-empty, every request named,
    /// no name used twice) plus that same per-request check for each one.
    ///
    /// `from_yaml_str`/`from_path` already run this before ever handing a
    /// `Document` back, so a `Document` that came from a real file is always
    /// already valid — this exists for the other direction: a `Document`
    /// built or mutated in memory (a front-end applying an edit, say) can
    /// check *before* [`save_to_path`](Self::save_to_path) writes it, rather
    /// than only discovering it was invalid the next time something tries to
    /// load it back. `save_to_path` calls this itself for exactly that
    /// reason — this is exposed as its own method mainly so a caller can ask
    /// the question earlier, e.g. to show a validation message before ever
    /// attempting a write.
    pub fn validate(&self) -> Result<(), SendraError> {
        match self {
            Document::Single(request) => request.validate(),
            Document::Collection(collection) => collection.validate(),
        }
    }

    /// Serializes this document back to YAML, exactly the shape
    /// [`from_yaml_str`](Self::from_yaml_str)/[`from_path`](Self::from_path)
    /// parse: a bare [`Request`] for `Single`, a [`Collection`] for
    /// `Collection`.
    ///
    /// **Not a derived `Serialize` impl on `Document` itself.** `Document`
    /// deliberately has no `#[derive(Serialize)]` (nor a hand-written
    /// externally-tagged one): serde's default representation for an enum
    /// like this one wraps the output in a `Single:`/`Collection:` key
    /// (`!Single ...` in YAML's own tag syntax, depending on the
    /// representation), which is not a shape `from_yaml_str`'s own shape
    /// detection — "a top-level `requests` key means a collection, anything
    /// else is a single request" (see this type's own doc comment) — was ever
    /// written to expect. Serializing whichever variant is actually held,
    /// unwrapped, is what keeps
    /// `Document::from_yaml_str(&doc.to_yaml_string()?)` equal to `doc` for
    /// every real collection or request file — round-tripping through the
    /// same shape a hand-written file already has, not a new one only this
    /// method would produce.
    pub fn to_yaml_string(&self) -> Result<String, SendraError> {
        match self {
            Document::Single(request) => serde_yaml::to_string(request),
            Document::Collection(collection) => serde_yaml::to_string(collection),
        }
        .map_err(SendraError::Serialize)
    }

    /// Writes this document back to `path`, atomically: the new content is
    /// written to a sibling temp file in the same directory first, then
    /// [`std::fs::rename`]d over `path` — never written in place — so a
    /// crash or a killed process mid-write can never leave `path` holding a
    /// truncated or half-written file. A rename onto an existing file is
    /// atomic on the same volume on both POSIX (`rename(2)`) and Windows
    /// (`std::fs::rename` there is implemented as `MoveFileExW` with
    /// `MOVEFILE_REPLACE_EXISTING`) — the two platforms sendra-tui ships
    /// on — so `path` is always either its old content in full or its new
    /// content in full, never a mix of both, no matter when the process is
    /// interrupted.
    ///
    /// The temp file is created in the *same directory* as `path`, not the
    /// system temp directory: a rename across filesystems/mount points is not
    /// atomic (POSIX `rename(2)` fails outright with `EXDEV`), so the temp
    /// file has to already live on whatever volume `path` is on for the final
    /// rename to be the one atomic operation this whole guarantee rests on.
    ///
    /// If either the initial write or the rename fails, `path` is left
    /// completely untouched (the failure can only ever happen to the temp
    /// file, before `path` itself is touched at all) and the temp file is
    /// removed on a best-effort basis rather than left behind as a stray
    /// dotfile — the original error is what gets returned either way, not
    /// whatever the cleanup did.
    ///
    /// **Refuses to write an invalid document at all** — [`validate`](Self::validate)
    /// is checked first, before the temp file is even created. Without this,
    /// an in-memory edit that left the document invalid (a collection request
    /// edited down to an empty `name`, say) would still write out a file that
    /// parses back as YAML but fails `Collection::validate` the very next
    /// time anything loads it — a real file that looks saved but is
    /// silently broken. Catching it here means the caller learns about it
    /// immediately, through the same `Result` a disk-level failure already
    /// comes back through, rather than the next `Document::from_path` call
    /// discovering it days later.
    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<(), SendraError> {
        self.validate()?;

        let path = path.as_ref();
        let yaml = self.to_yaml_string()?;
        let temp_path = unique_temp_path(path);

        std::fs::write(&temp_path, yaml.as_bytes()).map_err(|source| SendraError::SaveIo {
            path: path.to_path_buf(),
            source,
        })?;

        std::fs::rename(&temp_path, path).map_err(|source| {
            let _ = std::fs::remove_file(&temp_path);
            SendraError::SaveIo {
                path: path.to_path_buf(),
                source,
            }
        })
    }
}

/// A path, next to `target`, that nothing else is using — what
/// [`Document::save_to_path`] writes the new content to before renaming it
/// over `target`. Named with a leading dot (hidden on Unix, and merely
/// unusual rather than special on Windows) and a `sendra-tmp-` marker so a
/// stray one left behind by a process that was killed between the write and
/// the rename reads as obviously disposable rather than a mystery file.
///
/// Unique per call within one process via a process-wide counter — `target`'s
/// own name plus the process id alone would collide if `save_to_path` were
/// ever called twice for the same path in quick succession (e.g. two rapid
/// saves) inside the same process.
fn unique_temp_path(target: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let dir = target
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("document.yaml");
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);

    dir.join(format!(
        ".{file_name}.sendra-tmp-{}-{unique}",
        std::process::id()
    ))
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
            "oauth.yaml",
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

    // --- to_yaml_string / save_to_path --------------------------------------

    #[test]
    fn to_yaml_string_round_trips_a_collection_with_every_nested_shape() {
        let yaml = "\
name: test
requests:
  - name: One
    method: POST
    url: https://example.com
    headers:
      X-Test: abc
    body: '{}'
    auth:
      bearer: secret-token
    assertions:
      status: 200
      json:
        $.ok: true
    capture:
      id: $.id
      trace:
        header: X-Trace-Id
";
        let document = Document::from_yaml_str(yaml).unwrap();

        let serialized = document
            .to_yaml_string()
            .expect("a valid document always serializes");
        let round_tripped =
            Document::from_yaml_str(&serialized).expect("what was just serialized must reparse");

        assert_eq!(
            round_tripped, document,
            "round-tripping through to_yaml_string must not lose or change anything"
        );
    }

    #[test]
    fn to_yaml_string_serializes_a_single_request_as_a_bare_request_not_wrapped() {
        let yaml = "method: GET\nurl: https://example.com\n";
        let document = Document::from_yaml_str(yaml).unwrap();

        let serialized = document.to_yaml_string().unwrap();

        assert_eq!(Document::from_yaml_str(&serialized).unwrap(), document);
        // The regression this guards against: a derived `Serialize` on
        // `Document` itself would wrap the output in a `Single:` key, which
        // `from_yaml_str`'s own shape detection was never written to expect.
        assert!(
            !serialized.contains("Single") && !serialized.contains("Collection"),
            "a Document must serialize as whichever bare shape it holds, not tagged with its \
             own variant name: got {serialized}"
        );
    }

    #[test]
    fn document_validate_accepts_a_valid_single_and_a_valid_collection() {
        let single = Document::from_yaml_str("method: GET\nurl: https://example.com\n").unwrap();
        single
            .validate()
            .expect("a real, already-parsed Single document must validate");

        let collection = Document::from_yaml_str(
            "requests:\n  - name: One\n    method: GET\n    url: https://example.com\n",
        )
        .unwrap();
        collection
            .validate()
            .expect("a real, already-parsed Collection document must validate");
    }

    #[test]
    fn document_validate_rejects_a_collection_with_an_unnamed_request() {
        // Built directly rather than through `from_yaml_str`, which would
        // already reject this at parse time — `validate` has to be checked
        // independently, since it exists precisely for a `Document` that
        // didn't come from a file (an in-memory edit, say).
        let request = Request::from_yaml_str("method: GET\nurl: https://example.com\n").unwrap();
        let document = Document::Collection(Collection {
            name: None,
            requests: vec![request],
        });

        let err = document
            .validate()
            .expect_err("an unnamed request in a collection is invalid");
        assert!(
            matches!(err, SendraError::InvalidCollection { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn save_to_path_refuses_to_write_an_invalid_document_and_touches_nothing() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = dir.path().join("collection.yaml");
        let request = Request::from_yaml_str("method: GET\nurl: https://example.com\n").unwrap();
        let invalid = Document::Collection(Collection {
            name: None,
            requests: vec![request], // unnamed -- invalid inside a collection
        });

        let err = invalid
            .save_to_path(&path)
            .expect_err("an invalid document must never be written");
        assert!(
            matches!(err, SendraError::InvalidCollection { .. }),
            "got {err:?}"
        );

        assert!(
            !path.exists(),
            "nothing should be written for a document that fails validation"
        );
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(
            entries.is_empty(),
            "no temp file should be created either, since validation happens before the write: \
             {entries:?}"
        );
    }

    #[test]
    fn save_to_path_writes_the_document_and_a_reload_from_disk_matches() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = dir.path().join("collection.yaml");
        let document = Document::from_yaml_str("method: GET\nurl: https://example.com\n").unwrap();

        document
            .save_to_path(&path)
            .expect("saving into a writable directory must succeed");

        let reloaded = Document::from_path(&path).expect("the saved file must parse back");
        assert_eq!(reloaded, document);
    }

    #[test]
    fn save_to_path_leaves_no_temp_file_behind_on_success() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = dir.path().join("collection.yaml");
        let document = Document::from_yaml_str("method: GET\nurl: https://example.com\n").unwrap();

        document.save_to_path(&path).unwrap();

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from("collection.yaml")],
            "no stray temp file should remain after a successful save: {entries:?}"
        );
    }

    #[test]
    fn save_to_path_fails_without_touching_anything_when_the_parent_is_not_a_directory() {
        // A real, deterministic write-phase failure (before `path` is ever
        // touched): the temp file's own write fails because its parent
        // component names a plain file, not a directory — reproducible on
        // both POSIX (`ENOTDIR`) and Windows without needing OS-specific
        // permission setup.
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let blocking_file = dir.path().join("not-a-directory");
        std::fs::write(&blocking_file, "just a file").unwrap();
        let path = blocking_file.join("collection.yaml");

        let document = Document::from_yaml_str("method: GET\nurl: https://example.com\n").unwrap();
        let err = document
            .save_to_path(&path)
            .expect_err("a non-directory parent must fail the write");
        assert!(matches!(err, SendraError::SaveIo { .. }), "got {err:?}");

        assert_eq!(
            std::fs::read_to_string(&blocking_file).unwrap(),
            "just a file",
            "the unrelated file the failure was caused by must be untouched"
        );
    }

    #[test]
    fn save_to_path_fails_without_corrupting_an_existing_directory_at_the_target() {
        // A different, later failure point than the previous test: the temp
        // file's own write succeeds (its parent — `dir` — is a real,
        // writable directory), and the failure is specifically the final
        // rename, which always fails on both POSIX (`EISDIR`) and Windows
        // when the destination is an existing directory. This proves the
        // target is left alone even when the new content was already written
        // somewhere, not just when nothing was ever written at all.
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = dir.path().join("collection.yaml");
        std::fs::create_dir(&path).unwrap();

        let document = Document::from_yaml_str("method: GET\nurl: https://example.com\n").unwrap();
        let err = document
            .save_to_path(&path)
            .expect_err("renaming a file over an existing directory must fail");
        assert!(matches!(err, SendraError::SaveIo { .. }), "got {err:?}");

        assert!(
            path.is_dir(),
            "the original directory at the target path must be left exactly as it was"
        );
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from("collection.yaml")],
            "no leftover temp file should remain after a failed rename: {entries:?}"
        );
    }

    /// The one failure mode the two tests above can't reach: a write refused
    /// purely by filesystem permissions rather than by the path shape.
    /// Windows-only because `std::fs::Permissions::set_readonly` on a
    /// *directory* is cosmetic there and does not actually block file
    /// creation inside it — reproducing a genuinely write-denied directory
    /// needs a real ACL deny via `icacls`, which only exists on Windows. The
    /// POSIX equivalent (`set_permissions` clearing the write bit on the
    /// directory) is not exercised here since this workspace's dev/CI
    /// environment for this crate is Windows; the *mechanism* being proved —
    /// a failed write leaves the original file completely untouched — is
    /// already covered cross-platform by the two tests above.
    #[test]
    #[cfg(windows)]
    fn a_write_denied_target_directory_leaves_the_original_file_completely_untouched() {
        use std::process::Command;

        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = dir.path().join("collection.yaml");
        let original = "method: GET\nurl: https://example.com/original\n";
        std::fs::write(&path, original).unwrap();

        let user = std::env::var("USERNAME").expect("USERNAME must be set on Windows");
        let deny = Command::new("icacls")
            .arg(dir.path())
            .arg("/deny")
            .arg(format!("{user}:(OI)(CI)W"))
            .status()
            .expect("icacls must be available on Windows");
        assert!(
            deny.success(),
            "icacls /deny must succeed to set up this test"
        );

        let new_document =
            Document::from_yaml_str("method: POST\nurl: https://example.com/new\n").unwrap();
        let result = new_document.save_to_path(&path);

        // Restore permissions before asserting anything, so a failing
        // assertion never leaves the temp directory locked for cleanup.
        let restore = Command::new("icacls")
            .arg(dir.path())
            .arg("/remove:d")
            .arg(&user)
            .status()
            .expect("icacls must be available on Windows");
        assert!(
            restore.success(),
            "icacls /remove:d must succeed to clean this test up"
        );

        assert!(
            result.is_err(),
            "a write-denied directory must fail the save rather than silently succeeding"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "the original file must be completely unchanged after the failed save"
        );
    }
}
