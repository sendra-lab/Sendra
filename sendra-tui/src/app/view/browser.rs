//! The request list and the read-only preview pane: `render_request_list`,
//! `render_detail_pane` (which shows the edit form or a run's response
//! panel instead, once either is active — see its own doc comment), and the
//! preview's own resolution/formatting helpers (`format_resolved_request`,
//! `mask_auth_derived_query_params`, `truncate_body`).

use std::collections::HashSet;
use std::path::Path;

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use sendra_core::{Document, Environment, Request};

use super::super::preview::resolve_browsing_preview;
use super::super::state::AppState;
use super::edit_form::render_edit_pane;
use super::response::{render_response_panel, REDACTED_CAPTURE_VALUE};
use super::{active_environment, format_error};

/// Body preview is capped rather than shown in full — scrolling through a
/// large body is a separate concern from the preview pane; this just keeps
/// a multi-megabyte body from making every draw slower without ever
/// crashing on one.
const MAX_BODY_PREVIEW_CHARS: usize = 2000;

/// A `ListState` built fresh every frame, `offset` always starting at `0`,
/// deliberately — nothing in `AppState` tracks a scroll/viewport position
/// for this list. That is not a gap: ratatui's `List` widget computes its
/// own visible window from `state.selected` and `state.offset` on every
/// render (`ratatui_widgets::list::rendering::List::get_items_bounds`),
/// walking the offset forward or back as needed until the selected index
/// falls inside the area actually available — so handing it a fresh
/// `offset: 0` and the real `selected` index each frame reliably scrolls
/// the list to keep the selection visible, including the two directions a
/// hand-rolled viewport-window calculation would have to special-case
/// itself: scrolling down past the bottom of the current window, and
/// wrap-around (`move_selection` in `super::update`) snapping straight from
/// the last item back to the first, or the first back to the last, which
/// must re-scroll the window to the opposite end in one step. Verified, not
/// assumed — see `collection_browser_scrolls_to_keep_selection_visible`.
pub(crate) fn render_request_list(
    frame: &mut Frame,
    area: Rect,
    document: &Document,
    selected: usize,
    dirty_requests: &HashSet<usize>,
) {
    let items: Vec<ListItem> = document
        .requests()
        .iter()
        .enumerate()
        .map(|(index, request)| {
            let name = request.name.as_deref().unwrap_or("(unnamed)");
            // `*` for unsaved edits, matching the same marker convention as
            // an editor's modified-buffer indicator — a leading space in the
            // ordinary case keeps every row's method column aligned rather
            // than shifting only dirty rows one character right.
            let marker = if dirty_requests.contains(&index) {
                "*"
            } else {
                " "
            };
            ListItem::new(format!("{marker}{} {name}", request.method))
        })
        .collect();

    let list = List::new(items).highlight_style(Style::new().add_modifier(Modifier::REVERSED));

    let mut list_state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, area, &mut list_state);
}

/// Once a run has completed for the currently selected request, this pane
/// shows the result **instead of** the request preview — replace, not a
/// toggle or a second tab. Two reasons: there is no natural third state to
/// toggle back and forth between once the answer to "what did this request
/// actually do" exists (the preview is only ever a stand-in for that
/// answer), and `RunState` already resets to `Idle` on every request-list
/// selection change (see its doc comment), so the preview reappears on its
/// own the moment it is relevant again — a toggle key would be one more
/// binding for a state that already un-does itself.
pub(crate) fn render_detail_pane(
    frame: &mut Frame,
    area: Rect,
    document: &Document,
    selected: usize,
    base_dir: &Path,
    state: &AppState,
) {
    let Some(request) = document.requests().get(selected) else {
        frame.render_widget(Paragraph::new("No request selected."), area);
        return;
    };

    let active = active_environment(state);
    let environment = active.map_or_else(Environment::default, |named| named.environment.clone());

    // Edit mode takes over the whole detail pane — checked first, ahead of
    // the completed-run response panel below, since editing is allowed
    // (see `Message::EnterEditMode`'s doc comment) whether or not a run has
    // completed, and there is no meaningful "preview vs. response" split to
    // preserve underneath an edit in progress: `method`/`url` in `EditState`
    // are what is authoritative right now, not whatever the last resolved
    // preview or response showed.
    if let Some(edit) = &state.edit_mode {
        render_edit_pane(frame, area, edit, request, &environment);
        return;
    }

    if let Some(outcome) = state.current_run() {
        render_response_panel(
            frame,
            area,
            outcome,
            state.response_scroll,
            state.reveal_captures,
        );
        return;
    }

    let text = match resolve_browsing_preview(request, base_dir, &environment) {
        Ok(preview) => format_resolved_request(
            &preview.request,
            &preview.auth_headers,
            &preview.auth_query,
            state.reveal_captures,
        ),
        // Honest about what's actually active: with nothing selected yet
        // (the default before any environment is picked), a `{{var}}`
        // request surfaces the same `VariableNotFound` core itself raises
        // against an empty environment; with one selected, the same error
        // means that environment specifically does not define the
        // variable — either way, the real sendra-core error is shown,
        // never a faked value.
        Err(error) => match active {
            Some(named) => format_error(
                &format!(
                    "Could not resolve preview against environment '{}'",
                    named.name
                ),
                &error,
            ),
            None => format_error(
                "Could not resolve preview (no environment selected yet)",
                &error,
            ),
        },
    };

    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), area);
}

