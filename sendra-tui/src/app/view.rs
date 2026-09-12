//! The view half of sendra-tui's Elm-style architecture: `view()` and every
//! `render_*`/`format_*` function it calls, plus the small pure UI helpers
//! (`centered_rect`, `inset`, `truncate_body`, `resolve_preview`) those
//! render functions build on. Nothing here ever mutates `AppState` — see
//! `super::update` for the one function that does.

use std::collections::HashSet;
use std::path::Path;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use sendra_core::{
    ApiKeyLocation, AssertionReport, CaptureReport, Document, Environment, OAuthGrantType, Request,
    Response, SendraError,
};

use crate::run_request::RunOutcome;

use super::state::{
    AppState, AuthEdit, AuthField, BodyEdit, EditField, EditState, LoadState, NamedEnvironment,
    RunState,
};

/// Body preview is capped rather than shown in full — scrolling through a
/// large body is a separate concern from the preview pane; this just keeps
/// a multi-megabyte body from making every draw slower without ever
/// crashing on one.
const MAX_BODY_PREVIEW_CHARS: usize = 2000;

pub fn view(state: &AppState, frame: &mut Frame) {
    // The bottom row is reserved in every `LoadState`, not only `Loaded` —
    // an error state must not dead-end the app, and the help bar showing
    // which keys still work (at minimum quit, per `status_help_text`) is
    // exactly what makes that visible rather than assumed. See that
    // function's own doc comment for how it reads `load_state` to keep this
    // honest instead of always claiming nav/run apply.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(frame.area());

    match &state.load_state {
        LoadState::Loading => render_message(frame, rows[0], "Loading collection..."),
        LoadState::NoPathProvided => {
            render_message(frame, rows[0], "No collection path provided.");
        }
        LoadState::Failed(error) => {
            render_error(frame, rows[0], "Failed to load collection", error);
        }
        LoadState::Loaded {
            document,
            selected,
            base_dir,
        } => {
            let panes = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                .split(rows[0]);

            render_request_list(frame, panes[0], document, *selected, &state.dirty_requests);
            render_detail_pane(frame, panes[1], document, *selected, base_dir, state);
        }
    }

    render_status_bar(frame, rows[1], state);

    if let Some(cursor) = state.environment_overlay {
        render_environment_overlay(frame, state, cursor);
    }
}

fn render_message(frame: &mut Frame, area: Rect, text: &str) {
    frame.render_widget(Paragraph::new(text.to_string()), area);
}

/// One error, formatted the same way everywhere sendra-tui shows one: a
/// short heading naming *what* failed, then the real error's own `Display`
/// text, verbatim, on the line(s) under it — never paraphrased or
/// re-summarized. Every place with an error to show (a collection that
/// failed to load, an environment file that failed to load, a preview that
/// could not be resolved, a run that failed) builds its text through this,
/// so there is one error box style in the whole crate rather than three or
/// four ad hoc ones that happen to drift apart over time.
fn format_error(heading: &str, error: &impl std::fmt::Display) -> String {
    format!("⚠ {heading}\n{error}")
}

/// [`format_error`], rendered into `area` as a wrapped `Paragraph` — the
/// standalone-error half of the pair; [`format_error`] alone is what the
/// call sites that embed an error inside other text (the request preview,
/// the response panel) use instead, since those need the string, not a
/// widget of their own.
fn render_error(frame: &mut Frame, area: Rect, heading: &str, error: &impl std::fmt::Display) {
    frame.render_widget(
        Paragraph::new(format_error(heading, error)).wrap(Wrap { trim: false }),
        area,
    );
}

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
fn render_request_list(
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
fn render_detail_pane(
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

    if let RunState::Completed(outcome) = &state.run_state {
        render_response_panel(
            frame,
            area,
            outcome,
            state.response_scroll,
            state.reveal_captures,
        );
        return;
    }

    let text = match resolve_preview(request, base_dir, &environment) {
        Ok(resolved) => format_resolved_request(&resolved),
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

/// Edit mode's own half of the detail pane: `method` and `url` as live,
/// cursor-addressable text fields, then a `Headers:` section listing every
/// header row the same way (`▶` marking whichever `EditState::focus`
/// currently points at, independently for a row's key and its value), then
/// a `Body` section — either the raw/JSON text area (see
/// `BodyEdit::Editable`) or a read-only line naming what isn't editable
/// here (see `BodyEdit::Unsupported`) — then an `Auth:` section (see
/// `AuthEdit`'s own doc comment for exactly what is and isn't editable
/// there) and, right under it, a live "Resolved auth" line — `method_error`/
/// `body_error` shown inline right under the field each is about, and the
/// keybinding reminder every other pane in this file puts in its own
/// footer/status text. The real terminal cursor is placed on the focused
/// field's real position via `Frame::set_cursor_position`, not just implied
/// by the `▶` marker — an ordinary text field shows a real blinking cursor,
/// not merely which line is active.
///
/// **A caveat shared with every field in this pane, not new here**: the
/// whole pane wraps (`Wrap { trim: false }`), so a line wider than the pane
/// (a long URL, a long header value, a long body line) wraps onto extra
/// visual rows that the row/column math below does not know about — the
/// real cursor can land a little off for such a line. Solving that would
/// mean switching to unwrapped rendering with horizontal scroll for every
/// field, a change orthogonal to what this issue asked for; not attempted
/// here.
///
/// `base_request`/`environment` are only needed for the "Resolved auth"
/// line — see `describe_resolved_auth`, which is the one place this
/// function looks past `edit` itself at what the rest of the request and
/// the active environment actually are.
fn render_edit_pane(
    frame: &mut Frame,
    area: Rect,
    edit: &EditState,
    base_request: &Request,
    environment: &Environment,
) {
    let method_marker = if edit.focus == EditField::Method {
        "▶"
    } else {
        " "
    };
    let url_marker = if edit.focus == EditField::Url {
        "▶"
    } else {
        " "
    };

    let mut lines = vec![
        format!("{method_marker} Method: {}", edit.method.value()),
        format!("  ⚠ {}", edit.method_error.as_deref().unwrap_or("")),
        String::new(),
        format!("{url_marker} URL:    {}", edit.url.value()),
        String::new(),
        "Headers:".to_string(),
    ];
    // An empty error line still reserves its row (see the `⚠ {}` line
    // above) rather than the whole pane shifting up and down every time a
    // keystroke fixes or breaks validity — the same "always show something,
    // never let a row silently disappear" posture `render_response_panel`'s
    // footer already takes. Blank it out here instead: showing a bare `⚠`
    // marker with nothing after it on a passing method would look like a
    // rendering bug, not "no error".
    if edit.method_error.is_none() {
        lines[1] = String::new();
    }

    // The first line index a header row could occupy — needed below to turn
    // a `HeaderKey(i)`/`HeaderValue(i)` focus into the real row/column the
    // terminal cursor goes on. Recorded here, right before pushing the
    // header rows themselves (or the `(none)` placeholder), so it always
    // matches however many fixed lines precede them, however those change.
    let headers_start_line = lines.len();
    if edit.headers.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        for (index, row) in edit.headers.iter().enumerate() {
            lines.push(header_row_line(edit.focus, index, row));
        }
    }

    lines.push(String::new());
    let body_marker = if edit.focus == EditField::Body {
        "▶"
    } else {
        " "
    };
    // Only set for `BodyEdit::Editable`, and only ever read back for
    // `EditField::Body`'s own cursor placement below — a focus that only
    // exists in the first place when the body actually is `Editable` (see
    // `EditField::next`/`prev`'s `has_body` parameter), so the `0` default
    // for `Unsupported` is never actually read as a real position.
    let mut body_start_line = 0;
    match &edit.body {
        BodyEdit::Unsupported { description } => {
            lines.push(format!("Body: {description}"));
        }
        BodyEdit::Editable { text, is_json } => {
            let kind = if *is_json { "json" } else { "raw" };
            lines.push(format!("{body_marker} Body ({kind}):"));
            // Like `method_error` above, this row is reserved only when the
            // body could ever have an error at all — a plain (`raw`) body
            // is never JSON-validated (see `validate_body_for_save`), so it
            // never gets a wasted blank error line under it.
            if *is_json {
                lines.push(format!("  ⚠ {}", edit.body_error.as_deref().unwrap_or("")));
            }
            body_start_line = lines.len();
            lines.extend(text.value().split('\n').map(str::to_string));
        }
    }

    lines.push(String::new());
    lines.push("Auth:".to_string());
    let auth_fields = edit.auth_field_order();
    let auth_start_line = lines.len();
    if auth_fields.is_empty() {
        lines.push("  (no auth configured on this request)".to_string());
    } else {
        for &field in auth_fields {
            let marker = if edit.focus == EditField::Auth(field) {
                "▶"
            } else {
                " "
            };
            lines.push(format!(
                "{marker}{}{}",
                auth_field_label(field),
                auth_field_display(&edit.auth, field)
            ));
        }
    }

    lines.push(String::new());
    lines.push(format!(
        "Resolved auth → {}",
        describe_resolved_auth(base_request, edit, environment)
    ));

    lines.push(String::new());
    lines.push(
        "Tab/Shift+Tab move focus  Ctrl+N add header  Ctrl+D delete header  \
         Ctrl+S save  Esc cancel  (Body: Enter for newline, ↑/↓ move lines; \
         Auth: ←/→ toggle option)"
            .to_string(),
    );

    frame.render_widget(
        Paragraph::new(lines.join("\n")).wrap(Wrap { trim: false }),
        area,
    );

    // Column offset: `▶ Method: ` / `▶ URL:    ` are the same width (10
    // cells) by construction — `"Method: "` and `"URL:    "` are both
    // 8 characters — so one constant serves both fields' cursor placement.
    const FIELD_PREFIX_WIDTH: u16 = 2 + 8;
    let (row, column) = match edit.focus {
        EditField::Method => (0, FIELD_PREFIX_WIDTH + edit.method.cursor_chars() as u16),
        EditField::Url => (3, FIELD_PREFIX_WIDTH + edit.url.cursor_chars() as u16),
        EditField::HeaderKey(index) => {
            let prefix = header_key_prefix(index);
            let cursor = edit.headers[index].key.cursor_chars();
            (
                headers_start_line + index,
                prefix.chars().count() as u16 + cursor as u16,
            )
        }
        EditField::HeaderValue(index) => {
            let prefix = header_value_prefix(index, edit.headers[index].key.value());
            let cursor = edit.headers[index].value.cursor_chars();
            (
                headers_start_line + index,
                prefix.chars().count() as u16 + cursor as u16,
            )
        }
        EditField::Body => {
            let BodyEdit::Editable { text, .. } = &edit.body else {
                unreachable!(
                    "focus is never Body while the body is Unsupported — see \
                     EditField::next/prev"
                );
            };
            let (line_offset, column) = text.cursor_row_col();
            (body_start_line + line_offset, column as u16)
        }
        EditField::Auth(field) => {
            let index = auth_fields
                .iter()
                .position(|candidate| *candidate == field)
                .unwrap_or(0);
            let label = auth_field_label(field);
            let cursor = auth_field_cursor_chars(&edit.auth, field);
            (
                auth_start_line + index,
                1 + label.chars().count() as u16 + cursor as u16,
            )
        }
    };
    frame.set_cursor_position((area.x + column, area.y + row as u16));
}

/// The prefix of a header row's line up to (not including) the key field's
/// own text — `header_row_line` builds the same prefix inline; kept as its
/// own function so the cursor-column math below can measure it without
/// duplicating the literal spacing. The marker character itself is not part
/// of what varies the width — `▶` and `" "` are both exactly one `char`
/// wide — so a plain space stands in for whichever one is actually showing.
fn header_key_prefix(index: usize) -> String {
    format!(" [{index}] Key: ")
}

/// Like [`header_key_prefix`], but up to (not including) the value field's
/// own text — needs `key_value` since the value field's column depends on
/// how long the key text ahead of it is.
fn header_value_prefix(index: usize, key_value: &str) -> String {
    format!("{}{key_value}   Value: ", header_key_prefix(index))
}

/// One header row's display line: independent `▶` markers for the key and
/// value fields, since exactly one of a row's two fields can be focused at a
/// time (or neither, for every row but the focused one) — mirrors the
/// `method_marker`/`url_marker` pattern `render_edit_pane` already uses for
/// method/URL, just per-row instead of per-field.
fn header_row_line(focus: EditField, index: usize, row: &super::state::HeaderRow) -> String {
    let key_marker = if focus == EditField::HeaderKey(index) {
        "▶"
    } else {
        " "
    };
    let value_marker = if focus == EditField::HeaderValue(index) {
        "▶"
    } else {
        " "
    };
    format!(
        "{key_marker}[{index}] Key: {}   {value_marker}Value: {}",
        row.key.value(),
        row.value.value()
    )
}

/// One auth field's fixed label — e.g. `"Bearer token: "` — used both to
/// render its row and, like `header_key_prefix`, to measure the real
/// cursor's column.
fn auth_field_label(field: AuthField) -> &'static str {
    match field {
        AuthField::BearerToken => "Bearer token: ",
        AuthField::BasicUser => "User: ",
        AuthField::BasicPass => "Password: ",
        AuthField::ApiKeyName => "Name: ",
        AuthField::ApiKeyValue => "Value: ",
        AuthField::ApiKeyLocation => "Location: ",
        AuthField::OAuthGrantType => "Grant type: ",
        AuthField::OAuthTokenUrl => "Token URL: ",
        AuthField::OAuthClientId => "Client ID: ",
        AuthField::OAuthClientSecret => "Client secret: ",
        AuthField::OAuthScope => "Scope: ",
        AuthField::OAuthUsername => "Username: ",
        AuthField::OAuthPassword => "Password: ",
    }
}

/// The current display text for one auth field's row: a live `TextField`'s
/// value for every text field, or the fixed enum's own value plus a toggle
/// hint for `ApiKeyLocation`/`OAuthGrantType`, which have no `TextField`
/// behind them at all (see `AuthEdit::toggle`).
fn auth_field_display(auth: &AuthEdit, field: AuthField) -> String {
    match (auth, field) {
        (AuthEdit::Bearer { token }, AuthField::BearerToken) => token.value().to_string(),
        (AuthEdit::Basic { user, .. }, AuthField::BasicUser) => user.value().to_string(),
        (AuthEdit::Basic { pass, .. }, AuthField::BasicPass) => pass.value().to_string(),
        (AuthEdit::ApiKey { name, .. }, AuthField::ApiKeyName) => name.value().to_string(),
        (AuthEdit::ApiKey { value, .. }, AuthField::ApiKeyValue) => value.value().to_string(),
        (AuthEdit::ApiKey { location, .. }, AuthField::ApiKeyLocation) => {
            format!("{} (←/→ to change)", api_key_location_str(*location))
        }
        (AuthEdit::OAuth { grant_type, .. }, AuthField::OAuthGrantType) => {
            format!("{} (←/→ to change)", oauth_grant_type_str(*grant_type))
        }
        (AuthEdit::OAuth { token_url, .. }, AuthField::OAuthTokenUrl) => {
            token_url.value().to_string()
        }
        (AuthEdit::OAuth { client_id, .. }, AuthField::OAuthClientId) => {
            client_id.value().to_string()
        }
        (AuthEdit::OAuth { client_secret, .. }, AuthField::OAuthClientSecret) => {
            client_secret.value().to_string()
        }
        (AuthEdit::OAuth { scope, .. }, AuthField::OAuthScope) => scope.value().to_string(),
        (AuthEdit::OAuth { username, .. }, AuthField::OAuthUsername) => {
            username.value().to_string()
        }
        (AuthEdit::OAuth { password, .. }, AuthField::OAuthPassword) => {
            password.value().to_string()
        }
        _ => unreachable!(
            "auth_field_display is only ever called for a field in this AuthEdit's own \
             field_order — see EditState::auth_field_order"
        ),
    }
}

/// The focused auth field's cursor column, in `char`s — `0` for
/// `ApiKeyLocation`/`OAuthGrantType`, which have no `TextField`/cursor at
/// all, placing the real terminal cursor right at the start of the
/// displayed value instead. Mirrors `auth_field_display`'s own match, but
/// immutable — this runs after `render_edit_pane` has already rendered the
/// text, only to place the cursor.
fn auth_field_cursor_chars(auth: &AuthEdit, field: AuthField) -> usize {
    match (auth, field) {
        (AuthEdit::Bearer { token }, AuthField::BearerToken) => token.cursor_chars(),
        (AuthEdit::Basic { user, .. }, AuthField::BasicUser) => user.cursor_chars(),
        (AuthEdit::Basic { pass, .. }, AuthField::BasicPass) => pass.cursor_chars(),
        (AuthEdit::ApiKey { name, .. }, AuthField::ApiKeyName) => name.cursor_chars(),
        (AuthEdit::ApiKey { value, .. }, AuthField::ApiKeyValue) => value.cursor_chars(),
        (AuthEdit::OAuth { token_url, .. }, AuthField::OAuthTokenUrl) => token_url.cursor_chars(),
        (AuthEdit::OAuth { client_id, .. }, AuthField::OAuthClientId) => client_id.cursor_chars(),
        (AuthEdit::OAuth { client_secret, .. }, AuthField::OAuthClientSecret) => {
            client_secret.cursor_chars()
        }
        (AuthEdit::OAuth { scope, .. }, AuthField::OAuthScope) => scope.cursor_chars(),
        (AuthEdit::OAuth { username, .. }, AuthField::OAuthUsername) => username.cursor_chars(),
        (AuthEdit::OAuth { password, .. }, AuthField::OAuthPassword) => password.cursor_chars(),
        _ => 0,
    }
}

fn api_key_location_str(location: ApiKeyLocation) -> &'static str {
    match location {
        ApiKeyLocation::Header => "header",
        ApiKeyLocation::Query => "query",
    }
}

