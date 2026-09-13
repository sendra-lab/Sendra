//! The view half of sendra-tui's Elm-style architecture: `view()` itself,
//! the tab bar, the status bar, and the `modal_frame`/`centered_rect`/`inset`
//! helpers every overlay in [`environment`]/[`response`]/[`modals`] builds
//! on. Nothing here ever mutates `AppState` — see `super::update` for the
//! one function that does. Split into submodules along the same lines the
//! detail pane naturally divides: [`browser`] for the request list and the
//! read-only preview pane, [`edit_form`] for the request edit form,
//! [`environment`] for the environment overlay/edit, [`response`] for a
//! run's response panel (live or from history), and [`modals`] for the
//! shared confirmation prompt and the open-collection prompt.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use sendra_core::{Document, Environment};

use super::preview;
use super::state::{
    AppState, CollectionSession, ConfirmPrompt, LoadState, NamedEnvironment, RunState,
};
use super::theme;

mod browser;
mod edit_form;
mod environment;
mod modals;
mod response;

use browser::{render_detail_pane, render_request_list};
use environment::render_environment_overlay;
use modals::{render_confirm_prompt, render_open_collection_prompt};
use response::render_history_overlay;

pub fn view(state: &AppState, frame: &mut Frame) {
    // The tab bar only ever takes a row away from the rest of the screen
    // once there is something to switch between — a single open collection
    // renders exactly as it always has, byte-for-byte the same layout every
    // existing single-collection test already checks, rather than every
    // screen growing a permanent one-tab bar nobody needed before this
    // issue.
    let show_tabs = state.collections.len() > 1;
    let constraints = if show_tabs {
        vec![
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ]
    } else {
        vec![Constraint::Min(0), Constraint::Length(1)]
    };
    // The bottom row is reserved in every `LoadState`, not only `Loaded` —
    // an error state must not dead-end the app, and the help bar showing
    // which keys still work (at minimum quit, per `status_help_text`) is
    // exactly what makes that visible rather than assumed. See that
    // function's own doc comment for how it reads `load_state` to keep this
    // honest instead of always claiming nav/run apply.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.area());
    let (main_row, status_row) = if show_tabs {
        render_tab_bar(frame, rows[0], state);
        (rows[1], rows[2])
    } else {
        (rows[0], rows[1])
    };

    match &state.load_state {
        LoadState::Loading => render_message(frame, main_row, "Loading collection..."),
        LoadState::NoPathProvided => {
            render_message(frame, main_row, "No collection path provided.");
        }
        LoadState::Failed(error) => {
            render_error(frame, main_row, "Failed to load collection", error);
        }
        LoadState::Loaded {
            document,
            selected,
            base_dir,
            ..
        } => {
            let panes = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                .split(main_row);

            render_request_list(frame, panes[0], document, *selected, &state.dirty_requests);
            render_detail_pane(frame, panes[1], document, *selected, base_dir, state);
        }
    }

    render_status_bar(frame, status_row, state);

    if let Some(cursor) = state.environment_overlay {
        render_environment_overlay(frame, state, cursor);
    }

    if let Some(overlay) = &state.history_overlay {
        render_history_overlay(frame, state, overlay);
    }

    if let Some(confirm) = &state.delete_confirm {
        render_confirm_prompt(frame, "Delete request", &confirm.prompt);
    }

    if let Some(pending) = state
        .environment_edit
        .as_ref()
        .and_then(|env_edit| env_edit.pending_delete.as_ref())
    {
        render_confirm_prompt(frame, "Delete variable", &pending.prompt);
    }

    if let Some(prompt) = &state.open_collection_prompt {
        render_open_collection_prompt(frame, prompt);
    }

    if let Some(confirm) = &state.close_confirm {
        render_confirm_prompt(frame, "Close collection", &confirm.prompt);
    }

    // Checked — and so drawn — last: `quit_confirm` can appear over *any*
    // other state (see its own doc comment), so it has to sit visually on
    // top of every other overlay this function might have just drawn, not
    // only the ordinary panes underneath.
    if let Some(prompt) = &state.quit_confirm {
        render_confirm_prompt(frame, "Quit sendra-tui", prompt);
    }
}