/// The read-only request preview `render_detail_pane` shows while browsing
/// (never while editing — see `render_edit_pane`'s own `Auth:` section,
/// which is unaffected by this and always shows the real values, since
/// editing them is the whole point there).
///
/// **Auth values are masked by default**, the same posture and the same
/// `<redacted>` placeholder `format_capture_section` already uses for
/// captured values, toggled by the same `c` keybinding
/// (`AppState::reveal_captures`) — a bearer token, a Basic password
/// (base64-encoded into the whole `Authorization` header, which is why the
/// entire header value is masked rather than trying to decode and mask
/// only the password half), or an API key are exactly the kind of secret a
/// screen-share or a terminal recording should not casually expose while
/// just browsing a collection, the same reasoning that already masks a
/// captured token. `auth_derived_headers`/`auth_derived_query` (see
/// `preview::BrowsingPreview`/`preview::added_by_auth_names`) name exactly
/// which entries came from resolving `auth:` rather than from the request's
/// own `headers`/`query`, so an ordinary, non-secret header (`Accept`, say)
/// is never masked. An OAuth-authed request never reaches this function
/// with anything to mask in the first place:
/// `preview::resolve_browsing_preview` skips real token acquisition, so
/// `resolve_auth` alone returns a typed error for it, shown as that error
/// instead of a resolved preview — see that function's own doc comment.
fn format_resolved_request(
    request: &Request,
    auth_derived_headers: &HashSet<String>,
    auth_derived_query: &HashSet<String>,
    reveal: bool,
) -> String {
    let mut lines = vec![
        format!("Method: {}", request.method),
        format!(
            "URL:    {}",
            mask_auth_derived_query_params(&request.url, auth_derived_query, reveal)
        ),
        String::new(),
        "Headers:".to_string(),
    ];

    if request.headers.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        for (name, value) in &request.headers {
            let masked = !reveal && auth_derived_headers.contains(&name.to_ascii_lowercase());
            let shown = if masked {
                REDACTED_CAPTURE_VALUE
            } else {
                value
            };
            lines.push(format!("  {name}: {shown}"));
        }
    }

    lines.push(String::new());
    lines.push("Body:".to_string());
    match request.body.as_deref().filter(|body| !body.is_empty()) {
        Some(body) => {
            let (preview, truncated) = truncate_body(body);
            lines.push(preview);
            if truncated {
                lines.push(format!(
                    "... [truncated to {MAX_BODY_PREVIEW_CHARS} characters]"
                ));
            }
        }
        None => lines.push("  (none)".to_string()),
    }

    lines.join("\n")
}

/// Masks the value of every query parameter in `url` whose name is in
/// `names` — what an `auth.api_key` placed `in: query` needs, since
/// `Request::resolve_query` merges it straight into the URL string rather
/// than leaving it as a separate, maskable `(name, value)` pair (see
/// `Request::resolve_query`'s own doc comment). Matches on the parameter
/// *name* only, never the value, so this works regardless of how the real
/// value was percent-encoded — there is no encoded form of a redaction
/// placeholder to get wrong. A no-op when `reveal` is true, `names` is
/// empty, or `url` has no query string at all.
fn mask_auth_derived_query_params(url: &str, names: &HashSet<String>, reveal: bool) -> String {
    if reveal || names.is_empty() {
        return url.to_string();
    }
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let masked = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((key, _value)) if names.contains(key) => {
                format!("{key}={REDACTED_CAPTURE_VALUE}")
            }
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&");
    format!("{base}?{masked}")
}