fn oauth_grant_type_str(grant_type: OAuthGrantType) -> &'static str {
    match grant_type {
        OAuthGrantType::ClientCredentials => "client_credentials",
        OAuthGrantType::Password => "password",
    }
}

/// What `Request::resolve_auth` (via `Environment::apply` first, the same
/// two-step pipeline `resolve_preview` already runs for the read-only,
/// not-editing preview) would actually send for this in-progress edit —
/// reusing that real sendra-core pipeline rather than reformatting
/// `AuthEdit` by hand, so this line is provably correct instead of merely a
/// plausible-looking mirror of it. Built from `edit.to_request(base_request)`,
/// which folds in every field this edit session could have changed
/// (method/url/headers/auth) — so an explicit `Authorization` header typed
/// into the Headers section alongside `auth.bearer`, say, shows the same
/// collision this pipeline would raise for a real run, not a preview that
/// only ever looks at auth in isolation.
///
/// **Reused across both directions of the environment-auth precedence
/// rule**: `environment.apply` is what decides whether the environment's own
/// default `auth:` applies at all (only when `preview.auth` is `None`) —
/// see `Environment::auth`'s own doc comment — so clearing a request's auth
/// down to nothing in this edit session and saving genuinely lets the
/// environment default take over, and this preview shows that happening
/// live, not just asserted.
///
/// **OAuth is skipped here deliberately** — the one place this issue's OAuth
/// scoping decision (see `AuthEdit`'s own doc comment) is visible in the UI
/// itself: acquiring a real token means a real network call
/// (`Request::resolve_oauth`), which has no place in drawing a frame, and
/// `resolve_auth` alone returns a typed error for an unresolved
/// `auth.oauth` — correct, but would read as if something were wrong with
/// what was typed rather than as the deliberate limitation it is.
fn describe_resolved_auth(
    base_request: &Request,
    edit: &EditState,
    environment: &Environment,
) -> String {
    let preview = edit.to_request(base_request);

    if matches!(edit.auth, AuthEdit::OAuth { .. }) {
        return "(OAuth token acquired at request time — not shown in this preview)".to_string();
    }

    match environment
        .apply(&preview)
        .and_then(|request| request.resolve_auth())
    {
        Ok(resolved) => {
            let mut parts: Vec<String> = resolved
                .headers
                .iter()
                .filter(|(name, _)| {
                    !preview
                        .headers
                        .iter()
                        .any(|(existing, _)| existing.eq_ignore_ascii_case(name))
                })
                .map(|(name, value)| format!("header {name}: {value}"))
                .collect();
            parts.extend(
                resolved
                    .query
                    .iter()
                    .filter(|(name, _)| !preview.query.iter().any(|(existing, _)| existing == name))
                    .map(|(name, value)| format!("query {name}={value}")),
            );
            if parts.is_empty() {
                "(no auth resolved)".to_string()
            } else {
                parts.join(", ")
            }
        }
        Err(error) => format!("could not resolve — {error}"),
    }
}

