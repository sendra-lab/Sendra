//! The environment overlay: `render_environment_overlay` (the picker list)
//! and `render_environment_edit` (the in-progress variables edit session it
//! shows instead, while one is open).

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use super::super::state::{AppState, EnvVarField, EnvironmentEditState};
use super::{format_error, modal_frame};

pub(crate) fn render_environment_overlay(frame: &mut Frame, state: &AppState, cursor: usize) {
    // An in-progress variable edit takes over the whole overlay, the same
    // way `render_detail_pane` lets edit mode take over the whole detail
    // pane: there is no meaningful "picker vs. editor" split to preserve
    // underneath one, and `update()`'s own guard keeps the overlay's cursor
    // from moving while this session is open, so the environment it names
    // cannot drift out from under `cursor` while this branch renders it.
    if let Some(env_edit) = &state.environment_edit {
        render_environment_edit(frame, state, env_edit);
        return;
    }

    let inner = modal_frame(
        frame,
        70,
        70,
        "Select environment — Enter to confirm, i to edit, Esc to cancel",
    );

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

/// The environment-variable edit session — what `render_environment_overlay`
/// shows instead of the picker list while `AppState::environment_edit` is
/// `Some`. One row per variable, `name = value`, with `»`/`«` bracketing
/// whichever half of whichever row has focus — the same "mark focus inline
/// in the text, no separate cursor widget" convention `render_edit_pane`'s
/// header rows already use. A row pending deletion
/// (`EnvironmentEditState::pending_delete`) is drawn as a real modal on top
/// of this screen, through the same [`render_confirm_prompt`] every other
/// destructive-action confirmation in this crate uses (see `view()`'s own
/// call site) — not called out inline the way it used to be, before this
/// issue's unified confirmation component covered this case too.
fn render_environment_edit(frame: &mut Frame, state: &AppState, env_edit: &EnvironmentEditState) {
    let name = state
        .environments
        .get(env_edit.index)
        .map(|named| named.name.as_str())
        .unwrap_or("?");

    let inner = modal_frame(
        frame,
        70,
        70,
        format!(
            "Edit variables for '{name}' — tab switch field, ctrl+n add, ctrl+d delete, \
             ctrl+s save, esc cancel"
        ),
    );

    let mut lines = Vec::new();
    if env_edit.rows.is_empty() {
        lines.push("(no variables — ctrl+n to add one)".to_string());
    } else {
        for (index, row) in env_edit.rows.iter().enumerate() {
            let name_text = if env_edit.focus == Some(EnvVarField::Name(index)) {
                format!("»{}«", row.key.value())
            } else {
                row.key.value().to_string()
            };
            let value_text = if env_edit.focus == Some(EnvVarField::Value(index)) {
                format!("»{}«", row.value.value())
            } else {
                row.value.value().to_string()
            };
            lines.push(format!("{name_text} = {value_text}"));
        }
    }

    if let Some(error) = &env_edit.save_error {
        lines.push(String::new());
        lines.push(format_error("Could not save", error));
    }

    frame.render_widget(
        Paragraph::new(lines.join("\n")).wrap(Wrap { trim: false }),
        inner,
    );
}

#[cfg(test)]
mod tests {

    use sendra_core::Environment;

    use crate::app::state::{AppState, Message, NamedEnvironment};
    use crate::app::test_support::*;
    use crate::app::update::update;

    // --- Editing environment variables --------------------------------------

    /// Proof requirement: after editing an environment variable and saving,
    /// the detail pane's resolved-request preview — built through the real
    /// `preview::resolve_browsing_preview` pipeline `render_detail_pane`
    /// always uses, not a reformatted stand-in — shows the new value live,
    /// with no further action needed once the overlay closes.
    #[test]
    fn saved_environment_variable_edits_show_up_live_in_the_resolved_preview() {
        let (mut state, _dir, _path) = loaded_state_with_saved_environment("old-value");

        let before = render_screen(&state);
        assert!(
            before.contains("https://example.com/old-value"),
            "sanity: the preview must resolve against the original value first:\n{before}"
        );

        update(&mut state, Message::OpenEnvironmentOverlay);
        update(&mut state, Message::EnterEnvironmentEdit);
        // The one row (`base_url`) starts focused on its name field — move
        // onto the value to edit it, the same way a real user would.
        update(&mut state, Message::EditFocusNext);
        for _ in 0.."old-value".len() {
            update(&mut state, Message::EditBackspace);
        }
        for ch in "new-value".chars() {
            update(&mut state, Message::EditInsertChar(ch));
        }
        update(&mut state, Message::SaveEnvironmentEdit);
        assert!(
            state.environment_edit.is_none(),
            "the save must succeed for this test to mean anything"
        );
        update(&mut state, Message::CloseEnvironmentOverlay);

        let after = render_screen(&state);
        assert!(
            after.contains("https://example.com/new-value"),
            "the resolved preview must reflect the saved edit live:\n{after}"
        );
        assert!(
            !after.contains("old-value"),
            "the stale value must not linger anywhere on screen:\n{after}"
        );
    }

    #[test]
    fn environment_edit_screen_renders_rows_and_the_focus_marker() {
        let (mut state, _dir, _path) = loaded_state_with_saved_environment("https://example.com");
        update(&mut state, Message::OpenEnvironmentOverlay);
        update(&mut state, Message::EnterEnvironmentEdit);

        let screen = render_screen(&state);
        assert!(
            screen.contains("base_url"),
            "the variable's name must be on screen:\n{screen}"
        );
        assert!(
            screen.contains("»base_url«"),
            "the focused name field must carry a visible focus marker:\n{screen}"
        );
        assert!(
            screen.contains("https://example.com"),
            "the variable's value must be on screen too:\n{screen}"
        );
    }

    #[test]
    fn environment_edit_screen_shows_no_variables_placeholder_when_empty() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = sendra_core::environment::environment_path(dir.path(), "empty");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        let environment = Environment::from_path(&path).expect("an empty file is still valid");

        let mut state = AppState::default();
        state.environments = vec![NamedEnvironment {
            name: "empty".to_string(),
            environment,
        }];
        state.environment_overlay = Some(0);
        update(&mut state, Message::EnterEnvironmentEdit);

        let screen = render_screen(&state);
        assert!(
            screen.contains("no variables"),
            "the zero-variables case must render a clear placeholder, not a blank \
             or broken screen:\n{screen}"
        );
    }

    #[test]
    fn env_var_delete_confirmation_renders_through_the_shared_confirm_prompt() {
        let (mut state, _dir, _path) = loaded_state_with_saved_environment("https://example.com");
        update(&mut state, Message::OpenEnvironmentOverlay);
        update(&mut state, Message::EnterEnvironmentEdit);

        update(&mut state, Message::RequestDeleteEnvVarRow);

        let screen = render_screen(&state);
        assert!(
            screen.contains("Delete variable"),
            "a pending row delete must show the shared confirmation modal, \
             the same component every other destructive action uses:\n{screen}"
        );
        assert!(
            screen.contains("y/Enter confirm"),
            "the shared confirmation modal's own keys must be advertised:\n{screen}"
        );
    }
}