fn truncate_body(body: &str) -> (String, bool) {
    if body.chars().count() <= MAX_BODY_PREVIEW_CHARS {
        (body.to_string(), false)
    } else {
        (body.chars().take(MAX_BODY_PREVIEW_CHARS).collect(), true)
    }
}

#[cfg(test)]
mod tests {
    use sendra_core::{Document, Environment, SendraError};

    use crate::app::state::{AppState, Message};
    use crate::app::test_support::*;
    use crate::app::update::update;
    use crate::app::view::{status_help_text, view};

    use super::*;

    const REQUEST_WITH_BEARER_AUTH: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
    auth:
      bearer: secret-token
";

    #[test]
    fn collection_browser_scrolls_to_keep_selection_visible() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        const REQUEST_COUNT: usize = 30;
        let mut state = loaded_state(&many_request_collection(REQUEST_COUNT));

        // 10 rows total: 9 for the panes, 1 for the status bar — so well
        // under half of the 30 requests can be on screen at once.
        let render = |state: &AppState| {
            let backend = TestBackend::new(60, 10);
            let mut terminal = Terminal::new(backend).expect("a test terminal builds");
            terminal
                .draw(|frame| view(state, frame))
                .expect("rendering must not panic");
            buffer_to_string(terminal.backend().buffer())
        };

        let top_screen = render(&state);
        assert!(
            top_screen.contains("Request0"),
            "the first request must be visible at the very top:\n{top_screen}"
        );
        assert!(
            !top_screen.contains("Request29"),
            "the last request must not already be visible before scrolling down to it:\n{top_screen}"
        );

        // Move well past the bottom of the initial visible window.
        for _ in 0..20 {
            update(&mut state, Message::SelectNext);
        }
        assert_eq!(selected(&state), 20);
        let scrolled_down_screen = render(&state);
        assert!(
            scrolled_down_screen.contains("Request20"),
            "the newly selected request must have scrolled into view:\n{scrolled_down_screen}"
        );
        assert!(
            !scrolled_down_screen.contains("Request0"),
            "the list must have actually scrolled — the far-away top item must no longer \
             be on screen:\n{scrolled_down_screen}"
        );

        // Wrap forward from somewhere in the middle straight past the last
        // item back to the first.
        for _ in 0..10 {
            update(&mut state, Message::SelectNext);
        }
        assert_eq!(
            selected(&state),
            0,
            "SelectNext must wrap from the last request to the first"
        );
        let wrapped_to_top_screen = render(&state);
        assert!(
            wrapped_to_top_screen.contains("Request0"),
            "wrapping to the first request must scroll the list back to the top:\n{wrapped_to_top_screen}"
        );
        assert!(
            !wrapped_to_top_screen.contains("Request29"),
            "the list must not still be showing the bottom after wrapping to the top:\n{wrapped_to_top_screen}"
        );