/// The completed-run half of the detail pane: a real response's status,
/// headers and body, or the real `SendraError` that stopped it — pulled
/// straight from the `Response`/`SendraError` already sitting in
/// `RunState::Completed`, nothing recomputed or re-derived.
///
/// **Not wrapped, and scrolled by whole lines only.** The request preview
/// above wraps long lines because a request body is authored by the same
/// person reading it and rarely wide; a response body has no such
/// guarantee — a minified JSON payload is one line that can run to
/// thousands of characters — so wrapping it would make `response_scroll`'s
/// "line N" meaningless (a wrapped line renders as several visual rows,
/// and the count depends on pane width). Scrolling raw lines and letting a
/// too-wide one clip at the pane edge is the same trade-off `less` (without
/// `-S`) and most response viewers make, and it is what keeps the scroll
/// math in this function simple and always correct rather than an
/// approximation of ratatui's own wrapping.
///
/// The last line is always a footer — `Line a-b of n` plus the scroll keys
/// — never only shown once content overflows, so the pane never scrolls
/// silently: there is always something on screen saying whether there is
/// more, exactly the same posture `truncate_body` takes with its own
/// `[truncated to N characters]` marker.
fn render_response_panel(
    frame: &mut Frame,
    area: Rect,
    outcome: &RunOutcome,
    scroll: usize,
    reveal_captures: bool,
) {
    let text = format_run_result(outcome, reveal_captures);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let content_height = rows[0].height as usize;
    let max_scroll = total.saturating_sub(content_height.max(1));
    let scroll = scroll.min(max_scroll);

    let paragraph = Paragraph::new(text.clone()).scroll((scroll.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(paragraph, rows[0]);

    let last_visible = (scroll + content_height).min(total);
    let reveal_hint = if outcome.capture.is_empty() {
        String::new()
    } else if reveal_captures {
        "  |  c: hide captures".to_string()
    } else {
        "  |  c: reveal captures".to_string()
    };
    let footer = format!(
        "Line {}-{} of {total} — PgUp/PgDn to scroll{reveal_hint}",
        scroll.saturating_add(1).min(total.max(1)),
        last_visible,
    );
    frame.render_widget(
        Paragraph::new(footer).style(Style::new().add_modifier(Modifier::DIM)),
        rows[1],
    );
}

/// The text `render_response_panel` shows: on success, the response laid out
/// by `format_response`, followed by the assertion and capture sections
/// (`format_assertions`/`format_capture_section`) — always both, even when
/// their reports are empty, so "no assertions declared" reads distinctly
/// from either a passing or a failing assertion block, and likewise for
/// captures. On failure, [`format_error`] over the real
/// [`run_request::RunError`]'s own `Display` — the same wording
/// `sendra run` itself would print for the same `sendra_core::SendraError`
/// (`RunError::Core`), or, for the one failure that is not core's to report
/// (`RunError::RuntimeUnavailable` — see that variant's doc comment), the
/// honest reason the pipeline never even reached the network. Either way
/// there is no response to lay out and nothing was checked or captured, so
/// this is the whole of it: the real error, not a TUI-invented summary of
/// it and not two sections falsely claiming "no assertions declared" for a
/// request that may well have some.
fn format_run_result(outcome: &RunOutcome, reveal_captures: bool) -> String {
    match &outcome.result {
        Ok(response) => {
            let mut text = format_response(response);
            text.push('\n');
            text.push_str(&format_assertions(&outcome.assertions));
            text.push('\n');
            text.push_str(&format_capture_section(&outcome.capture, reveal_captures));
            text
        }
        Err(error) => format_error("Request failed", error),
    }
}

/// `assertions` heading, one line per check (`✓`/`✗` plus core's own
/// expectation/failure wording — [`sendra_core::AssertionResult`] renders
/// the words, this only lays them out), then a pass/fail count — the same
/// per-assertion granularity and the same wording as sendra-cli's own
/// `print_assertions` in `sendra-cli/src/output/human.rs`, reimplemented
/// (uncoloured) rather than imported for the same cross-crate reason
/// `body_for_display` is. **Declared distinctly from "declared and all
/// passed"**: an empty report — no `assertions:` block, or an empty one —
/// renders `no assertions declared` instead of silently matching the "0
/// failed" case a passing block would also produce, mirroring the
/// skipped-vs-passed distinction sendra-cli's own `--junit` output already
/// makes for the same report type.
fn format_assertions(report: &AssertionReport) -> String {
    if report.is_empty() {
        return "assertions\n  no assertions declared".to_string();
    }

    let mut lines = vec!["assertions".to_string()];
    for result in report.results() {
        match &result.failure {
            None => lines.push(format!("  ✓ {}", result.expectation)),
            Some(detail) => lines.push(format!("  ✗ {} — {detail}", result.expectation)),
        }
    }

    if report.passed() {
        lines.push(format!("  {} passed", report.passed_count()));
    } else {
        lines.push(format!(
            "  {} passed, {} failed",
            report.passed_count(),
            report.failed_count()
        ));
    }

    lines.join("\n")
}

/// A captured value that has not been revealed this session — the TUI's own
/// interactive counterpart to `--show-captures`'s default, not a value
/// sendra-cli itself ever prints to a terminal (its own `print_capture`
/// never shows the value at all, revealed or not; only `--json` carries it,
/// gated by that same flag). Same placeholder text `--json`'s
/// `REDACTED_CAPTURE_VALUE` uses (`sendra-cli/src/output/json.rs`), so the
/// two tools agree on what "hidden" is spelled as.
const REDACTED_CAPTURE_VALUE: &str = "<redacted>";

/// `capture` heading, one line per entry — a captured value shown as
/// [`REDACTED_CAPTURE_VALUE`] unless `reveal` is true, a failed entry's real
/// `CaptureFailure` message always shown regardless of `reveal` (a failure
/// never carried a value to begin with, so there is nothing to redact — the
/// same rule `--json`'s `CaptureRecord` follows: "failures is never
/// redacted") — then, like [`format_assertions`], `no captures declared`
/// for an empty report rather than looking like a capture block that
/// declared nothing wrong.
///
/// `reveal` is `AppState::reveal_captures` — session-only and reset on every
/// new run and every selection change (see its doc comment); nothing here
/// writes it anywhere durable, so a masked capture is masked again the next
/// time this function runs unless the user asks again.
fn format_capture_section(report: &CaptureReport, reveal: bool) -> String {
    if report.is_empty() {
        return "capture\n  no captures declared".to_string();
    }

    let mut lines = vec!["capture".to_string()];
    for result in report.results() {
        let from = format!("{} from `{}`", result.variable, result.path);
        match result.failure() {
            None => {
                let value = if reveal {
                    result.value().unwrap_or_default()
                } else {
                    REDACTED_CAPTURE_VALUE
                };
                lines.push(format!("  ✓ {from} = {value}"));
            }
            Some(failure) => lines.push(format!("  ✗ {from} — {failure}")),
        }
    }

    lines.join("\n")
}

/// Mirrors sendra-cli's own response layout
/// (`sendra-cli/src/output/human.rs::print_response`/`print_status_line`)
/// closely enough that the two are a direct side-by-side match for the same
/// response: status code, status text and elapsed time on one line, every
/// header in the order the response actually carried them, a blank line,
/// then the body. Uncoloured, unlike the CLI's terminal output — colour is
/// the one thing this deliberately does not reproduce, since ratatui styling
/// is a separate concern from the data being correct.
fn format_response(response: &Response) -> String {
    let mut lines = vec![format!(
        "{} {}  {} ms",
        response.status,
        response.status_text,
        response.elapsed.as_millis()
    )];

    for (name, value) in &response.headers {
        lines.push(format!("{name}: {value}"));
    }

    if !response.body.is_empty() {
        lines.push(String::new());
        lines.push(body_for_display(response));
    }

    lines.join("\n")
}

/// Pretty-prints the body when `Content-Type` claims JSON and it actually
/// parses as JSON, otherwise returns it unchanged — the same rule, sniffed
/// the same way, as sendra-cli's `body_for_display`/`claims_json` in
/// `output/human.rs`. Reimplemented here rather than imported, since
/// sendra-tui depends on sendra-core and not on sendra-cli, and this is
/// display formatting, not something sendra-core itself does or should do.
fn body_for_display(response: &Response) -> String {
    if !claims_json(&response.headers) {
        return response.body.clone();
    }

    match serde_json::from_str::<serde_json::Value>(&response.body) {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|_| response.body.clone()),
        Err(_) => response.body.clone(),
    }
}

/// Whether these response headers say the body is JSON — see the doc
/// comment on [`body_for_display`] for why this mirrors sendra-cli's own
/// `claims_json` instead of calling it.
fn claims_json(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .any(|(_, value)| {
            let media_type = value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            media_type == "application/json" || media_type.ends_with("+json")
        })
}

/// Cycled by `spinner_tick` while a run is in flight — an ordinary braille
/// spinner, no library, since ratatui ships no widget for one.
const SPINNER_FRAMES: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];

/// The one-line status/help bar under the panes: a status summary (in
/// flight, or what the last run did) where there is one, and — always — the
/// keybindings that actually do something right now.
///
/// Built entirely from [`status_help_text`], which is also what the tests
/// below exercise directly: this function's only job is handing that string
/// to a `Paragraph`.
fn render_status_bar(frame: &mut Frame, area: Rect, state: &AppState) {
    frame.render_widget(Paragraph::new(status_help_text(state)), area);
}

/// The bottom bar's full text — status where there is one, then the
/// keybindings currently live — chosen from exactly the state `update()`
/// itself branches on, so this can never say a key does something `update()`
/// would actually refuse, or omit one it would accept. Five contexts, in the
/// same priority order `update()`'s own InFlight/edit-mode guards and
/// `view()`'s own overlay-vs-response-panel-vs-preview dispatch already
/// imply:
///
/// 1. **The environment overlay is open** (`environment_overlay.is_some()`,
///    the same condition `view()` checks to draw it) — only the overlay's
///    own keys apply, checked first because the overlay is drawn on top of
///    everything else and is what has the user's attention, and because
///    nothing about `next_message` stops it opening over a `load_state`
///    that isn't `Loaded` (see the next context). Mutually exclusive with
///    edit mode below — `update()` refuses to open the overlay while
///    `edit_mode` is set, and refuses to enter edit mode while the overlay
///    is open — so these two contexts never need to be prioritized against
///    each other, only checked in some order.
/// 2. **The selected request is being edited** (`edit_mode.is_some()`) —
///    `update()`'s own edit-mode guard refuses every navigation/overlay/run
///    message while this holds, the same way the InFlight guard does for a
///    run, so only `Ctrl+S`/`Esc`/quit are genuinely live.
/// 3. **No collection is loaded** (`load_state` is `Loading`,
///    `NoPathProvided` or `Failed` — see `view()`'s own match on it): there
///    is no request list and nothing to run, so nav/run are not offered;
///    the environment overlay and quit are the only two keys that do
///    anything, and both keep working, which is the whole point of this
///    context existing — a failed collection load must not read as a dead
///    end.
/// 4. **A run is in flight** (`RunState::InFlight`) — `update()`'s own guard
///    at the top of the function refuses every navigation/overlay/run
///    message while this holds, so `q` (never blocked — see that guard's own
///    comment) is genuinely the only key left to advertise.
/// 5. **A run has completed** (`RunState::Completed`) — the response panel
///    is what `render_detail_pane` is showing (see its own doc comment), so
///    this is where the scroll keys and, when there is something to reveal,
///    the capture-reveal key belong; nav/run/env/quit are back too, since
///    the InFlight guard no longer applies.
/// 6. **Otherwise** (`RunState::Idle`, a collection is loaded) — the
///    ordinary collection browser, showing the request preview.
///
/// No key is added here that `update`/`main::next_message` do not already
/// bind — this only narrates keys already wired elsewhere.
///
/// Visible to `super::update`'s own tests: a couple of reducer-focused tests
/// (invalid-method save-refusal, dirty-marker bookkeeping) confirm their
/// effect is visible in this bar too, rather than duplicating a second
/// "what does the help bar say" check inside `view`'s own test module.
pub(super) fn status_help_text(state: &AppState) -> String {
    if state.environment_overlay.is_some() {
        return "↑/↓ nav  enter confirm  esc cancel  q quit".to_string();
    }

    if let Some(edit) = &state.edit_mode {
        let dirty = if edit.dirty { " (unsaved changes)" } else { "" };
        let invalid = if edit.method_error.is_some() {
            "  (fix method to save)"
        } else {
            ""
        };
        return format!(
            "Editing{dirty}{invalid}  |  tab/shift+tab switch field  ctrl+n add header  \
             ctrl+d delete header  ctrl+s save  esc cancel  q quit"
        );
    }

    if !matches!(state.load_state, LoadState::Loaded { .. }) {
        return "e env  q quit".to_string();
    }

    match &state.run_state {
        RunState::Idle => "↑/↓ nav  enter/r run  i edit  e env  q quit".to_string(),
        RunState::InFlight => {
            let frame_char = SPINNER_FRAMES[state.spinner_tick % SPINNER_FRAMES.len()];
            format!("{frame_char} Running request...  |  q quit")
        }
        RunState::Completed(outcome) => {
            let status = match &outcome.result {
                Ok(response) => {
                    let assertions = if outcome.assertions.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " — {} passed, {} failed",
                            outcome.assertions.passed_count(),
                            outcome.assertions.failed_count()
                        )
                    };
                    format!("Done — {}{assertions}", response.status)
                }
                Err(error) => format!("Failed — {error}"),
            };
            let reveal = if outcome.capture.is_empty() {
                ""
            } else {
                "  c reveal/hide captures"
            };
            format!(
                "{status}  |  ↑/↓ nav  enter/r run again  PgUp/PgDn/Home/End scroll{reveal}  i edit  e env  q quit"
            )
        }
    }
}

