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
//! The schema stays small on purpose, and grows only as a feature actually
//! needs a new key — [`ConfigFile`] documents the current full set. This
//! module keeps Sendra's header and certificate rules while dotcfg handles
//! project discovery, YAML reads, and global/project layer merging.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{Request, SendraError};

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

/// reqwest's own default: up to 10 redirects in a chain before giving up.
/// Sendra keeps this as its default too, so a config that never mentions
/// `follow_redirects` behaves exactly as it always has.
pub const DEFAULT_MAX_REDIRECTS: u32 = 10;

/// How a run treats an HTTP redirect: follow up to some maximum number of
/// hops, or not at all.
///
/// On disk this is the `follow_redirects` key, and it is deliberately
/// bool-or-number rather than two separate keys:
///
/// ```text
/// follow_redirects: false   # report the 3xx response itself, do not chase it
/// follow_redirects: true    # follow, up to the default of 10 hops
/// follow_redirects: 3       # follow, up to a custom maximum
/// ```
///
/// Leaving the key out entirely is the same as `true`: [`Config::default`]
/// resolves to [`FollowRedirects::Follow`] with [`DEFAULT_MAX_REDIRECTS`],
/// matching reqwest's own default and so changing nothing for a config that
/// does not touch this key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowRedirects {
    /// Follow a redirect chain up to this many hops before it is an error.
    Follow(u32),
    /// Do not follow redirects at all: a 3xx response is reported as-is,
    /// Location header and all, rather than chased.
    Disabled,
}

impl Default for FollowRedirects {
    fn default() -> Self {
        FollowRedirects::Follow(DEFAULT_MAX_REDIRECTS)
    }
}

/// Hand-written rather than `#[serde(untagged)]`, for the same reason as
/// `Request`'s header values: a value of the wrong shape should say "expected
/// `true`, `false`, or a number", not "data did not match any variant".
impl<'de> Deserialize<'de> for FollowRedirects {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct FollowRedirectsVisitor;

        impl serde::de::Visitor<'_> for FollowRedirectsVisitor {
            type Value = FollowRedirects;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("`true`, `false`, or a maximum number of redirects to follow")
            }

            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(if value {
                    FollowRedirects::default()
                } else {
                    FollowRedirects::Disabled
                })
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                u32::try_from(value)
                    .map(FollowRedirects::Follow)
                    .map_err(|_| E::custom("redirect limit is too large"))
            }

            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                if value < 0 {
                    return Err(E::custom("redirect limit cannot be negative"));
                }
                self.visit_u64(value as u64)
            }
        }

        deserializer.deserialize_any(FollowRedirectsVisitor)
    }
}

/// The inverse of the visitor above: `Disabled` writes back as `false`, and a
/// maximum writes back as the plain number — `true` never round-trips as
/// `true`, since a resolved maximum is exactly as meaningful and there is
/// only one of these in a merged [`Config`] to write back out anyway.
impl Serialize for FollowRedirects {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            FollowRedirects::Disabled => serializer.serialize_bool(false),
            FollowRedirects::Follow(max) => serializer.serialize_u32(*max),
        }
    }
}

