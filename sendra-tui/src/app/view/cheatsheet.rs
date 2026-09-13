//! The `?` keybinding cheatsheet (`AppState::cheatsheet_open`) — a single
//! modal listing every keybinding this crate binds anywhere, organized by
//! the same mode/screen sections `main::translate_event` itself checks in
//! (global keys first, then one section per context in that function's own
//! priority order). Kept as one hand-written list rather than derived from
//! `translate_event` at runtime — that function is a pure `Event -> Message`
//! mapping with no notion of "what should a human call this key", so a
//! generated list would show raw `KeyCode`/`Message` names instead of the
//! same prose `status_help_text` already uses. Staying accurate is instead a
//! matter of grepping `main.rs`'s `KeyCode` matches against the sections
//! below by hand whenever either one changes — exactly the audit this
//! module's own tests below perform.

use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use super::{modal_frame, theme};

/// One heading (rendered bold via [`theme::colorize`]'s trailing-`:` rule)
/// per section, each followed by its own keybinding lines.
const SECTIONS: &[(&str, &[&str])] = &[
    (
        "Global — always available:",
        &[
            "q / Ctrl+C    quit (asks first if anything is unsaved)",
            "?             open/close this cheatsheet",
        ],
    ),
    (
        "Browsing (a collection is loaded, nothing else open):",
        &[
            "↑/k, ↓/j      move selection",
            "Enter / r     run the selected request",
            "i             edit the selected request",
            "n             add a new request",
            "d             delete the selected request",
            "e             open the environment picker",
            "h             open the run-history browser",
            "o             open another collection",
            "/             filter the request list by name",
            "] / [         next / previous collection tab",
            "Ctrl+w        close the current collection tab",
            "PgUp/PgDn/Home/End   scroll the response panel",
            "c             reveal / hide captured values",
        ],
    ),
    (
        "Filtering the request list (after pressing /):",
        &[
            "(type)        narrow the list to matching names, live",
            "↑/↓           move selection within the filtered list",
            "Enter         run the highlighted (filtered) request",
            "Esc           clear the filter and show every request again",
        ],
    ),
    (
        "Welcome screen (no collection path given yet):",
        &[
            "↑/k, ↓/j      choose a discovered collection",
            "Enter / r     open the chosen collection",
            "o             type a collection path instead",
            "e             open the environment picker",
        ],
    ),
    (
        "Editing a request:",
        &[
            "Tab / Shift+Tab      next / previous field",
            "Left/Right           move cursor (or toggle an enum field)",
            "Up/Down              move within the body field (body focused only)",
            "Enter                newline (body field only)",
            "Backspace/Delete     edit text",
            "Ctrl+n / Ctrl+d      add / delete a header row",
            "Ctrl+a / Ctrl+x      add / delete an assertion row",
            "Ctrl+p / Ctrl+k      add / delete a capture row",
            "Ctrl+s               save",
            "Esc                  cancel",
            "(any other character)   type into the focused field",
        ],
    ),
    (
        "Environment picker:",
        &[
            "↑/k, ↓/j      move selection",
            "Enter         select this environment",
            "i             edit this environment's variables",
            "Esc           close",
        ],
    ),
    (
        "Editing an environment's variables:",
        &[
            "Tab / Shift+Tab      next / previous field",
            "Left/Right           move cursor",
            "Backspace/Delete     edit text",
            "Ctrl+n               add a variable row",
            "Ctrl+d               delete the focused row (asks to confirm)",
            "Ctrl+s               save",
            "Esc                  cancel",
            "(any other character)   type into the focused field",
        ],
    ),
    (
        "Run-history browser:",
        &[
            "↑/k, ↓/j      move selection",
            "Enter         view this entry's full result",
            "Space         expand / collapse this entry",
            "Esc           close",
        ],
    ),
    (
        "Viewing one history entry:",
        &[
            "PgUp/PgDn/Home/End   scroll the response",
            "c                    reveal / hide captured values",
            "Esc                  back to the history list",
        ],
    ),
    (
        "Any confirmation (delete request, delete variable, close tab, quit):",
        &["y / Enter     confirm", "n / Esc       cancel"],
    ),
    (
        "Open-collection prompt:",
        &[
            "Left/Right           move cursor",
            "Backspace/Delete     edit text",
            "Enter                open the typed path",
            "Esc                  cancel",
            "(any other character)   type into the path field",
        ],
    ),
];

/// Draws the cheatsheet over everything else — called from `view()` after
/// even `quit_confirm` (see that call site's own comment): the same
/// "answers 'what can I press right now' regardless of what that turns out
/// to be" reasoning that puts its own key check ahead of every other key in
/// `main::translate_event`.
pub(crate) fn render_cheatsheet(frame: &mut Frame) {
    let inner = modal_frame(frame, 80, 90, "Keybindings — ? or Esc to close");
    frame.render_widget(
        Paragraph::new(theme::colorize(&cheatsheet_text())).wrap(Wrap { trim: false }),
        inner,
    );
}

