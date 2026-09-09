//! [`Auth`]/[`BasicAuth`]/[`ApiKeyAuth`]: [`crate::Request::auth`]'s shape,
//! resolved to a header or query parameter by [`crate::Request::resolve_auth`].

use serde::{Deserialize, Serialize};

/// [`crate::Request::auth`]: exactly one of `bearer`, `basic` or `api_key`,
/// enforced by [`crate::Request::validate`].
///
/// This is also the shape a default `auth:` at the environment/config level
/// is expected to reuse unchanged — `api_key` joins `bearer`/`basic` as a
/// third mutually-exclusive case in that same shape, so kept as its own type
/// rather than inlined onto `Request`, the same way `MultipartPart` is its
/// own type rather than an inline tuple.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
}

/// [`Auth::basic`]'s credentials, base64-encoded as `user:pass` by
/// [`crate::Request::resolve_auth`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
