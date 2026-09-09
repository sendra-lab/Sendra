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
//! `url`, `headers` (names and values), `body`, and the values of an
//! `assertions` block. A value written as `${VAR}` is read from the OS
//! environment when it is used, so a file that names a secret can still be
//! committed — the secret itself never is.
//!
//! One top-level key is reserved rather than a variable: `auth`. An
//! environment may carry a default [`Auth`](crate::Auth) block — exactly the
//! same shape [`Request::auth`](crate::Request::auth) is — applied to every
//! request run against it that sets no `auth:` of its own:
//!
//! ```text
//! base_url: https://staging.api.example.com
//! auth:
//!   bearer: ${API_TOKEN}
//! ```
//!
//! See [`Environment::auth`] for the full precedence rule (a request's own
//! `auth:` fully replaces the environment's, never merges with it) and
//! [`Environment::apply`] for where it is filled in.
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
//! of a [`Request`](crate::Request), rather than as a find-and-replace over the
//! raw file text before parsing. Text-level substitution is easier to write and
//! wrong in ways that only show up on someone else's machine:
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
//!   key name — which is a far larger contract than substitution is meant to
//!   make, and not one that could be walked back later.
//!
//! The cost is that only the fields listed above are templated. `method` is a
//! closed enum with no useful placeholder, and `name` is deliberately excluded
//! because it is the selector `sendra run <file> <name>` matches on: a label
//! that changed with the environment could not be typed on the command line.
//! Inside `assertions`, values are templated but the keys that select part of
//! the response — header names, JSON paths — are not, for a related reason:
//! see [`Environment::apply_assertions`].
//!
//! # `EnvironmentFile` vs. `Environment`
//!
//! [`Config`](crate::Config) splits into a `ConfigFile` (every field optional,
//! because that optionality is the merge information) and a resolved `Config`
//! because several config *sources* — project file, global file, CLI flags —
//! merge into one. [`EnvironmentFile`] splits from [`Environment`] for a much
//! narrower reason: it is purely the on-disk shape `serde` deserializes,
//! while `Environment` additionally carries `source`, the per-run `captured`
//! store, and (in tests) a stand-in OS environment — none of which come from
//! the file itself. There is still no merging or layering here: one
//! environment file, read once, is the whole story, the same non-goal for v1
//! as always. `EnvironmentFile` exists for `schemars` to derive a real schema
//! from (see `xtask`), not because a second environment file could combine
//! with a first.

mod substitute;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::PROJECT_DIR_NAME;
use crate::{Auth, SendraError};

/// Directory holding environment files, under a project's `.sendra/`.
const ENVIRONMENTS_DIR_NAME: &str = "environments";

