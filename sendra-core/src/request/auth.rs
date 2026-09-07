//! [`Auth`]/[`BasicAuth`]: [`crate::Request::auth`]'s shape, resolved to an
//! `Authorization` header by [`crate::Request::resolve_auth`].

use serde::{Deserialize, Serialize};

/// [`crate::Request::auth`]: exactly one of `bearer` or `basic`, enforced by
/// [`crate::Request::validate`].
///
/// This is also the shape a default `auth:` at the environment/config level
/// is expected to reuse unchanged, and the shape a future `api_key` variant
/// is expected to join as a third mutually-exclusive case — so kept as its
/// own type rather than inlined onto `Request`, the same way `MultipartPart`
/// is its own type rather than an inline tuple.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    /// Sets `Authorization: Bearer <bearer>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer: Option<String>,
    /// Sets `Authorization: Basic <base64(user:pass)>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basic: Option<BasicAuth>,
}

/// [`Auth::basic`]'s credentials, base64-encoded as `user:pass` by
/// [`crate::Request::resolve_auth`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BasicAuth {
    pub user: String,
    pub pass: String,
}