/// The environment the detail pane resolves against and a run sends
/// against — the one `active_environment` names, or `None` before the user
/// has picked one. `pub` (not `fn`-private) because `main` needs the exact
/// same lookup to resolve a run's request against, rather than a second
/// implementation of "which environment is active" living outside this
/// module.
pub fn active_environment(state: &AppState) -> Option<&NamedEnvironment> {
    state
        .active_environment
        .and_then(|index| state.environments.get(index))
}

/// The same substitution + auth/query/body resolution pipeline sendra-cli's
/// `--dry-run` runs before sending, reused directly rather than
/// reimplemented. Deliberately narrower than the CLI's full pipeline in two
/// ways, both because this is a preview, not a run: it skips OAuth token
/// acquisition (`Request::resolve_oauth`), a real network call with no place
/// in drawing a frame, and it skips `Config::apply`, since the TUI has no
/// project `Config` loaded anywhere yet — so the headers shown here are the
/// request's own, substituted, not the final wire-level set an actual run
/// would send after config defaults are layered on. `environment` is passed
/// in as a real, caller-chosen value rather than assumed here — an empty
/// one when nothing is active, or the one the user picked in the overlay
/// — so any environment-level default (its own `auth` block
/// included) resolves exactly as `Environment::apply` already defines it.
fn resolve_preview(
    request: &Request,
    base_dir: &Path,
    environment: &Environment,
) -> Result<Request, SendraError> {
    environment
        .apply(request)
        .and_then(|request| request.resolve_auth())
        .and_then(|request| request.resolve_query())
        .and_then(|request| request.resolve_body(base_dir))
}

