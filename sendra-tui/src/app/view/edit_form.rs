//! The request edit form half of the detail pane: `render_edit_pane` and
//! every row-rendering helper it builds on — header rows, auth fields,
//! assertion rows, capture rows — plus `describe_resolved_auth`, the live
//! "Resolved auth" preview line shown under the edit session's own `Auth:`
//! section.

use ratatui::layout::Rect;
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;
use sendra_core::{ApiKeyLocation, Environment, OAuthGrantType, Request};

use super::super::preview;
use super::super::state::{
    AssertionRow, AuthEdit, AuthField, BodyEdit, CaptureKind, CaptureRow, EditField, EditState,
    HeaderRow, JsonOperator, OAuthLoginState,
};
use super::super::theme;
use super::{format_error, SPINNER_FRAMES};

/// Edit mode's own half of the detail pane: `name`, `method` and `url` as
/// live, cursor-addressable text fields, then a `Headers:` section listing every
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
pub(crate) fn render_edit_pane(
    frame: &mut Frame,
    area: Rect,
    edit: &EditState,
    base_request: &Request,
    environment: &Environment,
    spinner_tick: usize,
) {
    let name_marker = if edit.focus == EditField::Name {
        "▶"
    } else {
        " "
    };
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
        format!("{name_marker} Name:   {}", edit.name.value()),
        String::new(),
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
    // rendering bug, not "no error". `Name` gets no such row at all — see
    // `EditField::Name`'s own doc comment for why it is never rejected here.
    if edit.method_error.is_none() {
        lines[3] = String::new();
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

    // Only ever shown for `AuthEdit::OAuth { grant_type: AuthorizationCode,
    // .. }` — the one auth shape with a login to attempt at all. Uses the
    // exact `SPINNER_FRAMES`/`spinner_tick` a running request's own status
    // bar cycles, so a login in progress reads as "something is running"
    // the same visual way a run does, rather than a second, unrelated
    // spinner alphabet — see `view::SPINNER_FRAMES`'s own doc comment.
    if let AuthEdit::OAuth {
        grant_type: OAuthGrantType::AuthorizationCode,
        authorization_code,
        ..
    } = &edit.auth
    {
        lines.push(oauth_login_status_line(
            &authorization_code.login,
            spinner_tick,
        ));
    }

    lines.push(String::new());
    lines.push(format!(
        "Resolved auth → {}",
        describe_resolved_auth(base_request, edit, environment)
    ));

    lines.push(String::new());
    lines.push("Assertions (json path):".to_string());
    let assertions_start_line = lines.len();
    if edit.assertions.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        for (index, row) in edit.assertions.iter().enumerate() {
            lines.push(assertion_row_line(edit.focus, index, row));
        }
    }

    lines.push(String::new());
    lines.push("Captures:".to_string());
    let captures_start_line = lines.len();
    if edit.captures.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        for (index, row) in edit.captures.iter().enumerate() {
            lines.push(capture_row_line(edit.focus, index, row));
        }
    }

    // A failed disk write (`EditState::save_error`'s own doc comment covers
    // when this is set) is shown the same `⚠ heading\nerror` way a failed run
    // or a failed collection load already are — see `format_error` — rather
    // than a bespoke error format invented just for this. Reserved only when
    // there is one to show: unlike `method_error`'s always-present blank row
    // (edited on every keystroke), a save attempt is a discrete event, not a
    // per-character validation state, so there is no "currently passing"
    // moment where a blank placeholder row would need to keep the layout
    // from jumping.
    if let Some(error) = &edit.save_error {
        lines.push(String::new());
        lines.push(format_error("Save failed", error));
    }

    lines.push(String::new());
    lines.push(
        "Tab/Shift+Tab move focus  Ctrl+N add header  Ctrl+D delete header  \
         Ctrl+A add assertion  Ctrl+X delete assertion  Ctrl+P add capture  \
         Ctrl+K delete capture  Ctrl+S save  Esc cancel  \
         (Body: Enter for newline, ↑/↓ move lines; Auth/Assertions/Captures: ←/→ toggle option)"
            .to_string(),
    );

    // The keybinding-reminder footer (the last line pushed above) is muted
    // the same way every other pane's own footer/hint text is — patched in
    // after `theme::colorize` since it is plain text with none of that
    // function's own markers, not a status this pane is reporting.
    let mut styled = theme::colorize(&lines.join("\n"));
    if let Some(footer) = styled.lines.last_mut() {
        footer.style = theme::muted();
    }
    frame.render_widget(Paragraph::new(styled).wrap(Wrap { trim: false }), area);

    // Column offset: `▶ Name:   ` / `▶ Method: ` / `▶ URL:    ` are the same
    // width (10 cells) by construction — `"Name:   "`, `"Method: "` and
    // `"URL:    "` are all 8 characters — so one constant serves all three
    // fields' cursor placement.
    const FIELD_PREFIX_WIDTH: u16 = 2 + 8;
    let (row, column) = match edit.focus {
        EditField::Name => (0, FIELD_PREFIX_WIDTH + edit.name.cursor_chars() as u16),
        EditField::Method => (2, FIELD_PREFIX_WIDTH + edit.method.cursor_chars() as u16),
        EditField::Url => (5, FIELD_PREFIX_WIDTH + edit.url.cursor_chars() as u16),
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
        EditField::AssertionPath(index) => {
            let prefix = assertion_path_prefix(index);
            let cursor = edit.assertions[index].path.cursor_chars();
            (
                assertions_start_line + index,
                prefix.chars().count() as u16 + cursor as u16,
            )
        }
        EditField::AssertionOperator(index) => {
            let prefix = assertion_operator_prefix(index, edit.assertions[index].path.value());
            (assertions_start_line + index, prefix.chars().count() as u16)
        }
        EditField::AssertionValue(index) => {
            let row = &edit.assertions[index];
            let prefix = assertion_value_prefix(index, row.path.value(), row.operator);
            let cursor = row.value.cursor_chars();
            (
                assertions_start_line + index,
                prefix.chars().count() as u16 + cursor as u16,
            )
        }
        EditField::AssertionNegate(index) => {
            let row = &edit.assertions[index];
            let prefix = assertion_negate_prefix(
                index,
                row.path.value(),
                row.operator,
                row.value.value(),
                row.value_error.as_deref(),
            );
            (assertions_start_line + index, prefix.chars().count() as u16)
        }
        EditField::CaptureName(index) => {
            let prefix = capture_name_prefix(index);
            let cursor = edit.captures[index].name.cursor_chars();
            (
                captures_start_line + index,
                prefix.chars().count() as u16 + cursor as u16,
            )
        }
        EditField::CaptureKind(index) => {
            let prefix = capture_kind_prefix(index, edit.captures[index].name.value());
            (captures_start_line + index, prefix.chars().count() as u16)
        }
        EditField::CaptureValue(index) => {
            let row = &edit.captures[index];
            let prefix = capture_value_prefix(index, row.name.value(), row.kind);
            let cursor = row.value.cursor_chars();
            (
                captures_start_line + index,
                prefix.chars().count() as u16 + cursor as u16,
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
fn header_row_line(focus: EditField, index: usize, row: &HeaderRow) -> String {
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
        AuthField::OAuthAuthorizationUrl => "Authorization URL: ",
        AuthField::OAuthRedirectUri => "Redirect URI: ",
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
        (
            AuthEdit::OAuth {
                authorization_code, ..
            },
            AuthField::OAuthAuthorizationUrl,
        ) => authorization_code.authorization_url.value().to_string(),
        (
            AuthEdit::OAuth {
                authorization_code, ..
            },
            AuthField::OAuthRedirectUri,
        ) => authorization_code.redirect_uri.value().to_string(),
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
        (
            AuthEdit::OAuth {
                authorization_code, ..
            },
            AuthField::OAuthAuthorizationUrl,
        ) => authorization_code.authorization_url.cursor_chars(),
        (
            AuthEdit::OAuth {
                authorization_code, ..
            },
            AuthField::OAuthRedirectUri,
        ) => authorization_code.redirect_uri.cursor_chars(),
        _ => 0,
    }
}

fn api_key_location_str(location: ApiKeyLocation) -> &'static str {
    match location {
        ApiKeyLocation::Header => "header",
        ApiKeyLocation::Query => "query",
    }
}

/// One line under an `authorization_code` OAuth block's fields, reporting
/// `login`'s current state — idle (with the reminder that Ctrl+L starts
/// one), waiting (with a cycling spinner glyph, exactly like a running
/// request's own status line), or the outcome of the most recent attempt.
/// `format_error`'s own two-line "heading, then reason" shape is deliberately
/// not reused here: unlike a failed save, this is a single always-present
/// status line whose text itself changes with the state, not an error
/// appearing under an otherwise-static field.
fn oauth_login_status_line(login: &OAuthLoginState, spinner_tick: usize) -> String {
    match login {
        OAuthLoginState::Idle => "  Login: not started (Ctrl+L to log in)".to_string(),
        OAuthLoginState::WaitingForBrowser => {
            let frame = SPINNER_FRAMES[spinner_tick % SPINNER_FRAMES.len()];
            format!("  Login: {frame} waiting for the browser login to complete...")
        }
        OAuthLoginState::Succeeded => {
            "  Login: ✓ succeeded — the acquired token is ready for this session".to_string()
        }
        OAuthLoginState::Failed(reason) => format!("  Login: ✗ failed — {reason}"),
    }
}

fn oauth_grant_type_str(grant_type: OAuthGrantType) -> &'static str {
    match grant_type {
        OAuthGrantType::ClientCredentials => "client_credentials",
        OAuthGrantType::Password => "password",
        OAuthGrantType::AuthorizationCode => "authorization_code",
    }
}

