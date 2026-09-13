//! The one place every color/style choice in sendra-tui's view layer comes
//! from — a small, named palette (success/fail/warning/in-progress status
//! colors, a selection style, a muted style, and an emphasis style) plus
//! [`colorize`], the single function most of `view::*`'s freeform text
//! passes through before it reaches a `Paragraph`. No `view::*` module
//! reaches for `ratatui::style::Color` directly — `grep -rn "Color::"
//! src/app/view` should show nothing outside this file.
//!
//! Placed at the `app/` level, alongside `preview.rs`, rather than inside
//! `view/`: it is used by every `view::*` submodule but is not itself
//! rendering logic — the same "shared, cross-cutting, but not view code
//! itself" role `preview.rs` already has for resolution.
//!
//! **Respecting the terminal's own theme.** Every status color here is one
//! of ratatui's *named* ANSI colors (`Color::Green`, `Red`, `Yellow`,
//! `Cyan`), not a fixed RGB triple — a named color renders through
//! whatever the user's terminal has that slot mapped to, so it already
//! adapts to a light- or dark-background theme the same way `ls --color`
//! or `git diff` already do, instead of this crate picking a shade that
//! might be unreadable against a background it never gets to see.
//! [`selection`] and [`muted`] go further and use no color at all:
//! `Modifier::REVERSED` swaps whatever the terminal's own foreground/
//! background already are, and `Modifier::DIM` reduces the terminal's own
//! foreground intensity — both look right no matter what palette the
//! terminal is running. [`emphasis`] is `Modifier::BOLD` alone, for the
//! same reason.
//!
//! **How color reaches a screen.** Every render function in `view::*`
//! still builds its own plain-text `String` exactly as it always has —
//! `format_assertions`, `format_error`, `status_help_text`, and every other
//! formatting helper are unchanged, and every existing test that checks
//! their literal text still passes unmodified. What changed is that the
//! handful of places that hand text to a `Paragraph` now hand it to
//! [`colorize`] first, which re-parses the same handful of markers those
//! functions already, consistently, emit — `✓`/`✗`/`⚠` at the start of a
//! line, `▶` marking a focused field, and a line ending in `:` marking a
//! section heading — and colors each line accordingly. One scanner, reused
//! everywhere, is what makes "the same marker always means the same color"
//! true across the whole app, rather than each screen picking its own
//! shade of green.
//!
//! Three things stay outside [`colorize`], styled directly at their own
//! call site because only the caller has the context to know which color
//! is right, or because coloring by fixed position (not by scanning
//! rendered text back apart) is simply more direct:
//! - A request's dirty marker (`browser::render_request_list`) and an
//!   environment's active marker (`environment::render_environment_overlay`)
//!   both use a bare `*`, but mean different things — "unsaved" versus
//!   "currently active" — so painting every leading `*` the same color
//!   would conflate them.
//! - The run-history list's heading line (`response::render_history_overlay`)
//!   and the response panel's own status line (`response::render_response_panel`)
//!   are colored success/fail straight from the real `RunOutcome` already in
//!   hand, not by pattern-matching text that was never given a `✓`/`✗` of
//!   its own.
//! - The status/help bar (`view::render_status_bar`) is colored by
//!   splitting on its own `"  |  "` status/keybinding separator, since a
//!   generic per-line scan has nothing to key off within a single line.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Text};

/// A passing assertion/capture, a successful run, a variable currently in
/// scope — anything this app confirms went right.
pub(crate) fn success() -> Style {
    Style::new().fg(Color::Green)
}

/// A failing assertion/capture, a run that errored, a save that failed, an
/// invalid field — anything backed by a real `Err`, a real `SendraError`,
/// or a real validation failure. Also what every `⚠` marker in this crate
/// already means (see `view::format_error`) — there is no separate "just a
/// warning, not quite an error" state anywhere sendra-tui shows today, so
/// there is nothing this color would be misapplied to.
pub(crate) fn fail() -> Style {
    Style::new().fg(Color::Red)
}

/// A cautionary state that stops short of failure — unsaved work, history
/// entries the cap has dropped, a value about to be overwritten. Distinct
/// from [`fail`]: nothing here is wrong, only worth a second look before
/// it's gone.
pub(crate) fn warning() -> Style {
    Style::new().fg(Color::Yellow)
}

/// A run actually in flight right now — the spinner and its status line,
/// the one state that is neither a result nor an error because there isn't
/// one yet.
pub(crate) fn in_progress() -> Style {
    Style::new().fg(Color::Cyan)
}

