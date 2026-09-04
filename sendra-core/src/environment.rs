//! Environments: named files of variables, and the substitution pass that puts
//! them into a request.
//!
//! An environment is one flat YAML file of name-to-value pairs, living at
//! `.sendra/environments/<name>.yaml` inside a project:
//!
//! ```text
//! base_url: https://staging.api.example.com
//! api_key: ${API_KEY}
//! ```
//!
//! Request and collection files reference those values with `{{name}}` inside
//! `url`, `headers` (names and values) and `body`. A value written as `${VAR}`
//! is read from the OS environment when it is used, so a file that names a
//! secret can still be committed — the secret itself never is.
//!
//! Two references, two syntaxes, on purpose. `{{name}}` only ever means "a
//! variable from the environment file" and is only looked for in request files;
//! `${VAR}` only ever means "a variable from the OS environment" and is only
//! looked for in environment-file values. Neither can appear where the other is
//! resolved, so there is never a question of which of the two a given
//! placeholder is, or of what order the two run in.
//!
//! # Why substitution is a pass over the parsed request
//!
//! Substitution happens **after** the YAML is parsed, walking the string fields
//! of a [`Request`], rather than as a find-and-replace over the raw file text
//! before parsing. Text-level substitution is easier to write and wrong in ways
//! that only show up on someone else's machine:
//!
//! - A value can change the shape of the document. A token containing `:` or
//!   `#`, a multi-line PEM key, a body starting with `-` — each of those turns
//!   a valid file into a different (or invalid) one once pasted in as raw text.
//!   Post-parse, a value is a string that was already a string, and nothing it
//!   contains can add a key, end a block or start a comment.
//! - It would make `deny_unknown_fields` and the collection rules run against
//!   text the author never wrote, so a parse error could point at a line that
//!   exists in no file, with a column that means nothing.
//! - It would let `{{var}}` appear anywhere at all — in `method`, in half of a
//!   key name — which is a far larger contract than this issue means to add,
//!   and not one that could be walked back later.
//!
//! The cost is that only the fields listed above are templated. `method` is a
//! closed enum with no useful placeholder, and `name` is deliberately excluded
//! because it is the selector `sendra run <file> <name>` matches on: a label
//! that changed with the environment could not be typed on the command line.
//!
//! # Why there is no `EnvironmentFile`/`Environment` pair
//!
//! [`Config`](crate::Config) splits into a `ConfigFile` (every field optional,
//! because that optionality is the merge information) and a resolved `Config`.
//! Environments do not layer — an explicit non-goal for v1, flat files only —
//! so there is nothing for a second type to merge and no `Option` to resolve
//! away. One type is the whole story.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::PROJECT_DIR_NAME;
use crate::{Collection, Document, Request, SendraError};

/// Directory holding environment files, under a project's `.sendra/`.
const ENVIRONMENTS_DIR_NAME: &str = "environments";

/// The environment loaded when nothing selects one.
///
/// **Temporary.** There is no `--env` flag yet (that is the next issue), so the
/// CLI always loads this name and a project that wants variables puts them in
/// `.sendra/environments/default.yaml`. Everything else in this module already
/// takes an environment name as an argument, so the flag replaces exactly this
/// constant at its one call site in `sendra-cli` and nothing else moves.
pub const DEFAULT_ENVIRONMENT_NAME: &str = "default";

/// Delimiters for a `{{variable}}` reference in a request file.
const TEMPLATE_OPEN: &str = "{{";
const TEMPLATE_CLOSE: &str = "}}";

/// Delimiters for a `${VAR}` reference in an environment-file value.
const OS_VAR_OPEN: &str = "${";
const OS_VAR_CLOSE: &str = "}";

