//! [`Auth`]/[`BasicAuth`]/[`ApiKeyAuth`]: [`crate::Request::auth`]'s shape,
//! resolved to a header or query parameter by [`crate::Request::resolve_auth`].
//!
//! Also reused verbatim as [`crate::environment::Environment::auth`] — an
//! environment-level default applied to a request that sets no `auth:` of
//! its own — so the validation this module owns (mutual exclusivity,
//! header/query collision) is written here once and called from both
//! [`crate::Request::validate`] and [`crate::environment`], never
//! duplicated.

use serde::{Deserialize, Serialize};

/// [`crate::Request::auth`]: exactly one of `bearer`, `basic`, `api_key` or
/// `oauth`, enforced by [`Auth::validate_exclusivity`].
///
/// This is also the shape a default `auth:` at the environment level reuses
/// unchanged (see [`crate::environment::Environment::auth`]) — `api_key`
/// and `oauth` join `bearer`/`basic` as mutually-exclusive cases in that same
/// shape, so kept as its own type rather than inlined onto `Request`, the
/// same way `MultipartPart` is its own type rather than an inline tuple.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Auth {
    /// Sets `Authorization: Bearer <bearer>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer: Option<String>,
    /// Sets `Authorization: Basic <base64(user:pass)>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basic: Option<BasicAuth>,
    /// Sets a named header or query parameter to a static value — see
    /// [`ApiKeyAuth`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<ApiKeyAuth>,
    /// Acquires a bearer token from an OAuth token endpoint before the
    /// request is sent — see [`OAuthAuth`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthAuth>,
}

impl Auth {
    /// Exactly one of `bearer`/`basic`/`api_key`/`oauth` must be set. Called
    /// wherever an `Auth` value is parsed — a request's own `auth:` block
    /// ([`crate::Request::validate`]) and an environment's default
    /// ([`crate::environment::Environment::from_yaml_str`]/`from_path`) —
    /// so the rule is written once and cannot drift between the two.
    pub(crate) fn validate_exclusivity(&self) -> Result<(), String> {
        let mut set = Vec::new();
        if self.bearer.is_some() {
            set.push("bearer");
        }
        if self.basic.is_some() {
            set.push("basic");
        }
        if self.api_key.is_some() {
            set.push("api_key");
        }
        if self.oauth.is_some() {
            set.push("oauth");
        }
        if set.len() != 1 {
            return Err(format!(
                "exactly one of `auth.bearer`, `auth.basic`, `auth.api_key` or `auth.oauth` must \
                 be set, but found: {}",
                if set.is_empty() {
                    "neither".to_string()
                } else {
                    set.join(", ")
                }
            ));
        }
        Ok(())
    }

    /// Whether sending this `auth` would collide with an explicit header or
    /// query entry already on the request — the same "two things claiming
    /// ownership of one header" rule for every case `auth` can set:
    /// `bearer`/`basic` vs. an explicit `Authorization` header, and
    /// `api_key` vs. an explicit `headers:`/`query:` entry of the same name.
    ///
    /// Reused for a request's own `auth:` (checked once, at parse time,
    /// against the request's own `headers`/`query`) and for an
    /// environment-level default (checked once substitution has produced
    /// the final header/query names, since an environment's `auth:` cannot
    /// know at parse time what a request that later picks it up will look
    /// like) — see [`crate::Request::validate`] and
    /// [`crate::environment::Environment::apply`].
    pub(crate) fn collision_reason(
        &self,
        headers: &[(String, String)],
        query: &[(String, String)],
    ) -> Option<String> {
        if (self.bearer.is_some() || self.basic.is_some() || self.oauth.is_some())
            && headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("Authorization"))
        {
            return Some(
                "`auth` and an explicit `Authorization` header cannot both be set on the same \
                 request; remove one"
                    .to_string(),
            );
        }

        if let Some(api_key) = &self.api_key {
            match api_key.r#in {
                ApiKeyLocation::Header => {
                    if headers
                        .iter()
                        .any(|(name, _)| name.eq_ignore_ascii_case(&api_key.name))
                    {
                        return Some(format!(
                            "`auth.api_key` and an explicit `headers.{}` cannot both be set on \
                             the same request; remove one",
                            api_key.name
                        ));
                    }
                }
                ApiKeyLocation::Query => {
                    if query.iter().any(|(name, _)| name == &api_key.name) {
                        return Some(format!(
                            "`auth.api_key` and an explicit `query.{}` cannot both be set on the \
                             same request; remove one",
                            api_key.name
                        ));
                    }
                }
            }
        }

        None
    }
}