fn format_resolved_request(request: &Request) -> String {
    let mut lines = vec![
        format!("Method: {}", request.method),
        format!("URL:    {}", request.url),
        String::new(),
        "Headers:".to_string(),
    ];

    if request.headers.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        for (name, value) in &request.headers {
            lines.push(format!("  {name}: {value}"));
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

fn truncate_body(body: &str) -> (String, bool) {
    if body.chars().count() <= MAX_BODY_PREVIEW_CHARS {
        (body.to_string(), false)
    } else {
        (body.chars().take(MAX_BODY_PREVIEW_CHARS).collect(), true)
    }
}

/// A rect roughly `percent_x`/`percent_y` of `area`, centered within it — the
/// standard ratatui popup-centering pattern, pure layout math with nothing
/// project-specific to reuse from elsewhere.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

/// `area` shrunk by one cell on every side — where content goes inside a
/// bordered `Block` drawn over the same `area`.
fn inset(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

fn render_environment_overlay(frame: &mut Frame, state: &AppState, cursor: usize) {
    let area = centered_rect(70, 70, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title("Select environment — Enter to confirm, Esc to cancel"),
        area,
    );

    let inner = inset(area);

    // Environments that were found in `.sendra/environments/` but failed to
    // load (see `AppState::environment_errors`'s own doc comment) get a
    // fixed strip at the bottom of the overlay, through the same
    // `format_error` every other error in the crate goes through — no
    // second, differently-styled error box for this one. Reserved only when
    // there is something to show, so an overlay with nothing wrong draws
    // exactly as it always has.
    let error_rows = if state.environment_errors.is_empty() {
        0
    } else {
        // A rough, not exact, line budget — good enough to make the errors
        // readable without starving the list below it; getting the wrapped
        // line count exactly right would need the width `Layout::split`
        // itself is about to decide, which is not available yet.
        ((state.environment_errors.len() * 2) as u16).min(inner.height / 2)
    };
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(error_rows)])
        .split(inner);
    let (list_area, error_area) = (sections[0], sections[1]);

    if !state.environment_errors.is_empty() {
        let text = state
            .environment_errors
            .iter()
            .map(|(name, error)| {
                format_error(&format!("Environment '{name}' failed to load"), error)
            })
            .collect::<Vec<_>>()
            .join("\n");
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), error_area);
    }

    if state.environments.is_empty() {
        let text = if state.environment_errors.is_empty() {
            "No environments found in .sendra/environments/."
        } else {
            "No environments loaded successfully — see the errors below."
        };
        frame.render_widget(Paragraph::new(text), list_area);
        return;
    }

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(list_area);

    let items: Vec<ListItem> = state
        .environments
        .iter()
        .enumerate()
        .map(|(index, named)| {
            let marker = if Some(index) == state.active_environment {
                "* "
            } else {
                "  "
            };
            ListItem::new(format!("{marker}{}", named.name))
        })
        .collect();
    let list = List::new(items).highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut list_state = ListState::default().with_selected(Some(cursor));
    frame.render_stateful_widget(list, panes[0], &mut list_state);

    // The cursor's environment, not necessarily the active one — showing
    // what a user is about to pick, before they confirm it.
    let highlighted = &state.environments[cursor.min(state.environments.len() - 1)];
    let mut lines = vec![format!("Variables for '{}':", highlighted.name)];
    if highlighted.environment.variables.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        for (name, value) in &highlighted.environment.variables {
            lines.push(format!("  {name} = {value}"));
        }
    }
    frame.render_widget(
        Paragraph::new(lines.join("\n")).wrap(Wrap { trim: false }),
        panes[1],
    );
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use sendra_core::SendraError;

    use super::super::state::Message;
    use super::super::test_support::*;
    use super::super::update::update;
    use super::*;

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

        let resolved = resolve_preview(request, Path::new("."), &Environment::default())
            .expect("no placeholders to fail on");

        assert_eq!(resolved.url, "https://example.com");
    }

    #[test]
    fn resolve_preview_surfaces_variable_not_found_with_no_environment() {
        let document = Document::from_yaml_str(
            "name: One\nmethod: GET\nurl: https://example.com/{{user_id}}\n",
        )
        .expect("valid single request");
        let request = &document.requests()[0];

        let error = resolve_preview(request, Path::new("."), &Environment::default()).expect_err(
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

        let resolved = resolve_preview(request, Path::new("."), &environment)
            .expect("the environment defines the variable the request needs");

        assert_eq!(resolved.url, "https://example.com/42");
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

    fn response_with(headers: &[(&str, &str)], body: &str) -> Response {
        Response {
            status: 201,
            status_text: "Created".to_string(),
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            body: body.to_string(),
            elapsed: std::time::Duration::from_millis(42),
            redirects: Vec::new(),
        }
    }

    #[test]
    fn format_response_matches_the_status_headers_body_layout() {
        let response = response_with(&[("X-Request-Id", "abc123")], "plain text body");

        let text = format_response(&response);

        assert_eq!(
            text,
            "201 Created  42 ms\nX-Request-Id: abc123\n\nplain text body"
        );
    }

    #[test]
    fn format_response_omits_the_body_section_when_the_body_is_empty() {
        let response = response_with(&[], "");

        let text = format_response(&response);

        assert_eq!(text, "201 Created  42 ms");
    }

    #[test]
    fn body_for_display_pretty_prints_a_json_content_type() {
        let response = response_with(
            &[("Content-Type", "application/json; charset=utf-8")],
            "{\"id\":1,\"name\":\"widget\"}",
        );

        let displayed = body_for_display(&response);

        assert_eq!(displayed, "{\n  \"id\": 1,\n  \"name\": \"widget\"\n}");
    }

    #[test]
    fn body_for_display_leaves_non_json_content_types_untouched() {
        let response = response_with(&[("Content-Type", "text/plain")], "{\"id\":1}");

        let displayed = body_for_display(&response);

        assert_eq!(
            displayed, "{\"id\":1}",
            "a non-JSON content type must not be re-formatted"
        );
    }

    #[test]
    fn body_for_display_leaves_malformed_json_untouched() {
        let response = response_with(&[("Content-Type", "application/json")], "not json");

        let displayed = body_for_display(&response);

        assert_eq!(
            displayed, "not json",
            "a JSON content type whose body does not actually parse must be shown verbatim, not dropped or panicked on"
        );
    }

    #[test]
    fn claims_json_matches_a_vendor_json_suffix() {
        assert!(claims_json(&[(
            "content-type".to_string(),
            "application/vnd.api+json".to_string()
        )]));
    }

    /// Parses a single request from `yaml` and evaluates its `assertions:`
    /// block against `response` via the real `Assertions::evaluate` —
    /// sendra-core's own machinery, the same thing `run_request::execute`
    /// calls, not a hand-built `AssertionReport`, which the type's private
    /// fields would not even allow from outside its own crate.
    fn evaluate_assertions(yaml: &str, response: &Response) -> AssertionReport {
        let request = Document::from_yaml_str(yaml)
            .expect("valid test request")
            .requests()[0]
            .clone();
        request
            .assertions
            .expect("the test YAML declares an `assertions:` block")
            .evaluate(response)
    }

    /// Same as [`evaluate_assertions`] for a `capture:` block, via the real
    /// `Captures::evaluate`.
    fn evaluate_capture(yaml: &str, response: &Response) -> CaptureReport {
        let request = Document::from_yaml_str(yaml)
            .expect("valid test request")
            .requests()[0]
            .clone();
        request
            .capture
            .expect("the test YAML declares a `capture:` block")
            .evaluate(response, &Environment::default())
    }

    #[test]
    fn format_assertions_marks_a_request_with_no_assertions_distinctly() {
        let text = format_assertions(&AssertionReport::default());

        assert_eq!(text, "assertions\n  no assertions declared");
    }

    #[test]
    fn format_assertions_shows_every_result_individually_with_pass_fail() {
        let response = response_with(&[], "");
        let report = evaluate_assertions(
            "method: GET\nurl: https://example.com\nassertions:\n  status: 201\n  status_in: [404]\n",
            &response,
        );

        let text = format_assertions(&report);

        assert!(
            text.contains("✓ status is 201"),
            "the passing assertion must be shown on its own line: {text}"
        );
        assert!(
            text.contains("✗ status is one of [404]"),
            "the failing assertion must be shown on its own line, not folded into an aggregate: {text}"
        );
        assert!(
            text.contains("1 passed, 1 failed"),
            "a mixed report must show both counts: {text}"
        );
        assert_ne!(
            text,
            format_assertions(&AssertionReport::default()),
            "a report with real (even if all-failing) results must not read the same as \
             \"no assertions declared\""
        );
    }

    #[test]
    fn format_assertions_all_passing_reads_differently_from_none_declared() {
        let response = response_with(&[], "");
        let report = evaluate_assertions(
            "method: GET\nurl: https://example.com\nassertions:\n  status: 201\n",
            &response,
        );

        let text = format_assertions(&report);

        assert!(text.contains("1 passed"));
        assert!(
            !text.contains("no assertions declared"),
            "an assertions block that all passed must not be confused with none being declared: {text}"
        );
    }

    #[test]
    fn format_capture_section_marks_a_request_with_no_captures_distinctly() {
        let text = format_capture_section(&CaptureReport::default(), true);

        assert_eq!(text, "capture\n  no captures declared");
    }

    #[test]
    fn format_capture_section_masks_the_value_by_default() {
        let response = response_with(&[("X-Token", "super-secret")], "");
        let report = evaluate_capture(
            "method: GET\nurl: https://example.com\ncapture:\n  token: {header: X-Token}\n",
            &response,
        );

        let path = report.results()[0].path.clone();
        let masked = format_capture_section(&report, false);

        assert!(
            masked.contains(&format!("token from `{path}` = {REDACTED_CAPTURE_VALUE}")),
            "the value must be masked by default: {masked}"
        );
        assert!(
            !masked.contains("super-secret"),
            "the real captured value must not appear when not revealed: {masked}"
        );
    }

    #[test]
    fn format_capture_section_reveals_the_value_when_asked() {
        let response = response_with(&[("X-Token", "super-secret")], "");
        let report = evaluate_capture(
            "method: GET\nurl: https://example.com\ncapture:\n  token: {header: X-Token}\n",
            &response,
        );

        let path = report.results()[0].path.clone();
        let revealed = format_capture_section(&report, true);

        assert!(
            revealed.contains(&format!("token from `{path}` = super-secret")),
            "the real value must appear once revealed: {revealed}"
        );
    }

    #[test]
    fn format_capture_section_never_masks_a_failure() {
        // No `X-Token` header in the response, so the capture fails — and a
        // failure never had a value to redact in the first place, the same
        // rule sendra-cli's own `--json` `CaptureRecord` follows.
        let response = response_with(&[], "");
        let report = evaluate_capture(
            "method: GET\nurl: https://example.com\ncapture:\n  token: {header: X-Token}\n",
            &response,
        );

        let path = report.results()[0].path.clone();
        let masked = format_capture_section(&report, false);
        let revealed = format_capture_section(&report, true);

        assert_eq!(
            masked, revealed,
            "a failed capture has no value, so masking it must not change its rendering"
        );
        assert!(
            !masked.contains(REDACTED_CAPTURE_VALUE),
            "a failure is shown as a failure, not as a redacted value: {masked}"
        );
        assert!(masked.contains(&format!("✗ token from `{path}`")));
    }

    #[test]
    fn view_masks_captures_by_default_reveals_on_toggle_and_remasks_on_a_new_run() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = loaded_state(VALID_COLLECTION);
        let response = response_with(&[("X-Token", "super-secret")], "");
        let capture = evaluate_capture(
            "method: GET\nurl: https://example.com\ncapture:\n  token: {header: X-Token}\n",
            &response,
        );
        let outcome = || RunOutcome {
            result: Ok(response.clone()),
            assertions: AssertionReport::default(),
            capture: capture.clone(),
        };

        let render = |state: &AppState| {
            let backend = TestBackend::new(100, 15);
            let mut terminal = Terminal::new(backend).expect("a test terminal builds");
            terminal
                .draw(|frame| view(state, frame))
                .expect("rendering must not panic");
            buffer_to_string(terminal.backend().buffer())
        };

        // A masked/revealed capture *line* is what's under test —
        // `= <redacted>` vs `= super-secret` — not whether "super-secret"
        // appears anywhere on screen at all: the raw `X-Token` response
        // header carries the same value and is shown in full regardless (by
        // design; see the doc comment on `format_capture_section`), so a
        // whole-screen search for the string would fail for the wrong
        // reason even when masking is working correctly.
        let capture_line_contains = |screen: &str, needle: &str| {
            screen
                .lines()
                .any(|line| line.contains("token from") && line.contains(needle))
        };

        update(&mut state, Message::RunRequested);
        update(&mut state, Message::RunCompleted(outcome()));
        let masked_screen = render(&state);
        assert!(
            capture_line_contains(&masked_screen, REDACTED_CAPTURE_VALUE),
            "captures must be masked by default:\n{masked_screen}"
        );
        assert!(
            !capture_line_contains(&masked_screen, "super-secret"),
            "the capture line must not show the real value by default:\n{masked_screen}"
        );

        update(&mut state, Message::ToggleRevealCaptures);
        let revealed_screen = render(&state);
        assert!(
            capture_line_contains(&revealed_screen, "super-secret"),
            "the real value must be on the capture line after the reveal keybinding:\n{revealed_screen}"
        );

        update(&mut state, Message::RunRequested);
        update(&mut state, Message::RunCompleted(outcome()));
        let next_run_screen = render(&state);
        assert!(
            capture_line_contains(&next_run_screen, REDACTED_CAPTURE_VALUE),
            "a new run must not carry the reveal over — masked again by default:\n{next_run_screen}"
        );
        assert!(
            !capture_line_contains(&next_run_screen, "super-secret"),
            "a new run's capture line must not still show the real value:\n{next_run_screen}"
        );
    }

    #[test]
    fn run_completed_err_renders_the_real_sendra_error_text() {
        let error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");
        let expected = error.to_string();

        let text = format_run_result(&failed_outcome(error), false);

        assert!(
            text.contains(&expected),
            "the panel must show the real SendraError text, got: {text}"
        );
    }

    /// A large body, drawn into a small area, must not overflow the pane,
    /// panic, or scroll past its own content — the actual `render_widget`
    /// call proves this rather than just the scroll-offset arithmetic
    /// (`format_response`/`format_run_result` above), since ratatui's own
    /// clipping is part of what makes this safe.
    #[test]
    fn a_large_body_renders_into_a_small_area_without_panicking() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let huge_body = (0..5000)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let outcome = RunOutcome {
            result: Ok(response_with(&[], &huge_body)),
            assertions: AssertionReport::default(),
            capture: CaptureReport::default(),
        };

        // Computed from the real formatted text rather than hand-counted,
        // so this stays correct however `format_run_result` lays out the
        // status/headers/body plus the assertions/capture sections below it.
        let total_lines = format_run_result(&outcome, false).lines().count();
        let backend = TestBackend::new(60, 5);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");

        // Scrolled absurdly far past the end of the content — proving the
        // clamp in `render_response_panel` (not just ratatui's own
        // clipping) keeps the footer's line numbers sane rather than
        // reporting a scroll position past `total`.
        terminal
            .draw(|frame| {
                render_response_panel(frame, frame.area(), &outcome, usize::MAX, false);
            })
            .expect("drawing a huge, over-scrolled body must not panic");

        let buffer = terminal.backend().buffer();
        let footer_row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, buffer.area.height - 1)].symbol())
            .collect();
        assert!(
            footer_row.contains(&format!("of {total_lines}")),
            "the footer must report the real total line count, got: {footer_row:?}"
        );
        assert!(
            !footer_row.contains(&format!("of {}", total_lines + 1)),
            "an over-scroll must not be reported as if it went past the real total"
        );
    }

    /// `Home`/`End` end to end, through the real `update()` + `view()`: `End`
    /// jumps straight to the bottom of a long body in one step (no repeated
    /// `PageDown`s needed), and `Home` from there jumps straight back to the
    /// top.
    #[test]
    fn home_and_end_jump_the_response_panel_to_the_real_top_and_bottom() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let body = (0..300)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let outcome = RunOutcome {
            result: Ok(response_with(&[], &body)),
            assertions: AssertionReport::default(),
            capture: CaptureReport::default(),
        };
        let total_lines = format_run_result(&outcome, false).lines().count();

        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        update(&mut state, Message::RunCompleted(outcome));

        let render = |state: &AppState| {
            let backend = TestBackend::new(60, 10);
            let mut terminal = Terminal::new(backend).expect("a test terminal builds");
            terminal
                .draw(|frame| view(state, frame))
                .expect("rendering must not panic");
            buffer_to_string(terminal.backend().buffer())
        };

        update(&mut state, Message::ScrollResponseBottom);
        let bottom_screen = render(&state);
        assert!(
            bottom_screen.contains(&format!("-{total_lines} of {total_lines}")),
            "End must jump straight to the real end of the body in one step, not \
             partway through it:\n{bottom_screen}"
        );

        update(&mut state, Message::ScrollResponseTop);
        let top_screen = render(&state);
        assert!(
            top_screen.contains("Line 1-"),
            "Home must jump straight back to the real top of the response:\n{top_screen}"
        );
        assert!(
            top_screen.contains("201 Created"),
            "the top of the response must show the status line:\n{top_screen}"
        );
    }

    /// Renders a completed successful run through the real `view()` — not
    /// just `format_response`/`render_response_panel` in isolation — and
    /// reads the actual character buffer back, so this is what the terminal
    /// would really show: proof the status, a header and the body all land
    /// on screen together, replacing the request preview as documented on
    /// `render_detail_pane`.
    #[test]
    fn view_renders_a_completed_success_as_a_real_response_panel() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        let response = response_with(&[("X-Request-Id", "abc123")], "hello world");
        update(
            &mut state,
            Message::RunCompleted(RunOutcome {
                result: Ok(response),
                assertions: AssertionReport::default(),
                capture: CaptureReport::default(),
            }),
        );

        let backend = TestBackend::new(100, 15);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        terminal
            .draw(|frame| view(&state, frame))
            .expect("rendering a completed run must not panic");

        let screen = buffer_to_string(terminal.backend().buffer());
        assert!(
            screen.contains("201 Created"),
            "the real status must be on screen:\n{screen}"
        );
        assert!(
            screen.contains("X-Request-Id: abc123"),
            "a real response header must be on screen:\n{screen}"
        );
        assert!(
            screen.contains("hello world"),
            "the real response body must be on screen:\n{screen}"
        );
    }

    /// Same as above for the failure path: a real `SendraError`'s message
    /// must appear on screen, not a generic "failed" placeholder.
    #[test]
    fn view_renders_a_completed_failure_with_the_real_error_message() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        let error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");
        let expected_message = error.to_string();
        update(&mut state, Message::RunCompleted(failed_outcome(error)));

        let backend = TestBackend::new(100, 15);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        terminal
            .draw(|frame| view(&state, frame))
            .expect("rendering a failed run must not panic");

        let screen = buffer_to_string(terminal.backend().buffer());
        assert!(
            screen.contains(&expected_message),
            "the real SendraError message must be on screen, not a generic \
             placeholder:\n{screen}\nexpected to find: {expected_message}"
        );
    }

    fn buffer_to_string(buffer: &ratatui::buffer::Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // --- status_help_text: the bar's single source of truth -------------

    #[test]
    fn help_bar_shows_browser_keys_when_idle() {
        let state = loaded_state(VALID_COLLECTION);

        let text = status_help_text(&state);

        assert!(text.contains("nav"), "{text}");
        assert!(text.contains("run"), "{text}");
        assert!(text.contains("env"), "{text}");
        assert!(text.contains('q'), "{text}");
        // Not yet relevant while idle: nothing has run, so nothing to
        // scroll or reveal.
        assert!(!text.contains("scroll"), "{text}");
        assert!(!text.contains("reveal"), "{text}");
    }

    #[test]
    fn help_bar_shows_only_overlay_keys_when_the_overlay_is_open() {
        let mut state = state_with_environments(&["default", "staging"]);
        update(&mut state, Message::OpenEnvironmentOverlay);

        let text = status_help_text(&state);

        assert!(text.contains("confirm"), "{text}");
        assert!(text.contains("cancel"), "{text}");
        assert!(text.contains('q'), "{text}");
        // The overlay owns the keyboard: the browser's own run/env keys
        // must not be advertised alongside it.
        assert!(!text.contains("run"), "{text}");
        assert!(!text.contains("env"), "{text}");
    }

    #[test]
    fn help_bar_shows_only_quit_while_a_run_is_in_flight() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        assert!(matches!(state.run_state, RunState::InFlight));

        let text = status_help_text(&state);

        assert!(text.contains('q'), "{text}");
        // Every one of these is genuinely refused right now by `update`'s
        // own InFlight guard, so none of them belongs in the hint.
        assert!(!text.contains("nav"), "{text}");
        assert!(!text.contains("run again"), "{text}");
        assert!(!text.contains("env"), "{text}");
        assert!(!text.contains("scroll"), "{text}");
    }

    #[test]
    fn help_bar_shows_scroll_and_nav_keys_once_a_run_has_completed() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        update(&mut state, Message::RunCompleted(sample_outcome(200)));

        let text = status_help_text(&state);

        assert!(text.contains("scroll"), "{text}");
        assert!(text.contains("nav"), "{text}");
        assert!(text.contains("run again"), "{text}");
        assert!(text.contains("env"), "{text}");
        assert!(text.contains('q'), "{text}");
    }

    #[test]
    fn help_bar_only_mentions_capture_reveal_when_there_is_something_to_reveal() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        update(&mut state, Message::RunCompleted(sample_outcome(200)));
        let no_captures_text = status_help_text(&state);
        assert!(
            !no_captures_text.contains("reveal"),
            "a run with no `capture:` block has nothing to reveal: {no_captures_text}"
        );

        let response = response_with(&[("X-Token", "super-secret")], "");
        let capture = evaluate_capture(
            "method: GET\nurl: https://example.com\ncapture:\n  token: {header: X-Token}\n",
            &response,
        );
        update(&mut state, Message::RunRequested);
        update(
            &mut state,
            Message::RunCompleted(RunOutcome {
                result: Ok(response),
                assertions: AssertionReport::default(),
                capture,
            }),
        );

        let with_captures_text = status_help_text(&state);
        assert!(
            with_captures_text.contains("reveal"),
            "a run with a non-empty `capture:` block must advertise the reveal key: {with_captures_text}"
        );
    }

    /// The four contexts must not all read the same — the whole point of a
    /// *context-sensitive* bar, proven here as one assertion over the same
    /// four states the tests above exercise individually.
    #[test]
    fn help_bar_text_differs_across_all_four_contexts() {
        let idle = loaded_state(VALID_COLLECTION);

        let mut overlaid = state_with_environments(&["default"]);
        update(&mut overlaid, Message::OpenEnvironmentOverlay);

        let mut in_flight = loaded_state(VALID_COLLECTION);
        update(&mut in_flight, Message::RunRequested);

        let mut completed = loaded_state(VALID_COLLECTION);
        update(&mut completed, Message::RunRequested);
        update(&mut completed, Message::RunCompleted(sample_outcome(200)));

        let texts = [
            status_help_text(&idle),
            status_help_text(&overlaid),
            status_help_text(&in_flight),
            status_help_text(&completed),
        ];

        for (i, a) in texts.iter().enumerate() {
            for (j, b) in texts.iter().enumerate() {
                if i != j {
                    assert_ne!(
                        a, b,
                        "contexts {i} and {j} must show different help text, both got: {a:?}"
                    );
                }
            }
        }
    }

    /// End-to-end proof, through the real `view()`, that the bottom row
    /// actually changes as the bar's own state changes — not just that
    /// `status_help_text` returns different strings in isolation.
    #[test]
    fn view_renders_different_help_bar_text_across_contexts() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let render = |state: &AppState| {
            let backend = TestBackend::new(100, 15);
            let mut terminal = Terminal::new(backend).expect("a test terminal builds");
            terminal
                .draw(|frame| view(state, frame))
                .expect("rendering must not panic");
            buffer_to_string(terminal.backend().buffer())
        };

        let mut state = loaded_state(VALID_COLLECTION);
        let browsing_screen = render(&state);
        assert!(browsing_screen.contains("nav"));
        assert!(!browsing_screen.contains("scroll"));

        update(&mut state, Message::RunRequested);
        let in_flight_screen = render(&state);
        assert!(in_flight_screen.contains("Running request"));
        assert!(!in_flight_screen.contains("nav"));

        update(&mut state, Message::RunCompleted(sample_outcome(200)));
        let completed_screen = render(&state);
        assert!(completed_screen.contains("scroll"));
        assert!(completed_screen.contains("nav"));

        // Back to idle, then the fourth context: the environment overlay.
        // `state_with_environments` alone has no loaded collection, so this
        // goes through `loaded_state` plus environments instead, to prove
        // the bottom row still renders correctly underneath the popup.
        let mut overlaid = loaded_state(VALID_COLLECTION);
        overlaid.environments = vec![named_environment("default", &[])];
        update(&mut overlaid, Message::OpenEnvironmentOverlay);
        let overlay_screen = render(&overlaid);
        assert!(overlay_screen.contains("confirm"));
        assert!(overlay_screen.contains("cancel"));
        assert!(!overlay_screen.contains("run again"));

        assert_ne!(browsing_screen, in_flight_screen);
        assert_ne!(in_flight_screen, completed_screen);
        assert_ne!(browsing_screen, completed_screen);
        assert_ne!(browsing_screen, overlay_screen);
        assert_ne!(completed_screen, overlay_screen);
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

    #[test]
    fn help_bar_shows_editing_state_and_its_own_keys() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);

        update(&mut state, Message::EnterEditMode);

        let text = status_help_text(&state);
        assert!(text.contains("Editing"));
        assert!(text.contains("ctrl+s save"));
        assert!(text.contains("esc cancel"));
        assert!(
            !text.contains("nav"),
            "browsing keys must not be advertised while editing"
        );
    }

    // --- Auth editing -------------------------------------------------------

    const REQUEST_WITH_BEARER_AUTH: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
    auth:
      bearer: secret-token
