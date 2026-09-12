//! The sendra-core call sequences behind `view.rs`'s read-only browsing
//! preview and its auth-derived masking — pulled out of `view` because they
//! are domain computation (substitution, auth/query/body resolution, and
//! diffing what auth added), not rendering. Nothing here touches a `Frame`
//! or `AppState`; it only sequences [`sendra_core::Environment::apply`] and
//! [`sendra_core::Request`]'s own `resolve_*` methods so that sequence is
//! written — and executed — once, in one place, instead of being
//! re-derived independently by every render function that needs it.
//!
//! This does **not** cover every auth-preview computation in the crate:
//! `view::describe_resolved_auth` (the "Resolved auth →" line shown while
//! *editing* a request) diffs against a different "before" value — the
//! in-progress edit's own pre-substitution candidate, not the substituted
//! request [`substitute_and_resolve_auth`] returns — so it keeps its own,
//! differently-shaped diff rather than sharing [`added_by_auth_names`]. It
//! does reuse [`substitute_and_resolve_auth`] for the two sendra-core calls
//! themselves; see that function's own doc comment.

use std::collections::HashSet;
use std::path::Path;

use sendra_core::{Environment, Request, SendraError};

/// What the read-only browsing preview (`view::render_detail_pane`) shows
/// for a request: the fully resolved request (after substitution, auth,
/// query and body resolution), plus which header names (lower-cased,
/// matching HTTP's own case-insensitivity) and query parameter names on it
/// were added by resolving `auth:` rather than written in the request file
/// — what `view::format_resolved_request` needs to know which entries to
/// mask by default.
#[derive(Debug)]
pub(super) struct BrowsingPreview {
    pub(super) request: Request,
    pub(super) auth_headers: HashSet<String>,
    pub(super) auth_query: HashSet<String>,
}

/// The same substitution + auth/query/body resolution pipeline sendra-cli's
/// `--dry-run` runs before sending, reused directly rather than
/// reimplemented — see [`BrowsingPreview`]'s own doc comment for what this
/// returns. Deliberately narrower than the CLI's full pipeline in two ways,
/// both because this is a preview, not a run: it skips OAuth token
/// acquisition (`Request::resolve_oauth`), a real network call with no
/// place in drawing a frame, and it skips `Config::apply`, since the TUI
/// has no project `Config` loaded anywhere yet — so the headers shown here
/// are the request's own, substituted, not the final wire-level set an
/// actual run would send after config defaults are layered on.
/// `environment` is passed in as a real, caller-chosen value rather than
/// assumed here — an empty one when nothing is active, or the one the user
/// picked in the overlay — so any environment-level default (its own
/// `auth` block included) resolves exactly as `Environment::apply` already
/// defines it.
///
/// **Computes `Environment::apply`/`Request::resolve_auth` exactly once.**
/// `auth_headers`/`auth_query` are derived from the same pass that produces
/// `request`, not a second, independent re-run of those two calls the way
/// `render_detail_pane` used to make on top of its own call into this
/// pipeline (one for the resolved request, a separate one via
/// `auth_derived_entries` for the masking sets).
pub(super) fn resolve_browsing_preview(
    request: &Request,
    base_dir: &Path,
    environment: &Environment,
) -> Result<BrowsingPreview, SendraError> {
    let (substituted, auth_resolved) = substitute_and_resolve_auth(request, environment)?;
    let (auth_headers, auth_query) = added_by_auth_names(&substituted, &auth_resolved);
    let resolved = auth_resolved.resolve_query()?.resolve_body(base_dir)?;
    Ok(BrowsingPreview {
        request: resolved,
        auth_headers,
        auth_query,
    })
}

/// Substitutes `environment` into `request`, then resolves its `auth:`
/// block against the substituted result — the two-step
/// `Environment::apply`/`Request::resolve_auth` pipeline every auth-derived
/// computation in this crate starts with. Returns both the substituted
/// ("before auth") and resolved ("after auth") requests: every caller
/// diffs between two requests to find what auth actually added, and *which*
/// two requests it diffs differs per caller (see [`resolve_browsing_preview`]
/// above vs. `view::describe_resolved_auth`, which diffs against its own,
/// pre-substitution candidate instead of the `substituted` half returned
/// here) — but the two sendra-core calls that produce the "after" side
/// never do, so they are written and executed here once, not once per
/// caller.
pub(super) fn substitute_and_resolve_auth(
    request: &Request,
    environment: &Environment,
) -> Result<(Request, Request), SendraError> {
    let substituted = environment.apply(request)?;
    let resolved = substituted.resolve_auth()?;
    Ok((substituted, resolved))
}

/// Which header names (lower-cased, matching HTTP's own case-insensitivity)
/// and query parameter names appear in `after` but not, by name, in
/// `before` — i.e. what resolving auth added on top of whatever `before`
/// already had. Used to decide *which* entries a read-only preview must
/// mask (see [`BrowsingPreview`]) — this never looks at what those values
/// *are*, only at whether resolving auth is what put them there.
pub(super) fn added_by_auth_names(
    before: &Request,
    after: &Request,
) -> (HashSet<String>, HashSet<String>) {
    let header_names = after
        .headers
        .iter()
        .filter(|(name, _)| {
            !before
                .headers
                .iter()
                .any(|(existing, _)| existing.eq_ignore_ascii_case(name))
        })
        .map(|(name, _)| name.to_ascii_lowercase())
        .collect();
    let query_names = after
        .query
        .iter()
        .filter(|(name, _)| !before.query.iter().any(|(existing, _)| existing == name))
        .map(|(name, _)| name.clone())
        .collect();
    (header_names, query_names)
}