/// One assertion row's display line: independent `▶` markers for its four
/// sub-fields (path, operator, value, negate), since exactly one can be
/// focused at a time — mirrors `header_row_line`'s role for header rows,
/// just with two more sub-fields. The operator and negate flag show their
/// real fixed-enum value plus a `(←/→)` toggle hint, the same convention
/// `auth_field_display` uses for `ApiKeyLocation`/`OAuthGrantType`. A value
/// parse error (see `AssertionRow::value_error`) is shown inline right after
/// the value, on the same line — unlike `method_error`/`body_error`, this
/// does not reserve a blank row when absent, since a list of rows already
/// grows and shrinks as they're added/removed, and reserving a whole extra
/// line under every single row "just in case" would waste far more space
/// than the one field it protects.
///
/// **The literal gaps here are load-bearing.** Each sub-field's prefix
/// function below (`assertion_operator_prefix`, etc.) reconstructs this same
/// line up to that field, character for character, to compute the real
/// terminal cursor's column — so a change to the spacing here must be
/// mirrored there, the same coupling `header_value_prefix` already has with
/// `header_row_line`.
fn assertion_row_line(focus: EditField, index: usize, row: &AssertionRow) -> String {
    let path_marker = if focus == EditField::AssertionPath(index) {
        "▶"
    } else {
        " "
    };
    let operator_marker = if focus == EditField::AssertionOperator(index) {
        "▶"
    } else {
        " "
    };
    let value_marker = if focus == EditField::AssertionValue(index) {
        "▶"
    } else {
        " "
    };
    let negate_marker = if focus == EditField::AssertionNegate(index) {
        "▶"
    } else {
        " "
    };
    let error = row
        .value_error
        .as_deref()
        .map(|message| format!("  ⚠ {message}"))
        .unwrap_or_default();
    format!(
        "{path_marker}[{index}] Path: {}   {operator_marker}Op: {} (←/→)   \
         {value_marker}Value: {}{error}   {negate_marker}Negate: {} (←/→)",
        row.path.value(),
        row.operator.label(),
        row.value.value(),
        if row.negate { "yes" } else { "no" },
    )
}

