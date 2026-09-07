//! The three "structured input becomes the final wire form" passes:
//! [`Request::resolve_query`], [`Request::resolve_body`] and
//! [`Request::resolve_auth`]. Each returns a new [`Request`] with its own
//! structured field(s) cleared and the plain wire-level field (`url`,
//! `body`/headers, `Authorization` header) set instead — see each method's
//! own doc comment for why the order among them and relative to the config
//! and `pre_request` matters.

use std::path::Path;

use crate::error::SendraError;
use crate::request::multipart::{encode_multipart, read_body_file};
use crate::request::Request;

impl Request {
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
    /// [`environment`](crate::environment) module docs.
    ///
    /// File content — for `body_file` and a multipart file part alike — is
    /// read as UTF-8 text; a file that is not valid UTF-8 is
    /// [`SendraError::BodyFileIo`]. Sendra's bodies are text throughout, the
    /// same way a [`Response`](crate::Response)'s is, and true binary uploads are out of scope
    /// for this version.
    pub fn resolve_body(&self, base_dir: &Path) -> Result<Request, SendraError> {
        let mut resolved = self.clone();

        if let Some(value) = &self.json {
            let body = serde_json::to_string(value).expect("a serde_json::Value always serializes");
            resolved.body = Some(body);
            crate::config::insert_if_absent(
                &mut resolved.headers,
                "Content-Type",
                "application/json",
            );
        } else if let Some(path) = &self.body_file {
            resolved.body = Some(read_body_file(base_dir, path)?);
        } else if !self.form.is_empty() {
            let body = serde_urlencoded::to_string(&self.form)
                .expect("a Vec<(String, String)> always encodes as x-www-form-urlencoded pairs");
            resolved.body = Some(body);
            crate::config::insert_if_absent(
                &mut resolved.headers,
                "Content-Type",
                "application/x-www-form-urlencoded",
            );
        } else if !self.multipart.is_empty() {
            let (body, content_type) = encode_multipart(&self.multipart, base_dir)?;
            resolved.body = Some(body);
            crate::config::insert_if_absent(&mut resolved.headers, "Content-Type", &content_type);
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
    /// the header rather than needing [`config::insert_if_absent`](crate::config::insert_if_absent)'s
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SendraError;

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
        let err = crate::Document::from_yaml_str(yaml).expect_err("must be rejected");
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