/// The selected row in every list this crate draws (the request list, the
/// environment picker, the run-history list) and the active tab in the tab
/// bar — `Modifier::REVERSED`, not a background color, so it reads
/// correctly against whatever foreground/background the terminal is
/// already running, the same reasoning [`muted`]/[`emphasis`] follow below.
/// This was already every list's own highlight style before this palette
/// existed; the only change is that all of them now name it through this
/// one function instead of writing `Style::new().add_modifier(Modifier::REVERSED)`
/// independently.
pub(crate) fn selection() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

/// De-emphasized chrome: a modal's border, a footer/help-bar hint, a
/// keybinding reminder — real information, but never the thing on screen
/// that matters most. `Modifier::DIM` rather than a gray `Color`, so it
/// dims whatever the terminal's real foreground color already is instead
/// of picking a fixed gray that may not contrast with every theme.
pub(crate) fn muted() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

/// A focused field's `▶` marker, a section heading, or anything else that
/// should stand out without claiming a status it doesn't have —
/// `Modifier::BOLD` alone, for the same "respect the terminal's own
/// colors" reason [`selection`]/[`muted`] use modifiers instead of a
/// `Color`.
pub(crate) fn emphasis() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// Re-parses `text`'s own, already-established markers — `✓`/`✗`/`⚠` at
/// the start of a line, a focused field's `▶` (`render_edit_pane`'s request
/// form) or `»...«` (`render_environment_edit`'s variable rows) appearing
/// anywhere on the line, a section heading ending in `:` — and styles each
/// line accordingly, so every screen that builds plain text through the
/// crate's existing formatting helpers (`format_assertions`, `format_error`,
/// `format_resolved_request`, the edit panes' own line-builders, ...) gets
/// the same colors for the same markers without any of those helpers
/// needing to know about `Style` at all. See this module's own doc comment
/// for why a bare leading `*` is deliberately not one of these markers.
///
/// The focus markers are matched anywhere on the line, not just at its
/// start, since a header row's own value-side `▶` (`edit_form::header_row_line`)
/// and both halves of an environment variable row's `»name«`/`»value«`
/// sit mid-line, not at column zero — the whole row is emphasized rather
/// than only the one field, which is a reasonable reading of "this row has
/// focus" and avoids needing to split a line into per-field spans just for
/// this.
pub(crate) fn colorize(text: &str) -> Text<'static> {
    Text::from(text.lines().map(colorize_line).collect::<Vec<_>>())
}

fn colorize_line(line: &str) -> Line<'static> {
    let trimmed = line.trim_start();
    let style = if trimmed.starts_with('✓') {
        success()
    } else if trimmed.starts_with('✗') || trimmed.starts_with('⚠') {
        fail()
    } else if line.contains('▶')
        || line.contains('»')
        || (!trimmed.is_empty() && line.ends_with(':'))
    {
        emphasis()
    } else {
        Style::default()
    };
    Line::styled(line.to_string(), style)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorize_styles_pass_fail_and_error_markers() {
        let text = "  ✓ status is 200\n  ✗ body matches — mismatch\n⚠ heading\nplain text";
        let styled = colorize(text);

        assert_eq!(styled.lines[0].style, success());
        assert_eq!(styled.lines[1].style, fail());
        assert_eq!(styled.lines[2].style, fail());
        assert_eq!(styled.lines[3].style, Style::default());
    }

    #[test]
    fn colorize_styles_a_focus_marker_and_a_section_heading_the_same_way() {
        let text = "▶ Name:   value\nHeaders:\n  (none)";
        let styled = colorize(text);

        assert_eq!(styled.lines[0].style, emphasis());
        assert_eq!(
            styled.lines[1].style,
            emphasis(),
            "a bare section heading ending in ':' gets the same emphasis a focus marker does"
        );
        assert_eq!(styled.lines[2].style, Style::default());
    }

    /// The environment-variable edit session's own `»value«` focus marker
    /// (mid-line, unlike the request edit form's line-leading `▶`) must be
    /// recognized too — see `colorize`'s own doc comment on why both are
    /// matched anywhere on the line rather than only at its start.
    #[test]
    fn colorize_styles_a_mid_line_focus_marker() {
        let styled = colorize("base_url = »https://example.com«");
        assert_eq!(styled.lines[0].style, emphasis());
    }

    /// A line that merely contains a colon somewhere in the middle (a URL
    /// with a port, a header's `Key: value`) is not a section heading —
    /// only a line that *ends* with one is.
    #[test]
    fn colorize_does_not_treat_a_mid_line_colon_as_a_heading() {
        let styled = colorize("URL:    https://example.com:8080/path");
        assert_eq!(styled.lines[0].style, Style::default());
    }

    /// A blank line *within* a multi-line block (unlike the fully empty
    /// input `""`, which `str::lines` yields zero lines for at all) must
    /// stay unstyled rather than accidentally matching the heading rule —
    /// an empty string trivially "ends with" nothing, but must not read as
    /// a colon-terminated heading.
    #[test]
    fn colorize_leaves_a_blank_line_unstyled() {
        let styled = colorize("Headers:\n\n  (none)");
        assert_eq!(styled.lines[1].style, Style::default());
    }

    #[test]
    fn colorize_preserves_the_original_text_verbatim() {
        let text = "✓ status is 200\nplain line";
        let styled = colorize(text);
        assert_eq!(styled.to_string(), text);
    }
}