/// The environment name `sendra run` falls back to when `--env` is omitted.
///
/// Nothing in this module treats it as special: it resolves like any other
/// name, and a project with no `default.yaml` gets the empty environment the
/// same way `staging` with no `staging.yaml` would. The front-end is what
/// decides an *explicitly named* environment with no file is an error while an
/// absent default is not — see `environment_for` in `sendra-cli`.
pub const DEFAULT_ENVIRONMENT_NAME: &str = "default";

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

    /// A default `auth:` block, reusing [`Request::auth`](crate::Request::auth)'s
    /// exact shape, applied by [`Environment::apply`] to every request run
    /// against this environment that sets no `auth:` of its own.
    ///
    /// **Fully replaced, never merged**, by a request's own `auth:` — the
    /// same "two things claiming ownership of one setting" stance
    /// `Request::auth` already takes against an explicit `Authorization`
    /// header, applied one layer up: a request that wants different
    /// credentials than its environment's default writes its own `auth:`
    /// block, in full, rather than overriding one field of this one.
    ///
    /// `bearer`/`basic`/`api_key` still hold unsubstituted `{{var}}`/`${VAR}`
    /// text here, exactly like [`variables`](Self::variables) — resolved,
    /// against this same environment, only when [`apply`](Self::apply)
    /// actually uses it.
    pub auth: Option<Auth>,

    /// The file this came from, or `None` for an environment that was not read
    /// from disk (the empty default, or one built in a test). Carried so a
    /// missing-variable error can name the file to go and fix.
    pub source: Option<PathBuf>,

    /// Variables captured by requests earlier in the same run, looked up
    /// alongside [`variables`](Self::variables) — see
    /// [`with_captured`](Self::with_captured), which is the only way to set it.
    ///
    /// Private, and set by rebuilding rather than by mutation, because the
    /// growth of this map is the *whole* of how ordering works: the store is a
    /// fact about a point in a run, and an `Environment` that could be mutated
    /// in place would let a value reach a request that ran before it was
    /// captured. Rebuilding per request makes each substitution see exactly the
    /// captures that existed when it started, which is what "file order is real
    /// order" has to mean.
    ///
    /// Disjoint from `variables` by construction: a capture whose name the file
    /// already defines is refused where it happens, as
    /// [`CaptureFailure::Shadowed`](crate::CaptureFailure::Shadowed), so it
    /// never reaches this map.
    captured: BTreeMap<String, String>,

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
        let file = parse(yaml, SendraError::ParseStr)?;
        Self::from_file(file, None)
    }

    /// Read and parse an environment file from disk.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, SendraError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|source| SendraError::EnvIo {
            path: path.to_path_buf(),
            source,
        })?;
        let file = parse(&raw, |source| SendraError::EnvParse {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_file(file, Some(path.to_path_buf()))
    }

    /// Common tail of [`from_yaml_str`](Self::from_yaml_str) and
    /// [`from_path`](Self::from_path): validate the parsed `auth:` block, if
    /// any, then assemble the runtime [`Environment`] around it.
    ///
    /// `auth`'s own mutual-exclusivity rule (`Auth::validate_exclusivity`) is
    /// checked here, once, at parse time — the same point
    /// [`Request::validate`](crate::Request::validate) checks it for a
    /// request's own `auth:` block, since this is the exact same rule on the
    /// exact same type. What it cannot check yet is a collision with a
    /// request's headers/query, since there is no request in scope until
    /// [`apply`](Self::apply) runs — see there.
    fn from_file(file: EnvironmentFile, source: Option<PathBuf>) -> Result<Self, SendraError> {
        if let Some(auth) = &file.auth {
            if let Err(reason) = auth.validate_exclusivity() {
                return Err(SendraError::InvalidEnvironment {
                    path: source,
                    reason,
                });
            }
            if let Some(oauth) = &auth.oauth {
                if let Err(reason) = oauth.validate_grant_fields() {
                    return Err(SendraError::InvalidEnvironment {
                        path: source,
                        reason,
                    });
                }
            }
        }
        Ok(Self {
            variables: file.variables,
            auth: file.auth,
            source,
            captured: BTreeMap::new(),
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

    /// This environment as it stands at one point in a run: the file's own
    /// variables, plus everything captured by the requests that have already
    /// finished.
    ///
    /// **This is the whole of the accumulating store.** A run holds one growing
    /// map and calls this once per request, so the environment a request is
    /// substituted against is a *view* built from the captures that existed
    /// when that request was reached — request 3 sees what 1 and 2 captured,
    /// request 1 sees nothing, and no request can see forwards. Threading the
    /// growth through a rebuilt value rather than through a mutable
    /// `Environment` is what makes that structural instead of a rule the loop
    /// has to remember: there is no `&mut Environment` anywhere for a later
    /// capture to reach an earlier request through.
    ///
    /// The copy is a `BTreeMap` clone per request, which is nothing at the
    /// sizes a hand-written collection reaches, and it buys the property that
    /// the value handed to [`apply`](Self::apply) cannot change underneath it.
    ///
    /// Nothing else changes: `source`, `auth`, and the OS-environment
    /// override tests use, are carried through untouched.
    pub fn with_captured(&self, captured: &BTreeMap<String, String>) -> Self {
        Self {
            variables: self.variables.clone(),
            auth: self.auth.clone(),
            source: self.source.clone(),
            captured: captured.clone(),
            os_env_override: self.os_env_override.clone(),
        }
    }

    /// The variable names this environment's **file** defines, sorted — the
    /// list a "no variable named X" error offers, the way
    /// [`RequestNotFound`](SendraError::RequestNotFound) offers request names.
    ///
    /// Captured names are deliberately not in here: this list is offered under
    /// the name of the file it came from, and a capture did not come from that
    /// file. They are reported beside it — see
    /// [`captured_names`](Self::captured_names).
    pub fn names(&self) -> Vec<String> {
        self.variables.keys().cloned().collect()
    }

    /// The names captured by earlier requests in this run, sorted. Empty
    /// unless [`with_captured`](Self::with_captured) put something there.
    pub fn captured_names(&self) -> Vec<String> {
        self.captured.keys().cloned().collect()
    }

    /// Whether this environment defines no variables at all — captures
    /// included, since a `{{name}}` can resolve against either.
    pub fn is_empty(&self) -> bool {
        self.variables.is_empty() && self.captured.is_empty()
    }

    /// The value of one variable, with any `${VAR}` in it resolved.
    ///
    /// Resolution is lazy — on use, not when the file is read — so an
    /// environment listing five secrets does not demand all five from the OS
    /// just to send the one request that needs one of them.
    ///
    /// The result is *not* re-scanned for `{{...}}`. Substitution is a single
    /// pass by design: recursion would let one environment variable reference
    /// another (a layering deliberately left out), and would let a value
    /// fetched from the OS environment be read as a template rather than as
    /// data.
    fn lookup(&self, name: &str) -> Result<String, SendraError> {
        // Captured first, and it costs nothing to be exact about why: the two
        // maps are disjoint by construction, since a capture whose name the
        // file already defines is refused at capture time rather than allowed
        // to shadow it. So this order is a statement of that invariant, not a
        // precedence rule — if it ever mattered, something upstream is broken.
        if let Some(value) = self.captured.get(name) {
            // **Not scanned for `${VAR}`.** A captured value is text that came
            // back from a server, not a line someone wrote in an environment
            // file, and a token that happens to contain `${` is data. This is
            // the same single-pass rule the doc comment above states for
            // `{{...}}`, applied to the other syntax.
            return Ok(value.clone());
        }

        let value = self
            .variables
            .get(name)
            .ok_or_else(|| SendraError::VariableNotFound {
                name: name.to_string(),
                available: self.names(),
                environment: self.source.clone(),
                captured: self.captured_names(),
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

/// The on-disk shape of one environment file: every top-level key is a
/// variable, **except `auth`**, which is reserved for an optional default
/// [`Auth`] block — see the module docs and [`Environment::auth`].
///
/// `auth` is the only top-level key this type gives a name to — every other
/// key becomes a variable, the same way it always has — so a file with no
/// `auth:` key behaves exactly like the flat `BTreeMap<String, String>` this
/// used to deserialize straight into. The one behavior change this trades
/// for that continuity: a project that happened to have a variable literally
/// named `auth` now needs a different name, or its own `auth:` block
/// instead.
///
/// Every variable value is a string, and an unquoted YAML scalar becomes
/// exactly the text it was written as: `port: 8080` is the string `8080`,
/// `flag: true` is `true`, `version: 1.0` is `1.0`. That is the only rule
/// that makes sense for a substitution engine — what is in the file is what
/// goes into the request, with no round trip through a number or a bool to
/// round `1.0` down to `1` or to re-spell `true` as `True`. Quoting changes
/// nothing, so `'8080'` is there for anyone who would rather be explicit.
///
/// A variable value that is a *sequence or a mapping* is a parse error, and
/// that is the rule keeping environments flat: `staging:` with variables
/// nested underneath fails to load rather than half-working, which is what
/// "no inheritance in v1" has to mean in practice. `auth:` is exempt from
/// this — it is a mapping on purpose — but only `auth` is; any other nested
/// key is still rejected exactly as before.
///
/// `Deserialize` is hand-written rather than `#[derive(Deserialize)]` with
/// `#[serde(flatten)]` on `variables`: `flatten` deserializes the whole
/// document through `serde`'s generic `Content` capture first, and that
/// buffering loses `serde_yaml`'s laxness at the leaves — a captured integer
/// or float no longer coerces to a string the way a value read straight off
/// the source text does, and `serde_yaml::Value`'s own `Number` has the same
/// problem (it does not even retain `1.0` vs `1` as written). Either would
/// silently break every existing environment file with a bare number/bool
/// value the moment `auth:` support was added — exactly the backward
/// compatibility this format change is not allowed to cost. Walking the map
/// by hand and calling `next_value::<String>()` per key, below, asks
/// `serde_yaml` for a string directly off that key's own source node, which
/// is the same call (and the same laxness) `BTreeMap<String, String>`'s own
/// `Deserialize` impl has always made.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EnvironmentFile {
    #[cfg_attr(feature = "schema", schemars(default))]
    pub auth: Option<Auth>,
    #[cfg_attr(feature = "schema", schemars(flatten))]
    pub variables: BTreeMap<String, String>,
}

impl<'de> Deserialize<'de> for EnvironmentFile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = EnvironmentFile;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a mapping of variable name to value, with an optional `auth` block")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut file = EnvironmentFile::default();
                while let Some(key) = map.next_key::<String>()? {
                    if key == "auth" {
                        file.auth = Some(map.next_value::<Auth>()?);
                    } else {
                        file.variables.insert(key, map.next_value::<String>()?);
                    }
                }
                Ok(file)
            }
        }

        deserializer.deserialize_map(Visitor)
    }
}

/// Parse an environment file's raw shape — see [`EnvironmentFile`].
fn parse(
    yaml: &str,
    wrap: impl Fn(serde_yaml::Error) -> SendraError,
) -> Result<EnvironmentFile, SendraError> {
    // An empty file, or one that is only comments, is YAML null. Creating the
    // file before filling it in is too reasonable to be an error, so read it as
    // an environment with no variables — the same call `ConfigFile` makes.
    let probe: serde_yaml::Value = serde_yaml::from_str(yaml).map_err(&wrap)?;
    if probe.is_null() {
        return Ok(EnvironmentFile::default());
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

/// The " or captured earlier in this run (...)" half of a
/// [`VariableNotFound`](SendraError::VariableNotFound) message, or nothing at
/// all when this run has captured nothing.
///
/// Its own clause rather than extra entries in `available`, because the two
/// lists have different answers to "where do I go to add this name": one is a
/// file to edit, the other is a `capture:` block on an earlier request. A run
/// with no captures produces the empty string, so the message every single
/// request has ever printed is unchanged.
pub(crate) fn describe_captured(captured: &[String]) -> String {
    if captured.is_empty() {
        String::new()
    } else {
        format!(" — captured so far in this run: {}", captured.join(", "))
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
pub(crate) mod test_helpers {
    use super::Environment;
    use std::collections::BTreeMap;

    /// An environment built in memory, with a fixed stand-in for the OS
    /// environment so `${VAR}` is testable without `std::env::set_var`.
    pub(crate) fn environment(variables: &[(&str, &str)], os_env: &[(&str, &str)]) -> Environment {
        Environment {
            variables: pairs(variables),
            auth: None,
            source: None,
            captured: BTreeMap::new(),
            os_env_override: Some(pairs(os_env)),
        }
    }

    pub(crate) fn pairs(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::test_helpers::{environment, pairs};
    use super::*;

    use crate::Request;

    /// Write `contents` to `path`, creating the directories above it.
    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().expect("a file has a parent")).unwrap();
        std::fs::write(path, contents).unwrap();
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

        assert_eq!(applied.header("Authorization"), Some("live-token"));
    }

    #[test]
    fn an_os_variable_can_be_embedded_in_a_larger_value() {
        let request = Request::from_yaml_str(
            "method: GET\nurl: https://example.com\nheaders:\n  Authorization: '{{auth}}'\n",
        )
        .unwrap();
        let environment = environment(&[("auth", "Bearer ${API_KEY}!")], &[("API_KEY", "abc")]);

        let applied = environment.apply(&request).unwrap();
        assert_eq!(applied.header("Authorization"), Some("Bearer abc!"));
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
            auth: None,
            source: None,
            captured: BTreeMap::new(),
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
    fn an_auth_block_parses_alongside_ordinary_variables() {
        let environment = Environment::from_yaml_str(
            "base_url: https://staging.example.com\nauth:\n  bearer: '{{token}}'\n",
        )
        .unwrap();

        // `auth` is reserved, not folded into `variables` as an ordinary
        // entry — the same way every other top-level key still is.
        assert_eq!(environment.names(), vec!["base_url"]);
        assert!(!environment.variables.contains_key("auth"));
        let auth = environment.auth.expect("the auth block was parsed");
        assert_eq!(auth.bearer.as_deref(), Some("{{token}}"));
    }

    #[test]
    fn an_environment_file_with_no_auth_key_leaves_auth_none() {
        let environment = Environment::from_yaml_str("base_url: https://example.com\n").unwrap();
        assert!(environment.auth.is_none());
    }

    #[test]
    fn an_auth_value_that_is_not_a_mapping_is_a_typed_error() {
        let err = Environment::from_yaml_str("auth: not-a-mapping\n")
            .expect_err("auth must be a bearer/basic/api_key mapping");
        assert!(matches!(err, SendraError::ParseStr(_)), "got {err:?}");
    }

    #[test]
    fn an_auth_block_naming_an_unknown_field_is_a_typed_error() {
        let err = Environment::from_yaml_str("auth:\n  bogus: x\n")
            .expect_err("Auth::deny_unknown_fields rejects it");
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
        // The layout is a documented path, so pin it: naming a different
        // environment changes which file is read, never where it lives.
        let path = environment_path(Path::new("/project"), DEFAULT_ENVIRONMENT_NAME);
        assert!(
            path.ends_with(Path::new(".sendra/environments/default.yaml")),
            "got {}",
            path.display()
        );
    }

    // --- captured variables ----------------------------------------------

    /// The store as it stands after one request captured `auth_token`.
    fn captured(pairs_in: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs(pairs_in)
    }

    #[test]
    fn a_captured_variable_substitutes_exactly_like_a_file_one() {
        let request = Request::from_yaml_str(
            "method: GET
url: '{{base_url}}/me?t={{auth_token}}'
",
        )
        .unwrap();
        let environment = environment(&[("base_url", "https://example.com")], &[]);

        // Before anything is captured the reference has nothing behind it...
        assert!(environment.apply(&request).is_err());

        // ...and once it does, it resolves through the same single pass.
        let view = environment.with_captured(&captured(&[("auth_token", "abc123")]));
        let applied = view.apply(&request).expect("both variables resolve");
        assert_eq!(applied.url, "https://example.com/me?t=abc123");
    }

    #[test]
    fn a_view_never_changes_the_environment_it_was_built_from() {
        // The property the run loop depends on: request 1 substitutes against
        // an environment that a later `with_captured` cannot reach back into.
        let environment = environment(&[("base_url", "https://example.com")], &[]);
        let view = environment.with_captured(&captured(&[("token", "t")]));

        assert_eq!(view.captured_names(), vec!["token".to_string()]);
        assert!(
            environment.captured_names().is_empty(),
            "the original must not have grown a capture"
        );
        assert!(environment
            .apply(
                &Request::from_yaml_str(
                    "method: GET
url: '{{token}}'
"
                )
                .unwrap()
            )
            .is_err());
    }

    #[test]
    fn a_captured_value_is_data_and_is_never_read_as_a_reference() {
        // A token that happens to contain `${...}` or `{{...}}` is text a
        // server sent, not a line someone wrote in a file: substitution is one
        // pass and what it hands back is copied out verbatim.
        let request = Request::from_yaml_str(
            "method: GET
url: 'https://x/{{token}}'
",
        )
        .unwrap();
        let view = environment(&[], &[("HOME", "/root")])
            .with_captured(&captured(&[("token", "${HOME}-{{base_url}}")]));

        let applied = view.apply(&request).expect("the captured value is data");
        assert_eq!(applied.url, "https://x/${HOME}-{{base_url}}");
    }

    #[test]
    fn a_missing_variable_names_what_was_captured_as_well_as_what_the_file_has() {
        let request = Request::from_yaml_str(
            "method: GET
url: '{{nope}}'
",
        )
        .unwrap();
        let view = environment(&[("base_url", "https://example.com")], &[])
            .with_captured(&captured(&[("auth_token", "abc")]));

        let err = view.apply(&request).expect_err("`nope` is neither");
        match &err {
            SendraError::VariableNotFound {
                available,
                captured,
                ..
            } => {
                // Kept in separate lists: one names a file to edit, the other
                // names a `capture:` block on an earlier request.
                assert_eq!(available, &["base_url".to_string()]);
                assert_eq!(captured, &["auth_token".to_string()]);
            }
            other => panic!("expected VariableNotFound, got {other:?}"),
        }

        let message = err.to_string();
        assert!(message.contains("base_url"), "got {message}");
        assert!(message.contains("auth_token"), "got {message}");
    }

    #[test]
    fn a_run_that_captured_nothing_prints_the_message_it_always_printed() {
        // The clause is additive: nothing captured, nothing said about it.
        let request = Request::from_yaml_str(
            "method: GET
url: '{{nope}}'
",
        )
        .unwrap();
        let message = environment(&[("base_url", "x")], &[])
            .apply(&request)
            .unwrap_err()
            .to_string();
        assert!(!message.contains("captured"), "got {message}");
    }
}
