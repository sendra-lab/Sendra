//! The recursive walk over the parsed domain model — [`Request`], its
//! [`Assertions`], [`Collection`] and [`Document`] — that puts an
//! [`Environment`]'s variables into place. See the module doc on
//! [`environment`](super) for why this happens post-parse rather than as a
//! find-and-replace over raw file text, and for the fields this walk
//! deliberately leaves untouched.

use std::collections::BTreeMap;

use crate::assertions::{Assertions, NotAssertions};
use crate::{
    ApiKeyAuth, Auth, BasicAuth, Collection, Document, MultipartPart, OAuthAuth, Request,
    SendraError,
};

use super::Environment;

/// Delimiters for a `{{variable}}` reference in a request file.
const TEMPLATE_OPEN: &str = "{{";
const TEMPLATE_CLOSE: &str = "}}";

impl Environment {
    /// Substitute this environment into `request`, returning the request as it
    /// will be sent.
    ///
    /// Every `{{name}}` in `url`, in each header name, in each header value, in
    /// `body` and in the *values* of the `assertions` block is replaced. An
    /// unknown name is [`VariableNotFound`](SendraError::VariableNotFound), and
    /// a value whose `${VAR}` is not in the OS environment is
    /// [`EnvVarNotSet`](SendraError::EnvVarNotSet) — never an empty string, and
    /// never a half-substituted request. The whole request is built before it is
    /// sent, so both failures land before any of its bytes go out.
    ///
    /// Headers are substituted in place, entry by entry, so order is preserved
    /// exactly as written in the file. Two header names that were distinct in
    /// the file can collide once substituted (`{{prefix}}-Key` and `X-Key`,
    /// say) — that used to be an error back when headers were a map and a
    /// silent collision would have dropped a value, but `Request.headers` is a
    /// `Vec` that allows a name to repeat, so a post-substitution collision is
    /// now exactly that: two headers of the same name, sent as written.
    ///
    /// Assertions are substituted for the same reason the rest of the file is:
    /// what a staging response should say is exactly as environment-dependent
    /// as what the request asks for, and `body_contains: '{{tenant}}'` would
    /// otherwise compare against the literal braces. See
    /// [`apply_assertions`](Self::apply_assertions) for the one line it draws.
    pub fn apply(&self, request: &Request) -> Result<Request, SendraError> {
        let mut headers = Vec::with_capacity(request.headers.len());
        for (name, value) in &request.headers {
            let name = self.expand_templates(name)?;
            let value = self.expand_templates(value)?;
            headers.push((name, value));
        }

        // `{{var}}` reaches query values (and list entries within them) the
        // same way it reaches header values above — consistent with every
        // other value field. Computed as a local, not inline in the struct
        // literal below, because the `auth` field below needs it too — an
        // environment-level default `auth.api_key` in `query` form has to be
        // checked for a collision against these same substituted names.
        let query: Vec<(String, String)> = request
            .query
            .iter()
            .map(|(name, value)| Ok((self.expand_templates(name)?, self.expand_templates(value)?)))
            .collect::<Result<_, SendraError>>()?;

        // `auth` precedence: a request's own `auth:` fully replaces this
        // environment's default — never merged — so the environment's
        // `auth` is only even substituted when the request set none of its
        // own. Either way `{{var}}` reaches every field (`bearer`, `basic`'s
        // `user`/`pass`, `api_key`'s `name`/`value`) the same way it reaches
        // every other value field.
        let auth = match &request.auth {
            Some(auth) => Some(self.substitute_auth(auth)?),
            None => match &self.auth {
                Some(auth) => {
                    let auth = self.substitute_auth(auth)?;
                    // `Request::validate` already checked a request's own
                    // `auth:` against its own headers/query at parse time;
                    // an environment's default cannot be checked until now,
                    // once it is known which request (and its
                    // already-substituted headers/query) it is being
                    // applied to.
                    if let Some(reason) = auth.collision_reason(&headers, &query) {
                        return Err(SendraError::InvalidRequest { reason });
                    }
                    Some(auth)
                }
                None => None,
            },
        };

        Ok(Request {
            // `name` is left alone: it is what `sendra run <file> <name>`
            // selects on, and a label that changed with the environment could
            // not be typed on the command line.
            name: request.name.clone(),
            method: request.method,
            url: self.expand_templates(&request.url)?,
            headers,
            query,
            body: request
                .body
                .as_deref()
                .map(|body| self.expand_templates(body))
                .transpose()?,
            // `{{var}}` reaches every string value here, the same way it
            // reaches `body` above — a JSON body wanting a substituted field
            // is exactly as ordinary a case as a substituted plain body.
            // `expand_json` already exists for `assertions.json`; the rule is
            // the same, values only, nothing about keys.
            json: request
                .json
                .as_ref()
                .map(|value| self.expand_json(value))
                .transpose()?,
            // The path itself is a value like any other and is substituted;
            // what it points to is not. See `Request::resolve_body`, which is
            // where that file is actually read, well after this runs.
            body_file: request
                .body_file
                .as_deref()
                .map(|path| self.expand_templates(path))
                .transpose()?,
            form: request
                .form
                .iter()
                .map(|(name, value)| {
                    Ok((self.expand_templates(name)?, self.expand_templates(value)?))
                })
                .collect::<Result<_, SendraError>>()?,
            multipart: request
                .multipart
                .iter()
                .map(|part| {
                    Ok(MultipartPart {
                        name: self.expand_templates(&part.name)?,
                        value: part
                            .value
                            .as_deref()
                            .map(|value| self.expand_templates(value))
                            .transpose()?,
                        // Same rule as `body_file`: the path is a value and is
                        // substituted, the file it names is not.
                        path: part
                            .path
                            .as_deref()
                            .map(|path| self.expand_templates(path))
                            .transpose()?,
                    })
                })
                .collect::<Result<_, SendraError>>()?,
            auth,
            assertions: request
                .assertions
                .as_ref()
                .map(|assertions| self.apply_assertions(assertions))
                .transpose()?,
            // **Script source is not substituted**, and this is the line that
            // says so. A `{{var}}` inside a script stays those five characters.
            //
            // Substitution is textual, and the reason it is confined to values
            // is that a value must never be able to change the structure of the
            // document around it. A script is not a value, it is *code*: the
            // failure mode is not a malformed URL but a variable's contents
            // being parsed as program text, which is the same problem one level
            // worse. A script that needs an environment value reads it off the
            // request it is handed, which arrives fully substituted by the time
            // it runs.
            pre_request: request.pre_request.clone(),
            post_request: request.post_request.clone(),
            // **The `capture` block is not substituted either**, and for the
            // rule `apply_assertions` already draws rather than the one above.
            // A capture's value is a JSON path, which selects *which part of
            // the response is being looked at* — exactly the role a JSON path
            // plays in an assertion, where keys are left literal so `--env`
            // cannot silently redirect a check onto a different field. Its key
            // is a variable name, and a name that changed with the environment
            // could not be written as `{{name}}` in the request that uses it,
            // for the same reason a request's `name` is left alone.
            capture: request.capture.clone(),
            // Not substituted, for the same reason `capture` is not: `count`
            // and `delay_ms` are plain numbers, not `{{var}}`-bearing string
            // fields, so there is nothing here for this pass to expand.
            retry: request.retry,
        })
    }

