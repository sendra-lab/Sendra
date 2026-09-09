//! The three "structured input becomes the final wire form" passes, run in
//! this order: [`Request::resolve_auth`], [`Request::resolve_query`] and
//! [`Request::resolve_body`]. Each returns a new [`Request`] with its own
//! structured field(s) cleared and the plain wire-level field (`url`,
//! `body`/headers, `Authorization` header) set instead — see each method's
//! own doc comment for why the order among them and relative to the config
//! and `pre_request` matters.

use std::path::Path;

use crate::error::SendraError;
use crate::http::client::HttpClient;
use crate::oauth::OAuthTokenCache;
use crate::request::auth::{ApiKeyLocation, Auth};
use crate::request::multipart::{encode_multipart, read_body_file};
use crate::request::Request;

impl Request {
    /// Merge `query` onto `url`'s own query string, percent-encoded
    /// properly, returning a request whose `url` is the final string that
    /// goes on the wire and whose `query` is empty.
    ///
    /// Called right after [`resolve_auth`](Self::resolve_auth) — which, for
    /// an `auth.api_key` in `query` form, has already appended its
    /// `name`/`value` pair onto `query` so it merges through this exact
    /// mechanism rather than a separate one — and before
    /// [`resolve_body`](Self::resolve_body), the config, or a `pre_request`
    /// script ever see the request. A `pre_request` script therefore sees
    /// `query` parameters (including any from `auth.api_key`) already merged
    /// into `request.url`, not a separate map, for consistency with
    /// `resolve_body`'s "scripts see the final resolved form" precedent.
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