        // And the reverse wrap: from the first request straight back to the
        // last.
        update(&mut state, Message::SelectPrevious);
        assert_eq!(
            selected(&state),
            REQUEST_COUNT - 1,
            "SelectPrevious must wrap from the first request to the last"
        );
        let wrapped_to_bottom_screen = render(&state);
        assert!(
            wrapped_to_bottom_screen.contains("Request29"),
            "wrapping to the last request must scroll the list down to show it:\n{wrapped_to_bottom_screen}"
        );
        assert!(
            !wrapped_to_bottom_screen.contains("Request0"),
            "the list must not still be showing the top after wrapping to the bottom:\n{wrapped_to_bottom_screen}"
        );
    }

    #[test]
    fn resolve_preview_succeeds_for_a_request_with_no_placeholders() {
        let document = Document::from_yaml_str(
            "name: One\nmethod: GET\nurl: https://example.com\nheaders:\n  Accept: application/json\n",
        )
        .expect("valid single request");
        let request = &document.requests()[0];

        let preview = resolve_browsing_preview(request, Path::new("."), &Environment::default())
            .expect("no placeholders to fail on");

        assert_eq!(preview.request.url, "https://example.com");
    }

    #[test]
    fn resolve_preview_surfaces_variable_not_found_with_no_environment() {
        let document = Document::from_yaml_str(
            "name: One\nmethod: GET\nurl: https://example.com/{{user_id}}\n",
        )
        .expect("valid single request");
        let request = &document.requests()[0];

        let error = resolve_browsing_preview(request, Path::new("."), &Environment::default())
            .expect_err(
                "a placeholder with no active environment must surface a real resolution error",
            );

        assert!(matches!(error, SendraError::VariableNotFound { .. }));
    }

    #[test]
    fn resolve_preview_resolves_against_a_real_environment() {
        let document = Document::from_yaml_str(
            "name: One\nmethod: GET\nurl: https://example.com/{{user_id}}\n",
        )
        .expect("valid single request");
        let request = &document.requests()[0];
        let mut environment = Environment::default();
        environment
            .variables
            .insert("user_id".to_string(), "42".to_string());

        let preview = resolve_browsing_preview(request, Path::new("."), &environment)
            .expect("the environment defines the variable the request needs");

        assert_eq!(preview.request.url, "https://example.com/42");
    }

    // --- Auth masking in the read-only preview -------------------------------
    //
    // `REQUEST_WITH_BEARER_AUTH` is defined further down, in the auth-editing
    // test section, and reused here too.

    const REQUEST_WITH_BASIC_AUTH_PREVIEW: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
    auth:
      basic:
        user: ada
        pass: hunter2
";

    const REQUEST_WITH_API_KEY_HEADER_AUTH: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
    headers:
      Accept: application/json
    auth:
      api_key:
        in: header
        name: X-Api-Key
        value: secret-key-value
";

    const REQUEST_WITH_API_KEY_QUERY_AUTH: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
    auth:
      api_key:
        in: query
        name: api_key
        value: secret-key-value