    /// [`Environment::apply`] for an `assertions` block.
    ///
    /// **Values are substituted; keys are not.** A header name and a JSON path
    /// select *what part of the response is being looked at*, and an object key
    /// inside an expected value names a field the same way. An environment is
    /// meant to change what a response is compared against — a tenant, an id, a
    /// host — not to change which field is inspected, and a run where `--env`
    /// silently redirected an assertion onto a different header would be very
    /// hard to read back. Keeping keys literal also means substitution here can
    /// never collapse two entries into one, so no assertion can go missing on
    /// the way to being checked.
    ///
    /// `status`, `status_in` and `elapsed_ms_under` are numbers and have
    /// nothing to substitute.
    fn apply_assertions(&self, assertions: &Assertions) -> Result<Assertions, SendraError> {
        Ok(Assertions {
            status: assertions.status,
            status_in: assertions.status_in.clone(),
            headers: self.apply_assertion_headers(&assertions.headers)?,
            body_contains: assertions
                .body_contains
                .as_deref()
                .map(|body| self.expand_templates(body))
                .transpose()?,
            body_matches: assertions
                .body_matches
                .as_deref()
                .map(|pattern| self.expand_templates(pattern))
                .transpose()?,
            elapsed_ms_under: assertions.elapsed_ms_under,
            json: self.apply_assertion_json(&assertions.json)?,
            not: assertions
                .not
                .as_ref()
                .map(|not| self.apply_not_assertions(not))
                .transpose()?,
        })
    }

