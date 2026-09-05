//! Tool-wide configuration: where Sendra's config files live, how a project
//! config overrides a global one, and what the result does to a request.
//!
//! Two files, either or both of which may be absent:
//!
//! - **Project**: `.sendra/config.yaml`, found by walking up from the current
//!   directory the way git looks for `.git`, so running from a subdirectory of
//!   a project still finds the project's config.
//! - **Global**: `config.yaml` under the platform's config directory — on
//!   Linux `$XDG_CONFIG_HOME/sendra` (i.e. `~/.config/sendra` by default), on
//!   macOS `~/Library/Application Support/sendra`, on Windows
//!   `%APPDATA%\sendra`. See [`global_config_path`].
//!
//! Project values override global values **per key**, not per file: a project
//! config that sets only a timeout still inherits the global default headers.
//! No config file anywhere is a perfectly ordinary state — everything falls
//! back to the hardcoded defaults in [`Config::default`].
//!
//! The schema is deliberately tiny (default headers, default timeout). This
//! module exists to prove the *resolution* mechanism; fields get added to
//! [`ConfigFile`] as features that need them land, not in advance.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{Request, SendraError};

/// File name of a config file, under `.sendra/` in a project and directly
/// under the global config directory.
const CONFIG_FILE_NAME: &str = "config.yaml";

/// Directory a project keeps its Sendra files in: `config.yaml` directly
/// inside it, and the environment files of
/// [`crate::environment`] under `environments/`. A directory rather than a bare
/// `.sendra.yaml` precisely so that the second of those had somewhere obvious
/// to go.
pub(crate) const PROJECT_DIR_NAME: &str = ".sendra";

/// Name of the global config directory, under the platform config root.
const APP_DIR_NAME: &str = "sendra";

/// Timeout applied when no config file sets one.
///
/// 30 seconds: long enough that a slow-but-working API is not cut off, short
/// enough that a hung connection fails within a coffee sip rather than hanging
/// a script forever. reqwest applies no timeout at all by default, which is the
/// one option a command-line tool should not have.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// One config file, exactly as it appears on disk.
///
/// Every field is optional, and stays optional after parsing, because that
/// optionality *is* the merge information: `None` means "this file said
/// nothing about it", which is what lets a project file override one key
/// without silently resetting the others. The all-decided resolved form is
/// [`Config`].
///
/// ```text
/// headers:                 # merged into every request; the request wins ties
///   User-Agent: sendra
///   Accept: application/json
/// timeout_seconds: 10      # whole-request timeout, connect through body read
/// ```
///
/// Unknown keys are rejected, matching [`Request`] and
/// [`Collection`](crate::Collection): a typo in a config key would otherwise be
/// a setting that silently never applies, which is worse here than in a request
/// file — there is no response in which to notice it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    /// Headers merged into every request. A header set by the request itself
    /// wins.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,

    /// Whole-request timeout in seconds.
    ///
    /// Seconds as an integer, with the unit in the key name, rather than a
    /// duration string like `"30s"`: there is then nothing to parse, no way to
    /// read the unit wrong, and no syntax to stay compatible with if a richer
    /// duration format is wanted later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
}

impl ConfigFile {
    /// Parse a config file from a YAML string.
    pub fn from_yaml_str(yaml: &str) -> Result<Self, SendraError> {
        Self::parse(yaml, SendraError::ParseStr)
    }

    /// Read and parse a config file from disk.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, SendraError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|source| SendraError::ConfigIo {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&raw, |source| SendraError::ConfigParse {
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
        // An empty file (or one that is nothing but comments) is YAML null,
        // which serde cannot deserialize into a struct even when every field
        // has a default. Creating `.sendra/config.yaml` and filling it in later
        // is too reasonable a thing to do for it to be an error, so null is
        // read as an empty config.
        let probe: serde_yaml::Value = serde_yaml::from_str(yaml).map_err(&wrap)?;
        if probe.is_null() {
            return Ok(Self::default());
        }
        serde_yaml::from_str(yaml).map_err(&wrap)
    }

    /// Merge `self` over `base`, key by key, with `self` winning.
    ///
    /// Per key rather than per file: a project config that sets only
    /// `timeout_seconds` must not discard the global config's `headers`. The
    /// header maps merge the same way one level down, so a project can override
    /// one default header without dropping the rest.
    fn merge_over(self, base: Self) -> Self {
        let mut headers = base.headers;
        for (name, value) in self.headers {
            insert_overriding(&mut headers, &name, &value);
        }

        Self {
            headers,
            timeout_seconds: self.timeout_seconds.or(base.timeout_seconds),
        }
    }
}