/// The prefix of an assertion row's line up to (not including) the path
/// field's own text — see [`assertion_row_line`]'s own doc comment on why
/// this must stay in lockstep with it. The marker character itself is not
/// part of what varies the width — `▶` and `" "` are both exactly one
/// `char` wide — so a plain space stands in for whichever one is actually
/// showing, the same convention `header_key_prefix` uses.
fn assertion_path_prefix(index: usize) -> String {
    format!(" [{index}] Path: ")
}

/// Like [`assertion_path_prefix`], but up to (not including) the operator
/// label's own text.
fn assertion_operator_prefix(index: usize, path_value: &str) -> String {
    format!("{}{path_value}    Op: ", assertion_path_prefix(index))
}

/// Like [`assertion_operator_prefix`], but up to (not including) the value
/// field's own text.
fn assertion_value_prefix(index: usize, path_value: &str, operator: JsonOperator) -> String {
    format!(
        "{}{} (←/→)    Value: ",
        assertion_operator_prefix(index, path_value),
        operator.label()
    )
}

/// Like [`assertion_value_prefix`], but up to (not including) the negate
/// flag's own text. Needs `value_error` since a shown error shifts the
/// negate field's real column, the same reason `header_value_prefix` needs
/// the key's value.
fn assertion_negate_prefix(
    index: usize,
    path_value: &str,
    operator: JsonOperator,
    value_value: &str,
    value_error: Option<&str>,
) -> String {
    let error = value_error
        .map(|message| format!("  ⚠ {message}"))
        .unwrap_or_default();
    format!(
        "{}{value_value}{error}    Negate: ",
        assertion_value_prefix(index, path_value, operator)
    )
}