/// One label per open collection, `id` (its 1-based position, not
/// `CollectionSession::id` — a stable id is what correctness needs
/// internally, but a person switching tabs thinks in terms of "the second
/// one", not an opaque counter) markers separated by `│`, the active one
/// reversed the same way the request list's own selection highlight already
/// is. Shown only once a second collection is open — see `view`'s own
/// comment on why a single open collection draws with no tab bar at all.
fn render_tab_bar(frame: &mut Frame, area: Rect, state: &AppState) {
    let mut spans: Vec<ratatui::text::Span> = Vec::new();
    for (index, session) in state.collections.iter().enumerate() {
        if index > 0 {
            spans.push(ratatui::text::Span::raw(" │ "));
        }
        let label = format!(" {}:{} ", index + 1, collection_label(session));
        let style = if index == state.active_collection {
            theme::selection()
        } else {
            Style::default()
        };
        spans.push(ratatui::text::Span::styled(label, style));
    }
    frame.render_widget(Paragraph::new(ratatui::text::Line::from(spans)), area);
}

/// What a tab shows for whichever collection it holds: the collection's own
/// name if it has one, the request's own label for a `Document::Single`
/// with none, the file's stem when a `Document::Collection` was never given
/// a `name:`, or a short, honest placeholder for a tab that never finished
/// loading anything.
pub(super) fn collection_label(session: &CollectionSession) -> String {
    match &session.load_state {
        LoadState::Loading => "loading…".to_string(),
        LoadState::NoPathProvided => "(none)".to_string(),
        LoadState::Failed(_) => "(failed)".to_string(),
        LoadState::Loaded { document, path, .. } => match &**document {
            Document::Single(request) => request
                .name
                .clone()
                .unwrap_or_else(|| request.label().to_string()),
            Document::Collection(collection) => collection.name.clone().unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("collection")
                    .to_string()
            }),
        },
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
pub(crate) fn format_error(heading: &str, error: &impl std::fmt::Display) -> String {
    format!("⚠ {heading}\n{error}")
}

/// [`format_error`], rendered into `area` as a wrapped `Paragraph` — the
/// standalone-error half of the pair; [`format_error`] alone is what the
/// call sites that embed an error inside other text (the request preview,
/// the response panel) use instead, since those need the string, not a
/// widget of their own.
fn render_error(frame: &mut Frame, area: Rect, heading: &str, error: &impl std::fmt::Display) {
    frame.render_widget(
        Paragraph::new(theme::colorize(&format_error(heading, error))).wrap(Wrap { trim: false }),
        area,
    );
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
/// to a `Paragraph`, styled through [`status_bar_line`].
///
/// [`status_help_text`] checks `delete_confirm` first, ahead of every other
/// context documented there — the delete confirmation prompt is modal and
/// drawn on top of everything else, the same reason the environment overlay
/// is checked ahead of edit mode.
fn render_status_bar(frame: &mut Frame, area: Rect, state: &AppState) {
    frame.render_widget(
        Paragraph::new(status_bar_line(&status_help_text(state))),
        area,
    );
}

/// Colors [`status_help_text`]'s own output for `render_status_bar` without
/// `status_help_text` itself (or any of its ~30 existing tests, which check
/// its literal string) knowing anything about `Style`: the bar's own
/// `"  |  "` separator — already how every status-bearing line in
/// `status_help_text` divides "what happened" from "what you can press
/// next" — is what this splits on. The left half is colored by what it
/// says (`"Failed"` → [`theme::fail`], `"Done"` → [`theme::success`], a
/// spinner frame → [`theme::in_progress`], anything else — a confirmation
/// question, an "Editing" label — → [`theme::emphasis`], since those are
/// attention-worthy without being a pass/fail verdict); the right half
/// (the keybindings themselves) is always [`theme::muted`]. A line with no
/// separator at all (the plain browsing/overlay keymaps, which carry no
/// status) is muted in full.
fn status_bar_line(text: &str) -> Line<'static> {
    let Some((left, right)) = text.split_once("  |  ") else {
        return Line::styled(text.to_string(), theme::muted());
    };
    let left_style = if left.contains("Failed") || left.contains("failed") {
        theme::fail()
    } else if left.contains("Done") {
        theme::success()
    } else if SPINNER_FRAMES.iter().any(|frame| left.starts_with(*frame)) {
        theme::in_progress()
    } else {
        theme::emphasis()
    };
    Line::from(vec![
        Span::styled(left.to_string(), left_style),
        Span::raw("  |  "),
        Span::styled(right.to_string(), theme::muted()),
    ])
}