/// Hand-written to match the hand-written [`Deserialize`] impl above:
/// `true`/`false`, or a non-negative integer maximum. The `minimum: 0` below
/// is one of the few `Request::validate`-adjacent business rules a JSON
/// Schema combinator can actually enforce, rather than merely document — see
/// `follow_redirects: -1`'s own parse-time rejection, which this mirrors.
#[cfg(feature = "schema")]
impl schemars::JsonSchema for FollowRedirects {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "FollowRedirects".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description": "Whether to follow redirects: `false` to report a 3xx response as-is, \
                `true` to follow up to the default maximum, or a non-negative integer maximum \
                number of hops.",
            "anyOf": [
                { "type": "boolean" },
                { "type": "integer", "minimum": 0 }
            ]
        })
    }
}

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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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

    /// Whether to follow redirects, and how many hops to allow. See
    /// [`FollowRedirects`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_redirects: Option<FollowRedirects>,

    /// Skip TLS certificate verification for every request this run sends —
    /// the config-file form of `--insecure`.
    ///
    /// A real security-relevant setting, not a convenience default: it exists
    /// for a self-signed or otherwise untrusted endpoint (an internal
    /// staging host, say) where there is no CA chain to verify against, and
    /// it disables the one thing standing between a request and a
    /// man-in-the-middle. See [`build_client`](crate::build_client) for where
    /// it is applied, and the CLI's `--insecure` doc comment for the warning
    /// printed whenever this resolves to `true`, from either source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insecure: Option<bool>,

    /// Route every request through this HTTP proxy — the config-file form of
    /// `--proxy`.
    ///
    /// ```text
    /// proxy: http://proxy.example.com:8080
    /// proxy: http://user:pass@proxy.example.com:8080   # credentials in the URL
    /// ```
    ///
    /// A plain URL string rather than a structured `{host, port, user, pass}`
    /// object: reqwest, which actually builds the proxying connector, already
    /// reads user/pass credentials straight out of the URL's userinfo, so a
    /// second, Sendra-specific way to spell the same thing would be a second
    /// thing to keep in sync with reqwest's own parsing rather than a real
    /// capability. Not validated here — an unparsable URL surfaces as a
    /// [`SendraError::Client`] when [`build_client`](crate::build_client)
    /// tries to build the client, the same place every other
    /// client-construction failure is reported.
    ///
    /// Setting this — from either the config file or `--proxy` — takes over
    /// proxying for the run entirely: the standard `HTTP_PROXY`/
    /// `HTTPS_PROXY`/`NO_PROXY` environment variables Sendra otherwise
    /// respects by default (matching curl, and every other common HTTP tool)
    /// are not consulted once an explicit proxy is configured. `None` — no
    /// `proxy:` key and no `--proxy` — is the plain "follow the environment,
    /// same as everyone else" default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,

    /// Present a client certificate for mutual TLS — the config-file form of
    /// `--client-cert`/`--client-key`.
    ///
    /// ```text
    /// client_cert:
    ///   cert: ./client.pem
    ///   key: ./client-key.pem
    /// ```
    ///
    /// PEM only, not PKCS#12: Sendra's `reqwest` is built against `rustls-tls`
    /// alone (no `native-tls`), and `reqwest::Identity`'s PKCS#12 and
    /// split-PEM constructors both require `native-tls` — pulling that in
    /// would mean shipping a second TLS backend just to accept one more input
    /// format. `Identity::from_pem` (the one constructor `rustls-tls` does
    /// expose) wants a single buffer containing both the certificate and its
    /// private key, so [`build_client`](crate::build_client) reads both files
    /// and concatenates them in memory before handing that buffer to reqwest.
    ///
    /// `cert`/`key` are resolved relative to *this config file's own
    /// directory*, not the current working directory — the same rule
    /// `body_file:` uses for the request file that names it, and for the same
    /// reason: a project config checked into version control should mean the
    /// same file on every machine it runs on, not whichever directory the
    /// command happened to be typed from. See
    /// [`resolve_client_cert_paths`]. `--client-cert`/`--client-key`, by
    /// contrast, resolve relative to the working directory, matching how
    /// every other CLI-supplied path (`--junit`, say) is read.
    ///
    /// Both halves are required together — a cert with no key, or a key with
    /// no cert, is refused as [`SendraError::ClientCertIncomplete`] when
    /// [`build_client`](crate::build_client) tries to use it, the same place
    /// every other client-construction failure is reported. That check runs
    /// after CLI overrides are folded in, so a config `cert:` paired with a
    /// `--client-key` override (or vice versa) is a valid combination, not an
    /// error — only ending up with just one side, from any mix of sources, is
    /// refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert: Option<ClientCertFile>,

    /// Persist cookies received via `Set-Cookie` and send them back
    /// automatically on later requests to the same host — the config-file
    /// form of `--cookie-jar`.
    ///
    /// **Opt-in, off by default**, deliberately matching curl: `curl` does
    /// not carry cookies between requests unless you pass `-c`/`-b`
    /// yourself, and Sendra follows the same convention rather than
    /// defaulting to "on" because it would be convenient for the
    /// login-flow case this exists for. See
    /// [`build_client`](crate::build_client) for where it is applied.
    ///
    /// In-memory only, for the duration of one invocation — nothing is
    /// written to disk, and nothing survives between separate `sendra run`/
    /// `sendra test` invocations, the same "no persistence across
    /// invocations" rule [`crate::capture`] already follows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookie_jar: Option<bool>,
}

