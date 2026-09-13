//! Message handlers for an environment's variables edit session:
//! `EnterEnvironmentEdit`/`SaveEnvironmentEdit`/`CancelEnvironmentEdit` and
//! the env-var-row messages (`AddEnvVarRow`, `RequestDeleteEnvVarRow`,
//! `ConfirmDeleteEnvVarRow`, `CancelDeleteEnvVarRow`), plus `try_save_environment`
//! and the shared `env_var_mutate`/`env_var_move` machinery every
//! focused-field message routes through while this session is open.

use std::collections::BTreeMap;
use std::path::Path;

use sendra_core::{Environment, SendraError};

use crate::app::state::{CollectionSession, EnvironmentEditState, HeaderRow, RunState, TextField};

/// `Message::EnterEnvironmentEdit`. `state.edit_mode.is_none()` is redundant
/// with real message flow alone but checked directly anyway, the same
/// defense-in-depth every other mode-entry handler already applies.
pub(crate) fn handle_enter_environment_edit(state: &mut CollectionSession) {
    if state.edit_mode.is_none()
        && state.environment_edit.is_none()
        && !matches!(state.run_state, RunState::InFlight)
    {
        if let Some(cursor) = state.environment_overlay {
            if let Some(named) = state.environments.get(cursor) {
                state.environment_edit =
                    Some(EnvironmentEditState::new(cursor, &named.environment));
            }
        }
    }
}

/// `Message::CancelEnvironmentEdit`. Dropping `environment_edit` is the
/// entire mechanism: nothing real is ever written into
/// `CollectionSession::environments` before `SaveEnvironmentEdit` actually
/// runs, so there is nothing else to undo.
pub(crate) fn handle_cancel_environment_edit(state: &mut CollectionSession) {
    state.environment_edit = None;
}

/// `Message::SaveEnvironmentEdit`.
pub(crate) fn handle_save_environment_edit(state: &mut CollectionSession) {
    // A fresh attempt clears a stale error from a previous one first.
    if let Some(env_edit) = &mut state.environment_edit {
        env_edit.save_error = None;
    }
    let Some((index, rows)) = state
        .environment_edit
        .as_ref()
        .map(|env_edit| (env_edit.index, env_edit.rows.clone()))
    else {
        return;
    };

    let variables = match validate_env_var_rows(&rows) {
        Ok(variables) => variables,
        Err(message) => {
            if let Some(env_edit) = &mut state.environment_edit {
                env_edit.save_error = Some(message);
            }
            return;
        }
    };

    // The environment list cannot change shape while this session is open
    // (see `update()`'s own `environment_edit` guard), so `index` staying
    // valid here is an invariant, not something this needs to recover from
    // gracefully — but matched rather than indexed directly, so a bug in
    // that invariant closes the session instead of panicking.
    let Some(named) = state.environments.get(index) else {
        state.environment_edit = None;
        return;
    };
    let Some(path) = named.environment.source.clone() else {
        if let Some(env_edit) = &mut state.environment_edit {
            env_edit.save_error =
                Some("this environment has no file on disk to save to".to_string());
        }
        return;
    };

    match try_save_environment(&named.environment, &path, variables) {
        Ok(candidate) => {
            state.environments[index].environment = candidate;
            state.environment_edit = None;
        }
        Err(error) => {
            if let Some(env_edit) = &mut state.environment_edit {
                env_edit.save_error = Some(error.to_string());
            }
        }
    }
}

/// `Message::AddEnvVarRow`.
pub(crate) fn handle_add_env_var_row(state: &mut CollectionSession) {
    if let Some(env_edit) = &mut state.environment_edit {
        env_edit.add_row();
    }
}

/// `Message::RequestDeleteEnvVarRow`.
pub(crate) fn handle_request_delete_env_var_row(state: &mut CollectionSession) {
    if let Some(env_edit) = &mut state.environment_edit {
        env_edit.request_delete_focused();
    }
}

/// `Message::ConfirmDeleteEnvVarRow`.
pub(crate) fn handle_confirm_delete_env_var_row(state: &mut CollectionSession) {
    if let Some(env_edit) = &mut state.environment_edit {
        env_edit.confirm_pending_delete();
    }
}

/// `Message::CancelDeleteEnvVarRow`.
pub(crate) fn handle_cancel_delete_env_var_row(state: &mut CollectionSession) {
    if let Some(env_edit) = &mut state.environment_edit {
        env_edit.cancel_pending_delete();
    }
}