    /// [`apply_assertions`](Self::apply_assertions) for the `not:` block,
    /// which carries the same substitutable fields.
    fn apply_not_assertions(&self, not: &NotAssertions) -> Result<NotAssertions, SendraError> {
        Ok(NotAssertions {
            status: not.status,
            status_in: not.status_in.clone(),
            headers: self.apply_assertion_headers(&not.headers)?,
            body_contains: not
                .body_contains
                .as_deref()
                .map(|body| self.expand_templates(body))
                .transpose()?,
            body_matches: not
                .body_matches
                .as_deref()
                .map(|pattern| self.expand_templates(pattern))
                .transpose()?,
            elapsed_ms_under: not.elapsed_ms_under,
            json: self.apply_assertion_json(&not.json)?,
        })
    }

    fn apply_assertion_headers(
        &self,
        headers: &BTreeMap<String, Option<String>>,
    ) -> Result<BTreeMap<String, Option<String>>, SendraError> {
        let mut expanded = BTreeMap::new();
        for (name, expected) in headers {
            let expected = expected
                .as_deref()
                .map(|value| self.expand_templates(value))
                .transpose()?;
            expanded.insert(name.clone(), expected);
        }
        Ok(expanded)
    }

    fn apply_assertion_json(
        &self,
        json: &BTreeMap<String, serde_json::Value>,
    ) -> Result<BTreeMap<String, serde_json::Value>, SendraError> {
        let mut expanded = BTreeMap::new();
        for (path, expected) in json {
            expanded.insert(path.clone(), self.expand_json(expected)?);
        }
        Ok(expanded)
    }