/// A set of variables a request can be sent against.
///
/// [`Environment::default`] is the empty environment: no variables, and no file
/// behind it. That is the state of a project with no `.sendra/environments/` at
/// all, and it is not an error — a request with no `{{...}}` in it is untouched
/// by substitution, so Sendra behaves exactly as it did before this existed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Environment {
    /// The file's contents, verbatim. Values still hold their `${VAR}`
    /// references: those are resolved when a variable is used, not when the
    /// file is read. See [`Environment::lookup`].
    pub variables: BTreeMap<String, String>,

    /// The file this came from, or `None` for an environment that was not read
    /// from disk (the empty default, or one built in a test). Carried so a
    /// missing-variable error can name the file to go and fix.
    pub source: Option<PathBuf>,

    /// Stands in for the OS environment when set.
    ///
    /// Tests need to know what `${VAR}` resolves to, and the alternative is
    /// `std::env::set_var`, which is process-global: one test setting a
    /// variable is visible to every other test running beside it. The config
    /// module dodged the same trap by taking paths as arguments instead of
    /// reading the working directory; this is that idea for the environment.
    /// `None` — the only value production code ever builds — means the real OS
    /// environment.
    os_env_override: Option<BTreeMap<String, String>>,
}

impl Environment {
    /// Parse an environment from a YAML string.
    pub fn from_yaml_str(yaml: &str) -> Result<Self, SendraError> {
        Ok(Self {
            variables: parse(yaml, SendraError::ParseStr)?,
            source: None,
            os_env_override: None,
        })
    }

    /// Read and parse an environment file from disk.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, SendraError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|source| SendraError::EnvIo {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self {
            variables: parse(&raw, |source| SendraError::EnvParse {
                path: path.to_path_buf(),
                source,
            })?,
            source: Some(path.to_path_buf()),
            os_env_override: None,
        })
    }

    /// Find and load the environment called `name`, starting from the current
    /// directory.
    ///
    /// A missing environment file is not an error, it is the empty
    /// environment — the same call the config module makes for a missing config
    /// file. What *is* an error is a request asking for a variable the
    /// environment does not have, empty or not; that surfaces in
    /// [`Environment::apply`], where the message can name the variable.
    pub fn resolve(name: &str) -> Result<Self, SendraError> {
        let cwd = std::env::current_dir().map_err(SendraError::CurrentDir)?;
        Self::resolve_from(&cwd, name)
    }

    /// [`Environment::resolve`] with the starting directory passed in, so the
    /// search is testable against a temporary tree without changing the
    /// process's working directory.
    pub fn resolve_from(start_dir: &Path, name: &str) -> Result<Self, SendraError> {
        match find_environment(start_dir, name) {
            Some(path) => Self::from_path(path),
            None => Ok(Self::default()),
        }
    }

    /// The variable names this environment defines, sorted — the list a
    /// "no variable named X" error offers, the way
    /// [`RequestNotFound`](SendraError::RequestNotFound) offers request names.
    pub fn names(&self) -> Vec<String> {
        self.variables.keys().cloned().collect()
    }

    /// Whether this environment defines no variables at all.
    pub fn is_empty(&self) -> bool {
        self.variables.is_empty()
    }

    /// Substitute this environment into `request`, returning the request as it
    /// will be sent.
    ///
    /// Every `{{name}}` in `url`, in each header name, in each header value and
    /// in `body` is replaced. An unknown name is
    /// [`VariableNotFound`](SendraError::VariableNotFound), and a value whose
    /// `${VAR}` is not in the OS environment is
    /// [`EnvVarNotSet`](SendraError::EnvVarNotSet) — never an empty string, and
    /// never a half-substituted request. The whole request is built before
    /// anything is sent, so both failures land before the first byte goes out.
    pub fn apply(&self, request: &Request) -> Result<Request, SendraError> {
        let mut headers = BTreeMap::new();
        for (name, value) in &request.headers {
            let name = self.expand_templates(name)?;
            let value = self.expand_templates(value)?;
            // Two header names that were distinct in the file can collide once
            // substituted (`{{prefix}}-Key` and `X-Key`, say). A map insert
            // would drop one value without a word; this says so instead.
            if headers.insert(name.clone(), value).is_some() {
                return Err(SendraError::InvalidHeader {
                    name,
                    reason: "two header names in this request resolve to it after variable \
                             substitution, so one of the values would be dropped"
                        .to_string(),
                });
            }
        }

        Ok(Request {
            // `name` is left alone: it is what `sendra run <file> <name>`
            // selects on, and a label that changed with the environment could
            // not be typed on the command line.
            name: request.name.clone(),
            method: request.method,
            url: self.expand_templates(&request.url)?,
            headers,
            body: request
                .body
                .as_deref()
                .map(|body| self.expand_templates(body))
                .transpose()?,
        })
    }

    /// [`Environment::apply`] for every request in a collection, in file order.
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
        expand(text, TEMPLATE_OPEN, TEMPLATE_CLOSE, |name| {
            self.lookup(name)
        })
    }

    /// The value of one variable, with any `${VAR}` in it resolved.
    ///
    /// Resolution is lazy — on use, not when the file is read — so an
    /// environment listing five secrets does not demand all five from the OS
    /// just to send the one request that needs one of them.
    ///
    /// The result is *not* re-scanned for `{{...}}`. Substitution is a single
    /// pass by design: recursion would let one environment variable reference
    /// another (the layering this issue explicitly does not do), and would let a
    /// value fetched from the OS environment be read as a template rather than
    /// as data.
    fn lookup(&self, name: &str) -> Result<String, SendraError> {
        let value = self
            .variables
            .get(name)
            .ok_or_else(|| SendraError::VariableNotFound {
                name: name.to_string(),
                available: self.names(),
                environment: self.source.clone(),
            })?;

        expand(value, OS_VAR_OPEN, OS_VAR_CLOSE, |os_var| {
            self.os_var(os_var, name)
        })
    }

    /// Read `os_var` from the OS environment. `referenced_by` is the
    /// environment-file variable whose value asked for it, so the error can say
    /// where to look rather than only which variable is missing.
    fn os_var(&self, os_var: &str, referenced_by: &str) -> Result<String, SendraError> {
        let found = match &self.os_env_override {
            Some(fixed) => fixed.get(os_var).cloned(),
            None => std::env::var(os_var).ok(),
        };

        found.ok_or_else(|| SendraError::EnvVarNotSet {
            name: os_var.to_string(),
            variable: referenced_by.to_string(),
            environment: self.source.clone(),
        })
    }
}