/// Builds the real `BTreeMap` a save writes from `rows`, or refuses with a
/// message naming the problem: an empty (post-trim) name, or two rows
/// sharing the same name.
fn validate_env_var_rows(rows: &[HeaderRow]) -> Result<BTreeMap<String, String>, String> {
    let mut variables = BTreeMap::new();
    for row in rows {
        let name = row.key.value().trim();
        if name.is_empty() {
            return Err("every variable needs a name".to_string());
        }
        if variables.contains_key(name) {
            return Err(format!(
                "two variables are both named '{name}'; names must be unique"
            ));
        }
        variables.insert(name.to_string(), row.value.value().to_string());
    }
    Ok(variables)
}

/// Builds `named`'s candidate `Environment` with `variables` replacing its
/// own, and attempts to save it to `path`. Pure with respect to `AppState`,
/// the same shape as `super::request_edit::try_save_edit`/`try_delete_request`.
fn try_save_environment(
    named: &Environment,
    path: &Path,
    variables: BTreeMap<String, String>,
) -> Result<Environment, SendraError> {
    let mut candidate = named.clone();
    candidate.variables = variables;
    candidate.save_to_path(path).map(|()| candidate)
}

/// Applies `mutate` to whichever field `CollectionSession::environment_edit`'s
/// own `EnvironmentEditState::focus` currently points at — the same role
/// `super::request_edit::edit_mutate` plays for a request's own fields.
pub(crate) fn env_var_mutate(state: &mut CollectionSession, mutate: impl FnOnce(&mut TextField)) {
    let Some(env_edit) = &mut state.environment_edit else {
        return;
    };
    let Some(field) = env_edit.focused_field_mut() else {
        return;
    };
    mutate(field);
}