/// One capture row's display line: independent `▶` markers for its three
/// sub-fields (name, kind, value), since exactly one can be focused at a
/// time — mirrors `assertion_row_line`'s role for assertion rows. `kind`
/// shows its real fixed-enum value plus a `(←/→)` toggle hint, the same
/// convention `assertion_row_line` uses for `operator`/`negate`. `value` is
/// shown even for `CaptureKind::Status`, whose real save always ignores it
/// (see `CaptureRow::to_capture_source`) — kept visible rather than hidden
/// so the row layout, and so every prefix function below it, never has to
/// change shape depending on which kind is selected.
///
/// **The literal gaps here are load-bearing** — see `assertion_row_line`'s
/// own doc comment for why: this line's prefix functions reconstruct it
/// character for character to place the real terminal cursor.
fn capture_row_line(focus: EditField, index: usize, row: &CaptureRow) -> String {
    let name_marker = if focus == EditField::CaptureName(index) {
        "▶"
    } else {
        " "
    };
    let kind_marker = if focus == EditField::CaptureKind(index) {
        "▶"
    } else {
        " "
    };
    let value_marker = if focus == EditField::CaptureValue(index) {
        "▶"
    } else {
        " "
    };
    format!(
        "{name_marker}[{index}] Name: {}   {kind_marker}Source: {} (←/→)   {value_marker}Value: {}",
        row.name.value(),
        row.kind.label(),
        row.value.value(),
    )
}

/// The prefix of a capture row's line up to (not including) the name field's
/// own text — see [`capture_row_line`]'s own doc comment on why this must
/// stay in lockstep with it.
fn capture_name_prefix(index: usize) -> String {
    format!(" [{index}] Name: ")
}

/// Like [`capture_name_prefix`], but up to (not including) the source kind
/// label's own text.
fn capture_kind_prefix(index: usize, name_value: &str) -> String {
    format!("{}{name_value}    Source: ", capture_name_prefix(index))
}

/// Like [`capture_kind_prefix`], but up to (not including) the value field's
/// own text.
fn capture_value_prefix(index: usize, name_value: &str, kind: CaptureKind) -> String {
    format!(
        "{}{} (←/→)    Value: ",
        capture_kind_prefix(index, name_value),
        kind.label()
    )
}