/// Proof that the palette is applied *consistently*, not merely present: a
/// handful of real screens, each rendered through the crate's ordinary
/// `view()` entry point (not a unit test of `colorize`/a style function in
/// isolation), are inspected cell-by-cell to confirm the same visual
/// meaning gets the same real `ratatui::style::Color`/`Modifier` no matter
/// which screen produced it — the request browser's selection highlight
/// matches the environment picker's and the run-history list's; a modal's
/// border is muted the same way regardless of which modal; a successful
/// run's status is colored the same in the status bar and in the response
/// panel, and likewise for a failed one.
#[cfg(test)]
mod screen_consistency_tests {
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Modifier;
    use ratatui::Terminal;
    use sendra_core::Document;

    use crate::app::test_support::*;
    use crate::app::update::update;
    use crate::app::view::view;
    use crate::app::Message;

    use super::{fail, muted, selection, success};

    fn render(state: &crate::app::AppState) -> Buffer {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        terminal
            .draw(|frame| view(state, frame))
            .expect("rendering must not panic");
        terminal.backend().buffer().clone()
    }

    fn row_text(buffer: &Buffer, y: u16) -> String {
        (buffer.area.x..buffer.area.x + buffer.area.width)
            .map(|x| {
                buffer
                    .cell((x, y))
                    .map(|cell| cell.symbol().to_string())
                    .unwrap_or_default()
            })
            .collect()
    }