/// [`Auth::basic`]'s credentials, base64-encoded as `user:pass` by
/// [`crate::Request::resolve_auth`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct BasicAuth {
    pub user: String,
    pub pass: String,
}

/// [`Auth::api_key`]: a static value sent as either a header or a query
/// parameter.
///
/// ```text
/// auth:
///   api_key:
///     in: header          # or: query
///     name: X-API-Key     # or a query param name
///     value: {{api_key}}
/// ```
///
/// `in: header` sets the header named `name` to `value`, the same as an
/// entry under `headers:` would. `in: query` adds `name`/`value` as an
/// additional query parameter through the same mechanism
/// [`crate::Request::resolve_query`] already uses for `query:` — not a
/// separate ad-hoc code path — so it inherits that mechanism's percent-
/// encoding and its "the more structured source wins on a name collision
/// with the URL's own query string" rule. See
/// [`crate::Request::resolve_auth`] for exactly how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ApiKeyAuth {
    pub r#in: ApiKeyLocation,
    pub name: String,
    pub value: String,
}

/// Where [`ApiKeyAuth`] places its `name`/`value` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum ApiKeyLocation {
    Header,
    Query,
}

/// [`Auth::oauth`]: acquire a bearer token from an OAuth token endpoint
/// before the request is sent, under either of two grants.
///
/// ```text
/// auth:
///   oauth:
///     grant_type: client_credentials   # or: password
///     token_url: https://auth.example.com/token
///     client_id: {{client_id}}
///     client_secret: {{client_secret}}
///     scope: read write                 # optional
///     # required only for grant_type: password
///     username: {{username}}
///     password: {{password}}
/// ```
///
/// Only these two grants — neither `authorization_code` (it needs a browser
/// redirect and a local callback listener, a different problem for a
/// headless CLI) nor `refresh_token` (no cheaper once expiry-checking
/// exists, deferred) — see [`crate::oauth`]'s module docs for both.
///
/// [`crate::Request::resolve_oauth`] acquires the token — through
/// [`crate::oauth::OAuthTokenCache`], reusing one already acquired for the
/// same `token_url`/`client_id`/`grant_type`/`scope` within this run rather
/// than re-authenticating per request — and hands it to the exact same
/// `Authorization: Bearer` code path [`Auth::bearer`] already resolves to;
/// see [`crate::Request::resolve_auth`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct OAuthAuth {
    pub grant_type: OAuthGrantType,
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Required when [`grant_type`](Self::grant_type) is
    /// [`OAuthGrantType::Password`] — see [`validate_grant_fields`](Self::validate_grant_fields).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Required when [`grant_type`](Self::grant_type) is
    /// [`OAuthGrantType::Password`] — see [`validate_grant_fields`](Self::validate_grant_fields).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

impl OAuthAuth {
    /// `grant_type: password` requires both `username` and `password`;
    /// `grant_type: client_credentials` needs neither. Checked wherever
    /// [`Auth::validate_exclusivity`] already is — a request's own `auth:`
    /// block and an environment's default — for the same typed-error rigor
    /// every other schema rule in this project gets, rather than
    /// discovering the gap only once a token request is attempted.
    pub(crate) fn validate_grant_fields(&self) -> Result<(), String> {
        if self.grant_type == OAuthGrantType::Password
            && (self.username.is_none() || self.password.is_none())
        {
            return Err(
                "`auth.oauth` with `grant_type: password` requires both `username` and \
                 `password` to be set"
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// [`OAuthAuth::grant_type`]: which of the two supported OAuth grants to
/// use. See [`crate::oauth`]'s module docs for why only these two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OAuthGrantType {
    ClientCredentials,
    Password,
}

impl OAuthGrantType {
    /// The exact `grant_type` value OAuth's token request wire format
    /// expects — see [RFC 6749 §4.3.2/§4.4.2].
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            OAuthGrantType::ClientCredentials => "client_credentials",
            OAuthGrantType::Password => "password",
        }
    }
}