/// The `client_cert:` config key's on-disk shape: a cert path and a key
/// path, both plain strings so [`resolve_client_cert_paths`] can rewrite a
/// relative one in place before it is ever turned into a [`PathBuf`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ClientCertFile {
    pub cert: String,
    pub key: String,
}

/// Resolve `client_cert.cert`/`client_cert.key` in `file`, if set and
/// relative, against the directory containing `config_path` — the config
/// file that named them, not the current working directory.
///
/// Called immediately after each config file is read, while the path it came
/// from is still in scope. A certificate path must be resolved against the
/// file that supplied it before the winning certificate pair is selected.
fn resolve_client_cert_paths(file: &mut ConfigFile, config_path: &Path) {
    let Some(client_cert) = &mut file.client_cert else {
        return;
    };
    let base = config_path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    for field in [&mut client_cert.cert, &mut client_cert.key] {
        let candidate = Path::new(field.as_str());
        if candidate.is_relative() {
            *field = base.join(candidate).to_string_lossy().into_owned();
        }
    }
}

impl ConfigFile {
    /// Parse a config file from a YAML string.
    pub fn from_yaml_str(yaml: &str) -> Result<Self, SendraError> {
        Self::parse(yaml, SendraError::ParseStr)
    }

    /// Read and parse one YAML config file through dotcfg, using its exact path.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, SendraError> {
        let path = path.as_ref();
        let cfg = dotcfg::DotCfg::new(APP_DIR_NAME).yaml().at_file(path);
        Self::from_source(&cfg, path)
    }

    /// Load one named stack source, preserving Sendra's path-bearing errors.
    fn from_source(source: &dotcfg::DotCfg, path: &Path) -> Result<Self, SendraError> {
        match source.load::<Self>() {
            Ok(Some(file)) => Ok(file),
            Ok(None) => Err(SendraError::ConfigIo {
                path: path.to_path_buf(),
                source: std::io::ErrorKind::NotFound.into(),
            }),
            Err(dotcfg::Error::Io(source)) => Err(SendraError::ConfigIo {
                path: path.to_path_buf(),
                source,
            }),
            Err(source) => Err(SendraError::ConfigParse {
                path: path.to_path_buf(),
                source: Box::new(source),
            }),
        }
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
    /// Whether to follow redirects, and how many hops to allow.
    pub redirects: FollowRedirects,
    /// Skip TLS certificate verification. See [`ConfigFile::insecure`].
    pub insecure: bool,
    /// Route every request through this HTTP proxy, or `None` to follow the
    /// standard proxy environment variables. See [`ConfigFile::proxy`].
    pub proxy: Option<String>,
    /// Client certificate file for mutual TLS, or `None` to present none. See
    /// [`ConfigFile::client_cert`]. Always set together with `client_key`, or
    /// not at all — [`build_client`](crate::build_client) refuses a `Config`
    /// where exactly one of the two is `Some`.
    pub client_cert: Option<PathBuf>,
    /// Private key matching `client_cert`. See [`ConfigFile::client_cert`].
    pub client_key: Option<PathBuf>,
    /// Persist and resend cookies automatically for this run. See
    /// [`ConfigFile::cookie_jar`].
    pub cookie_jar: bool,
    /// The project config this was built from, if one was found.
    pub project_config: Option<PathBuf>,
    /// The global config this was built from, if one was found.
    pub global_config: Option<PathBuf>,
}