";

    const REQUEST_WITH_BASIC_AUTH: &str = "\
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

    const REQUEST_WITH_API_KEY_AUTH: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
    auth:
      api_key:
        in: header
        name: X-Api-Key
        value: abc123
";

    const REQUEST_WITH_OAUTH_AUTH: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
    auth:
      oauth:
        grant_type: client_credentials
        token_url: https://auth.example.com/token
        client_id: my-client
        client_secret: my-secret
";

    fn render_screen(state: &AppState) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        // Tall enough that the edit pane's auth section and its "Resolved
        // auth" preview line are never clipped by the pane's own height —
        // the pane has no scrolling of its own (see `render_edit_pane`'s
        // doc comment), so a too-short terminal would silently cut off
        // exactly what these tests assert on.
        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        terminal
            .draw(|frame| view(state, frame))
            .expect("rendering must not panic");
        buffer_to_string(terminal.backend().buffer())
    }

    #[test]
    fn edit_pane_shows_the_bearer_token_and_its_resolved_authorization_header() {
        let mut state = loaded_state(REQUEST_WITH_BEARER_AUTH);
        update(&mut state, Message::EnterEditMode);

        let screen = render_screen(&state);

        assert!(
            screen.contains("Bearer token: secret-token"),
            "the bearer token field must be shown:\n{screen}"
        );
        assert!(
            screen.contains("Resolved auth"),
            "the live resolved-auth preview must be shown:\n{screen}"
        );
        assert!(
            screen.contains("Authorization: Bearer secret-token"),
            "the resolved preview must show the real Authorization header \
             resolve_auth would send:\n{screen}"
        );
    }

    #[test]
    fn edit_pane_shows_basic_auth_user_and_password_fields() {
        let mut state = loaded_state(REQUEST_WITH_BASIC_AUTH);
        update(&mut state, Message::EnterEditMode);

        let screen = render_screen(&state);

        assert!(screen.contains("User: ada"), "{screen}");
        assert!(screen.contains("Password: hunter2"), "{screen}");
    }

    #[test]
    fn edit_pane_shows_api_key_fields_and_its_fixed_location_enum() {
        let mut state = loaded_state(REQUEST_WITH_API_KEY_AUTH);
        update(&mut state, Message::EnterEditMode);

        let screen = render_screen(&state);

        assert!(screen.contains("Name: X-Api-Key"), "{screen}");
        assert!(screen.contains("Value: abc123"), "{screen}");
        assert!(
            screen.contains("Location: header"),
            "the location must show its real fixed-enum value, not free text:\n{screen}"
        );
        assert!(
            screen.contains("←/→ to change"),
            "a toggle hint must be shown for the fixed-enum location field:\n{screen}"
        );
        assert!(
            screen.contains("X-Api-Key: abc123"),
            "the resolved preview must show the real header resolve_auth would add:\n{screen}"
        );
    }

    #[test]
    fn edit_pane_shows_oauth_config_fields_as_editable_plain_text() {
        let mut state = loaded_state(REQUEST_WITH_OAUTH_AUTH);
        update(&mut state, Message::EnterEditMode);

        let screen = render_screen(&state);

        assert!(
            screen.contains("Grant type: client_credentials"),
            "{screen}"
        );
        assert!(
            screen.contains("Token URL: https://auth.example.com/token"),
            "{screen}"
        );
        assert!(screen.contains("Client ID: my-client"), "{screen}");
        assert!(screen.contains("Client secret: my-secret"), "{screen}");
    }

    #[test]
    fn edit_pane_marks_oauth_as_read_only_in_the_resolved_auth_preview() {
        // This is the one place this issue's OAuth scoping decision is
        // visible in the UI itself: the config fields above are editable,
        // but the live "Resolved auth" preview must say plainly that no
        // token was actually acquired here, rather than silently showing
        // nothing or a fabricated bearer value.
        let mut state = loaded_state(REQUEST_WITH_OAUTH_AUTH);
        update(&mut state, Message::EnterEditMode);

        let screen = render_screen(&state);

        assert!(
            screen.contains("OAuth token acquired at request time"),
            "the resolved-auth preview must name the OAuth limitation, not just be blank:\n{screen}"
        );
    }

    #[test]
    fn edit_pane_shows_no_auth_configured_for_a_request_with_no_auth_block() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::EnterEditMode);

        let screen = render_screen(&state);

        assert!(
            screen.contains("no auth configured on this request"),
            "{screen}"
        );
    }

    #[test]
    fn tab_reaches_the_auth_section_and_toggling_the_api_key_location_shows_up_live() {
        let mut state = loaded_state(REQUEST_WITH_API_KEY_AUTH);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::Auth(AuthField::ApiKeyLocation);

        let before = render_screen(&state);
        assert!(before.contains("X-Api-Key: abc123"), "{before}");
        assert!(!before.contains("query X-Api-Key=abc123"), "{before}");

        update(&mut state, Message::EditCursorRight);
        let after = render_screen(&state);
        assert!(
            after.contains("Location: query"),
            "the displayed location must reflect the toggle:\n{after}"
        );
        assert!(
            after.contains("query X-Api-Key=abc123"),
            "the resolved preview must move the api key into the query string too:\n{after}"
        );
    }

    // --- error handling ---------------------------------------------------

    #[test]
    fn format_error_shows_a_heading_and_the_real_error_verbatim() {
        let error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");
        let expected = error.to_string();

        let text = format_error("Failed to load collection", &error);

        assert!(text.starts_with('⚠'), "{text}");
        assert!(text.contains("Failed to load collection"), "{text}");
        assert!(
            text.contains(&expected),
            "the real error text must appear verbatim: {text}"
        );
    }

    #[test]
    fn help_bar_offers_only_env_and_quit_with_no_collection_loaded() {
        for state in [
            AppState::default(),
            {
                let mut s = AppState::default();
                update(&mut s, Message::NoCollectionPath);
                s
            },
            {
                let mut s = AppState::default();
                let error =
                    Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");
                update(
                    &mut s,
                    Message::CollectionLoaded {
                        base_dir: PathBuf::from("."),
                        result: Box::new(Err(error)),
                    },
                );
                s
            },
        ] {
            let text = status_help_text(&state);
            assert!(text.contains("env"), "{text}");
            assert!(text.contains('q'), "{text}");
            assert!(
                !text.contains("nav") && !text.contains("run"),
                "no collection means nothing to navigate or run: {text}"
            );
        }
    }

    /// End-to-end proof, through the real `view()`, of three failure
    /// triggers: a malformed collection, a malformed environment
    /// file, and — separately, in `run_request`'s own tests — an
    /// unreachable host. Each must show a readable message via the same
    /// [`format_error`] path, and the bar underneath must still say `q quit`
    /// works.
    #[test]
    fn view_shows_a_readable_error_for_a_malformed_collection_and_stays_responsive() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = AppState::default();
        let error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");
        let expected = error.to_string();
        update(
            &mut state,
            Message::CollectionLoaded {
                base_dir: PathBuf::from("."),
                result: Box::new(Err(error)),
            },
        );

        let backend = TestBackend::new(100, 15);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        terminal
            .draw(|frame| view(&state, frame))
            .expect("rendering a load failure must not panic");

        let screen = buffer_to_string(terminal.backend().buffer());
        assert!(
            screen.contains(&expected),
            "the real parse error must be readable on screen:\n{screen}"
        );
        assert!(
            screen.contains('q'),
            "the help bar must still show quit works:\n{screen}"
        );
    }

    #[test]
    fn view_shows_a_readable_error_for_a_malformed_environment_file() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = loaded_state(VALID_COLLECTION);
        let bad_error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");
        let expected = bad_error.to_string();
        update(
            &mut state,
            Message::EnvironmentsLoaded {
                environments: Vec::new(),
                errors: vec![("staging".to_string(), bad_error)],
            },
        );
        update(&mut state, Message::OpenEnvironmentOverlay);

        let backend = TestBackend::new(100, 15);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        terminal
            .draw(|frame| view(&state, frame))
            .expect("rendering an environment load failure must not panic");

        let screen = buffer_to_string(terminal.backend().buffer());
        assert!(
            screen.contains("staging"),
            "the broken environment's name must be visible:\n{screen}"
        );
        assert!(
            screen.contains(&expected),
            "the real environment error must be readable on screen:\n{screen}"
        );
    }

    #[test]
    fn run_error_display_covers_both_the_core_and_runtime_variants() {
        // `Core` displays as the wrapped `SendraError`'s own `Display` —
        // core's fixed wording for this variant, not the raw `io::Error`
        // `#[source]` alone does not interpolate into it.
        let core_error = crate::run_request::RunError::from(SendraError::CurrentDir(
            std::io::Error::other("boom"),
        ));
        assert_eq!(
            core_error.to_string(),
            SendraError::CurrentDir(std::io::Error::other("boom")).to_string()
        );

        let runtime_error =
            crate::run_request::RunError::RuntimeUnavailable(std::io::Error::other("no threads"));
        assert!(runtime_error.to_string().contains("no threads"));
        assert!(runtime_error.to_string().contains("runtime"));
    }

    // --- resize handling ---------------------------------------------

    /// The `Message::Resize` no-op, end to end: a resized backend, re-drawn
    /// through the real `view()` between two calls with no `update()` call
    /// for the resize itself in between (mirroring how `main::run` actually
    /// handles it — `Message::Resize` changes no state; the next
    /// `terminal.draw` picks up the new size on its own). Both a shrink and
    /// a subsequent grow are exercised on a completed run's response panel,
    /// scrolled deep into a long body, so this also proves `response_scroll`
    /// re-clamps against whichever area is current at render time rather
    /// than pointing past a now-gone taller viewport.
    #[test]
    fn resizing_between_renders_relayouts_without_panicking() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let body = (0..500)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let outcome = RunOutcome {
            result: Ok(response_with(&[], &body)),
            assertions: AssertionReport::default(),
            capture: CaptureReport::default(),
        };
        let total_lines = format_run_result(&outcome, false).lines().count();

        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        update(&mut state, Message::RunCompleted(outcome));
        // Deep enough to be well past what any of the sizes below can show,
        // so every draw below exercises the render-time clamp.
        state.response_scroll = 10_000;

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        terminal
            .draw(|frame| view(&state, frame))
            .expect("initial draw must not panic");

        // Shrink drastically — down to a sliver of a terminal — and redraw.
        terminal.backend_mut().resize(20, 6);
        terminal
            .draw(|frame| view(&state, frame))
            .expect("drawing after a drastic shrink must not panic");
        let shrunk = terminal.backend().buffer();
        assert_eq!(
            shrunk.area,
            Rect::new(0, 0, 20, 6),
            "the buffer must track the new, smaller size exactly — no leftover \
             cells from the previous 100x30 frame"
        );
        let shrunk_screen = buffer_to_string(shrunk);
        // At 20 columns wide the footer's `of {total}` tail is clipped off
        // screen, so what actually proves the clamp worked is the line
        // range itself ending at (not past) the real total.
        assert!(
            shrunk_screen.contains(&format!("-{total_lines}")),
            "the footer's visible range must end exactly at the real total \
             line count ({total_lines}) once scrolled past the end, not \
             beyond it or blank:\n{shrunk_screen}"
        );

        // Grow back past the original size and redraw again.
        terminal.backend_mut().resize(150, 40);
        terminal
            .draw(|frame| view(&state, frame))
            .expect("drawing after growing must not panic");
        let grown = terminal.backend().buffer();
        assert_eq!(
            grown.area,
            Rect::new(0, 0, 150, 40),
            "the buffer must track the new, larger size exactly"
        );
        let grown_screen = buffer_to_string(grown);
        assert!(
            grown_screen.contains("line 4"),
            "the grown frame must show real body content, not a blank pane \
             left over from the shrunk size:\n{grown_screen}"
        );
    }

    /// Every top-level `view()` state — loading, a failed load, the request
    /// browser, a completed response panel, and the environment overlay on
    /// top of it — drawn into a terminal shrunk to a single-digit size, the
    /// smallest a real terminal resize could plausibly produce. Content is
    /// necessarily cramped or clipped; nothing may panic.
    #[test]
    fn every_screen_survives_a_very_small_terminal_size() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let draw = |state: &AppState, width: u16, height: u16| {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("a test terminal builds");
            terminal
                .draw(|frame| view(state, frame))
                .unwrap_or_else(|err| panic!("{width}x{height} draw must not panic: {err}"));
        };

        let sizes: [(u16, u16); 4] = [(1, 1), (2, 1), (1, 2), (4, 3)];

        for (width, height) in sizes {
            draw(&AppState::default(), width, height);

            let mut browsing = loaded_state(THREE_REQUEST_COLLECTION);
            draw(&browsing, width, height);

            update(&mut browsing, Message::RunRequested);
            update(&mut browsing, Message::RunCompleted(sample_outcome(200)));
            browsing.response_scroll = 9_999;
            draw(&browsing, width, height);

            let mut overlaid = loaded_state(VALID_COLLECTION);
            overlaid.environments = vec![
                named_environment("default", &[("token", "abc")]),
                named_environment("staging", &[]),
            ];
            update(&mut overlaid, Message::OpenEnvironmentOverlay);
            draw(&overlaid, width, height);

            let mut failed = AppState::default();
            let error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");
            update(
                &mut failed,
                Message::CollectionLoaded {
                    base_dir: PathBuf::from("."),
                    result: Box::new(Err(error)),
                },
            );
            draw(&failed, width, height);

            let mut editing = loaded_state(THREE_REQUEST_COLLECTION);
            update(&mut editing, Message::EnterEditMode);
            type_into_focused_field(&mut editing, "X");
            draw(&editing, width, height);
        }
    }

    /// The edit pane itself: both fields' current text, the `▶` focus
    /// marker moving with `Message::EditFocusNext`, and the inline
    /// validation message appearing for an invalid method and disappearing
    /// once it is fixed — the visual half of the coverage
    /// `invalid_method_shows_an_inline_error_blocks_save_and_stays_editable`
    /// already gives the underlying state.
    #[test]
    fn edit_pane_renders_both_fields_focus_marker_and_validation_message() {
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
        update(&mut state, Message::EnterEditMode);

        let initial_screen = render(&state);
        assert!(initial_screen.contains("Method: GET"));
        assert!(initial_screen.contains("URL:    https://example.com"));
        assert!(
            initial_screen.contains("▶ Method"),
            "focus starts on the method field:\n{initial_screen}"
        );

        backspace_n(&mut state, "GET".len());
        type_into_focused_field(&mut state, "FOOBAR");
        let invalid_screen = render(&state);
        assert!(
            invalid_screen.contains("FOOBAR"),
            "the invalid text itself must still be shown:\n{invalid_screen}"
        );
        assert!(
            // Line-wrapped across rows at this width, so checked as two
            // shorter substrings rather than one that could straddle a
            // wrap point.
            invalid_screen.contains("not a valid HTTP") && invalid_screen.contains("method"),
            "the inline validation message must be visible:\n{invalid_screen}"
        );

        update(&mut state, Message::EditFocusNext);
        let focus_moved_screen = render(&state);
        assert!(
            focus_moved_screen.contains("▶ URL"),
            "focus must have moved to the URL field:\n{focus_moved_screen}"
        );
    }

    fn render_state(state: &AppState) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(80, 15);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        terminal
            .draw(|frame| view(state, frame))
            .expect("rendering must not panic");
        buffer_to_string(terminal.backend().buffer())
    }

    const REQUEST_WITH_HEADERS: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
    headers:
      Accept: application/json
      X-Env: staging