/// Parse the flat map an environment file holds.
///
/// Every value is a string, and an unquoted YAML scalar becomes exactly the text
/// it was written as: `port: 8080` is the string `8080`, `flag: true` is `true`,
/// `version: 1.0` is `1.0`. That is the only rule that makes sense for a
/// substitution engine — what is in the file is what goes into the request, with
/// no round trip through a number or a bool to round `1.0` down to `1` or to
/// re-spell `true` as `True`. Quoting changes nothing, so `'8080'` is there for
/// anyone who would rather be explicit.
///
/// A value that is a *sequence or a mapping* is a parse error, and that is the
/// rule keeping environments flat: `staging:` with variables nested underneath
/// fails to load rather than half-working, which is what "no inheritance in v1"
/// has to mean in practice.
fn parse(
    yaml: &str,
    wrap: impl Fn(serde_yaml::Error) -> SendraError,
) -> Result<BTreeMap<String, String>, SendraError> {
    // An empty file, or one that is only comments, is YAML null. Creating the
    // file before filling it in is too reasonable to be an error, so read it as
    // an environment with no variables — the same call `ConfigFile` makes.
    let probe: serde_yaml::Value = serde_yaml::from_str(yaml).map_err(&wrap)?;
    if probe.is_null() {
        return Ok(BTreeMap::new());
    }
    serde_yaml::from_str(yaml).map_err(&wrap)
}