impl Default for Config {
    /// What Sendra does with no config file anywhere: no extra headers,
    /// [`DEFAULT_TIMEOUT`], redirects followed up to
    /// [`DEFAULT_MAX_REDIRECTS`] — reqwest's own default — TLS verification
    /// on, and no explicit proxy (so the standard proxy environment
    /// variables apply, same as any other HTTP tool).
    fn default() -> Self {
        Self {
            headers: BTreeMap::new(),
            timeout: DEFAULT_TIMEOUT,
            redirects: FollowRedirects::default(),
            insecure: false,
            proxy: None,
            client_cert: None,
            client_key: None,
            cookie_jar: false,
            project_config: None,
            global_config: None,
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
        let mut stack = dotcfg::ConfigStack::new();
        let mut layer_paths = Vec::new();
        let mut headers = BTreeMap::new();
        let mut client_cert = None;

        // Build the stack once. Each source keeps the exact path selected by
        // the caller or discovered by dotcfg, including `.yml` files.
        for (name, path) in [
            ("global", global_config.map(Path::to_path_buf)),
            ("project", find_project_config(start_dir)),
        ] {
            let Some(path) = path else { continue };
            if !path.is_file() {
                continue;
            }
            let source = dotcfg::DotCfg::new(APP_DIR_NAME).yaml().at_file(&path);
            stack = stack.layer(name, source);
            layer_paths.push((name, path));
        }

        let stack = stack.build().expect("fixed source names are unique");
        let mut project_config = None;
        let mut global_config = None;
        // Inspect the named sources for Sendra-specific rules. ConfigStack
        // handles ordinary key precedence; HTTP header names are
        // case-insensitive and a client certificate pair is atomic, with
        // paths relative to the file that supplied it.
        for (name, path) in layer_paths {
            let source = stack.source(name).expect("layer was just added");
            let mut file = ConfigFile::from_source(source, &path)?;
            resolve_client_cert_paths(&mut file, &path);
            for (name, value) in file.headers {
                insert_overriding(&mut headers, &name, &value);
            }
            if file.client_cert.is_some() {
                client_cert = file.client_cert;
            }
            match name {
                "global" => global_config = Some(path),
                "project" => project_config = Some(path),
                _ => unreachable!("layer names are fixed"),
            }
        }

        let merged = stack
            .load::<ConfigFile>()
            .map_err(SendraError::ConfigMerge)?
            .unwrap_or_default();
        let (client_cert, client_key) = match client_cert {
            Some(client_cert) => (
                Some(PathBuf::from(client_cert.cert)),
                Some(PathBuf::from(client_cert.key)),
            ),
            None => (None, None),
        };

        Ok(Self {
            headers,
            timeout: merged
                .timeout_seconds
                .map_or(DEFAULT_TIMEOUT, Duration::from_secs),
            redirects: merged.follow_redirects.unwrap_or_default(),
            insecure: merged.insecure.unwrap_or(false),
            proxy: merged.proxy,
            client_cert,
            client_key,
            cookie_jar: merged.cookie_jar.unwrap_or(false),
            project_config,
            global_config,
        })
    }

    /// Apply this config to `request`, returning the request as it will be
    /// sent.
    ///
    /// Only the headers show up on a [`Request`]; the timeout is applied to the
    /// client [`build_client`](crate::build_client) makes for the run. A config header is
    /// added only when the request does not already set one with that name,
    /// **compared case-insensitively**, because HTTP header names are
    /// case-insensitive: a config `User-Agent` and a request `user-agent` are
    /// the same header, and adding both would give the request two entries
    /// under a name it never repeated itself, rather than to the stated rule.
    ///
    /// This is still a suppression, not a merge: `Request.headers` allowing a
    /// name to repeat is about what the *request* is allowed to say, not an
    /// invitation for a config default to duplicate something the request
    /// already set. A repeated header only happens when the request file (or
    /// a script) asks for it.
    pub fn apply(&self, request: &Request) -> Request {
        let mut applied = request.clone();
        for (name, value) in &self.headers {
            insert_if_absent(&mut applied.headers, name, value);
        }
        applied
    }
}

/// Push `name: value` unless a header with that name is already present
/// under any casing.
///
/// `pub(crate)` rather than private: [`Request::resolve_body`](crate::Request::resolve_body)
/// reuses this exact rule for the `Content-Type` a structured body implies —
/// set only when the request has not already said one itself, compared the
/// same case-insensitive way.
pub(crate) fn insert_if_absent(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    if headers
        .iter()
        .any(|(existing, _)| existing.eq_ignore_ascii_case(name))
    {
        return;
    }
    headers.push((name.to_string(), value.to_string()));
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
///
/// The walk itself is delegated to [`dotcfg`](https://docs.rs/dotcfg):
/// `DotCfg::new("sendra").yaml().find_in_ancestors_from(start)` searches the
/// same `.sendra/config.yaml` shape with the same nearest-wins, no-fallback
/// rule, so this stays byte-identical behavior with the discovery tested on
/// dotcfg's side. What stays Sendra-specific — and therefore here — is
/// Sendra-specific header merging, cert-path resolution, and
/// [`SendraError`] mapping.
/// [`global_config_path`] chooses dotcfg's native strategy unless an
/// absolute `XDG_CONFIG_HOME` explicitly requests XDG layout.
pub fn find_project_config(start_dir: &Path) -> Option<PathBuf> {
    dotcfg::DotCfg::new("sendra")
        .yaml()
        .find_in_ancestors_from(start_dir)
        .ok()
        .flatten()
        .and_then(|cfg| cfg.file_path().ok())
}

/// Path to the global `config.yaml`, or `None` if the platform cannot
/// determine its config directory.
///
/// An absolute `XDG_CONFIG_HOME` explicitly requests XDG layout, even on
/// macOS and Windows. Otherwise Sendra uses the platform-native location.
/// Dotcfg resolves both locations and constructs the file path.
pub fn global_config_path() -> Option<PathBuf> {
    let cfg = dotcfg::DotCfg::new(APP_DIR_NAME).yaml();
    let cfg = match std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => cfg.xdg(),
        _ => cfg.native(),
    };
    cfg.file_path().ok()
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

    /// Fixture file path via the same dotcfg handle resolution uses.
    fn cfg_file(dir: &Path) -> PathBuf {
        dotcfg::DotCfg::new(APP_DIR_NAME)
            .yaml()
            .at_dir(dir)
            .file_path()
            .unwrap()
    }

    /// A project root under `dir` with `.sendra/config.yaml` holding `config`.
    fn project(dir: &Path, config: &str) -> PathBuf {
        let root = dir.join("project");
        write(&cfg_file(&root.join(PROJECT_DIR_NAME)), config);
        root
    }

    /// A global config file under `dir` holding `config`.
    fn global(dir: &Path, config: &str) -> PathBuf {
        let path = cfg_file(&dir.join("global").join(APP_DIR_NAME));
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
    }

    #[test]
    fn no_config_anywhere_falls_back_to_the_hardcoded_defaults() {
        let temp = tempfile::tempdir().unwrap();
        // An empty directory, and a global path that does not exist: both
        // absent is an ordinary state, not an error.
        let missing = temp.path().join("nowhere").join("config.yaml");

        let config = Config::resolve_from(temp.path(), Some(&missing))
            .expect("no config file is not a failure");

        assert_eq!(config, Config::default());
        assert!(config.headers.is_empty());
        assert_eq!(config.timeout, DEFAULT_TIMEOUT);
        assert_eq!(config.project_config, None, "nothing was read");
        assert_eq!(config.global_config, None, "nothing was read");
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
        assert_eq!(config.global_config, Some(global));
        assert_eq!(config.project_config, None);
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
            config.project_config,
            Some(root.join(PROJECT_DIR_NAME).join("config.yaml"))
        );
        assert_eq!(config.global_config, None);
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
    fn malformed_yaml_in_a_config_file_is_a_typed_error_carrying_the_path() {
        let temp = tempfile::tempdir().unwrap();
        // Unclosed flow sequence: not valid YAML at all.
        let root = project(temp.path(), "headers: [oops\n");

        let err = Config::resolve_from(&root, None).expect_err("malformed config must error");
        match err {
            SendraError::ConfigParse { path, .. } => assert_eq!(
                path,
                root.join(PROJECT_DIR_NAME).join("config.yaml"),
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

    // --- `follow_redirects` -----------------------------------------------

    #[test]
    fn no_follow_redirects_key_resolves_to_the_default_of_ten() {
        let temp = tempfile::tempdir().unwrap();
        let root = project(temp.path(), "timeout_seconds: 5\n");

        let config = Config::resolve_from(&root, None).unwrap();

        assert_eq!(
            config.redirects,
            FollowRedirects::Follow(DEFAULT_MAX_REDIRECTS)
        );
    }

    #[test]
    fn follow_redirects_accepts_bool_and_number() {
        let temp = tempfile::tempdir().unwrap();

        let disabled =
            Config::resolve_from(&project(temp.path(), "follow_redirects: false\n"), None).unwrap();
        assert_eq!(disabled.redirects, FollowRedirects::Disabled);

        let enabled =
            Config::resolve_from(&project(temp.path(), "follow_redirects: true\n"), None).unwrap();
        assert_eq!(
            enabled.redirects,
            FollowRedirects::Follow(DEFAULT_MAX_REDIRECTS)
        );

        let custom =
            Config::resolve_from(&project(temp.path(), "follow_redirects: 3\n"), None).unwrap();
        assert_eq!(custom.redirects, FollowRedirects::Follow(3));
    }

    #[test]
    fn invalid_follow_redirects_values_are_parse_errors() {
        let negative = ConfigFile::from_yaml_str("follow_redirects: -1\n")
            .expect_err("a negative redirect count makes no sense");
        assert!(
            matches!(negative, SendraError::ParseStr(_)),
            "got {negative:?}"
        );

        let string = ConfigFile::from_yaml_str("follow_redirects: sometimes\n")
            .expect_err("a string is not a valid value");
        assert!(matches!(string, SendraError::ParseStr(_)), "got {string:?}");
        assert!(
            string.to_string().contains("could not parse"),
            "got {string}"
        );
    }

    // --- scalar keys ---------------------------------------------------------

    /// The plain scalar keys land on `Config` unchanged. Layer precedence is
    /// dotcfg's job, covered once by
    /// `project_values_override_global_values_key_by_key_not_file_by_file`.
    #[test]
    fn scalar_keys_resolve_from_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = project(
            temp.path(),
            "insecure: true\ncookie_jar: true\nproxy: http://user:pass@proxy.example.com:8080\n",
        );

        let config = Config::resolve_from(&root, None).unwrap();

        assert!(config.insecure);
        assert!(config.cookie_jar);
        // Credentials stay in the URL: reqwest parses them when the client is
        // built, so Sendra must not rewrite them.
        assert_eq!(
            config.proxy.as_deref(),
            Some("http://user:pass@proxy.example.com:8080")
        );
    }

    // --- `client_cert` -------------------------------------------------------

    #[test]
    fn a_relative_client_cert_resolves_against_the_project_configs_own_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = project(
            temp.path(),
            "client_cert:\n  cert: ./client.pem\n  key: ./client-key.pem\n",
        );

        let config = Config::resolve_from(&root, None).unwrap();

        // Relative to `.sendra/`, the directory the config file itself is
        // in — not the project root, and not the process's cwd.
        assert_eq!(
            config.client_cert,
            Some(root.join(PROJECT_DIR_NAME).join("client.pem"))
        );
        assert_eq!(
            config.client_key,
            Some(root.join(PROJECT_DIR_NAME).join("client-key.pem"))
        );
    }

    #[test]
    fn a_relative_client_cert_resolves_against_the_global_configs_own_directory_not_the_project() {
        // The global and project configs live under different roots in this
        // test; a relative `client_cert:` in the global file must resolve
        // against *its* directory even when a project config exists too.
        let temp = tempfile::tempdir().unwrap();
        let global = global(
            temp.path(),
            "client_cert:\n  cert: ./g.pem\n  key: ./g-key.pem\n",
        );
        let root = project(temp.path(), "timeout_seconds: 5\n");

        let config = Config::resolve_from(&root, Some(&global)).unwrap();

        assert_eq!(
            config.client_cert,
            Some(global.parent().unwrap().join("g.pem"))
        );
        assert_eq!(
            config.client_key,
            Some(global.parent().unwrap().join("g-key.pem"))
        );
    }

    #[test]
    fn an_absolute_client_cert_path_is_left_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let absolute = temp.path().join("elsewhere").join("client.pem");
        // A plain YAML scalar does not interpret `\`, so an absolute Windows
        // path is written as-is rather than escaped.
        let root = project(
            temp.path(),
            &format!(
                "client_cert:\n  cert: {}\n  key: ./client-key.pem\n",
                absolute.display()
            ),
        );

        let config = Config::resolve_from(&root, None).unwrap();

        assert_eq!(config.client_cert, Some(absolute));
    }

    #[test]
    fn a_project_client_cert_overrides_a_global_one_wholesale() {
        let temp = tempfile::tempdir().unwrap();
        let global = global(
            temp.path(),
            "client_cert:\n  cert: ./g.pem\n  key: ./g-key.pem\n",
        );
        let root = project(
            temp.path(),
            "client_cert:\n  cert: ./p.pem\n  key: ./p-key.pem\n",
        );

        let config = Config::resolve_from(&root, Some(&global)).unwrap();

        assert_eq!(
            config.client_cert,
            Some(root.join(PROJECT_DIR_NAME).join("p.pem"))
        );
        assert_eq!(
            config.client_key,
            Some(root.join(PROJECT_DIR_NAME).join("p-key.pem"))
        );
    }

    #[test]
    fn client_cert_with_only_a_cert_key_is_a_parse_error() {
        // `cert`/`key` are both required inside `client_cert:` — a config
        // that names only one is a malformed pair, not a partial setting to
        // merge with the other source later. See `Config::client_cert`'s doc
        // comment for the case that *is* allowed: a config cert paired with a
        // CLI-supplied key, or vice versa.
        let err = ConfigFile::from_yaml_str("client_cert:\n  cert: ./c.pem\n")
            .expect_err("`key` is required alongside `cert`");
        assert!(matches!(err, SendraError::ParseStr(_)), "got {err:?}");
    }

    #[test]
    fn an_unknown_key_inside_client_cert_is_rejected() {
        let err = ConfigFile::from_yaml_str(
            "client_cert:\n  cert: ./c.pem\n  key: ./k.pem\n  password: hunter2\n",
        )
        .expect_err("`password` is not a known field of `client_cert`");
        assert!(matches!(err, SendraError::ParseStr(_)), "got {err:?}");
    }

    #[test]
    fn an_empty_config_file_is_an_empty_config_not_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let root = project(temp.path(), "# nothing set yet\n");

        let config = Config::resolve_from(&root, None).expect("an empty file is valid");
        assert_eq!(config.headers, BTreeMap::new());
        assert_eq!(config.timeout, DEFAULT_TIMEOUT);
        // It was still read, so the provenance field reflects what was on disk.
        assert!(config.project_config.is_some());
        assert_eq!(config.global_config, None);
    }