    /// Resolve `auth` into the header (`bearer`/`basic`/an `api_key` in
    /// `header` form) or query parameter (an `api_key` in `query` form) that
    /// goes on the wire, clearing `auth` on the way out.
    ///
    /// Called right after environment substitution and before
    /// [`resolve_query`](Self::resolve_query), [`resolve_body`](Self::resolve_body), the
    /// config, or a `pre_request` script ever see the request. It runs
    /// *before* `resolve_query` specifically so that an `auth.api_key` in
    /// `query` form can hand its `name`/`value` pair to `query` and let
    /// `resolve_query` do the actual merging onto `url` — the same
    /// percent-encoding and "the more structured source wins on a name
    /// collision with the URL's own query string" rule an ordinary `query:`
    /// entry gets, rather than a second, parallel implementation. A
    /// `pre_request` script therefore sees a plain `Authorization` (or other)
    /// header like any other, with no separate `request.auth` API — and, for
    /// the `query` form, sees the parameter already merged into
    /// `request.url` by the time `resolve_query` has also run.
    ///
    /// [`Request::validate`] has already rejected a request that sets `auth`
    /// alongside an explicit header or query parameter of the same name it
    /// would itself set, so this always adds the header/parameter rather than
    /// needing [`config::insert_if_absent`](crate::config::insert_if_absent)'s
    /// suppression rule.
    pub fn resolve_auth(&self) -> Result<Request, SendraError> {
        let mut resolved = self.clone();

        if let Some(auth) = &self.auth {
            match (&auth.bearer, &auth.basic, &auth.api_key, &auth.oauth) {
                (Some(token), None, None, None) => {
                    resolved
                        .headers
                        .push(("Authorization".to_string(), format!("Bearer {token}")));
                }
                (None, Some(basic), None, None) => {
                    let credentials = format!("{}:{}", basic.user, basic.pass);
                    let encoded = base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        credentials,
                    );
                    resolved
                        .headers
                        .push(("Authorization".to_string(), format!("Basic {encoded}")));
                }
                (None, None, Some(api_key), None) => match api_key.r#in {
                    ApiKeyLocation::Header => {
                        resolved
                            .headers
                            .push((api_key.name.clone(), api_key.value.clone()));
                    }
                    ApiKeyLocation::Query => {
                        resolved
                            .query
                            .push((api_key.name.clone(), api_key.value.clone()));
                    }
                },
                (None, None, None, Some(_)) => {
                    // `resolve_oauth` collapses `auth.oauth` into `auth.bearer`
                    // before this ever runs — see its doc comment. Reaching
                    // this branch means that step was skipped, which is a
                    // caller bug rather than a fact about the request, but it
                    // still gets a typed error rather than the `unreachable!`
                    // below, since `auth.oauth` is otherwise a value
                    // `Request::validate` accepts.
                    return Err(SendraError::InvalidRequest {
                        reason: "auth.oauth must be resolved via Request::resolve_oauth before \
                                 resolve_auth"
                            .to_string(),
                    });
                }
                // `validate` already rejected any other combination.
                _ => unreachable!(
                    "Request::validate enforces exactly one of bearer/basic/api_key/oauth"
                ),
            };
        }
        resolved.auth = None;

        Ok(resolved)
    }

    /// Acquire an OAuth token for `auth.oauth`, collapsing it into the exact
    /// `bearer` form [`resolve_auth`](Self::resolve_auth) already knows how
    /// to turn into an `Authorization` header — so `oauth` is a *front end*
    /// for the bearer case, not a second header-setting implementation. A
    /// request whose `auth` is `None`, or whose `auth.oauth` is `None`, is
    /// returned unchanged; there is nothing to acquire.
    ///
    /// Called once, before `resolve_auth`, from the request-resolution
    /// pipeline in `sendra-cli` — the one step in that pipeline that needs
    /// the shared [`HttpClient`] and an `.await`, since acquiring a token is
    /// a real HTTP call to `token_url`. See [`crate::oauth`] for the cache
    /// this reads and writes, the retry-vs-fail-fast decision for a broken
    /// config, and the expiry margin.
    pub async fn resolve_oauth(
        &self,
        client: &HttpClient,
        cache: &OAuthTokenCache,
    ) -> Result<Request, SendraError> {
        let Some(oauth) = self.auth.as_ref().and_then(|auth| auth.oauth.as_ref()) else {
            return Ok(self.clone());
        };

        let access_token = crate::oauth::acquire_token(oauth, client, cache).await?;

        let mut resolved = self.clone();
        resolved.auth = Some(Auth {
            bearer: Some(access_token),
            basic: None,
            api_key: None,
            oauth: None,
        });
        Ok(resolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::auth::OAuthGrantType;
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

    // --- auth: api_key -------------------------------------------------------

    #[test]
    fn auth_api_key_header_resolves_to_the_named_header() {
        let request = request_with(
            "auth:\n  api_key:\n    in: header\n    name: X-API-Key\n    value: s3cr3t\n",
        );
        let resolved = request.resolve_auth().expect("resolves");
        assert_eq!(resolved.header("X-API-Key"), Some("s3cr3t"));
        assert!(resolved.auth.is_none());
    }

    #[test]
    fn auth_api_key_query_merges_through_resolve_query_not_a_parallel_path() {
        let request =
            get_with("auth:\n  api_key:\n    in: query\n    name: api_key\n    value: s3cr3t\n");
        let resolved = request
            .resolve_auth()
            .and_then(|request| request.resolve_query())
            .expect("resolves");
        assert_eq!(resolved.url, "https://example.com/search?api_key=s3cr3t");
        assert!(resolved.auth.is_none());
        assert!(resolved.query.is_empty());
    }

    #[test]
    fn auth_api_key_query_still_wins_over_an_existing_url_query_key_of_the_same_name() {
        // Proves the api_key value flows through the exact same "query wins"
        // precedence as an ordinary `query:` entry, not a separate rule.
        let request = Request::from_yaml_str(
            "method: GET\nurl: https://example.com/search?api_key=stale\nauth:\n  api_key:\n    in: query\n    name: api_key\n    value: fresh\n",
        )
        .unwrap();
        let resolved = request
            .resolve_auth()
            .and_then(|request| request.resolve_query())
            .expect("resolves");
        assert_eq!(resolved.url, "https://example.com/search?api_key=fresh");
    }

    #[test]
    fn a_request_with_no_auth_field_resolves_to_no_api_key_header_or_query_param() {
        let request = get_with("");
        let resolved = request
            .resolve_auth()
            .and_then(|request| request.resolve_query())
            .expect("nothing to resolve");
        assert_eq!(resolved.url, "https://example.com/search");
    }

    #[test]
    fn auth_naming_bearer_and_api_key_is_rejected_at_parse_time() {
        let err = Request::from_yaml_str(
            "method: GET\nurl: https://example.com\nauth:\n  bearer: x\n  api_key:\n    in: header\n    name: X-API-Key\n    value: y\n",
        )
        .expect_err("bearer and api_key together must be rejected");
        assert!(
            matches!(&err, SendraError::InvalidRequest { reason } if reason.contains("bearer") && reason.contains("api_key")),
            "got {err:?}"
        );
    }

    #[test]
    fn auth_api_key_header_colliding_with_an_explicit_header_is_rejected_at_parse_time() {
        let err = Request::from_yaml_str(
            "method: GET\nurl: https://example.com\nheaders:\n  X-API-Key: hand-written\nauth:\n  api_key:\n    in: header\n    name: X-API-Key\n    value: y\n",
        )
        .expect_err("api_key and an explicit header of the same name together must be rejected");
        assert!(
            matches!(&err, SendraError::InvalidRequest { reason } if reason.contains("X-API-Key")),
            "got {err:?}"
        );
    }

    #[test]
    fn the_api_key_header_collision_check_is_case_insensitive() {
        let err = Request::from_yaml_str(
            "method: GET\nurl: https://example.com\nheaders:\n  x-api-key: hand-written\nauth:\n  api_key:\n    in: header\n    name: X-API-Key\n    value: y\n",
        )
        .expect_err("a differently-cased header name must still collide");
        assert!(matches!(&err, SendraError::InvalidRequest { .. }));
    }

    #[test]
    fn auth_api_key_query_colliding_with_an_explicit_query_entry_is_rejected_at_parse_time() {
        let err = Request::from_yaml_str(
            "method: GET\nurl: https://example.com\nquery:\n  api_key: hand-written\nauth:\n  api_key:\n    in: query\n    name: api_key\n    value: y\n",
        )
        .expect_err("api_key and an explicit query entry of the same name together must be rejected");
        assert!(
            matches!(&err, SendraError::InvalidRequest { reason } if reason.contains("api_key")),
            "got {err:?}"
        );
    }

    #[test]
    fn environment_substitution_reaches_api_key_name_and_value() {
        let request = request_with(
            "auth:\n  api_key:\n    in: header\n    name: '{{header_name}}'\n    value: '{{token}}'\n",
        );
        let environment =
            crate::Environment::from_yaml_str("header_name: X-API-Key\ntoken: s3cr3t\n").unwrap();
        let substituted = environment.apply(&request).expect("both are set");
        let auth = substituted.auth.expect("auth survives substitution");
        let api_key = auth.api_key.expect("api_key survives substitution");
        assert_eq!(api_key.name, "X-API-Key");
        assert_eq!(api_key.value, "s3cr3t");
    }

    // --- auth: oauth ----------------------------------------------------------

    fn client() -> HttpClient {
        crate::http::client::build_client(&crate::Config::default()).expect("a client builds")
    }

    fn token_server(body: &'static str) -> std::net::SocketAddr {
        crate::test_support::start_route_server(vec![(
            "/token",
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_bytes(),
        )])
    }

    #[tokio::test]
    async fn a_request_with_no_oauth_auth_is_unchanged_by_resolve_oauth() {
        let request = request_with("auth:\n  bearer: unrelated\n");
        let client = client();
        let cache = OAuthTokenCache::new();
        let resolved = request
            .resolve_oauth(&client, &cache)
            .await
            .expect("nothing to acquire");
        assert_eq!(resolved, request);

        let no_auth = request_with("");
        let resolved = no_auth
            .resolve_oauth(&client, &cache)
            .await
            .expect("nothing to acquire");
        assert_eq!(resolved, no_auth);
    }

    #[tokio::test]
    async fn auth_oauth_resolves_via_resolve_oauth_then_resolve_auth_to_a_bearer_header() {
        let addr = token_server(r#"{"access_token": "acquired-token"}"#);
        let request = request_with(&format!(
            "auth:\n  oauth:\n    grant_type: client_credentials\n    token_url: http://{addr}/token\n    client_id: id\n    client_secret: secret\n"
        ));
        let client = client();
        let cache = OAuthTokenCache::new();

        let resolved = request
            .resolve_oauth(&client, &cache)
            .await
            .expect("the mock token endpoint answers");
        assert_eq!(
            resolved
                .auth
                .as_ref()
                .and_then(|auth| auth.bearer.as_deref()),
            Some("acquired-token"),
            "resolve_oauth must collapse auth.oauth into auth.bearer"
        );

        let resolved = resolved.resolve_auth().expect("resolves");
        assert_eq!(
            resolved.header("Authorization"),
            Some("Bearer acquired-token")
        );
        assert!(resolved.auth.is_none());
    }

    #[test]
    fn calling_resolve_auth_directly_on_unresolved_oauth_is_a_typed_error_not_a_panic() {
        let request = request_with(
            "auth:\n  oauth:\n    grant_type: client_credentials\n    token_url: http://example.com/token\n    client_id: id\n    client_secret: secret\n",
        );
        let err = request
            .resolve_auth()
            .expect_err("oauth must be resolved via resolve_oauth first");
        assert!(matches!(err, SendraError::InvalidRequest { .. }));
    }

    #[test]
    fn oauth_password_grant_missing_username_is_rejected_at_parse_time() {
        let err = Request::from_yaml_str(
            "method: GET\nurl: https://example.com\nauth:\n  oauth:\n    grant_type: password\n    token_url: https://example.com/token\n    client_id: id\n    client_secret: secret\n    password: pw\n",
        )
        .expect_err("password grant needs username too");
        assert!(
            matches!(&err, SendraError::InvalidRequest { reason } if reason.contains("username") && reason.contains("password")),
            "got {err:?}"
        );
    }

    #[test]
    fn oauth_client_credentials_needs_neither_username_nor_password() {
        let request = Request::from_yaml_str(
            "method: GET\nurl: https://example.com\nauth:\n  oauth:\n    grant_type: client_credentials\n    token_url: https://example.com/token\n    client_id: id\n    client_secret: secret\n",
        )
        .expect("client_credentials needs no username/password");
        let oauth = request
            .auth
            .expect("auth survives parse")
            .oauth
            .expect("oauth is set");
        assert_eq!(oauth.grant_type, OAuthGrantType::ClientCredentials);
    }

    #[test]
    fn auth_naming_oauth_and_bearer_together_is_rejected_at_parse_time() {
        let err = Request::from_yaml_str(
            "method: GET\nurl: https://example.com\nauth:\n  bearer: x\n  oauth:\n    grant_type: client_credentials\n    token_url: https://example.com/token\n    client_id: id\n    client_secret: secret\n",
        )
        .expect_err("bearer and oauth together must be rejected");
        assert!(
            matches!(&err, SendraError::InvalidRequest { reason } if reason.contains("bearer") && reason.contains("oauth")),
            "got {err:?}"
        );
    }

    #[test]
    fn auth_oauth_alongside_an_explicit_authorization_header_is_rejected_at_parse_time() {
        let err = Request::from_yaml_str(
            "method: GET\nurl: https://example.com\nheaders:\n  Authorization: Bearer hand-written\nauth:\n  oauth:\n    grant_type: client_credentials\n    token_url: https://example.com/token\n    client_id: id\n    client_secret: secret\n",
        )
        .expect_err("auth.oauth and an explicit Authorization header together must be rejected");
        assert!(
            matches!(&err, SendraError::InvalidRequest { reason } if reason.contains("Authorization"))
        );
    }

    #[test]
    fn environment_substitution_reaches_oauth_fields() {
        let request = request_with(
            "auth:\n  oauth:\n    grant_type: password\n    token_url: '{{token_url}}'\n    client_id: '{{client_id}}'\n    client_secret: '{{client_secret}}'\n    username: '{{username}}'\n    password: '{{password}}'\n    scope: '{{scope}}'\n",
        );
        let environment = crate::Environment::from_yaml_str(
            "token_url: https://auth.example.com/token\nclient_id: id-value\nclient_secret: secret-value\nusername: ada\npassword: s3cr3t\nscope: read write\n",
        )
        .unwrap();
        let substituted = environment.apply(&request).expect("every variable is set");
        let oauth = substituted
            .auth
            .expect("auth survives substitution")
            .oauth
            .expect("oauth survives substitution");
        assert_eq!(oauth.token_url, "https://auth.example.com/token");
        assert_eq!(oauth.client_id, "id-value");
        assert_eq!(oauth.client_secret, "secret-value");
        assert_eq!(oauth.username.as_deref(), Some("ada"));
        assert_eq!(oauth.password.as_deref(), Some("s3cr3t"));
        assert_eq!(oauth.scope.as_deref(), Some("read write"));
    }
}