";

    #[test]
    fn browsing_preview_masks_a_bearer_token_by_default() {
        let state = loaded_state(REQUEST_WITH_BEARER_AUTH);

        let screen = render_screen(&state);

        assert!(
            !screen.contains("secret-token"),
            "the real bearer token must not appear while merely browsing:\n{screen}"
        );
        assert!(
            screen.contains(REDACTED_CAPTURE_VALUE),
            "the resolved Authorization header must show the redaction placeholder:\n{screen}"
        );
    }

    #[test]
    fn browsing_preview_masks_a_basic_auth_password_by_masking_the_whole_header() {
        let state = loaded_state(REQUEST_WITH_BASIC_AUTH_PREVIEW);

        let screen = render_screen(&state);

        // Basic auth base64-encodes `user:pass` into one opaque header value
        // — trivially reversible, so the whole header must be masked, not
        // just checked for the literal password substring.
        assert!(
            !screen.contains("aunter2") && !screen.contains("hunter2"),
            "the real password must not be recoverable from the screen:\n{screen}"
        );
        assert!(screen.contains(REDACTED_CAPTURE_VALUE), "{screen}");
    }

    #[test]
    fn browsing_preview_masks_an_api_key_placed_in_a_header_but_not_an_ordinary_header() {
        let state = loaded_state(REQUEST_WITH_API_KEY_HEADER_AUTH);

        let screen = render_screen(&state);

        assert!(
            !screen.contains("secret-key-value"),
            "the real api key value must not appear while merely browsing:\n{screen}"
        );
        assert!(
            screen.contains("X-Api-Key") && screen.contains(REDACTED_CAPTURE_VALUE),
            "the api key header must still be listed by name, with its value masked:\n{screen}"
        );
        assert!(
            screen.contains("Accept: application/json"),
            "an ordinary, non-auth header must never be masked:\n{screen}"
        );
    }

    #[test]
    fn browsing_preview_masks_an_api_key_placed_in_the_query_string() {
        let state = loaded_state(REQUEST_WITH_API_KEY_QUERY_AUTH);

        let screen = render_screen(&state);

        assert!(
            !screen.contains("secret-key-value"),
            "the real api key value must not appear in the URL while merely browsing:\n{screen}"
        );
        assert!(
            screen.contains(&format!("api_key={REDACTED_CAPTURE_VALUE}")),
            "the query parameter's name must stay visible, only its value masked:\n{screen}"
        );
    }

    #[test]
    fn pressing_c_reveals_auth_in_the_browsing_preview_and_hides_it_again() {
        let mut state = loaded_state(REQUEST_WITH_BEARER_AUTH);

        let masked = render_screen(&state);
        assert!(!masked.contains("secret-token"), "{masked}");

        update(&mut state, Message::ToggleRevealCaptures);
        let revealed = render_screen(&state);
        assert!(
            revealed.contains("Bearer secret-token"),
            "the same key that reveals captures must reveal auth too:\n{revealed}"
        );

        update(&mut state, Message::ToggleRevealCaptures);
        let masked_again = render_screen(&state);
        assert!(!masked_again.contains("secret-token"), "{masked_again}");
    }

    #[test]
    fn editing_the_request_always_shows_the_real_auth_value_regardless_of_reveal_state() {
        // The one place this masking must never apply: edit mode, where
        // seeing (and changing) the real value is the entire point — see
        // `render_edit_pane`'s own `Auth:` section and its "Resolved auth"
        // preview line, both untouched by this feature.
        let mut state = loaded_state(REQUEST_WITH_BEARER_AUTH);
        assert!(
            !state.reveal_captures,
            "masked by default, same as browsing"
        );

        update(&mut state, Message::EnterEditMode);
        let screen = render_screen(&state);

        assert!(
            screen.contains("secret-token"),
            "edit mode must always show the real value, never masked:\n{screen}"
        );
    }

    #[test]
    fn a_request_with_no_auth_shows_nothing_masked_and_advertises_no_reveal_key() {
        let state = loaded_state(THREE_REQUEST_COLLECTION);

        let screen = render_screen(&state);
        let help = status_help_text(&state);

        assert!(
            !screen.contains(REDACTED_CAPTURE_VALUE),
            "nothing to mask means nothing should show the placeholder:\n{screen}"
        );
        assert!(
            !help.contains("reveal"),
            "advertising the reveal key for a request with nothing to reveal would be \
             misleading: {help}"
        );
    }

    #[test]
    fn body_under_the_cap_is_shown_in_full() {
        let (preview, truncated) = truncate_body("short body");
        assert_eq!(preview, "short body");
        assert!(!truncated);
    }

    #[test]
    fn body_over_the_cap_is_truncated_not_panicked_on() {
        let huge = "x".repeat(MAX_BODY_PREVIEW_CHARS * 3);

        let (preview, truncated) = truncate_body(&huge);

        assert_eq!(preview.chars().count(), MAX_BODY_PREVIEW_CHARS);
        assert!(truncated);
    }

    #[test]
    fn dirty_marker_renders_next_to_the_edited_request_in_the_collection_browser() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let render = |state: &AppState| {
            let backend = TestBackend::new(60, 10);
            let mut terminal = Terminal::new(backend).expect("a test terminal builds");
            terminal
                .draw(|frame| view(state, frame))
                .expect("rendering must not panic");
            buffer_to_string(terminal.backend().buffer())
        };

        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        let clean_screen = render(&state);
        assert!(
            !clean_screen.contains('*'),
            "nothing is dirty yet, so no marker should render:\n{clean_screen}"
        );

        update(&mut state, Message::EnterEditMode);
        type_into_focused_field(&mut state, "X");
        // The request list is the left pane (`render_request_list`), drawn
        // unconditionally whenever a collection is loaded — edit mode only
        // takes over the right-hand detail pane (see `render_detail_pane`'s
        // own edit-mode branch) — so the marker must already be visible
        // here, mid-edit, not only after returning to browsing.
        let dirty_screen = render(&state);
        assert!(
            dirty_screen.contains("*GET One"),
            "the dirty request must show a marker in the collection browser:\n{dirty_screen}"
        );

        update(&mut state, Message::CancelEdit);
        let cancelled_screen = render(&state);
        assert!(
            !cancelled_screen.contains('*'),
            "cancelling must remove the marker again:\n{cancelled_screen}"
        );
    }
}