/// Walk up from `start_dir` looking for `.sendra/environments/<name>.yaml`,
/// returning the first one found.
///
/// The same search, and the same "nearest wins, no stacking" rule, as
/// [`find_project_config`](crate::config::find_project_config): an environment
/// at the repository root applies from anywhere inside the repository.
///
/// There is no global equivalent. A config file holds preferences that travel
/// with a person (a `User-Agent`, a timeout); an environment holds the hosts and
/// keys of one particular API, which belongs to the project it describes, not to
/// the machine that project is checked out on.
pub fn find_environment(start_dir: &Path, name: &str) -> Option<PathBuf> {
    start_dir
        .ancestors()
        .map(|dir| environment_path(dir, name))
        .find(|candidate| candidate.is_file())
}

/// Where the environment called `name` lives for the project rooted at `root`.
pub fn environment_path(root: &Path, name: &str) -> PathBuf {
    root.join(PROJECT_DIR_NAME)
        .join(ENVIRONMENTS_DIR_NAME)
        .join(format!("{name}.yaml"))
}

/// Replace every `open`…`close` placeholder in `text` with whatever `resolve`
/// returns for the name inside it.
///
/// Two things are deliberately *not* errors, because each is far likelier to be
/// text that happens to contain a brace than a mistyped placeholder: an
/// unterminated `open` (the rest of the string is literal), and an empty name
/// such as `{{}}` (emitted as written — there is no variable to name in a "no
/// variable named ``" message). A `{{name}}` with a real name in it, on the
/// other hand, is unambiguously a reference, and must resolve or fail.
///
/// Substitution is not recursive: what `resolve` hands back is copied out
/// verbatim and never scanned again.
fn expand(
    text: &str,
    open: &str,
    close: &str,
    mut resolve: impl FnMut(&str) -> Result<String, SendraError>,
) -> Result<String, SendraError> {
    if !text.contains(open) {
        return Ok(text.to_string());
    }

    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = rest.find(open) {
        let after_open = &rest[start + open.len()..];
        let Some(end) = after_open.find(close) else {
            // Unterminated: nothing left in the string can be a placeholder.
            break;
        };

        let name = after_open[..end].trim();
        if name.is_empty() {
            // Keep the delimiter as written and carry on looking after it.
            out.push_str(&rest[..start + open.len()]);
            rest = after_open;
            continue;
        }

        out.push_str(&rest[..start]);
        out.push_str(&resolve(name)?);
        rest = &after_open[end + close.len()..];
    }

    out.push_str(rest);
    Ok(out)
}

/// "`path/to/env.yaml` (available: a, b)" and its awkward cases, for the message
/// [`VariableNotFound`](SendraError::VariableNotFound) shows.
pub(crate) fn describe_variables(environment: &Option<PathBuf>, available: &[String]) -> String {
    match (environment, available.is_empty()) {
        (Some(path), false) => {
            format!("`{}` (available: {})", path.display(), available.join(", "))
        }
        (Some(path), true) => format!("`{}`, which defines no variables", path.display()),
        (None, false) => format!(
            "the active environment (available: {})",
            available.join(", ")
        ),
        (None, true) => "the active environment: no environment file was found".to_string(),
    }
}