";

    #[test]
    fn edit_pane_shows_no_headers_placeholder_for_a_request_with_none() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::EnterEditMode);

        let screen = render_state(&state);

        assert!(screen.contains("Headers:"));
        assert!(
            screen.contains("(none)"),
            "an edit with no headers must say so, not just show a bare heading:\n{screen}"
        );
    }

    #[test]
    fn edit_pane_lists_every_header_row_with_its_key_and_value() {
        let mut state = loaded_state(REQUEST_WITH_HEADERS);
        update(&mut state, Message::EnterEditMode);

        let screen = render_state(&state);

        assert!(screen.contains("Key: Accept"));
        assert!(screen.contains("Value: application/json"));
        assert!(screen.contains("Key: X-Env"));
        assert!(screen.contains("Value: staging"));
    }

    #[test]
    fn edit_pane_marks_whichever_header_side_has_focus() {
        let mut state = loaded_state(REQUEST_WITH_HEADERS);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::HeaderKey(0);

        let key_focused = render_state(&state);
        let key_line = key_focused
            .lines()
            .find(|line| line.contains("Key: Accept"))
            .expect("the Accept row must be on screen");
        assert!(
            key_line.trim_start().starts_with('▶'),
            "the key side must carry the focus marker:\n{key_line}"
        );

        state.edit_mode.as_mut().unwrap().focus = EditField::HeaderValue(0);
        let value_focused = render_state(&state);
        let value_line = value_focused
            .lines()
            .find(|line| line.contains("Key: Accept"))
            .expect("the Accept row must be on screen");
        assert!(
            !value_line.trim_start().starts_with('▶'),
            "focus moved off the key side, so it must no longer carry the marker:\n{value_line}"
        );
        assert!(
            value_line.contains('▶'),
            "the value side must now carry the focus marker:\n{value_line}"
        );
    }

    #[test]
    fn add_header_row_key_bind_shows_up_as_a_new_empty_row_on_screen() {
        let mut state = loaded_state(REQUEST_WITH_HEADERS);
        update(&mut state, Message::EnterEditMode);

        update(&mut state, Message::AddHeaderRow);
        let screen = render_state(&state);

        assert!(
            screen.contains("[2] Key:"),
            "a third, empty header row must now be on screen:\n{screen}"
        );
    }

    #[test]
    fn delete_header_row_key_bind_removes_a_row_from_screen() {
        let mut state = loaded_state(REQUEST_WITH_HEADERS);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::HeaderKey(0);

        update(&mut state, Message::DeleteHeaderRow);
        let screen = render_state(&state);

        assert!(
            !screen.contains("Accept"),
            "the deleted row's key must no longer be on screen:\n{screen}"
        );
        assert!(
            screen.contains("X-Env"),
            "the remaining row must still be on screen:\n{screen}"
        );
    }

    const REQUEST_WITH_PLAIN_BODY: &str = "\
name: test
requests:
  - name: One
    method: POST
    url: https://example.com
    body: hello world