    /// Substitute into every string *value* of an expected JSON value,
    /// including those nested in arrays and objects. Object keys are left alone,
    /// per the rule on [`apply_assertions`](Self::apply_assertions); numbers,
    /// booleans and null have no text to expand.
    fn expand_json(&self, value: &serde_json::Value) -> Result<serde_json::Value, SendraError> {
        use serde_json::Value;
        Ok(match value {
            Value::String(text) => Value::String(self.expand_templates(text)?),
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|item| self.expand_json(item))
                    .collect::<Result<_, _>>()?,
            ),
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(key, value)| Ok((key.clone(), self.expand_json(value)?)))
                    .collect::<Result<_, SendraError>>()?,
            ),
            other => other.clone(),
        })
    }

    /// [`Environment::apply`] for every request in a collection, in file order.
    ///
    /// All or nothing: one request that cannot be substituted fails the whole
    /// call. That suits a caller that wants a fully-resolved collection in hand,
    /// which is what this returns. It is *not* what `sendra run` does with a
    /// collection — there each request is substituted as it is reached, so a
    /// broken one fails on its own and the requests around it are still sent, in
    /// the same way a refused connection does not cancel its siblings. A caller
    /// wanting those per-request outcomes calls [`Environment::apply`] in a loop
    /// and keeps each `Result`.
    ///
    /// The collection's own `name` is left alone for the same reason a
    /// request's is.
    pub fn apply_collection(&self, collection: &Collection) -> Result<Collection, SendraError> {
        Ok(Collection {
            name: collection.name.clone(),
            requests: collection
                .requests
                .iter()
                .map(|request| self.apply(request))
                .collect::<Result<_, _>>()?,
        })
    }

    /// [`Environment::apply`] over whichever shape a file turned out to hold.
    pub fn apply_document(&self, document: &Document) -> Result<Document, SendraError> {
        Ok(match document {
            Document::Single(request) => Document::Single(self.apply(request)?),
            Document::Collection(collection) => {
                Document::Collection(self.apply_collection(collection)?)
            }
        })
    }

    /// Replace every `{{name}}` in `text`.
    fn expand_templates(&self, text: &str) -> Result<String, SendraError> {
        super::expand(text, TEMPLATE_OPEN, TEMPLATE_CLOSE, |name| {
            self.lookup(name)
        })
    }

    /// `{{var}}` reaches a bearer token, basic user/pass, or an api_key's
    /// name/value the same way it reaches every other value field — see the
    /// note on `Request::auth`. Shared between substituting a request's own
    /// `auth:` and this environment's default `auth:` ([`apply`](Self::apply)),
    /// since both are the exact same [`Auth`] shape substituted against the
    /// exact same environment.
    fn substitute_auth(&self, auth: &Auth) -> Result<Auth, SendraError> {
        Ok(Auth {
            bearer: auth
                .bearer
                .as_deref()
                .map(|token| self.expand_templates(token))
                .transpose()?,
            basic: auth
                .basic
                .as_ref()
                .map(|basic| -> Result<BasicAuth, SendraError> {
                    Ok(BasicAuth {
                        user: self.expand_templates(&basic.user)?,
                        pass: self.expand_templates(&basic.pass)?,
                    })
                })
                .transpose()?,
            api_key: auth
                .api_key
                .as_ref()
                .map(|api_key| -> Result<ApiKeyAuth, SendraError> {
                    Ok(ApiKeyAuth {
                        r#in: api_key.r#in,
                        name: self.expand_templates(&api_key.name)?,
                        value: self.expand_templates(&api_key.value)?,
                    })
                })
                .transpose()?,
            oauth: auth
                .oauth
                .as_ref()
                .map(|oauth| -> Result<OAuthAuth, SendraError> {
                    Ok(OAuthAuth {
                        grant_type: oauth.grant_type,
                        token_url: self.expand_templates(&oauth.token_url)?,
                        client_id: self.expand_templates(&oauth.client_id)?,
                        client_secret: self.expand_templates(&oauth.client_secret)?,
                        scope: oauth
                            .scope
                            .as_deref()
                            .map(|value| self.expand_templates(value))
                            .transpose()?,
                        username: oauth
                            .username
                            .as_deref()
                            .map(|value| self.expand_templates(value))
                            .transpose()?,
                        password: oauth
                            .password
                            .as_deref()
                            .map(|value| self.expand_templates(value))
                            .transpose()?,
                    })
                })
                .transpose()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::environment;
    use super::*;

    use crate::Method;

    /// A request touching all three substitutable places at once.
    const TEMPLATED: &str = "\
name: Templated
method: POST
url: '{{base_url}}/users/{{user_id}}'
headers:
  Authorization: 'Bearer {{api_key}}'
  '{{header_name}}': fixed-value
body: '{\"host\": \"{{base_url}}\"}'
";

    #[test]
    fn substitutes_url_headers_and_body() {
        let request = Request::from_yaml_str(TEMPLATED).unwrap();
        let environment = environment(
            &[
                ("base_url", "https://staging.example.com"),
                ("user_id", "42"),
                ("api_key", "s3cret"),
                ("header_name", "X-Tenant"),
            ],
            &[],
        );

        let applied = environment.apply(&request).expect("every variable is set");

        assert_eq!(applied.url, "https://staging.example.com/users/42");
        assert_eq!(applied.header("Authorization"), Some("Bearer s3cret"));
        // Header *names* are substituted too, not just values.
        assert_eq!(applied.header("X-Tenant"), Some("fixed-value"));
        assert_eq!(
            applied.body.as_deref(),
            Some("{\"host\": \"https://staging.example.com\"}")
        );
        // The label is deliberately untouched: it is the run selector.
        assert_eq!(applied.name.as_deref(), Some("Templated"));
        assert_eq!(applied.method, Method::Post);
    }

    #[test]
    fn substitution_reaches_json_form_and_multipart_values_but_not_a_body_files_content() {
        let yaml = "\
method: POST
url: https://example.com
";
        // Three requests, one per structured body field, each with a
        // `{{tenant}}` inside a *value*: a JSON/form/multipart body wanting a
        // substituted field is exactly as ordinary as a substituted plain
        // `body`.
        let json_request = Request::from_yaml_str(&format!(
            "{yaml}json:\n  tenant: '{{{{tenant}}}}'\n  nested:\n    id: '{{{{tenant}}}}'\n"
        ))
        .unwrap();
        let form_request =
            Request::from_yaml_str(&format!("{yaml}form:\n  tenant: '{{{{tenant}}}}'\n")).unwrap();
        let multipart_request = Request::from_yaml_str(&format!(
            "{yaml}multipart:\n  - name: '{{{{tenant}}}}'\n    value: '{{{{tenant}}}}'\n"
        ))
        .unwrap();

        let environment = environment(&[("tenant", "acme")], &[]);

        let applied_json = environment.apply(&json_request).expect("tenant is set");
        assert_eq!(
            applied_json.json,
            Some(serde_json::json!({"tenant": "acme", "nested": {"id": "acme"}}))
        );

        let applied_form = environment.apply(&form_request).expect("tenant is set");
        assert_eq!(
            applied_form.form,
            vec![("tenant".to_string(), "acme".to_string())]
        );

        let applied_multipart = environment
            .apply(&multipart_request)
            .expect("tenant is set");
        assert_eq!(applied_multipart.multipart[0].name, "acme");
        assert_eq!(
            applied_multipart.multipart[0].value.as_deref(),
            Some("acme")
        );

        // `body_file`'s *path* is a value like any other and is substituted...
        let body_file_request =
            Request::from_yaml_str(&format!("{yaml}body_file: './{{{{tenant}}}}.json'\n")).unwrap();
        let applied_body_file = environment
            .apply(&body_file_request)
            .expect("tenant is set");
        assert_eq!(applied_body_file.body_file.as_deref(), Some("./acme.json"));

        // ...but what a `body_file` or multipart file *path points to* is
        // never read here at all — substitution is a pass over the parsed
        // document, and a file on disk is not part of it. A placeholder
        // inside the file's actual content survives untouched all the way to
        // `Request::resolve_body`, which reads the file only after this runs.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("acme.json"), "{{tenant}}").unwrap();
        let resolved = applied_body_file
            .resolve_body(dir.path())
            .expect("the file is there");
        assert_eq!(
            resolved.body.as_deref(),
            Some("{{tenant}}"),
            "a placeholder inside the file's content must not be substituted"
        );
    }

    #[test]
    fn substitution_preserves_header_order_including_a_repeated_name() {
        let yaml = "\
method: GET
url: https://example.com
headers:
  Accept: application/json
  X-Forwarded-For:
    - '{{first}}'
    - '{{second}}'
  X-Tenant: '{{tenant}}'
";
        let request = Request::from_yaml_str(yaml).unwrap();
        let environment = environment(
            &[
                ("first", "1.2.3.4"),
                ("second", "5.6.7.8"),
                ("tenant", "acme"),
            ],
            &[],
        );

        let applied = environment.apply(&request).expect("every variable is set");

        assert_eq!(
            applied.headers,
            vec![
                ("Accept".to_string(), "application/json".to_string()),
                ("X-Forwarded-For".to_string(), "1.2.3.4".to_string()),
                ("X-Forwarded-For".to_string(), "5.6.7.8".to_string()),
                ("X-Tenant".to_string(), "acme".to_string()),
            ],
            "order among distinct names and among a repeated name must both survive"
        );
    }

    /// A request whose every assertion carries a placeholder, in each of the
    /// three places one can appear.
    const TEMPLATED_ASSERTIONS: &str = "\
method: GET
url: '{{base_url}}/users/{{user_id}}'
assertions:
  status: 200
  headers:
    x-tenant: '{{tenant}}'
    content-type:
  body_contains: '{{tenant}}'
  json:
    $.id: '{{user_id}}'
    $.nested:
      tenant: '{{tenant}}'
      tags: ['{{tenant}}', literal]
";

    #[test]
    fn substitutes_the_values_in_an_assertions_block() {
        let request = Request::from_yaml_str(TEMPLATED_ASSERTIONS).unwrap();
        let environment = environment(
            &[
                ("base_url", "https://staging.example.com"),
                ("user_id", "42"),
                ("tenant", "acme"),
            ],
            &[],
        );

        let assertions = environment
            .apply(&request)
            .expect("every variable is set")
            .assertions
            .expect("the block survives substitution");

        assert_eq!(assertions.status, Some(200));
        assert_eq!(
            assertions.headers.get("x-tenant"),
            Some(&Some("acme".to_string()))
        );
        // A presence-only assertion has no value to substitute and stays one.
        assert_eq!(assertions.headers.get("content-type"), Some(&None));
        assert_eq!(assertions.body_contains.as_deref(), Some("acme"));
        // Including strings nested inside an expected object or array.
        assert_eq!(assertions.json["$.id"], serde_json::json!("42"));
        assert_eq!(
            assertions.json["$.nested"],
            serde_json::json!({"tenant": "acme", "tags": ["acme", "literal"]})
        );
    }

    #[test]
    fn script_source_is_not_substituted() {
        // The other line, one step further out than `assertion_keys_are_not_
        // substituted`: an environment changes *values*, and a script is not a
        // value, it is code. `{{secret}}` and `${SECRET}` inside a script are
        // five and nine characters of Rhai source, not a placeholder.
        //
        // The reasoning is the one that made substitution value-only in the
        // first place, one level worse: a value that could rewrite the document
        // around it is a bug, and a value that could rewrite a *program* is the
        // same bug where the document is executable. A script that needs an
        // environment value reads it off the request it is handed, which by
        // then has been fully substituted.
        let request = Request::from_yaml_str(
            "\
method: GET
url: 'https://example.com/{{tenant}}'
pre_request: |
  request.headers[\"X-Tenant\"] = \"{{tenant}}\";
  request.headers[\"X-Secret\"] = \"${SECRET}\";
post_request: |
  if response.body != \"{{tenant}}\" { throw \"{{tenant}}\"; }
",
        )
        .unwrap();
        let environment = environment(&[("tenant", "acme")], &[]);

        let applied = environment.apply(&request).unwrap();

        // The url was substituted, so the environment is live and this is not
        // passing by accident.
        assert_eq!(applied.url, "https://example.com/acme");

        // Both scripts came through byte for byte.
        assert_eq!(applied.pre_request, request.pre_request);
        assert_eq!(applied.post_request, request.post_request);

        // `${SECRET}` is the case that would be loudest if this ever changed:
        // nothing exports it, so a substituted script would have failed the
        // whole request with `EnvVarNotSet` rather than quietly meaning
        // something else.
        assert!(applied
            .pre_request
            .as_deref()
            .unwrap()
            .contains("${SECRET}"));
        assert!(applied
            .pre_request
            .as_deref()
            .unwrap()
            .contains("{{tenant}}"));
    }

    #[test]
    fn assertion_keys_are_not_substituted() {
        // The line drawn in `apply_assertions`: an environment changes what a
        // response is compared against, never which part of it is inspected. A
        // header name, a JSON path, and a key inside an expected object all
        // stay exactly as written — including one that looks like a
        // placeholder, which is then simply text that never matches.
        let request = Request::from_yaml_str(
            "\
method: GET
url: https://example.com
assertions:
  headers:
    '{{header_name}}': fixed
  json:
    '$.{{field}}': 1
    $.obj:
      '{{key}}': 2
",
        )
        .unwrap();
        let environment = environment(
            &[("header_name", "X-Tenant"), ("field", "id"), ("key", "k")],
            &[],
        );

        let assertions = environment.apply(&request).unwrap().assertions.unwrap();

        assert!(assertions.headers.contains_key("{{header_name}}"));
        assert!(assertions.json.contains_key("$.{{field}}"));
        assert_eq!(assertions.json["$.obj"], serde_json::json!({"{{key}}": 2}));
    }

    #[test]
    fn a_missing_variable_in_an_assertion_fails_the_request_like_any_other() {
        // Assertions are substituted on the way to the wire, so an unresolvable
        // one is the same failure as an unresolvable URL: the request is never
        // sent, rather than being sent and then checked against `{{nope}}`.
        let request = Request::from_yaml_str(
            "method: GET\nurl: https://example.com\nassertions:\n  body_contains: '{{nope}}'\n",
        )
        .unwrap();

        let err = environment(&[("tenant", "acme")], &[])
            .apply(&request)
            .expect_err("`nope` is not defined");

        assert!(
            matches!(err, SendraError::VariableNotFound { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn a_request_with_no_placeholders_is_unchanged() {
        // Substitution has to be a no-op for every file written before this
        // feature existed.
        let request =
            Request::from_yaml_str("method: GET\nurl: https://example.com/a\nbody: 'plain'\n")
                .unwrap();
        let applied = Environment::default().apply(&request).unwrap();
        assert_eq!(applied, request);
    }

    #[test]
    fn substitution_works_inside_a_collection() {
        let yaml = "\
name: Example API
requests:
  - name: List users
    method: GET
    url: '{{base_url}}/users'
  - name: Create user
    method: POST
    url: '{{base_url}}/users'
    headers:
      Authorization: 'Bearer {{api_key}}'
    body: '{\"name\": \"ada\"}'
";
        let document = Document::from_yaml_str(yaml).unwrap();
        let environment = environment(
            &[("base_url", "https://staging.example.com")],
            &[("API_KEY", "s3cret")],
        );
        // `api_key` comes from the OS environment, through the file.
        let environment = Environment {
            variables: {
                let mut variables = environment.variables.clone();
                variables.insert("api_key".to_string(), "${API_KEY}".to_string());
                variables
            },
            ..environment
        };

        let Document::Collection(applied) = environment.apply_document(&document).unwrap() else {
            panic!("a collection must stay a collection");
        };

        // File order, and the collection's own name, survive the pass.
        assert_eq!(applied.name.as_deref(), Some("Example API"));
        assert_eq!(applied.names(), vec!["List users", "Create user"]);
        assert_eq!(applied.requests[0].url, "https://staging.example.com/users");
        assert_eq!(applied.requests[1].url, "https://staging.example.com/users");
        assert_eq!(
            applied.requests[1].header("Authorization"),
            Some("Bearer s3cret")
        );
        // Untemplated fields are carried through untouched.
        assert_eq!(
            applied.requests[1].body.as_deref(),
            Some("{\"name\": \"ada\"}")
        );
    }

    #[test]
    fn applying_to_a_whole_document_is_all_or_nothing() {
        // `apply_document` substitutes a collection as a unit, so one bad
        // variable fails the lot. That is this function's contract, not the
        // CLI's behaviour: `sendra run` substitutes each request as it reaches
        // it, so a broken request there fails alone and its siblings are still
        // sent. Anything wanting per-request outcomes calls `apply` in a loop.
        let yaml = "\
requests:
  - name: Fine
    method: GET
    url: '{{base_url}}/a'
  - name: Broken
    method: GET
    url: '{{missing}}/b'
";
        let document = Document::from_yaml_str(yaml).unwrap();
        let environment = environment(&[("base_url", "https://example.com")], &[]);

        let err = environment
            .apply_document(&document)
            .expect_err("the second request references nothing");
        assert!(
            matches!(&err, SendraError::VariableNotFound { name, .. } if name == "missing"),
            "got {err:?}"
        );
    }

    #[test]
    fn a_value_is_not_rescanned_for_placeholders() {
        // Single pass by design: a value that happens to contain `{{...}}` is
        // data, not a further reference to resolve.
        let request = Request::from_yaml_str("method: GET\nurl: '{{a}}'\n").unwrap();
        let environment = environment(&[("a", "literal-{{b}}"), ("b", "never-used")], &[]);

        let applied = environment.apply(&request).unwrap();
        assert_eq!(applied.url, "literal-{{b}}");
    }

    #[test]
    fn whitespace_inside_a_placeholder_is_ignored() {
        let request = Request::from_yaml_str("method: GET\nurl: '{{  base_url  }}/x'\n").unwrap();
        let environment = environment(&[("base_url", "https://example.com")], &[]);
        assert_eq!(
            environment.apply(&request).unwrap().url,
            "https://example.com/x"
        );
    }

    #[test]
    fn text_that_only_looks_like_a_placeholder_is_left_alone() {
        // An unterminated `{{`, and an empty `{{}}`: both much likelier to be
        // ordinary text (a JSON body, a templating language) than a typo, so
        // neither is an error.
        for url in ["https://example.com/{{unclosed", "https://example.com/{{}}"] {
            let request = Request::from_yaml_str(&format!("method: GET\nurl: '{url}'\n")).unwrap();
            let applied = Environment::default()
                .apply(&request)
                .unwrap_or_else(|e| panic!("{url} should not error: {e}"));
            assert_eq!(applied.url, url);
        }
    }

    #[test]
    fn two_header_names_resolving_to_the_same_name_after_substitution_keeps_both() {
        // Back when `Request.headers` was a map, two names colliding after
        // substitution would silently drop one value, so this used to be a
        // reported error. Now that a name is allowed to repeat, a
        // post-substitution collision is just that: two headers under the
        // same name, both sent — nothing is lost, so there is nothing to
        // report.
        let yaml = "\
method: GET
url: https://example.com
headers:
  '{{name}}': from-template
  X-Key: from-literal
";
        let request = Request::from_yaml_str(yaml).unwrap();
        let environment = environment(&[("name", "X-Key")], &[]);

        let applied = environment
            .apply(&request)
            .expect("a post-substitution collision is legal, not an error");
        assert_eq!(
            applied.headers,
            vec![
                ("X-Key".to_string(), "from-template".to_string()),
                ("X-Key".to_string(), "from-literal".to_string()),
            ]
        );
    }

    #[test]
    fn the_capture_block_is_carried_through_substitution_untouched() {
        // Same rule as an assertion's JSON path keys and a script's source: a
        // path selects *which* part of the response is read, and `--env` must
        // not be able to redirect it.
        let request = Request::from_yaml_str(
            "method: GET
url: '{{base_url}}'
capture:
  token: '$.{{field}}'
",
        )
        .unwrap();
        let environment = environment(&[("base_url", "https://example.com")], &[]);

        let applied = environment
            .apply(&request)
            .expect("`{{field}}` is inside the capture block, which is not substituted");
        assert_eq!(
            applied.capture, request.capture,
            "the block goes through verbatim"
        );
        assert_eq!(
            applied.capture.as_ref().unwrap().entries()["token"],
            crate::CaptureSource::JsonPath("$.{{field}}".to_string())
        );
    }

    // --- environment-level default `auth:` -----------------------------------

    #[test]
    fn environment_level_auth_applies_when_the_request_sets_none_of_its_own() {
        let environment = Environment::from_yaml_str(
            "base_url: https://example.com\nauth:\n  bearer: env-token\n",
        )
        .unwrap();
        let request = Request::from_yaml_str("method: GET\nurl: '{{base_url}}'\n").unwrap();

        let applied = environment.apply(&request).expect("resolves");
        let auth = applied.auth.expect("the environment default was filled in");
        assert_eq!(auth.bearer.as_deref(), Some("env-token"));
    }

    #[test]
    fn a_requests_own_auth_fully_replaces_the_environments_default_not_merges() {
        let environment = Environment::from_yaml_str("auth:\n  bearer: env-token\n").unwrap();
        let request = Request::from_yaml_str(
            "method: GET\nurl: https://example.com\nauth:\n  basic:\n    user: a\n    pass: b\n",
        )
        .unwrap();

        let applied = environment.apply(&request).expect("resolves");
        let auth = applied.auth.expect("the request's own auth survives");
        // The request's own `basic` wins outright — not merged with the
        // environment's `bearer` into some combination of both.
        assert!(auth.bearer.is_none());
        assert_eq!(auth.basic.map(|basic| basic.user), Some("a".to_string()));
    }

    #[test]
    fn environment_level_auth_substitutes_against_its_own_environments_variables() {
        let environment =
            Environment::from_yaml_str("token: s3cret\nauth:\n  bearer: '{{token}}'\n").unwrap();
        let request = Request::from_yaml_str("method: GET\nurl: https://example.com\n").unwrap();

        let applied = environment.apply(&request).expect("`token` resolves");
        assert_eq!(
            applied.auth.and_then(|auth| auth.bearer),
            Some("s3cret".to_string())
        );
    }

    #[test]
    fn environment_level_auth_header_colliding_with_an_explicit_header_is_rejected() {
        let environment = Environment::from_yaml_str("auth:\n  bearer: env-token\n").unwrap();
        let request = Request::from_yaml_str(
            "method: GET\nurl: https://example.com\nheaders:\n  Authorization: hand-written\n",
        )
        .unwrap();

        let err = environment
            .apply(&request)
            .expect_err("the environment default would collide with the explicit header");
        assert!(
            matches!(&err, SendraError::InvalidRequest { reason } if reason.contains("Authorization")),
            "got {err:?}"
        );
    }

    #[test]
    fn environment_level_api_key_in_query_form_resolves_end_to_end() {
        let environment = Environment::from_yaml_str(
            "auth:\n  api_key:\n    in: query\n    name: api_key\n    value: s3cret\n",
        )
        .unwrap();
        let request =
            Request::from_yaml_str("method: GET\nurl: https://example.com/search\n").unwrap();

        let resolved = environment
            .apply(&request)
            .and_then(|request| request.resolve_auth())
            .and_then(|request| request.resolve_query())
            .expect("resolves end to end");
        assert_eq!(resolved.url, "https://example.com/search?api_key=s3cret");
    }

    #[test]
    fn a_malformed_environment_level_auth_block_is_a_typed_error_at_parse_time() {
        let err =
            Environment::from_yaml_str("auth:\n  bearer: x\n  basic:\n    user: a\n    pass: b\n")
                .expect_err("bearer and basic together must be rejected");
        assert!(
            matches!(&err, SendraError::InvalidEnvironment { reason, .. } if reason.contains("bearer") && reason.contains("basic")),
            "got {err:?}"
        );
    }
}