/// The bottom bar's full text — status where there is one, then the
/// keybindings currently live — chosen from exactly the state `update()`
/// itself branches on, so this can never say a key does something `update()`
/// would actually refuse, or omit one it would accept. Eight contexts, in the
/// same priority order `update()`'s own InFlight/edit-mode guards and
/// `view()`'s own overlay-vs-response-panel-vs-preview dispatch already
/// imply:
///
/// 0. **An environment's variables are being edited**
///    (`environment_edit.is_some()`) — checked ahead of the plain overlay
///    context below since this session only ever opens from inside the
///    overlay and takes over the whole thing (see
///    `render_environment_overlay`'s own comment). Within it, a pending
///    row deletion (`EnvironmentEditState::pending_delete`) is its own,
///    innermost context: only `y`/`n`/Enter/Esc/quit apply while it is
///    open, the exact same "nested modal" shape `AppState::delete_confirm`
///    already has one level up.
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
/// 2. **The run-history browser is open** (`history_overlay.is_some()`) — a
///    sibling of the environment overlay one level up: only its own keys
///    apply (the list's while merely browsing, `PgUp`/`PgDn`/`Home`/`End`/`c`
///    once `HistoryOverlay::viewing` picks an entry), and it is likewise
///    mutually exclusive with edit mode and the environment overlay (see
///    `update()`'s own guards).
/// 3. **The selected request is being edited** (`edit_mode.is_some()`) —
///    `update()`'s own edit-mode guard refuses every navigation/overlay/run
///    message while this holds, the same way the InFlight guard does for a
///    run, so only `Ctrl+S`/`Esc`/quit are genuinely live.
/// 4. **No collection is loaded** (`load_state` is `Loading`,
///    `NoPathProvided` or `Failed` — see `view()`'s own match on it): there
///    is no request list and nothing to run, so nav/run are not offered;
///    the environment overlay and quit are the only two keys that do
///    anything, and both keep working, which is the whole point of this
///    context existing — a failed collection load must not read as a dead
///    end.
/// 5. **A run is in flight** (`RunState::InFlight`) — `update()`'s own guard
///    at the top of the function refuses every navigation/overlay/run
///    message while this holds, so `q` (never blocked — see that guard's own
///    comment) is genuinely the only key left to advertise.
/// 6. **A run has completed** (`RunState::Completed`) — the response panel
///    is what `render_detail_pane` is showing (see its own doc comment), so
///    this is where the scroll keys and, when there is something to reveal,
///    the capture-reveal key belong; nav/run/env/quit are back too, since
///    the InFlight guard no longer applies. `h` for the history browser is
///    offered too, once there is any history yet to browse.
/// 7. **Otherwise** (`RunState::Idle`, a collection is loaded) — the
///    ordinary collection browser, showing the request preview.
///
/// No key is added here that `update`/`main::next_message` do not already
/// bind — this only narrates keys already wired elsewhere.
///
/// Visible to `super::update`'s own tests: a couple of reducer-focused tests
/// (invalid-method save-refusal, dirty-marker bookkeeping) confirm their
/// effect is visible in this bar too, rather than duplicating a second
/// "what does the help bar say" check inside `view`'s own test module.
/// The status-bar line for any [`ConfirmPrompt`] — one format string shared
/// by every destructive-action confirmation's `status_help_text` branch
/// (request-delete, env-var-delete, close-tab, quit), the exact status-bar
/// counterpart to [`render_confirm_prompt`] sharing the same content one
/// level up in the UI.
fn confirm_status_line(prompt: &ConfirmPrompt) -> String {
    let failed = if prompt.error.is_some() {
        "  (failed — see message)"
    } else {
        ""
    };
    format!(
        "{}{failed}  |  y/enter confirm  n/esc cancel  q quit",
        prompt.message
    )
}