/// Like `env_var_mutate`, but for cursor movement — mirrors
/// `super::request_edit::edit_move` exactly.
pub(crate) fn env_var_move(state: &mut CollectionSession, mutate: impl FnOnce(&mut TextField)) {
    if let Some(env_edit) = &mut state.environment_edit {
        if let Some(field) = env_edit.focused_field_mut() {
            mutate(field);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use sendra_core::{Document, Environment};

    use crate::app::state::{AppState, EnvVarField, Message, NamedEnvironment};
    use crate::app::test_support::*;
    use crate::app::update::update;
    use crate::run_request::RunOutcome;

    use super::*;

    // --- Editing environment variables -------------------------------------

    /// Builds an `AppState` with one real environment file on disk (unlike
    /// `state_with_environments`, whose `Environment`s have no `source` and
    /// so cannot be saved), the overlay open with its cursor on that
    /// environment, and returns `(state, path)` — everything a test needs
    /// to open an edit session and later reload the same file fresh from
    /// disk to check what was actually written.
    fn state_with_saved_environment(
        name: &str,
        variables: &[(&str, &str)],
    ) -> (AppState, tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = sendra_core::environment::environment_path(dir.path(), name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let yaml: String = variables
            .iter()
            .map(|(key, value)| format!("{key}: {value}\n"))
            .collect();
        std::fs::write(&path, yaml).unwrap();

        let environment = Environment::from_path(&path).expect("the fixture file must parse");
        let mut state = AppState::default();
        state.environments = vec![NamedEnvironment {
            name: name.to_string(),
            environment,
        }];
        state.environment_overlay = Some(0);
        // `dir` (the `TempDir` guard) is returned alongside `state`/`path`
        // rather than dropped here: dropping it deletes the whole directory
        // tree immediately, which would pull the file out from under every
        // caller before it ever gets to read or write it again.
        (state, dir, path)
    }

    #[test]
    fn enter_environment_edit_seeds_rows_from_the_overlay_cursors_environment() {
        let (mut state, _dir, _path) =
            state_with_saved_environment("staging", &[("base_url", "https://example.com")]);

        update(&mut state, Message::EnterEnvironmentEdit);

        let env_edit = state
            .environment_edit
            .as_ref()
            .expect("EnterEnvironmentEdit must open a session");
        assert_eq!(env_edit.index, 0);
        assert_eq!(env_edit.rows.len(), 1);
        assert_eq!(env_edit.rows[0].key.value(), "base_url");
        assert_eq!(env_edit.rows[0].value.value(), "https://example.com");
    }

    /// The zero-variables investigation, demonstrated directly against the
    /// reducer: an environment with nothing in it opens a session with no
    /// rows and no focus, rather than refusing to open or panicking —
    /// unlike a request's `Document`, there is no shape an `Environment`
    /// needs to be lifted into just to hold zero variables.
    #[test]
    fn enter_environment_edit_on_an_empty_environment_opens_with_no_rows_and_no_focus() {
        let (mut state, _dir, _path) = state_with_saved_environment("empty", &[]);

        update(&mut state, Message::EnterEnvironmentEdit);

        let env_edit = state.environment_edit.as_ref().unwrap();
        assert!(env_edit.rows.is_empty());
        assert_eq!(env_edit.focus, None);
    }

    #[test]
    fn cancel_environment_edit_leaves_everything_untouched() {
        let (mut state, _dir, path) =
            state_with_saved_environment("staging", &[("base_url", "https://example.com")]);
        let original_bytes = std::fs::read(&path).unwrap();

        update(&mut state, Message::EnterEnvironmentEdit);
        update(&mut state, Message::AddEnvVarRow);
        type_into_focused_field(&mut state, "token");

        update(&mut state, Message::CancelEnvironmentEdit);

        assert!(state.environment_edit.is_none());
        assert_eq!(
            state.environments[0].environment.variables.len(),
            1,
            "the in-memory environment must be exactly as it was"
        );
        let bytes_after = std::fs::read(&path).unwrap();
        assert_eq!(
            original_bytes, bytes_after,
            "cancelling must never touch the file on disk"
        );
    }

    /// Proof requirement, end to end: add a variable, edit an existing one,
    /// delete a third, save, and reload the file with a brand-new
    /// `Environment::from_path` — not anything still sitting in `state` —
    /// to confirm every change genuinely reached disk.
    #[test]
    fn adding_editing_and_deleting_variables_then_saving_persists_all_three_changes_to_disk() {
        let (mut state, _dir, path) = state_with_saved_environment(
            "staging",
            &[
                ("base_url", "https://old.example.com"),
                ("to_delete", "gone-soon"),
            ],
        );

        update(&mut state, Message::EnterEnvironmentEdit);
        assert_eq!(
            state.environment_edit.as_ref().unwrap().focus,
            Some(EnvVarField::Name(0)),
            "sanity: a fresh session focuses the first row's name field"
        );

        // Edit `base_url` (row 0): move onto its value field, clear it and
        // type a new value.
        update(&mut state, Message::EditFocusNext);
        backspace_n(&mut state, "https://old.example.com".len());
        type_into_focused_field(&mut state, "https://new.example.com");

        // Delete `to_delete` (row 1): focus its name field, request then
        // confirm the delete.
        state.environment_edit.as_mut().unwrap().focus = Some(EnvVarField::Name(1));
        update(&mut state, Message::RequestDeleteEnvVarRow);
        update(&mut state, Message::ConfirmDeleteEnvVarRow);

        // Add a brand-new `token` variable.
        update(&mut state, Message::AddEnvVarRow);
        type_into_focused_field(&mut state, "token");
        update(&mut state, Message::EditFocusNext);
        type_into_focused_field(&mut state, "abc123");

        update(&mut state, Message::SaveEnvironmentEdit);
        assert!(state.environment_edit.is_none(), "the save must succeed");

        // The proof itself: a fresh `Environment::from_path`.
        let reloaded = Environment::from_path(&path).expect("the saved file must exist and parse");
        assert_eq!(reloaded.variables.len(), 2);
        assert_eq!(
            reloaded.variables.get("base_url").map(String::as_str),
            Some("https://new.example.com")
        );
        assert_eq!(
            reloaded.variables.get("token").map(String::as_str),
            Some("abc123")
        );
        assert!(!reloaded.variables.contains_key("to_delete"));

        // The in-memory `CollectionSession::environments` was updated too, not just
        // the file — the resolved preview reads from here, live.
        assert_eq!(
            state.environments[0].environment.variables,
            reloaded.variables
        );
    }

    /// The other half of the zero-variables investigation: deleting every
    /// variable and saving must succeed and produce a real, reloadable
    /// empty environment file — never an error, and never a document this
    /// crate refuses to write the way it refuses an empty `Collection`.
    #[test]
    fn deleting_every_variable_and_saving_persists_a_genuinely_empty_environment() {
        let (mut state, _dir, path) = state_with_saved_environment("staging", &[("only", "value")]);

        update(&mut state, Message::EnterEnvironmentEdit);
        state.environment_edit.as_mut().unwrap().focus = Some(EnvVarField::Name(0));
        update(&mut state, Message::RequestDeleteEnvVarRow);
        update(&mut state, Message::ConfirmDeleteEnvVarRow);
        assert_eq!(state.environment_edit.as_ref().unwrap().rows.len(), 0);

        update(&mut state, Message::SaveEnvironmentEdit);

        assert!(
            state.environment_edit.is_none(),
            "saving down to zero variables must succeed, not refuse"
        );
        let reloaded = Environment::from_path(&path).unwrap();
        assert!(reloaded.variables.is_empty());
    }

    #[test]
    fn saving_with_an_empty_variable_name_refuses_and_keeps_the_session_open() {
        let (mut state, _dir, path) = state_with_saved_environment("staging", &[("base_url", "x")]);
        update(&mut state, Message::EnterEnvironmentEdit);
        backspace_n(&mut state, "base_url".len());

        update(&mut state, Message::SaveEnvironmentEdit);

        assert!(
            state.environment_edit.is_some(),
            "a refused save must not discard the session"
        );
        let error = state
            .environment_edit
            .as_ref()
            .unwrap()
            .save_error
            .as_ref()
            .expect("an empty name must set a real error message");
        assert!(!error.is_empty());
        // Untouched on disk: still exactly the original file.
        let reloaded = Environment::from_path(&path).unwrap();
        assert_eq!(
            reloaded.variables.get("base_url").map(String::as_str),
            Some("x")
        );
    }

    #[test]
    fn saving_with_two_rows_sharing_a_name_refuses_and_keeps_the_session_open() {
        let (mut state, _dir, path) = state_with_saved_environment("staging", &[("base_url", "x")]);
        update(&mut state, Message::EnterEnvironmentEdit);
        update(&mut state, Message::AddEnvVarRow);
        type_into_focused_field(&mut state, "base_url");

        update(&mut state, Message::SaveEnvironmentEdit);

        assert!(
            state.environment_edit.is_some(),
            "a refused save must not discard the session"
        );
        assert!(state
            .environment_edit
            .as_ref()
            .unwrap()
            .save_error
            .is_some());
        let reloaded = Environment::from_path(&path).unwrap();
        assert_eq!(
            reloaded.variables.len(),
            1,
            "the original single-variable file must be untouched"
        );
    }

    #[test]
    fn cancelling_a_pending_row_delete_leaves_the_row_untouched() {
        let (mut state, _dir, _path) =
            state_with_saved_environment("staging", &[("base_url", "https://example.com")]);
        update(&mut state, Message::EnterEnvironmentEdit);
        state.environment_edit.as_mut().unwrap().focus = Some(EnvVarField::Name(0));

        update(&mut state, Message::RequestDeleteEnvVarRow);
        assert_eq!(
            state
                .environment_edit
                .as_ref()
                .unwrap()
                .pending_delete
                .as_ref()
                .map(|pending| pending.index),
            Some(0)
        );

        update(&mut state, Message::CancelDeleteEnvVarRow);

        let env_edit = state.environment_edit.as_ref().unwrap();
        assert_eq!(env_edit.pending_delete, None);
        assert_eq!(env_edit.rows.len(), 1, "the row must still be there");
        assert_eq!(env_edit.rows[0].key.value(), "base_url");
    }

    #[test]
    fn environment_edit_is_a_no_op_while_already_editing_a_request_or_run_in_flight() {
        // Real message flow already keeps `edit_mode` and `environment_
        // overlay` mutually exclusive two different ways (`OpenEnvironmentOverlay`
        // refuses while `edit_mode` is `Some`; `EnterEditMode` refuses while
        // the overlay is open) — so reaching "both at once" needs setting
        // `edit_mode` directly rather than through `Message::EnterEditMode`,
        // to prove `EnterEnvironmentEdit`'s own defense-in-depth check
        // actually does something rather than merely restating an invariant
        // enforced elsewhere.
        let mut editing = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut editing, Message::EnterEditMode);
        assert!(editing.edit_mode.is_some(), "sanity: edit mode is active");
        editing.environments = vec![named_environment("staging", &[])];
        editing.environment_overlay = Some(0);

        update(&mut editing, Message::EnterEnvironmentEdit);

        assert!(editing.environment_edit.is_none());

        let mut running = loaded_state(THREE_REQUEST_COLLECTION);
        running.environments = vec![named_environment("staging", &[])];
        running.environment_overlay = Some(0);
        update(&mut running, Message::RunRequested);
        update(&mut running, Message::EnterEnvironmentEdit);
        assert!(running.environment_edit.is_none());
    }

    /// The same exclusivity `navigation_and_edit_entry_are_blocked_while_
    /// the_delete_confirm_prompt_is_open` already proves for request
    /// deletion, checked for the environment-variable edit session's own
    /// guard: while it is open, the overlay's cursor must not move out from
    /// under it, and no other mode can be entered on top of it.
    #[test]
    fn navigation_and_mode_entry_are_blocked_while_editing_environment_variables() {
        let (mut state, _dir, _path) =
            state_with_saved_environment("staging", &[("base_url", "https://example.com")]);
        update(&mut state, Message::EnterEnvironmentEdit);
        assert!(state.environment_edit.is_some());

        update(&mut state, Message::SelectNext);
        assert_eq!(state.environment_overlay, Some(0), "cursor must not move");

        update(&mut state, Message::CloseEnvironmentOverlay);
        assert!(
            state.environment_overlay.is_some(),
            "overlay must stay open"
        );

        assert!(
            state.environment_edit.is_some(),
            "the edit session itself must still be open throughout"
        );
    }

    #[test]
    fn run_requested_moves_run_state_to_in_flight_when_a_request_is_selected() {
        let mut state = loaded_state(VALID_COLLECTION);

        update(&mut state, Message::RunRequested);

        assert!(matches!(state.run_state, RunState::InFlight));
    }

    #[test]
    fn run_requested_is_a_no_op_with_nothing_loaded() {
        let mut state = AppState::default();

        update(&mut state, Message::RunRequested);

        assert!(matches!(state.run_state, RunState::Idle));
    }

    #[test]
    fn run_completed_stores_the_real_response_in_run_state() {
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

        assert!(matches!(state.run_state, RunState::Completed));
        match state.current_run() {
            Some(RunOutcome {
                result: Ok(response),
                ..
            }) => assert_eq!(response.status, 200),
            other => panic!(
                "expected current_run() to be Some(RunOutcome {{ result: Ok(_), .. }}), got {other:?}"
            ),
        }
    }

    #[test]
    fn run_completed_stores_the_real_error_in_run_state() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        let error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");

        let collection_id = state.active().id;
        update(
            &mut state,
            Message::RunCompleted {
                collection_id,
                outcome: failed_outcome(error),
            },
        );

        assert!(matches!(state.run_state, RunState::Completed));
        assert!(matches!(
            state.current_run(),
            Some(RunOutcome { result: Err(_), .. })
        ));
    }

    #[test]
    fn navigation_and_a_second_run_are_blocked_while_a_run_is_in_flight() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        state.environments = ["default", "staging"]
            .into_iter()
            .map(|name| named_environment(name, &[]))
            .collect();
        update(&mut state, Message::RunRequested);
        assert!(matches!(state.run_state, RunState::InFlight));

        update(&mut state, Message::SelectNext);
        update(&mut state, Message::SelectPrevious);
        update(&mut state, Message::OpenEnvironmentOverlay);
        update(&mut state, Message::RunRequested);

        assert_eq!(
            selected(&state),
            0,
            "selection must not move while a run is in flight"
        );
        assert_eq!(
            state.environment_overlay, None,
            "the environment overlay must not open while a run is in flight"
        );
        assert!(
            matches!(state.run_state, RunState::InFlight),
            "a second RunRequested must not restart or otherwise disturb the in-flight run"
        );
    }

    #[test]
    fn run_completed_is_accepted_while_a_run_is_in_flight() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);

        let collection_id = state.active().id;
        update(
            &mut state,
            Message::RunCompleted {
                collection_id,
                outcome: sample_outcome(204),
            },
        );

        assert!(
            matches!(state.run_state, RunState::Completed)
                && matches!(state.current_run(), Some(RunOutcome { result: Ok(_), .. })),
            "RunCompleted must end the in-flight state even though it would \
             otherwise be blocked by it"
        );
    }

    #[test]
    fn navigation_works_again_once_a_run_has_completed() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::RunRequested);
        let collection_id = state.active().id;
        update(
            &mut state,
            Message::RunCompleted {
                collection_id,
                outcome: sample_outcome(200),
            },
        );

        update(&mut state, Message::SelectNext);

        assert_eq!(selected(&state), 1);
    }
}