/// Resolved, ready-to-use configuration: every field decided, no `Option`s
/// left. Built by merging whichever config files exist over the hardcoded
/// defaults, so the rest of the crate never has to ask whether a file was
/// found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Headers added to every request that does not set them itself.
    pub headers: BTreeMap<String, String>,
    /// Whole-request timeout: connect, send and body read.
    pub timeout: Duration,
    /// The config files this was built from, in the order they were merged
    /// (global first). Empty when no config file was found anywhere.
    pub sources: Vec<PathBuf>,
}

impl Default for Config {
    /// What Sendra does with no config file anywhere: no extra headers, and
    /// [`DEFAULT_TIMEOUT`].
    fn default() -> Self {
        Self {
            headers: BTreeMap::new(),
            timeout: DEFAULT_TIMEOUT,
            sources: Vec::new(),
        }
    }
}

impl Config {
    /// Resolve configuration for a run starting from the current directory.
    ///
    /// The walk-up starts at the working directory rather than at the request
    /// file's directory, so which config applies depends on where you are, the
    /// same way `git status` does. `sendra run ../other/req.yaml` uses *this*
    /// project's defaults, which is the reading that stays predictable when a
    /// path is typed by hand.
    pub fn resolve() -> Result<Self, SendraError> {
        let cwd = std::env::current_dir().map_err(SendraError::CurrentDir)?;
        Self::resolve_from(&cwd, global_config_path().as_deref())
    }

    /// The resolution itself, with both starting points passed in.
    ///
    /// [`Config::resolve`] is this with the real working directory and the real
    /// global path. Taking them as arguments keeps the merge logic testable
    /// against temporary directories without setting process-global environment
    /// variables or changing the working directory, neither of which tests
    /// running in parallel threads can do safely.
    ///
    /// `global_config` is the config *file*, not its directory, and neither
    /// path needs to exist: a missing file is not an error, only an unreadable
    /// or unparseable one is.
    pub fn resolve_from(
        start_dir: &Path,
        global_config: Option<&Path>,
    ) -> Result<Self, SendraError> {
        let mut sources = Vec::new();
        let mut merged = ConfigFile::default();

        // Global first, then project on top of it: later files win.
        for path in [
            global_config.map(Path::to_path_buf),
            find_project_config(start_dir),
        ]
        .into_iter()
        .flatten()
        {
            if !path.is_file() {
                continue;
            }
            merged = ConfigFile::from_path(&path)?.merge_over(merged);
            sources.push(path);
        }

        Ok(Self {
            headers: merged.headers,
            timeout: merged
                .timeout_seconds
                .map_or(DEFAULT_TIMEOUT, Duration::from_secs),
            sources,
        })
    }

    /// Apply this config to `request`, returning the request as it will be
    /// sent.
    ///
    /// Only the headers show up on a [`Request`]; the timeout is applied by
    /// [`send`](crate::send) when it builds the client. A config header is
    /// added only when the request does not already set one with that name,
    /// **compared case-insensitively**, because HTTP header names are
    /// case-insensitive: a config `User-Agent` and a request `user-agent` are
    /// the same header, and keeping both as separate map keys would leave which
    /// one survives up to the HTTP client rather than to the stated rule.
    pub fn apply(&self, request: &Request) -> Request {
        let mut applied = request.clone();
        for (name, value) in &self.headers {
            insert_if_absent(&mut applied.headers, name, value);
        }
        applied
    }
}

/// Insert `name: value` unless a header with that name is already present
/// under any casing.
fn insert_if_absent(headers: &mut BTreeMap<String, String>, name: &str, value: &str) {
    if headers
        .keys()
        .any(|existing| existing.eq_ignore_ascii_case(name))
    {
        return;
    }
    headers.insert(name.to_string(), value.to_string());
}

/// Insert `name: value`, dropping any header already present under a different
/// casing so the same header cannot end up in the map twice.
fn insert_overriding(headers: &mut BTreeMap<String, String>, name: &str, value: &str) {
    headers.retain(|existing, _| !existing.eq_ignore_ascii_case(name));
    headers.insert(name.to_string(), value.to_string());
}