pub(super) fn status_help_text(state: &AppState) -> String {
    // Checked before every other context: `quit_confirm` can appear over
    // *any* of them (see its own doc comment on why quitting is guarded
    // ahead of everything else), so its own keys are the only ones actually
    // live no matter what else this bar would otherwise say.
    if let Some(prompt) = &state.quit_confirm {
        return confirm_status_line(prompt);
    }

    if let Some(confirm) = &state.close_confirm {
        return confirm_status_line(&confirm.prompt);
    }

    if let Some(prompt) = &state.open_collection_prompt {
        let failed = if prompt.error.is_some() {
            "  (open failed — see message)"
        } else {
            ""
        };
        return format!("Open collection{failed}  |  enter confirm  esc cancel  q quit");
    }

    if let Some(confirm) = &state.delete_confirm {
        return confirm_status_line(&confirm.prompt);
    }

    if let Some(env_edit) = &state.environment_edit {
        if let Some(pending) = &env_edit.pending_delete {
            return confirm_status_line(&pending.prompt);
        }
        let failed = if env_edit.save_error.is_some() {
            "  (save failed — see message)"
        } else {
            ""
        };
        return format!(
            "Editing variables{failed}  |  tab/shift+tab switch field  ctrl+n add  \
             ctrl+d delete  ctrl+s save  esc cancel  q quit"
        );
    }

    if state.environment_overlay.is_some() {
        return "↑/↓ nav  enter confirm  i edit variables  esc cancel  q quit".to_string();
    }

    if let Some(overlay) = &state.history_overlay {
        return if overlay.viewing.is_some() {
            let reveal = if state
                .selected_history()
                .get(overlay.viewing.unwrap_or(0))
                .is_some_and(|entry| !entry.outcome.capture.is_empty())
            {
                "  c reveal/hide captures"
            } else {
                ""
            };
            format!("PgUp/PgDn/Home/End scroll{reveal}  esc back  q quit")
        } else {
            "↑/↓ nav  enter view  space expand  esc close  q quit".to_string()
        };
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
        return format!("e env  o open{}  q quit", tab_hint(state));
    }

    match &state.run_state {
        RunState::Idle => {
            format!(
                "↑/↓ nav  enter/r run  i edit  n new request  d delete  e env  o open{}{}  q quit",
                tab_hint(state),
                idle_reveal_hint(state)
            )
        }
        RunState::InFlight => {
            let frame_char = SPINNER_FRAMES[state.spinner_tick % SPINNER_FRAMES.len()];
            format!("{frame_char} Running request...  |  q quit")
        }
        RunState::Completed => {
            // `state.current_run()` is `Some` here by construction — see
            // that method's own doc comment: `run_state` only ever becomes
            // `Completed` in the same step `Message::RunCompleted` records
            // the history entry it names.
            let outcome = state
                .current_run()
                .expect("RunState::Completed implies a history entry exists");
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
            let history_hint = if state.selected_history().is_empty() {
                ""
            } else {
                "  h history"
            };
            format!(
                "{status}  |  ↑/↓ nav  enter/r run again  PgUp/PgDn/Home/End scroll{reveal}  i edit  n new request  d delete  e env  o open{history_hint}{}  q quit",
                tab_hint(state)
            )
        }
    }
}

/// `  [/] tabs  ctrl+w close` — advertised only once switching or closing a
/// tab would actually do something, i.e. once a second collection is open;
/// the same "don't advertise a key that would be a no-op right now" rule
/// `idle_reveal_hint` already follows for its own auth-reveal hint.
fn tab_hint(state: &AppState) -> &'static str {
    if state.collections.len() > 1 {
        "  [/] tabs  ctrl+w close"
    } else {
        ""
    }
}