/// "`path/to/env.yaml`", or a stand-in when the environment came from nowhere.
pub(crate) fn describe_environment(environment: &Option<PathBuf>) -> String {
    match environment {
        Some(path) => format!("`{}`", path.display()),
        None => "the active environment".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::Method;

    /// An environment built in memory, with a fixed stand-in for the OS
    /// environment so `${VAR}` is testable without `std::env::set_var`.
    fn environment(variables: &[(&str, &str)], os_env: &[(&str, &str)]) -> Environment {
        Environment {
            variables: pairs(variables),
            source: None,
            os_env_override: Some(pairs(os_env)),
        }
    }

    fn pairs(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    /// Write `contents` to `path`, creating the directories above it.
    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().expect("a file has a parent")).unwrap();
        std::fs::write(path, contents).unwrap();
    }

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
        assert_eq!(
            applied.headers.get("Authorization").map(String::as_str),
            Some("Bearer s3cret")
        );
        // Header *names* are substituted too, not just values.
        assert_eq!(
            applied.headers.get("X-Tenant").map(String::as_str),
            Some("fixed-value")
        );
        assert_eq!(
            applied.body.as_deref(),
            Some("{\"host\": \"https://staging.example.com\"}")
        );
        // The label is deliberately untouched: it is the run selector.
        assert_eq!(applied.name.as_deref(), Some("Templated"));
        assert_eq!(applied.method, Method::Post);
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
    fn a_missing_variable_is_a_typed_error_listing_what_is_available() {
        let request = Request::from_yaml_str("method: GET\nurl: '{{base_url}}/x'\n").unwrap();
        let environment = environment(&[("host", "example.com"), ("port", "443")], &[]);

        let err = environment
            .apply(&request)
            .expect_err("`base_url` is not defined");

        match &err {
            SendraError::VariableNotFound {
                name, available, ..
            } => {
                assert_eq!(name, "base_url");
                assert_eq!(available, &["host".to_string(), "port".to_string()]);
            }
            other => panic!("expected VariableNotFound, got {other:?}"),
        }
        // Not a panic, and not a silent empty string: the message names the
        // variable and offers the ones that do exist.
        let message = err.to_string();
        assert!(message.contains("base_url"), "got {message}");
        assert!(message.contains("host, port"), "got {message}");
    }

    #[test]
    fn a_missing_variable_error_names_the_environment_file_it_looked_in() {
        let temp = tempfile::tempdir().unwrap();
        let path = environment_path(temp.path(), "staging");
        write(&path, "host: example.com\n");

        let environment = Environment::from_path(&path).unwrap();
        let request = Request::from_yaml_str("method: GET\nurl: '{{base_url}}'\n").unwrap();

        let err = environment.apply(&request).unwrap_err();
        match &err {
            SendraError::VariableNotFound { environment, .. } => {
                assert_eq!(environment.as_deref(), Some(path.as_path()))
            }
            other => panic!("expected VariableNotFound, got {other:?}"),
        }
        assert!(
            err.to_string().contains("staging.yaml"),
            "the message should name the file to fix: {err}"
        );
    }

    #[test]
    fn a_missing_variable_with_no_environment_file_says_so() {
        let request = Request::from_yaml_str("method: GET\nurl: '{{base_url}}'\n").unwrap();
        let err = Environment::default().apply(&request).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("base_url"), "got {message}");
        assert!(
            message.contains("no environment file was found"),
            "an empty available-list must not read as `(available: )`: {message}"
        );
    }

    #[test]
    fn an_os_variable_is_read_from_the_environment_at_use_time() {
        let request = Request::from_yaml_str(
            "method: GET\nurl: https://example.com\nheaders:\n  Authorization: '{{api_key}}'\n",
        )
        .unwrap();
        // The environment file holds the *reference*, never the secret.
        let environment = environment(&[("api_key", "${API_KEY}")], &[("API_KEY", "live-token")]);

        let applied = environment.apply(&request).unwrap();

        assert_eq!(
            applied.headers.get("Authorization").map(String::as_str),
            Some("live-token")
        );
    }

    #[test]
    fn an_os_variable_can_be_embedded_in_a_larger_value() {
        let request = Request::from_yaml_str(
            "method: GET\nurl: https://example.com\nheaders:\n  Authorization: '{{auth}}'\n",
        )
        .unwrap();
        let environment = environment(&[("auth", "Bearer ${API_KEY}!")], &[("API_KEY", "abc")]);

        let applied = environment.apply(&request).unwrap();
        assert_eq!(
            applied.headers.get("Authorization").map(String::as_str),
            Some("Bearer abc!")
        );
    }

    #[test]
    fn a_missing_os_variable_is_a_typed_error_not_an_empty_string() {
        let request = Request::from_yaml_str("method: GET\nurl: '{{host}}'\n").unwrap();
        // Nothing in the stand-in OS environment, so `${API_KEY}` has no value.
        let environment = environment(&[("host", "https://x/${API_KEY}")], &[]);

        let err = environment.apply(&request).expect_err("API_KEY is not set");
        match &err {
            SendraError::EnvVarNotSet { name, variable, .. } => {
                assert_eq!(name, "API_KEY");
                // The error says which environment variable pulled it in, so
                // there is somewhere to go and look.
                assert_eq!(variable, "host");
            }
            other => panic!("expected EnvVarNotSet, got {other:?}"),
        }
        let message = err.to_string();
        assert!(message.contains("API_KEY"), "got {message}");
    }

    #[test]
    fn a_missing_os_variable_is_reported_against_the_real_os_environment_too() {
        // The tests above use the stand-in; this one exercises the real
        // `std::env` path with a name nothing could plausibly have set. It
        // reads the environment and never writes it, so it is safe beside
        // every other test in the suite.
        let request = Request::from_yaml_str("method: GET\nurl: '{{token}}'\n").unwrap();
        let environment = Environment {
            variables: pairs(&[("token", "${SENDRA_TEST_DEFINITELY_NOT_SET_9F3A}")]),
            source: None,
            os_env_override: None,
        };

        let err = environment.apply(&request).expect_err("no such variable");
        assert!(
            matches!(err, SendraError::EnvVarNotSet { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn an_unused_variable_with_a_missing_os_variable_does_not_fail_the_run() {
        // Resolution is lazy: an environment listing five secrets must not
        // demand all five to send the one request that needs none of them.
        let request = Request::from_yaml_str("method: GET\nurl: '{{host}}'\n").unwrap();
        let environment = environment(
            &[("host", "https://example.com"), ("unused", "${NOT_SET}")],
            &[],
        );

        let applied = environment.apply(&request).expect("`unused` is not used");
        assert_eq!(applied.url, "https://example.com");
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
            applied.requests[1]
                .headers
                .get("Authorization")
                .map(String::as_str),
            Some("Bearer s3cret")
        );
        // Untemplated fields are carried through untouched.
        assert_eq!(
            applied.requests[1].body.as_deref(),
            Some("{\"name\": \"ada\"}")
        );
    }

    #[test]
    fn one_bad_variable_fails_the_whole_collection_before_anything_is_sent() {
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
    fn two_header_names_resolving_to_the_same_name_is_an_error_not_a_dropped_value() {
        let yaml = "\
method: GET
url: https://example.com
headers:
  '{{name}}': from-template
  X-Key: from-literal
";
        let request = Request::from_yaml_str(yaml).unwrap();
        let environment = environment(&[("name", "X-Key")], &[]);

        let err = environment
            .apply(&request)
            .expect_err("one of the two values would be silently lost");
        match err {
            SendraError::InvalidHeader { name, .. } => assert_eq!(name, "X-Key"),
            other => panic!("expected InvalidHeader, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_flat_environment_file() {
        let environment = Environment::from_yaml_str(
            "base_url: https://staging.example.com\napi_key: ${API_KEY}\n",
        )
        .unwrap();

        assert_eq!(environment.names(), vec!["api_key", "base_url"]);
        // Stored verbatim: `${API_KEY}` is resolved on use, not on read, so the
        // secret is never held in the parsed file.
        assert_eq!(
            environment.variables.get("api_key").map(String::as_str),
            Some("${API_KEY}")
        );
    }

    #[test]
    fn an_empty_environment_file_is_an_empty_environment_not_an_error() {
        let environment = Environment::from_yaml_str("# nothing yet\n")
            .expect("creating the file before filling it in is reasonable");
        assert!(environment.is_empty());
    }

    #[test]
    fn an_unquoted_scalar_substitutes_as_the_text_it_was_written_as() {
        // The property that matters for a substitution engine: no value takes a
        // round trip through a number or a bool on the way in, so `1.0` cannot
        // arrive as `1`, and quoting is a matter of taste rather than of meaning.
        let environment =
            Environment::from_yaml_str("port: 8080\nquoted: '8080'\nversion: 1.0\nflag: true\n")
                .expect("a plain scalar is a perfectly good variable value");

        for (name, expected) in [
            ("port", "8080"),
            ("quoted", "8080"),
            ("version", "1.0"),
            ("flag", "true"),
        ] {
            assert_eq!(
                environment.variables.get(name).map(String::as_str),
                Some(expected),
                "`{name}` should substitute as written"
            );
        }
    }

    #[test]
    fn a_nested_environment_file_is_rejected() {
        // Flat files only for v1; "staging extends base" is a non-goal, and a
        // parse error is a better answer than half-supporting it.
        let err = Environment::from_yaml_str("staging:\n  base_url: https://x\n")
            .expect_err("environments do not nest");
        assert!(matches!(err, SendraError::ParseStr(_)), "got {err:?}");

        let err = Environment::from_yaml_str("hosts:\n  - https://x\n")
            .expect_err("a variable is one value, not a list");
        assert!(matches!(err, SendraError::ParseStr(_)), "got {err:?}");
    }

    #[test]
    fn malformed_yaml_in_an_environment_file_is_a_typed_error_carrying_the_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = environment_path(temp.path(), "default");
        write(&path, "base_url: [oops\n");

        let err = Environment::resolve_from(temp.path(), "default")
            .expect_err("malformed yaml must error");
        match err {
            SendraError::EnvParse { path: reported, .. } => assert_eq!(reported, path),
            other => panic!("expected EnvParse, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_environment_file_is_the_empty_environment_not_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let environment = Environment::resolve_from(temp.path(), "default")
            .expect("no environment file is an ordinary state");
        assert_eq!(environment, Environment::default());
        assert!(environment.source.is_none());
    }

    #[test]
    fn the_environment_at_the_project_root_is_found_from_a_nested_subdirectory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let path = environment_path(&root, "default");
        write(&path, "base_url: https://example.com\n");

        let nested = root.join("crates").join("api").join("tests");
        std::fs::create_dir_all(&nested).unwrap();

        let environment = Environment::resolve_from(&nested, "default").unwrap();
        assert_eq!(environment.source.as_deref(), Some(path.as_path()));
        assert_eq!(
            environment.variables.get("base_url").map(String::as_str),
            Some("https://example.com")
        );
    }

    #[test]
    fn environments_are_selected_by_name() {
        let temp = tempfile::tempdir().unwrap();
        write(
            &environment_path(temp.path(), "staging"),
            "base_url: https://staging.example.com\n",
        );
        write(
            &environment_path(temp.path(), "prod"),
            "base_url: https://api.example.com\n",
        );

        for (name, expected) in [
            ("staging", "https://staging.example.com"),
            ("prod", "https://api.example.com"),
        ] {
            let environment = Environment::resolve_from(temp.path(), name).unwrap();
            assert_eq!(
                environment.variables.get("base_url").map(String::as_str),
                Some(expected),
                "`{name}` should have loaded its own file"
            );
        }
    }

    #[test]
    fn the_nearest_environment_wins_over_one_further_up() {
        let temp = tempfile::tempdir().unwrap();
        let outer = temp.path().join("outer");
        write(&environment_path(&outer, "default"), "which: outer\n");
        let inner = outer.join("inner");
        write(&environment_path(&inner, "default"), "which: inner\n");

        let environment = Environment::resolve_from(&inner, "default").unwrap();
        assert_eq!(
            environment.variables.get("which").map(String::as_str),
            Some("inner")
        );
    }

    #[test]
    fn the_default_environment_lives_where_the_readme_says_it_does() {
        // The temporary wiring is a documented path, so pin it: issue 5 swaps
        // the name, not the layout.
        let path = environment_path(Path::new("/project"), DEFAULT_ENVIRONMENT_NAME);
        assert!(
            path.ends_with(Path::new(".sendra/environments/default.yaml")),
            "got {}",
            path.display()
        );
    }
}
