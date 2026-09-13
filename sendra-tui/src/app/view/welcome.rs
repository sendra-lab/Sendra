//! The "nothing loaded yet" screen — what [`view`](super::view) draws for
//! `LoadState::NoPathProvided` instead of a bare "No collection path
//! provided" message. [`render_welcome`] picks between two states depending
//! on what `discovery::discover_collections` found near the current
//! directory: a selectable list of candidates (the same `List`/`ListState`
//! picker shape `environment::render_environment_overlay` already uses), or,
//! only when genuinely nothing was found, the plain welcome message with the
//! Sendra mark (`logo::logo_lines`).

use std::path::{Path, PathBuf};

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use super::super::logo;
use super::super::theme;
use super::bordered_pane;

pub(crate) fn render_welcome(frame: &mut Frame, area: Rect, discovered: &[PathBuf], cursor: usize) {
    if discovered.is_empty() {
        render_plain_welcome(frame, area);
    } else {
        render_discovery_picker(frame, area, discovered, cursor);
    }
}

/// The genuinely-nothing-nearby case: the Sendra mark (`logo::logo_lines`),
/// a short explanation, and the two keys that do something from here
/// (`o`/`e`; `q` is implied the way it always is).
fn render_plain_welcome(frame: &mut Frame, area: Rect) {
    let inner = bordered_pane(frame, area, " sendra-tui ", theme::muted());

    let logo_lines = logo::logo_lines();
    let logo_height = (logo_lines.len() as u16).min(inner.height);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(logo_height), Constraint::Min(0)])
        .split(inner);

    frame.render_widget(
        Paragraph::new(logo_lines).alignment(Alignment::Center),
        rows[0],
    );

    let message = "No collection loaded yet.\n\n\
        Press o to open a collection file, or pass one on the command line:\n\
        sendra tui path/to/collection.yaml\n\n\
        e  open the environment picker\n\
        o  open a collection\n\
        q  quit";
    frame.render_widget(
        Paragraph::new(theme::colorize(message))
            .wrap(Wrap { trim: false })
            .alignment(Alignment::Center),
        rows[1],
    );
}

/// The discovery-picker case: a short instruction line, then the candidates
/// themselves as an ordinary selectable list — highlighted the same way
/// every other list in this crate is (`theme::selection`), cursor-driven by
/// the same `SelectNext`/`SelectPrevious` messages the request list and the
/// environment overlay already use (see `update::select`'s own doc comment).
fn render_discovery_picker(frame: &mut Frame, area: Rect, discovered: &[PathBuf], cursor: usize) {
    let inner = bordered_pane(frame, area, " Open a collection ", theme::muted());

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(0)])
        .split(inner);

    frame.render_widget(
        Paragraph::new(theme::colorize(
            "Found these collection files nearby. ↑/↓ choose, Enter to open, \
             o to type a different path instead.",
        ))
        .wrap(Wrap { trim: true }),
        rows[0],
    );

    let items: Vec<ListItem> = discovered
        .iter()
        .map(|path| ListItem::new(display_path(path)))
        .collect();
    let list = List::new(items).highlight_style(theme::selection());
    let mut list_state =
        ListState::default().with_selected(Some(cursor.min(discovered.len().saturating_sub(1))));
    frame.render_stateful_widget(list, rows[1], &mut list_state);
}

/// How one discovered path is shown in the picker: its file name alone —
/// candidates all come from one flat, non-recursive directory listing (see
/// `discovery::discover_collections`), so distinct files always have
/// distinct names, and the file name alone is far more scannable in a list
/// than repeating the same directory prefix on every row. Falls back to the
/// full path on the rare chance `to_str` fails (a non-UTF-8 name on a
/// platform that allows one).
fn display_path(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use crate::app::state::{AppState, LoadState, Message};
    use crate::app::test_support::buffer_to_string;
    use crate::app::update::update;
    use crate::app::view::view;

    fn render(state: &AppState) -> String {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        terminal
            .draw(|frame| view(state, frame))
            .expect("rendering must not panic");
        buffer_to_string(terminal.backend().buffer())
    }

    fn state_with_discovered(paths: Vec<std::path::PathBuf>) -> AppState {
        let mut state = AppState::default();
        state.load_state = LoadState::NoPathProvided {
            discovered: paths,
            cursor: 0,
        };
        state
    }

    #[test]
    fn the_plain_welcome_screen_shows_the_mark_and_the_open_key() {
        let state = state_with_discovered(Vec::new());

        let screen = render(&state);

        assert!(
            screen.contains("No collection loaded"),
            "the empty-state explanation must be visible:\n{screen}"
        );
        assert!(
            screen.contains('o'),
            "the open-collection key must be advertised"
        );
        // A solid block is distinctive enough on its own to prove the
        // character-art mark actually rendered, without asserting on every
        // exact character in `logo::MARK` (which is still being tuned).
        assert!(screen.contains('█'), "the mark must render:\n{screen}");
    }

    #[test]
    fn discovered_collections_are_shown_as_a_picker_not_the_plain_welcome_message() {
        let state = state_with_discovered(vec![
            std::path::PathBuf::from("/tmp/found-one.yaml"),
            std::path::PathBuf::from("/tmp/found-two.yaml"),
        ]);

        let screen = render(&state);

        assert!(screen.contains("found-one.yaml"), "{screen}");
        assert!(screen.contains("found-two.yaml"), "{screen}");
        assert!(
            !screen.contains("No collection loaded"),
            "discovering candidates must replace the plain empty-state message, \
             not sit alongside it:\n{screen}"
        );
    }

    #[test]
    fn the_highlighted_candidate_carries_the_shared_selection_style() {
        use ratatui::style::Modifier;

        let mut state = state_with_discovered(vec![
            std::path::PathBuf::from("/tmp/one.yaml"),
            std::path::PathBuf::from("/tmp/two.yaml"),
        ]);
        update(&mut state, Message::SelectNext);

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        terminal
            .draw(|frame| view(&state, frame))
            .expect("rendering must not panic");
        let buffer = terminal.backend().buffer();

        let row = (buffer.area.y..buffer.area.y + buffer.area.height)
            .find(|&y| {
                (buffer.area.x..buffer.area.x + buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .contains("two.yaml")
            })
            .expect("the second entry must be on screen");
        let is_selected = (buffer.area.x..buffer.area.x + buffer.area.width)
            .any(|x| buffer[(x, row)].modifier.contains(Modifier::REVERSED));
        assert!(
            is_selected,
            "the moved-to entry must carry the shared selection style"
        );
    }

    #[test]
    fn a_genuinely_malformed_collection_still_shows_its_real_error_not_the_welcome_screen() {
        let mut state = AppState::default();
        let error = sendra_core::Document::from_yaml_str("requests: [not valid")
            .expect_err("deliberately malformed YAML");
        update(
            &mut state,
            Message::CollectionLoaded {
                base_dir: std::path::PathBuf::from("."),
                path: std::path::PathBuf::from("bad.yaml"),
                result: Box::new(Err(error)),
            },
        );

        let screen = render(&state);

        assert!(
            !screen.contains("No collection loaded"),
            "a real load failure must never be hidden behind the friendly \
             welcome framing:\n{screen}"
        );
        assert!(screen.contains("Failed to load collection"), "{screen}");
    }
}