/// The `c reveal/hide auth` hint `status_help_text`'s `RunState::Idle` arm
/// appends — only when the currently selected request's preview actually
/// has something auth-derived to mask (see
/// `preview::added_by_auth_names`), the same "don't advertise a key that
/// would be a no-op right now" rule the `RunState::Completed` arm already
/// follows for its own `c reveal/hide captures` hint. Empty for every other
/// reason there might be nothing to check yet: nothing selected, an empty
/// collection, a resolution failure, or a request with no `auth:` (and no
/// environment-level default) at all.
///
/// A second, independent call into `preview::substitute_and_resolve_auth`
/// on top of the one `render_detail_pane` already makes for the same
/// request this frame — `status_help_text` (this function's only caller) is
/// rendered from a separate call in `view()`, with no already-computed
/// preview in scope to read instead. Threading one through would mean
/// `status_help_text`/`render_status_bar` taking on a precomputed
/// dependency neither needs for anything else, purely to save one cheap,
/// already-cached-nowhere resolution call per frame — not worth losing
/// their current shape as plain functions of `&AppState` alone, which is
/// what keeps them directly testable against ~15 hand-built states with no
/// resolution machinery involved.
fn idle_reveal_hint(state: &AppState) -> &'static str {
    let LoadState::Loaded {
        document, selected, ..
    } = &state.load_state
    else {
        return "";
    };
    let Some(request) = document.requests().get(*selected) else {
        return "";
    };
    let environment = active_environment(state)
        .map_or_else(Environment::default, |named| named.environment.clone());
    let Ok((substituted, auth_resolved)) =
        preview::substitute_and_resolve_auth(request, &environment)
    else {
        return "";
    };
    let (headers, query) = preview::added_by_auth_names(&substituted, &auth_resolved);
    if headers.is_empty() && query.is_empty() {
        ""
    } else {
        "  c reveal/hide auth"
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

/// Draws a bordered, titled popup centered at `percent_x`/`percent_y` of the
/// frame — `Clear` first, so nothing behind it shows through, then a
/// bordered `Block` with `title` — and returns the inset `Rect` inside the
/// border where the caller's own content goes. The shared shell behind
/// every full-screen modal this crate draws (the environment overlay and
/// its variable editor, the delete and close confirmations, and the
/// open-collection prompt), which differ only in their percentage, title,
/// and what they draw inside.
pub(crate) fn modal_frame(
    frame: &mut Frame,
    percent_x: u16,
    percent_y: u16,
    title: impl Into<String>,
) -> Rect {
    let area = centered_rect(percent_x, percent_y, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(theme::muted())
            .title(title.into()),
        area,
    );
    inset(area)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use sendra_core::{AssertionReport, CaptureReport, Document, SendraError};

    use crate::app::state::{AppState, Message, RunState};
    use crate::app::test_support::*;
    use crate::app::update::update;
    use crate::app::view::response::format_run_result;
    use crate::run_request::RunOutcome;

    use super::*;

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
        let collection_id = state.active().id;
        update(
            &mut state,
            Message::RunCompleted {
                collection_id,
                outcome: sample_outcome(200),
            },
        );

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
        let collection_id = state.active().id;
        update(
            &mut state,
            Message::RunCompleted {
                collection_id,
                outcome: sample_outcome(200),
            },
        );
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
        let collection_id = state.active().id;
        update(
            &mut state,
            Message::RunCompleted {
                collection_id,
                outcome: RunOutcome {
                    result: Ok(response),
                    assertions: AssertionReport::default(),
                    capture,
                },
            },
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
        let collection_id = completed.active().id;
        update(
            &mut completed,
            Message::RunCompleted {
                collection_id,
                outcome: sample_outcome(200),
            },
        );

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

        let collection_id = state.active().id;
        update(
            &mut state,
            Message::RunCompleted {
                collection_id,
                outcome: sample_outcome(200),
            },
        );
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
                        path: PathBuf::from("collection.yaml"),
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
                path: PathBuf::from("collection.yaml"),
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
        let collection_id = state.active().id;
        update(
            &mut state,
            Message::RunCompleted {
                collection_id,
                outcome,
            },
        );
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
            let collection_id = browsing.active().id;
            update(
                &mut browsing,
                Message::RunCompleted {
                    collection_id,
                    outcome: sample_outcome(200),
                },
            );
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
                    path: PathBuf::from("collection.yaml"),
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

    #[test]
    fn help_bar_shows_environment_edit_keys_and_the_row_delete_confirmation_keys() {
        let (mut state, _dir, _path) = loaded_state_with_saved_environment("https://example.com");
        update(&mut state, Message::OpenEnvironmentOverlay);
        update(&mut state, Message::EnterEnvironmentEdit);

        let editing_text = status_help_text(&state);
        assert!(editing_text.contains("ctrl+s save"), "{editing_text}");
        assert!(editing_text.contains("ctrl+d delete"), "{editing_text}");

        update(&mut state, Message::RequestDeleteEnvVarRow);
        let pending_text = status_help_text(&state);
        assert!(pending_text.contains("y/enter confirm"), "{pending_text}");
    }
}