    /// The y-coordinate of the one row whose text contains `needle` —
    /// panics if there is none or more than one, so a test relying on this
    /// fails loudly rather than silently checking the wrong row.
    fn find_row_containing(buffer: &Buffer, needle: &str) -> u16 {
        let matches: Vec<u16> = (buffer.area.y..buffer.area.y + buffer.area.height)
            .filter(|&y| row_text(buffer, y).contains(needle))
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "expected exactly one row containing {needle:?}, found {matches:?} in:\n{}",
            (buffer.area.y..buffer.area.y + buffer.area.height)
                .map(|y| row_text(buffer, y))
                .collect::<Vec<_>>()
                .join("\n")
        );
        matches[0]
    }

    fn row_has(buffer: &Buffer, y: u16, matches: impl Fn(&ratatui::buffer::Cell) -> bool) -> bool {
        (buffer.area.x..buffer.area.x + buffer.area.width)
            .any(|x| buffer.cell((x, y)).is_some_and(&matches))
    }

    /// The request browser's own selection highlight (`browser::render_request_list`),
    /// the environment picker's (`environment::render_environment_overlay`),
    /// and the run-history list's (`response::render_history_overlay`) must
    /// all be exactly [`selection`] — not three independently chosen
    /// highlight styles that happen to look similar.
    #[test]
    fn selection_highlight_matches_across_every_list() {
        assert_eq!(
            selection().fg,
            None,
            "sanity: selection() must carry no fg override — REVERSED alone is what \
             makes it swap whatever colors the terminal already has, which is the \
             whole point (see theme's own doc comment on respecting the terminal's \
             theme); if that ever changes this test's own check below needs to change with it"
        );
        let is_selection_style =
            |cell: &ratatui::buffer::Cell| cell.modifier.contains(Modifier::REVERSED);

        let browser_state = loaded_state(VALID_COLLECTION);
        let browser_buffer = render(&browser_state);
        let browser_row = find_row_containing(&browser_buffer, "GET One");
        assert!(
            row_has(&browser_buffer, browser_row, is_selection_style),
            "the request browser's selected row must carry the shared selection style"
        );

        let mut env_state = state_with_environments(&["staging", "prod"]);
        update(&mut env_state, Message::OpenEnvironmentOverlay);
        let env_buffer = render(&env_state);
        let env_row = find_row_containing(&env_buffer, "staging");
        assert!(
            row_has(&env_buffer, env_row, is_selection_style),
            "the environment picker's selected row must carry the same selection style"
        );

        let mut history_state = loaded_state(VALID_COLLECTION);
        update(&mut history_state, Message::RunRequested);
        let collection_id = history_state.active().id;
        update(
            &mut history_state,
            Message::RunCompleted {
                collection_id,
                outcome: sample_outcome(200),
            },
        );
        update(&mut history_state, Message::OpenHistoryOverlay);
        let history_buffer = render(&history_state);
        let history_row = find_row_containing(&history_buffer, "ago");
        assert!(
            row_has(&history_buffer, history_row, is_selection_style),
            "the run-history list's selected row must carry the same selection style too"
        );
    }

    /// Every modal this crate draws goes through the one shared
    /// `view::modal_frame`, so its border must be [`muted`] no matter which
    /// modal it is — checked here against three different modals (the
    /// environment overlay, a delete confirmation, and the open-collection
    /// prompt) rather than trusting that sharing one function implies
    /// sharing one style.
    #[test]
    fn modal_border_is_muted_across_every_modal() {
        let is_muted_border = |cell: &ratatui::buffer::Cell| {
            cell.symbol() == "─" && cell.modifier.contains(Modifier::DIM)
        };
        assert_eq!(
            muted().add_modifier,
            Modifier::DIM,
            "sanity: this test's own notion of 'muted' must track theme::muted()"
        );

        let mut env_state = state_with_environments(&["staging"]);
        update(&mut env_state, Message::OpenEnvironmentOverlay);
        let env_buffer = render(&env_state);
        assert!(
            (env_buffer.area.y..env_buffer.area.y + env_buffer.area.height).any(|y| row_has(
                &env_buffer,
                y,
                is_muted_border
            )),
            "the environment overlay's border must be muted"
        );

        let mut delete_state = loaded_state(VALID_COLLECTION);
        update(&mut delete_state, Message::RequestDelete);
        let delete_buffer = render(&delete_state);
        assert!(
            (delete_buffer.area.y..delete_buffer.area.y + delete_buffer.area.height)
                .any(|y| row_has(&delete_buffer, y, is_muted_border)),
            "the delete-confirmation modal's border must be muted the same way"
        );

        let mut open_state = crate::app::AppState::default();
        update(&mut open_state, Message::OpenCollectionPrompt);
        let open_buffer = render(&open_state);
        assert!(
            (open_buffer.area.y..open_buffer.area.y + open_buffer.area.height).any(|y| row_has(
                &open_buffer,
                y,
                is_muted_border
            )),
            "the open-collection prompt's border must be muted the same way too"
        );
    }

    /// A successful run's status must be colored [`success`] in both places
    /// it appears on screen at once: the bottom status bar's "Done — ..."
    /// summary and the response panel's own status line above it — the same
    /// real `RunOutcome`, shown twice, colored once.
    #[test]
    fn a_successful_run_is_colored_success_in_the_status_bar_and_the_response_panel() {
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
        let buffer = render(&state);

        let status_bar_row = find_row_containing(&buffer, "Done —");
        assert!(
            row_has(&buffer, status_bar_row, |cell| cell.fg
                == success().fg.unwrap()),
            "the status bar's 'Done —' summary must be colored success"
        );

        let response_panel_row = find_row_containing(&buffer, "200 OK");
        assert!(
            row_has(&buffer, response_panel_row, |cell| cell.fg
                == success().fg.unwrap()),
            "the response panel's own status line must be colored the same success"
        );
    }

    /// The failure counterpart: both the status bar's "Failed — ..." summary
    /// and the response panel's `⚠` error heading must be colored [`fail`],
    /// the one color this crate uses for every real error.
    #[test]
    fn a_failed_run_is_colored_fail_in_the_status_bar_and_the_response_panel() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        let error = Document::from_yaml_str("requests: [not valid")
            .expect_err("deliberately malformed YAML");
        let collection_id = state.active().id;
        update(
            &mut state,
            Message::RunCompleted {
                collection_id,
                outcome: failed_outcome(error),
            },
        );
        let buffer = render(&state);

        let status_bar_row = find_row_containing(&buffer, "Failed —");
        assert!(
            row_has(&buffer, status_bar_row, |cell| cell.fg
                == fail().fg.unwrap()),
            "the status bar's 'Failed —' summary must be colored fail"
        );

        let error_heading_row = find_row_containing(&buffer, "⚠ Request failed");
        assert!(
            row_has(&buffer, error_heading_row, |cell| cell.fg
                == fail().fg.unwrap()),
            "the response panel's own error heading must be colored the same fail"
        );
    }
}