    #[test]
    fn config_headers_are_added_to_a_request_that_does_not_set_them() {
        let config = Config {
            headers: BTreeMap::from([("User-Agent".to_string(), "sendra".to_string())]),
            ..Config::default()
        };

        let applied = config.apply(&request_with_headers(&[("Accept", "text/plain")]));

        assert_eq!(applied.header("User-Agent"), Some("sendra"));
        assert_eq!(applied.header("Accept"), Some("text/plain"));
    }

    #[test]
    fn a_request_header_beats_the_config_default_of_the_same_name() {
        let config = Config {
            headers: BTreeMap::from([("User-Agent".to_string(), "from-config".to_string())]),
            ..Config::default()
        };

        let applied = config.apply(&request_with_headers(&[("User-Agent", "from-request")]));

        assert_eq!(applied.header("User-Agent"), Some("from-request"));
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
        assert_eq!(applied.header("user-agent"), Some("from-request"));
    }

    #[test]
    fn a_config_default_is_suppressed_even_when_the_request_repeats_that_name() {
        // Repetition is a request-level choice; a config default of the same
        // name must still be suppressed rather than becoming a third value the
        // request never asked for.
        let config = Config {
            headers: BTreeMap::from([("X-Tag".to_string(), "from-config".to_string())]),
            ..Config::default()
        };
        let mut request = request_with_headers(&[]);
        request.headers = vec![
            ("X-Tag".to_string(), "one".to_string()),
            ("X-Tag".to_string(), "two".to_string()),
        ];

        let applied = config.apply(&request);

        assert_eq!(
            applied.headers,
            vec![
                ("X-Tag".to_string(), "one".to_string()),
                ("X-Tag".to_string(), "two".to_string()),
            ],
            "got {:?}",
            applied.headers
        );
    }