/// Walk up from `start_dir` looking for `.sendra/config.yaml`, returning the
/// first one found.
///
/// This is how a command run from `crates/api/tests/` still picks up the config
/// at the repository root — the same search git does for `.git`. The walk goes
/// all the way to the filesystem root: stopping at a repository boundary would
/// make Sendra behave differently inside and outside a git checkout, for a tool
/// that otherwise has nothing to do with git.
///
/// Nearest wins, and only the nearest is read. A `.sendra/config.yaml` further
/// up is not merged in as a third layer: stacking project configs would make
/// what a directory resolves to depend on a file the reader has no particular
/// reason to look at, and "settings for everything" is what the global config
/// is already for.
pub fn find_project_config(start_dir: &Path) -> Option<PathBuf> {
    start_dir
        .ancestors()
        .map(|dir| dir.join(PROJECT_DIR_NAME).join(CONFIG_FILE_NAME))
        .find(|candidate| candidate.is_file())
}

/// Path to the global config file, or `None` if the platform cannot say where
/// config belongs (a daemon with no home directory, say) — in which case there
/// is simply no global config.
///
/// `$XDG_CONFIG_HOME` is honoured first, on every platform, when it is set to
/// an absolute path (the XDG spec says to ignore a relative one). On Linux that
/// is exactly what [`dirs::config_dir`] already does; the explicit check
/// extends it to macOS and Windows, where the crate returns the native location
/// instead. That is a deliberate deviation: someone who has set
/// `XDG_CONFIG_HOME` has said where their config lives, and the check costs
/// nothing on Windows, where the variable is effectively never set.
pub fn global_config_path() -> Option<PathBuf> {
    let root = match std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        _ => dirs::config_dir()?,
    };
    Some(root.join(APP_DIR_NAME).join(CONFIG_FILE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::Method;

    /// Write `contents` to `path`, creating the directories above it.
    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().expect("a file has a parent")).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    /// A project root under `dir` with `.sendra/config.yaml` holding `config`.
    fn project(dir: &Path, config: &str) -> PathBuf {
        let root = dir.join("project");
        write(&root.join(PROJECT_DIR_NAME).join(CONFIG_FILE_NAME), config);
        root
    }

    /// A global config file under `dir` holding `config`.
    fn global(dir: &Path, config: &str) -> PathBuf {
        let path = dir.join("global").join(APP_DIR_NAME).join(CONFIG_FILE_NAME);
        write(&path, config);
        path
    }

    fn request_with_headers(headers: &[(&str, &str)]) -> Request {
        Request {
            name: None,
            method: Method::Get,
            url: "https://example.com".to_string(),
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            body: None,
            assertions: None,
            pre_request: None,
            post_request: None,
            capture: None,
        }
    }

    #[test]
    fn no_config_anywhere_falls_back_to_the_hardcoded_defaults() {
        let temp = tempfile::tempdir().unwrap();
        // An empty directory, and a global path that does not exist: both
        // absent is an ordinary state, not an error.
        let missing = temp.path().join("nowhere").join(CONFIG_FILE_NAME);

        let config = Config::resolve_from(temp.path(), Some(&missing))
            .expect("no config file is not a failure");

        assert_eq!(config, Config::default());
        assert!(config.headers.is_empty());
        assert_eq!(config.timeout, DEFAULT_TIMEOUT);
        assert!(config.sources.is_empty(), "nothing was read");
    }

    #[test]
    fn a_global_config_applies_when_there_is_no_project_config() {
        let temp = tempfile::tempdir().unwrap();
        let global = global(
            temp.path(),
            "headers:\n  User-Agent: sendra-global\ntimeout_seconds: 5\n",
        );
        // A directory with no `.sendra` above it anywhere inside the tempdir.
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();

        let config = Config::resolve_from(&elsewhere, Some(&global)).unwrap();

        assert_eq!(
            config.headers.get("User-Agent").map(String::as_str),
            Some("sendra-global")
        );
        assert_eq!(config.timeout, Duration::from_secs(5));
        assert_eq!(config.sources, vec![global]);
    }

    #[test]
    fn a_project_config_applies_when_there_is_no_global_config() {
        let temp = tempfile::tempdir().unwrap();
        let root = project(
            temp.path(),
            "headers:\n  X-Project: yes\ntimeout_seconds: 7\n",
        );

        let config = Config::resolve_from(&root, None).expect("no global config is fine");

        assert_eq!(
            config.headers.get("X-Project").map(String::as_str),
            Some("yes")
        );
        assert_eq!(config.timeout, Duration::from_secs(7));
        assert_eq!(
            config.sources,
            vec![root.join(PROJECT_DIR_NAME).join(CONFIG_FILE_NAME)]
        );
    }

    #[test]
    fn project_values_override_global_values_key_by_key_not_file_by_file() {
        let temp = tempfile::tempdir().unwrap();
        // Global sets both keys; the project overrides only the timeout.
        let global = global(
            temp.path(),
            "headers:\n  User-Agent: sendra-global\n  Accept: application/json\ntimeout_seconds: 60\n",
        );
        let root = project(temp.path(), "timeout_seconds: 3\n");

        let config = Config::resolve_from(&root, Some(&global)).unwrap();

        // The overridden key takes the project's value...
        assert_eq!(config.timeout, Duration::from_secs(3));
        // ...and the key the project said nothing about survives from global.
        // This is the whole point: a partial project config is a patch, not a
        // replacement.
        assert_eq!(
            config.headers.get("User-Agent").map(String::as_str),
            Some("sendra-global")
        );
        assert_eq!(
            config.headers.get("Accept").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(config.sources.len(), 2, "both files were read");
    }

    #[test]
    fn header_maps_merge_per_key_too() {
        let temp = tempfile::tempdir().unwrap();
        let global = global(
            temp.path(),
            "headers:\n  User-Agent: sendra-global\n  Accept: application/json\n",
        );
        // Overriding one header must not drop the other.
        let root = project(temp.path(), "headers:\n  User-Agent: sendra-project\n");

        let config = Config::resolve_from(&root, Some(&global)).unwrap();

        assert_eq!(
            config.headers.get("User-Agent").map(String::as_str),
            Some("sendra-project")
        );
        assert_eq!(
            config.headers.get("Accept").map(String::as_str),
            Some("application/json")
        );
        // No timeout in either file, so the hardcoded default still stands.
        assert_eq!(config.timeout, DEFAULT_TIMEOUT);
    }

    #[test]
    fn a_project_header_overrides_a_global_one_spelled_with_different_casing() {
        let temp = tempfile::tempdir().unwrap();
        let global = global(temp.path(), "headers:\n  User-Agent: sendra-global\n");
        let root = project(temp.path(), "headers:\n  user-agent: sendra-project\n");

        let config = Config::resolve_from(&root, Some(&global)).unwrap();

        // One header, not two: HTTP header names are case-insensitive.
        assert_eq!(config.headers.len(), 1, "got {:?}", config.headers);
        assert_eq!(
            config.headers.values().next().map(String::as_str),
            Some("sendra-project")
        );
    }

    #[test]
    fn the_config_at_the_project_root_is_found_from_a_nested_subdirectory() {
        let temp = tempfile::tempdir().unwrap();
        let root = project(temp.path(), "headers:\n  X-Project: yes\n");
        // Several levels down, the way `crates/api/tests` sits under a repo.
        let nested = root.join("crates").join("api").join("tests");
        std::fs::create_dir_all(&nested).unwrap();

        let found = find_project_config(&nested).expect("the walk-up must reach the root");
        assert_eq!(
            found,
            root.join(PROJECT_DIR_NAME).join(CONFIG_FILE_NAME),
            "the config at the project root should have been found from {}",
            nested.display()
        );

        // And the resolved config is the same as it is from the root itself.
        assert_eq!(
            Config::resolve_from(&nested, None).unwrap().headers,
            Config::resolve_from(&root, None).unwrap().headers
        );
    }

    #[test]
    fn the_nearest_project_config_wins_over_one_further_up() {
        let temp = tempfile::tempdir().unwrap();
        let outer = project(temp.path(), "headers:\n  X-Which: outer\n");
        let inner = outer.join("nested");
        write(
            &inner.join(PROJECT_DIR_NAME).join(CONFIG_FILE_NAME),
            "headers:\n  X-Which: inner\n",
        );

        let config = Config::resolve_from(&inner, None).unwrap();
        assert_eq!(
            config.headers.get("X-Which").map(String::as_str),
            Some("inner")
        );
        assert_eq!(config.sources.len(), 1, "only the nearest is read");
    }

    #[test]
    fn malformed_yaml_in_a_config_file_is_a_typed_error_carrying_the_path() {
        let temp = tempfile::tempdir().unwrap();
        // Unclosed flow sequence: not valid YAML at all.
        let root = project(temp.path(), "headers: [oops\n");

        let err = Config::resolve_from(&root, None).expect_err("malformed config must error");
        match err {
            SendraError::ConfigParse { path, .. } => assert_eq!(
                path,
                root.join(PROJECT_DIR_NAME).join(CONFIG_FILE_NAME),
                "the error should name the file to fix"
            ),
            other => panic!("expected ConfigParse, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_config_key_is_rejected_rather_than_ignored() {
        let temp = tempfile::tempdir().unwrap();
        // `timeout` instead of `timeout_seconds`: a typo that would otherwise
        // be a setting that silently never applies.
        let root = project(temp.path(), "timeout: 5\n");

        let err = Config::resolve_from(&root, None).expect_err("a typo must not be ignored");
        assert!(
            matches!(err, SendraError::ConfigParse { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn a_wrongly_typed_config_value_is_a_parse_error() {
        let err = ConfigFile::from_yaml_str("timeout_seconds: soon\n")
            .expect_err("seconds must be a number");
        assert!(matches!(err, SendraError::ParseStr(_)), "got {err:?}");
    }

    #[test]
    fn an_empty_config_file_is_an_empty_config_not_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let root = project(temp.path(), "# nothing set yet\n");

        let config = Config::resolve_from(&root, None).expect("an empty file is valid");
        assert_eq!(config.headers, BTreeMap::new());
        assert_eq!(config.timeout, DEFAULT_TIMEOUT);
        // It was still read, so `sources` reflects what was on disk.
        assert_eq!(config.sources.len(), 1);
    }

    #[test]
    fn config_headers_are_added_to_a_request_that_does_not_set_them() {
        let config = Config {
            headers: BTreeMap::from([("User-Agent".to_string(), "sendra".to_string())]),
            ..Config::default()
        };

        let applied = config.apply(&request_with_headers(&[("Accept", "text/plain")]));

        assert_eq!(
            applied.headers.get("User-Agent").map(String::as_str),
            Some("sendra")
        );
        assert_eq!(
            applied.headers.get("Accept").map(String::as_str),
            Some("text/plain")
        );
    }

    #[test]
    fn a_request_header_beats_the_config_default_of_the_same_name() {
        let config = Config {
            headers: BTreeMap::from([("User-Agent".to_string(), "from-config".to_string())]),
            ..Config::default()
        };

        let applied = config.apply(&request_with_headers(&[("User-Agent", "from-request")]));

        assert_eq!(
            applied.headers.get("User-Agent").map(String::as_str),
            Some("from-request")
        );
    }

    #[test]
    fn a_request_header_beats_a_config_default_spelled_with_different_casing() {
        let config = Config {
            headers: BTreeMap::from([("User-Agent".to_string(), "from-config".to_string())]),
            ..Config::default()
        };

        let applied = config.apply(&request_with_headers(&[("user-agent", "from-request")]));

        // One header, and it is the request's: sending both and letting the
        // HTTP client pick would make the documented rule a coin flip.
        assert_eq!(applied.headers.len(), 1, "got {:?}", applied.headers);
        assert_eq!(
            applied.headers.get("user-agent").map(String::as_str),
            Some("from-request")
        );
    }

    #[test]
    fn applying_a_config_changes_nothing_else_about_the_request() {
        let config = Config {
            headers: BTreeMap::from([("X-Added".to_string(), "1".to_string())]),
            ..Config::default()
        };
        let request = Request {
            name: Some("Create".to_string()),
            method: Method::Post,
            url: "https://example.com/things".to_string(),
            headers: BTreeMap::new(),
            body: Some("{}".to_string()),
            // Config merges headers and nothing else; assertions are checked
            // against the response, which a default header cannot change, and
            // scripts run later still — the config has finished by then.
            assertions: Some(crate::Assertions {
                status: Some(200),
                ..crate::Assertions::default()
            }),
            pre_request: Some(
                "request.url = request.url;
"
                .to_string(),
            ),
            post_request: Some(
                "// nothing
"
                .to_string(),
            ),
            capture: Some(
                [("id".to_string(), "$.id".to_string())]
                    .into_iter()
                    .collect(),
            ),
        };

        let applied = config.apply(&request);

        assert_eq!(applied.name, request.name);
        assert_eq!(applied.method, request.method);
        assert_eq!(applied.url, request.url);
        assert_eq!(applied.body, request.body);
        assert_eq!(applied.pre_request, request.pre_request);
        assert_eq!(applied.post_request, request.post_request);
        assert_eq!(applied.assertions, request.assertions);
    }

    #[test]
    fn the_default_config_leaves_a_request_untouched() {
        let request = request_with_headers(&[("Accept", "application/json")]);
        assert_eq!(Config::default().apply(&request), request);
    }

    #[test]
    fn the_global_config_path_ends_where_it_should() {
        // Whatever the platform root turns out to be, the tail is ours.
        let Some(path) = global_config_path() else {
            // No home directory in this environment: no global config, which
            // `resolve_from` already treats as an ordinary state.
            return;
        };
        assert!(
            path.ends_with(Path::new(APP_DIR_NAME).join(CONFIG_FILE_NAME)),
            "got {}",
            path.display()
        );
        assert!(path.is_absolute(), "got {}", path.display());
    }
}