/// [`SECTIONS`] flattened into the plain text [`render_cheatsheet`] hands to
/// [`theme::colorize`] — split out from that function so the audit tests
/// below can check its content directly, with no `Frame`/terminal needed.
fn cheatsheet_text() -> String {
    let mut text = String::new();
    for (index, (heading, lines)) in SECTIONS.iter().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        text.push_str(heading);
        text.push('\n');
        for line in *lines {
            text.push_str("  ");
            text.push_str(line);
            text.push('\n');
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::app::state::{AppState, Message};
    use crate::app::test_support::*;
    use crate::app::update::update;

    /// The real, non-test body of `main::translate_event` (and the
    /// `next_message` wrapper right above it) — sliced out of the crate's
    /// own source by the same two markers every time, so this audit reads
    /// whatever that function currently says rather than a copy that could
    /// silently drift from it. Excludes `#[cfg(test)] mod tests` below it,
    /// which is full of `KeyCode`s that are test *inputs*, not real
    /// bindings (see e.g. `control_letter_combinations_other_than_ctrl_s_n_d_a_x_p_k_do_nothing_while_editing`'s
    /// own `KeyCode::Char('b')`, deliberately not a real binding).
    fn real_keymap_source() -> &'static str {
        const MAIN_RS: &str = include_str!("../../main.rs");
        let start = MAIN_RS
            .find("fn next_message(")
            .expect("main.rs must still define next_message");
        // Not `"#[cfg(test)]\nmod tests {"` — this crate's checked-in
        // `main.rs` uses CRLF line endings, so a literal `\n` between the
        // two lines would never match; `"mod tests {"` alone is unique in
        // the file (`resolution_parity_tests` further down has a different
        // name) and needs no line-ending assumption at all.
        let end = MAIN_RS
            .find("mod tests {")
            .expect("main.rs must still have its own #[cfg(test)] mod tests");
        &MAIN_RS[start..end]
    }

    /// Every distinct `KeyCode::Char('x')` literal the real keymap binds —
    /// both a plain key and, indistinguishably by this scan, the same
    /// letter used with a Ctrl modifier elsewhere in the same match (e.g.
    /// `d`/browsing-delete vs `Ctrl+d`/header-delete) — this test only
    /// checks that the *letter* is mentioned somewhere in the cheatsheet,
    /// not which modifier combination.
    fn bound_chars() -> BTreeSet<char> {
        let source = real_keymap_source();
        let mut chars = BTreeSet::new();
        let mut rest = source;
        while let Some(pos) = rest.find("KeyCode::Char('") {
            let after = &rest[pos + "KeyCode::Char('".len()..];
            let ch = after.chars().next().expect("a char literal follows");
            chars.insert(ch);
            rest = &after[ch.len_utf8()..];
        }
        chars
    }

    /// Every named (non-`Char`) `KeyCode` variant the real keymap matches on
    /// — `Esc`, `Enter`, `Tab`, arrows, and so on.
    fn bound_named_keys() -> BTreeSet<&'static str> {
        const NAMED: &[&str] = &[
            "Esc",
            "Enter",
            "Tab",
            "BackTab",
            "Backspace",
            "Delete",
            "Left",
            "Right",
            "Up",
            "Down",
            "PageUp",
            "PageDown",
            "Home",
            "End",
        ];
        let source = real_keymap_source();
        NAMED
            .iter()
            .copied()
            .filter(|name| source.contains(&format!("KeyCode::{name}")))
            .collect()
    }

    /// The actual drift guard: every `KeyCode` the real keymap in
    /// `main::translate_event`/`next_message` binds must be mentioned
    /// somewhere in the cheatsheet text — a key that exists in code but is
    /// missing here would fail this test, exactly the "bug in this issue's
    /// list, not an acceptable gap" this module's own doc comment promises.
    #[test]
    fn every_real_keybinding_is_mentioned_somewhere_in_the_cheatsheet() {
        let text = cheatsheet_text();

        for ch in bound_chars() {
            // `Char(' ')` reads as "Space" in prose, not a literal space —
            // every other bound letter/punctuation is written verbatim.
            let mentioned = if ch == ' ' {
                text.contains("Space")
            } else {
                text.contains(ch)
            };
            assert!(
                mentioned,
                "KeyCode::Char({ch:?}) is bound in main::translate_event's real keymap \
                 but no cheatsheet section mentions it — see this test's own doc comment"
            );
        }

        for name in bound_named_keys() {
            // `BackTab` is what crossterm calls Shift+Tab — the cheatsheet
            // writes it the way a person would actually press it, the same
            // "prose, not raw enum names" reasoning this module's own doc
            // comment gives for not generating this list from `KeyCode`
            // itself.
            // `PageUp`/`PageDown` are written the same abbreviated way
            // `status_help_text` already writes them everywhere else in
            // this crate's UI (`"PgUp/PgDn/Home/End"`), for consistency
            // with that existing convention rather than spelling them out
            // here alone.
            let mentioned = match name {
                "BackTab" => text.contains("Shift+Tab"),
                "PageUp" => text.contains("PgUp"),
                "PageDown" => text.contains("PgDn"),
                _ => text.contains(name),
            };
            assert!(
                mentioned,
                "KeyCode::{name} is bound in main::translate_event's real keymap but no \
                 cheatsheet section mentions it"
            );
        }
    }

    /// Sanity check on the audit itself: the scan must actually find the
    /// real keymap's bindings (not, say, an empty set because the source
    /// markers stopped matching after a refactor) — otherwise the test
    /// above would trivially pass having checked nothing.
    #[test]
    fn the_source_scan_actually_finds_the_real_keymap_bindings() {
        let chars = bound_chars();
        assert!(
            chars.contains(&'q'),
            "bare q must be found as a real binding"
        );
        assert!(
            chars.contains(&'?'),
            "the cheatsheet's own ? key must be found"
        );
        assert!(
            chars.len() >= 15,
            "expected at least 15 distinct bound characters, found {chars:?}"
        );

        let named = bound_named_keys();
        assert!(named.contains("Esc"));
        assert!(named.contains("Enter"));
        assert!(
            named.len() >= 10,
            "expected at least 10 distinct named keys, found {named:?}"
        );
    }

    /// Opening the cheatsheet from ordinary browsing shows the modal, its
    /// dismiss hint, and a representative binding from more than one
    /// section — proof it isn't just the browsing section repeated. Rendered
    /// into a taller-than-`render_screen` terminal: the full list runs to
    /// more lines than this crate's every other (much shorter) overlay, so
    /// a realistic-but-generous terminal height is what proves nothing past
    /// the first section or two is silently missing, rather than merely
    /// clipped off the bottom of a short test buffer.
    #[test]
    fn cheatsheet_opens_from_browsing_and_lists_multiple_sections() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::OpenCheatsheet);
        assert!(state.cheatsheet_open);

        let backend = TestBackend::new(100, 90);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        terminal
            .draw(|frame| crate::app::view(&state, frame))
            .expect("rendering must not panic");
        let screen = buffer_to_string(terminal.backend().buffer());
        assert!(screen.contains("Keybindings"));
        assert!(screen.contains("? or Esc to close"));
        assert!(screen.contains("Global"));
        assert!(screen.contains("Editing a request"));
        assert!(screen.contains("Run-history browser"));
        assert!(screen.contains("Any confirmation"));
    }

    /// The cheatsheet can also be summoned from on top of the environment
    /// overlay and the run-history browser — it must still render (drawn
    /// last, over both) rather than being hidden behind them.
    #[test]
    fn cheatsheet_opens_over_the_environment_overlay_and_history_overlay() {
        let mut env_state = state_with_environments(&["staging", "prod"]);
        update(&mut env_state, Message::OpenEnvironmentOverlay);
        update(&mut env_state, Message::OpenCheatsheet);
        let env_screen = render_screen(&env_state);
        assert!(env_screen.contains("Keybindings"));

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
        update(&mut history_state, Message::OpenCheatsheet);
        let history_screen = render_screen(&history_state);
        assert!(history_screen.contains("Keybindings"));
    }

    /// From the welcome/discovery screen too — the cheatsheet is reachable
    /// before any collection is even loaded.
    #[test]
    fn cheatsheet_opens_from_the_welcome_screen() {
        let mut state = AppState::default();
        update(&mut state, Message::OpenCheatsheet);
        let screen = render_screen(&state);
        assert!(screen.contains("Keybindings"));
        assert!(screen.contains("Welcome screen"));
    }

    /// And over the quit confirmation — the topmost modal this crate has —
    /// proof the cheatsheet really is drawn last of all, as `view()`'s own
    /// comment on the call site claims.
    #[test]
    fn cheatsheet_opens_over_the_quit_confirmation() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::EnterEditMode);
        update(&mut state, Message::Quit);
        assert!(state.quit_confirm.is_some());
        update(&mut state, Message::OpenCheatsheet);

        let screen = render_screen(&state);
        assert!(screen.contains("Keybindings"));
    }

    /// Closing it again — `CloseCheatsheet` (what `?`/`Esc` both translate
    /// to while it's open — see `main::translate_event`) clears the flag.
    #[test]
    fn closing_the_cheatsheet_clears_the_flag() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::OpenCheatsheet);
        assert!(state.cheatsheet_open);
        update(&mut state, Message::CloseCheatsheet);
        assert!(!state.cheatsheet_open);
    }
}