/// What `Request::resolve_auth` (via `Environment::apply` first, the same
/// two-step pipeline `preview::substitute_and_resolve_auth` runs for the
/// read-only, not-editing preview) would actually send for this in-progress
/// edit —
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

    // Reuses `preview::substitute_and_resolve_auth` for the two sendra-core
    // calls themselves, but — unlike `resolve_browsing_preview` — diffs the
    // result against `preview` (this edit's own pre-substitution candidate),
    // not the substituted request the helper also returns: a request or
    // environment whose header/query *names* contain `{{var}}` templates
    // can have those names change during substitution, so diffing against
    // the wrong "before" could misclassify a header/query entry that only
    // looks new because its name was just substituted. `preview` is what
    // this pane's `Auth:` section itself shows, so it is the correct
    // "before" for what the user just typed.
    match preview::substitute_and_resolve_auth(&preview, environment) {
        Ok((_substituted, resolved)) => {
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

#[cfg(test)]
mod tests {
    use sendra_core::Document;

    use crate::app::state::{AppState, AuthField, EditField, Message};
    use crate::app::test_support::*;
    use crate::app::update::update;
    use crate::app::view::view;

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

    // --- Assertion editing --------------------------------------------------

    const REQUEST_WITH_JSON_ASSERTION: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
    assertions:
      json:
        $.status: ok
";

    const REQUEST_WITH_NO_ASSERTIONS: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
";

    #[test]
    fn edit_pane_shows_no_assertions_placeholder_for_a_request_with_none() {
        let mut state = loaded_state(REQUEST_WITH_NO_ASSERTIONS);
        update(&mut state, Message::EnterEditMode);

        let screen = render_screen(&state);

        assert!(screen.contains("Assertions (json path):"), "{screen}");
        assert!(screen.contains("(none)"), "{screen}");
    }

    #[test]
    fn edit_pane_lists_an_existing_json_assertion_row() {
        let mut state = loaded_state(REQUEST_WITH_JSON_ASSERTION);
        update(&mut state, Message::EnterEditMode);

        let screen = render_screen(&state);

        assert!(screen.contains("Path: $.status"), "{screen}");
        assert!(screen.contains("Op: equals"), "{screen}");
        assert!(screen.contains("Value: ok"), "{screen}");
        assert!(screen.contains("Negate: no"), "{screen}");
    }

    #[test]
    fn add_assertion_row_key_bind_shows_up_as_a_new_empty_row_on_screen() {
        let mut state = loaded_state(REQUEST_WITH_JSON_ASSERTION);
        update(&mut state, Message::EnterEditMode);

        update(&mut state, Message::AddAssertionRow);

        let screen = render_screen(&state);
        assert!(screen.contains("[1] Path: "), "{screen}");
    }

    #[test]
    fn delete_assertion_row_key_bind_removes_a_row_from_screen() {
        let mut state = loaded_state(REQUEST_WITH_JSON_ASSERTION);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::AssertionPath(0);

        update(&mut state, Message::DeleteAssertionRow);

        let screen = render_screen(&state);
        assert!(!screen.contains("$.status"), "{screen}");
        assert!(screen.contains("(none)"), "{screen}");
    }

    #[test]
    fn edit_pane_marks_whichever_assertion_sub_field_has_focus() {
        let mut state = loaded_state(REQUEST_WITH_JSON_ASSERTION);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::AssertionValue(0);

        let screen = render_screen(&state);
        let value_line = screen
            .lines()
            .find(|line| line.contains("Value: ok"))
            .expect("the assertion row's line must be on screen");
        assert!(
            value_line.contains('▶'),
            "the focused sub-field's line must carry a focus marker: {value_line}"
        );
    }

    #[test]
    fn tab_reaches_the_assertion_section_and_toggling_the_operator_shows_up_live() {
        let mut state = loaded_state(REQUEST_WITH_JSON_ASSERTION);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::AssertionOperator(0);

        let before = render_screen(&state);
        assert!(before.contains("Op: equals"), "{before}");

        update(&mut state, Message::EditCursorRight);
        let after = render_screen(&state);
        assert!(
            after.contains("Op: greater_than"),
            "the displayed operator must reflect the toggle:\n{after}"
        );
    }

    #[test]
    fn tab_reaches_the_assertion_section_and_toggling_negate_shows_up_live() {
        let mut state = loaded_state(REQUEST_WITH_JSON_ASSERTION);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::AssertionNegate(0);

        update(&mut state, Message::EditCursorRight);
        let after = render_screen(&state);
        assert!(
            after.contains("Negate: yes"),
            "the displayed negate flag must reflect the toggle:\n{after}"
        );
    }

    #[test]
    fn edit_pane_shows_a_value_parse_error_inline_on_the_assertion_row() {
        let mut state = loaded_state(REQUEST_WITH_JSON_ASSERTION);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::AssertionValue(0);
        backspace_n(&mut state, "ok".len());
        type_into_focused_field(&mut state, "[a, b");

        let screen = render_screen(&state);
        assert!(
            screen.contains('⚠'),
            "a malformed value must show an inline warning:\n{screen}"
        );
    }

    // --- Capture editing ------------------------------------------------------

    const REQUEST_WITH_JSON_PATH_CAPTURE: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
    capture:
      id: $.id
";

    const REQUEST_WITH_NO_CAPTURE: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
";

    #[test]
    fn edit_pane_shows_no_captures_placeholder_for_a_request_with_none() {
        let mut state = loaded_state(REQUEST_WITH_NO_CAPTURE);
        update(&mut state, Message::EnterEditMode);

        let screen = render_screen(&state);

        assert!(screen.contains("Captures:"), "{screen}");
        assert!(screen.contains("(none)"), "{screen}");
    }

    #[test]
    fn edit_pane_lists_an_existing_json_path_capture_row() {
        let mut state = loaded_state(REQUEST_WITH_JSON_PATH_CAPTURE);
        update(&mut state, Message::EnterEditMode);

        let screen = render_screen(&state);

        assert!(screen.contains("Name: id"), "{screen}");
        assert!(screen.contains("Source: json path"), "{screen}");
        assert!(screen.contains("Value: $.id"), "{screen}");
    }

    #[test]
    fn add_capture_row_key_bind_shows_up_as_a_new_empty_row_on_screen() {
        let mut state = loaded_state(REQUEST_WITH_JSON_PATH_CAPTURE);
        update(&mut state, Message::EnterEditMode);

        update(&mut state, Message::AddCaptureRow);

        let screen = render_screen(&state);
        assert!(screen.contains("[1] Name: "), "{screen}");
    }

    #[test]
    fn delete_capture_row_key_bind_removes_a_row_from_screen() {
        let mut state = loaded_state(REQUEST_WITH_JSON_PATH_CAPTURE);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::CaptureName(0);

        update(&mut state, Message::DeleteCaptureRow);

        let screen = render_screen(&state);
        assert!(!screen.contains("Name: id"), "{screen}");
        assert!(screen.contains("(none)"), "{screen}");
    }

    #[test]
    fn edit_pane_marks_whichever_capture_sub_field_has_focus() {
        let mut state = loaded_state(REQUEST_WITH_JSON_PATH_CAPTURE);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::CaptureValue(0);

        let screen = render_screen(&state);
        let value_line = screen
            .lines()
            .find(|line| line.contains("Value: $.id"))
            .expect("the capture row's line must be on screen");
        assert!(
            value_line.contains('▶'),
            "the focused sub-field's line must carry a focus marker: {value_line}"
        );
    }

    #[test]
    fn tab_reaches_the_capture_section_and_toggling_the_kind_shows_up_live() {
        let mut state = loaded_state(REQUEST_WITH_JSON_PATH_CAPTURE);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::CaptureKind(0);

        let before = render_screen(&state);
        assert!(before.contains("Source: json path"), "{before}");

        update(&mut state, Message::EditCursorRight);
        let after = render_screen(&state);
        assert!(
            after.contains("Source: header"),
            "the displayed source kind must reflect the toggle:\n{after}"
        );
    }

    /// The visual half of `update`'s own
    /// `a_failed_disk_write_keeps_the_edit_dirty_and_surfaces_a_real_error_without_losing_it`:
    /// a save that fails to reach disk must show a real, readable message in
    /// the pane itself, through the same `format_error` shape a failed run or
    /// a failed collection load already use — not just set a field nothing
    /// on screen ever reads.
    #[test]
    fn edit_pane_shows_a_save_error_inline_after_a_failed_write() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = dir.path().join("collection.yaml");
        // A directory at the target path makes the final rename in
        // `Document::save_to_path` fail deterministically — the same
        // portable failure mode sendra-core's own tests use.
        std::fs::create_dir(&path).unwrap();

        let mut state = AppState::default();
        let document = Document::from_yaml_str("method: GET\nurl: https://example.com\n").unwrap();
        update(
            &mut state,
            Message::CollectionLoaded {
                base_dir: dir.path().to_path_buf(),
                path,
                result: Box::new(Ok(document)),
            },
        );
        update(&mut state, Message::EnterEditMode);

        update(&mut state, Message::SaveEdit);

        let screen = render_screen(&state);
        assert!(
            screen.contains("Save failed"),
            "a failed save must show a readable error in the pane:\n{screen}"
        );
        assert!(
            state.edit_mode.is_some(),
            "the pane must still be in edit mode after a failed save"
        );
    }

    /// The visual half of `update`'s own
    /// `adding_a_request_selects_it_and_opens_it_in_edit_mode_immediately`:
    /// pressing `n` must show the new, dirty-marked request in the
    /// collection browser and land straight in its edit pane, not a
    /// separate "create request" screen.
    #[test]
    fn pressing_n_shows_the_new_request_selected_dirty_and_already_in_edit_mode() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);

        update(&mut state, Message::AddRequest);

        let screen = render_screen(&state);
        assert!(
            screen.contains("New request"),
            "the new request must show up in the collection browser:\n{screen}"
        );
        assert!(
            screen.contains('*'),
            "the new, unsaved request must carry the same dirty marker any other unsaved \
             edit does:\n{screen}"
        );
        assert!(
            screen.contains("▶ Name:   New request"),
            "AddRequest must land straight in the edit pane, focused on Name:\n{screen}"
        );
    }

    /// The visual half of `update`'s own naming tests: the `Name:` field
    /// shows the real name, carries the focus marker by default, and a
    /// typed rename shows up live.
    #[test]
    fn edit_pane_shows_the_name_field_with_its_focus_marker_and_live_edits() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::EnterEditMode);

        let before = render_screen(&state);
        assert!(before.contains("▶ Name:   One"), "{before}");

        type_into_focused_field(&mut state, " (renamed)");
        let after = render_screen(&state);
        assert!(after.contains("▶ Name:   One (renamed)"), "{after}");
    }

    /// The visual half of `update`'s own
    /// `save_edit_refuses_to_leave_a_collection_request_unnamed`: a save
    /// blocked for having no name must show a real, readable error the same
    /// way a disk-level save failure already does.
    #[test]
    fn edit_pane_shows_a_save_error_inline_when_a_collection_request_is_left_unnamed() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::EnterEditMode);
        backspace_n(&mut state, "One".len());

        update(&mut state, Message::SaveEdit);

        let screen = render_screen(&state);
        assert!(
            screen.contains("Save failed"),
            "an unnamed request inside a collection must be refused with a visible \
             error:\n{screen}"
        );
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
            // 13, not 10: the header bar above the panes and the detail
            // pane's own border (top+bottom) now claim 3 rows that used to
            // be part of the edit form's own content area.
            let backend = TestBackend::new(60, 13);
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
            initial_screen.contains("▶ Name"),
            "focus starts on the name field:\n{initial_screen}"
        );

        update(&mut state, Message::EditFocusNext); // -> Method

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

        // 20, not 17: the edit pane gained a `Name:` field and its own
        // spacer line above `Method:`, so every fixed section below it sits
        // two rows further down than before; the header bar and the detail
        // pane's own top/bottom border claim 3 more rows on top of that.
        let backend = TestBackend::new(80, 20);
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
        // The marker sits immediately before `[0] Key:` — checked as a
        // substring, not by trimming the line's start, since the detail
        // pane's own left border is now the first character of every row.
        assert!(
            key_line.contains("▶[0] Key: Accept"),
            "the key side must carry the focus marker:\n{key_line}"
        );

        state.edit_mode.as_mut().unwrap().focus = EditField::HeaderValue(0);
        let value_focused = render_state(&state);
        let value_line = value_focused
            .lines()
            .find(|line| line.contains("Key: Accept"))
            .expect("the Accept row must be on screen");
        assert!(
            !value_line.contains("▶[0] Key:"),
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

        // Checked as a substring, not by trimming the line's start, since
        // the detail pane's own left border is now the first character of
        // every row.
        assert!(
            screen.lines().any(|line| line.contains("▶ Body (raw)")),
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
}