    #[test]
    fn a_request_that_repeats_a_header_keeps_both_after_config_is_applied() {
        // A config default of a *different* name must not disturb a request's
        // own repeated header.
        let config = Config {
            headers: BTreeMap::from([("User-Agent".to_string(), "sendra".to_string())]),
            ..Config::default()
        };
        let mut request = request_with_headers(&[]);
        request.headers = vec![
            ("X-Forwarded-For".to_string(), "1.2.3.4".to_string()),
            ("X-Forwarded-For".to_string(), "5.6.7.8".to_string()),
        ];

        let applied = config.apply(&request);

        let forwarded: Vec<&str> = applied
            .headers
            .iter()
            .filter(|(name, _)| name == "X-Forwarded-For")
            .map(|(_, value)| value.as_str())
            .collect();
        assert_eq!(forwarded, vec!["1.2.3.4", "5.6.7.8"]);
        assert_eq!(applied.header("User-Agent"), Some("sendra"));
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
            headers: Vec::new(),
            query: Vec::new(),
            body: Some("{}".to_string()),
            json: None,
            body_file: None,
            form: Vec::new(),
            multipart: Vec::new(),
            auth: None,
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
                [(
                    "id".to_string(),
                    crate::CaptureSource::JsonPath("$.id".to_string()),
                )]
                .into_iter()
                .collect(),
            ),
            retry: None,
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
}