";

    const REQUEST_WITH_JSON_BODY: &str = "\
name: test
requests:
  - name: One
    method: POST
    url: https://example.com
    json:
      name: ada
";

    const REQUEST_WITH_BODY_FILE: &str = "\
name: test
requests:
  - name: One
    method: POST
    url: https://example.com
    body_file: ./payload.json
";

    const REQUEST_WITH_FORM_BODY: &str = "\
name: test
requests:
  - name: One
    method: POST
    url: https://example.com
    form:
      username: ada
";

    const REQUEST_WITH_MULTIPART_BODY: &str = "\
name: test
requests:
  - name: One
    method: POST
    url: https://example.com
    multipart:
      - name: description
        value: a photo of my cat
";

    #[test]
    fn edit_pane_shows_a_plain_text_body_as_an_editable_raw_area() {
        let mut state = loaded_state(REQUEST_WITH_PLAIN_BODY);
        update(&mut state, Message::EnterEditMode);

        let screen = render_state(&state);

        assert!(screen.contains("Body (raw)"));
        assert!(screen.contains("hello world"));
    }

    #[test]
    fn edit_pane_shows_a_json_body_pretty_printed_and_editable() {
        let mut state = loaded_state(REQUEST_WITH_JSON_BODY);
        update(&mut state, Message::EnterEditMode);

        let screen = render_state(&state);

        assert!(screen.contains("Body (json)"));
        assert!(screen.contains("\"name\""));
        assert!(screen.contains("\"ada\""));
    }

    #[test]
    fn edit_pane_shows_body_file_as_read_only_with_its_path_visible() {
        let mut state = loaded_state(REQUEST_WITH_BODY_FILE);
        update(&mut state, Message::EnterEditMode);

        let screen = render_state(&state);

        assert!(
            screen.contains("./payload.json"),
            "the path must be visible so the limitation is honest, not just documented:\n{screen}"
        );
        assert!(screen.contains("not editable"));
        // No text area marker for a field that isn't there.
        assert!(!screen.contains("▶ Body"));
    }

    #[test]
    fn edit_pane_shows_form_body_as_read_only_with_field_count() {
        let mut state = loaded_state(REQUEST_WITH_FORM_BODY);
        update(&mut state, Message::EnterEditMode);

        let screen = render_state(&state);

        assert!(screen.contains("form body"));
        assert!(screen.contains("not editable"));
    }

    #[test]
    fn edit_pane_shows_multipart_body_as_read_only_with_part_count() {
        let mut state = loaded_state(REQUEST_WITH_MULTIPART_BODY);
        update(&mut state, Message::EnterEditMode);

        let screen = render_state(&state);

        assert!(screen.contains("multipart body"));
        assert!(screen.contains("not editable"));
    }

    #[test]
    fn tab_reaches_the_body_field_and_shows_its_focus_marker() {
        let mut state = loaded_state(REQUEST_WITH_PLAIN_BODY);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::Body;

        let screen = render_state(&state);

        assert!(
            screen
                .lines()
                .any(|line| line.contains("Body (raw)") && line.trim_start().starts_with('▶')),
            "the Body heading must carry the focus marker:\n{screen}"
        );
    }

    #[test]
    fn invalid_json_body_shows_an_inline_error_on_screen_after_a_failed_save() {
        let mut state = loaded_state(REQUEST_WITH_JSON_BODY);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::Body;
        type_into_focused_field(&mut state, "not valid json at all");

        update(&mut state, Message::SaveEdit);
        let screen = render_state(&state);

        assert!(
            screen.contains("not valid JSON") || screen.contains("Body is not valid JSON"),
            "the JSON parse error must be visible in the pane itself:\n{screen}"
        );
        assert!(
            screen.contains("not valid json at all"),
            "the invalid text the user typed must still be shown, not discarded:\n{screen}"
        );
    }

    #[test]
    fn a_multiline_body_renders_every_line() {
        let mut state = loaded_state(REQUEST_WITH_PLAIN_BODY);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::Body;
        update(&mut state, Message::EditInsertChar('\n'));
        type_into_focused_field(&mut state, "second line");

        let screen = render_state(&state);

        assert!(screen.contains("hello world"));
        assert!(screen.contains("second line"));
    }

    #[test]
    fn response_panel_scroll_reclamps_to_a_shrunk_area() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let body = (0..200)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let outcome = RunOutcome {
            result: Ok(response_with(&[], &body)),
            assertions: AssertionReport::default(),
            capture: CaptureReport::default(),
        };
        let total_lines = format_run_result(&outcome, false).lines().count();

        let backend = TestBackend::new(80, 40);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        // A scroll position that was in range for an 80x40 area but is
        // nowhere close to the top of the much shorter area used below.
        let deep_scroll = total_lines - 5;

        terminal
            .draw(|frame| {
                render_response_panel(frame, frame.area(), &outcome, deep_scroll, false);
            })
            .expect("initial draw must not panic");

        terminal.backend_mut().resize(80, 4);
        terminal
            .draw(|frame| {
                render_response_panel(frame, frame.area(), &outcome, deep_scroll, false);
            })
            .expect("drawing the same stale scroll offset into a shrunk area must not panic");

        let screen = buffer_to_string(terminal.backend().buffer());
        assert!(
            !screen.trim().is_empty(),
            "a re-clamped scroll must still show real content, not a blank pane:\n{screen}"
        );
        assert!(
            screen.contains(&format!("of {total_lines}")),
            "the footer must report the real total even after the area shrank:\n{screen}"
        );
    }
}
