//! The reducer half of sendra-tui's Elm-style architecture: `update()`
//! itself, and the private helpers it alone calls. Every `CollectionSession`
//! mutation in the whole crate happens through this one function — see its
//! own doc comment and `super::state::CollectionSession::edit_mode`'s for why
//! that invariant matters.
//!
//! Split into submodules along the same lines the messages themselves
//! naturally group into: [`request_edit`] for a request's own edit session,
//! [`environment_edit`] for an environment's variables edit session, and
//! [`collection`] for opening/closing/loading collections (tabs). This file
//! keeps `update()`/`update_session()` themselves — the dispatcher and the
//! mode-exclusivity guard logic every message passes through first — plus
//! the handful of helpers (`select`, `push_history_entry`,
//! `reindex_dirty_after_delete`/`reindex_history_after_delete`,
//! `viewed_history_entry_mut`) that are genuinely about browsing/dispatch
//! itself rather than any one edit session.

use std::collections::{HashMap, HashSet};

use super::state::{
    AppState, CollectionSession, EditState, HistoryOverlay, LoadState, Message, RunHistoryEntry,
    RunState, TextField, RUN_HISTORY_CAP,
};

mod collection;
mod environment_edit;
mod request_edit;

/// Moves `selected` by `delta` (`1` or `-1`) through `len` items, wrapping at
/// both ends: past the last item goes to the first, and back past the first
/// goes to the last. A `len` of `0` leaves `selected` at `0`.
fn move_selection(selected: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let len = len as isize;
    let next = (selected as isize + delta).rem_euclid(len);
    next as usize
}

/// The real entry point — every message passes through here first.
///
/// Messages that are genuinely about the *process* (quitting, the shared
/// spinner clock, opening/switching/closing tabs, and routing a finished run
/// back to whichever collection it actually belongs to — see
/// `Message::RunCompleted`'s own doc comment) are handled directly against
/// `AppState::collections`/`active_collection` right here. Everything else —
/// the vast majority of messages, covering selection, running, editing a
/// request, the environment overlay, and every confirmation — is exactly the
/// single-collection logic this crate had before multi-collection support,
/// moved into `update_session` verbatim and handed the *active* session
/// (`AppState::active_mut`) to operate on. That split is the whole of how
/// tab isolation is enforced: a session-scoped message can only ever reach
/// `update_session` with the one `&mut CollectionSession` it's allowed to
/// touch, never `AppState` itself, so there is no field access anywhere in
/// that (large, unchanged) body of code that could reach a different tab by
/// accident.
///
/// Two modal, process-wide states — the open-collection prompt and a
/// pending tab-close confirmation — block every session-scoped message the
/// same way a session's own modals (edit mode, delete confirmation, ...)
/// already block browsing *within* that session: this is that same rule one
/// level up. `Quit`/`Tick` still get through regardless, for the same reason
/// they always do everywhere else in this crate.
pub fn update(state: &mut AppState, msg: Message) {
    match msg {
        // Checked before even `Quit`/`quit_confirm` below: the cheatsheet
        // can be summoned over literally anything else in this crate (see
        // `AppState::cheatsheet_open`'s own doc comment), and closing it
        // again must work no matter what, if anything, turns out to be
        // sitting underneath it.
        Message::OpenCheatsheet => {
            state.cheatsheet_open = true;
            return;
        }
        Message::CloseCheatsheet => {
            state.cheatsheet_open = false;
            return;
        }
        Message::Quit => {
            // Already confirming — the quit key pressed again (`q` or
            // Ctrl+C, whichever) confirms it, the same low-friction "ask
            // once, then get out of the way" shape `Message::ConfirmQuit`'s
            // own dedicated key gives. See `Message::Quit`'s own doc
            // comment.
            if state.quit_confirm.take().is_some() {
                state.should_quit = true;
                return;
            }
            let dirty_tabs = state.collections.iter().filter(|s| s.is_dirty()).count();
            if dirty_tabs == 0 {
                state.should_quit = true;
            } else {
                state.quit_confirm = Some(super::state::ConfirmPrompt::new(quit_confirm_message(
                    dirty_tabs,
                )));
            }
            return;
        }
        Message::ConfirmQuit => {
            if state.quit_confirm.take().is_some() {
                state.should_quit = true;
            }
            return;
        }
        Message::CancelQuit => {
            state.quit_confirm = None;
            return;
        }
        Message::Tick => {
            state.spinner_tick = state.spinner_tick.wrapping_add(1);
            return;
        }
        Message::OpenCollectionPrompt => {
            collection::handle_open_collection_prompt(state);
            return;
        }
        // `main::run`'s loop always intercepts this and replaces it with a
        // real `Message::CollectionOpened` before `update()` ever sees it —
        // see this variant's own doc comment. Reachable here only from a
        // test (or any other caller) that constructs it directly, where a
        // no-op is the correct, safe behavior.
        Message::ConfirmOpenCollectionPath => return,
        Message::CancelOpenCollectionPrompt => {
            state.open_collection_prompt = None;
            return;
        }
        Message::CollectionOpened {
            base_dir,
            path,
            result,
            environments,
            environment_errors,
        } => {
            collection::handle_collection_opened(
                state,
                base_dir,
                path,
                result,
                environments,
                environment_errors,
            );
            return;
        }
        Message::NextCollection => {
            collection::handle_next_collection(state);
            return;
        }
        Message::PreviousCollection => {
            collection::handle_previous_collection(state);
            return;
        }
        Message::CloseCollectionRequested => {
            collection::handle_close_collection_requested(state);
            return;
        }
        Message::ConfirmCloseCollection => {
            collection::handle_confirm_close_collection(state);
            return;
        }
        Message::CancelCloseCollection => {
            collection::handle_cancel_close_collection(state);
            return;
        }
        Message::RunCompleted {
            collection_id,
            outcome,
        } => {
            if let Some(session) = state.session_by_id_mut(collection_id) {
                push_history_entry(session, outcome);
                session.run_state = RunState::Completed;
            }
            return;
        }
        _ => {}
    }

    // The generic text-field messages (also shared by `edit_mode`/
    // `environment_edit` — see `environment_edit::env_var_mutate`'s own doc
    // comment for that precedent) route to the open-collection prompt's own
    // path field while it is open, rather than being blocked outright by the
    // "every session-scoped message is refused" check below: this *is* how
    // the prompt itself gets typed into. Any edit clears a stale error from
    // a previous failed attempt.
    if let Some(prompt) = &mut state.open_collection_prompt {
        match msg {
            Message::EditInsertChar(ch) => {
                prompt.path.insert_char(ch);
                prompt.error = None;
                return;
            }
            Message::EditBackspace => {
                prompt.path.backspace();
                prompt.error = None;
                return;
            }
            Message::EditDelete => {
                prompt.path.delete();
                prompt.error = None;
                return;
            }
            Message::EditCursorLeft => {
                prompt.path.move_left();
                return;
            }
            Message::EditCursorRight => {
                prompt.path.move_right();
                return;
            }
            _ => {}
        }
    }

    // Every other message is scoped to whichever collection is on screen —
    // refused here, exactly like a session's own modals refuse browsing
    // messages, while any of the three process-wide modals above is open.
    if state.open_collection_prompt.is_some()
        || state.close_confirm.is_some()
        || state.quit_confirm.is_some()
        || state.cheatsheet_open
    {
        return;
    }
    update_session(state.active_mut(), msg);
}

/// The question `Message::Quit`'s own arm asks once it finds at least one
/// dirty tab — `dirty_tabs` is how many of `AppState::collections` reported
/// `is_dirty()`, never zero.
fn quit_confirm_message(dirty_tabs: usize) -> String {
    format!(
        "Quit with unsaved changes in {dirty_tabs} tab{}? This cannot be undone.",
        if dirty_tabs == 1 { "" } else { "s" }
    )
}

/// Lines moved per `PageUp`/`PageDown` on the response panel. Not tied to
/// the pane's actual height — a fixed step that comfortably outruns typical
/// pane heights is simplest, rather than full viewport-aware paging.
const SCROLL_STEP_LINES: usize = 10;

/// The single-collection reducer this crate had before multi-collection
/// support — unchanged in every way that matters (same guards, same
/// messages, same behavior) except that it now takes the one
/// `&mut CollectionSession` it is allowed to touch directly, rather than
/// `&mut AppState`.
fn update_session(state: &mut CollectionSession, msg: Message) {
    // While a run is in flight, every navigation/overlay/run message is
    // refused outright.
    if matches!(state.run_state, RunState::InFlight)
        && matches!(
            msg,
            Message::SelectNext
                | Message::SelectPrevious
                | Message::OpenEnvironmentOverlay
                | Message::CloseEnvironmentOverlay
                | Message::ConfirmEnvironmentSelection
                | Message::OpenHistoryOverlay
                | Message::RunRequested
                | Message::EnterEditMode
                | Message::AddRequest
                | Message::RequestDelete
                | Message::EnterEnvironmentEdit
        )
    {
        return;
    }

    // While the selected request is being edited, browsing/overlay/run
    // messages are refused the same way the InFlight guard above refuses
    // them.
    if state.edit_mode.is_some()
        && matches!(
            msg,
            Message::SelectNext
                | Message::SelectPrevious
                | Message::OpenEnvironmentOverlay
                | Message::CloseEnvironmentOverlay
                | Message::ConfirmEnvironmentSelection
                | Message::OpenHistoryOverlay
                | Message::RunRequested
                | Message::RequestDelete
        )
    {
        return;
    }

    // While the delete confirmation prompt is open, browsing/overlay/run/
    // edit-entry messages are refused the same way edit mode's own guard
    // above refuses them.
    if state.delete_confirm.is_some()
        && matches!(
            msg,
            Message::SelectNext
                | Message::SelectPrevious
                | Message::OpenEnvironmentOverlay
                | Message::CloseEnvironmentOverlay
                | Message::ConfirmEnvironmentSelection
                | Message::OpenHistoryOverlay
                | Message::RunRequested
                | Message::EnterEditMode
                | Message::AddRequest
        )
    {
        return;
    }

    // While an environment's variables are being edited, browsing/overlay-
    // closing/run/edit-entry messages are refused the same way edit mode's
    // own guard above refuses them.
    if state.environment_edit.is_some()
        && matches!(
            msg,
            Message::SelectNext
                | Message::SelectPrevious
                | Message::CloseEnvironmentOverlay
                | Message::ConfirmEnvironmentSelection
                | Message::OpenHistoryOverlay
                | Message::RunRequested
                | Message::EnterEditMode
                | Message::AddRequest
                | Message::RequestDelete
        )
    {
        return;
    }

    // While the run-history browser is open, browsing/overlay/run/edit-entry
    // messages are refused the same way every other modal's own guard above
    // refuses them.
    if state.history_overlay.is_some()
        && matches!(
            msg,
            Message::OpenEnvironmentOverlay
                | Message::RunRequested
                | Message::EnterEditMode
                | Message::AddRequest
                | Message::RequestDelete
                | Message::EnterEnvironmentEdit
        )
    {
        return;
    }

    match msg {
        Message::NoCollectionPath => collection::handle_no_collection_path(state),
        Message::CollectionLoaded {
            base_dir,
            path,
            result,
        } => collection::handle_collection_loaded(state, base_dir, path, result),
        Message::EnvironmentsLoaded {
            environments,
            errors,
        } => collection::handle_environments_loaded(state, environments, errors),
        Message::SelectNext => select(state, 1),
        Message::SelectPrevious => select(state, -1),
        Message::OpenEnvironmentOverlay => {
            if state.environment_overlay.is_none() {
                state.environment_overlay = Some(state.active_environment.unwrap_or(0));
            }
        }
        Message::CloseEnvironmentOverlay => state.environment_overlay = None,
        Message::ConfirmEnvironmentSelection => {
            if let Some(cursor) = state.environment_overlay.take() {
                if !state.environments.is_empty() {
                    state.active_environment = Some(cursor);
                }
            }
        }
        Message::RunRequested => {
            // While the welcome screen's discovery picker has a candidate
            // highlighted, Enter/`r` open it instead — see
            // `collection::confirm_discovered_selection`'s own doc comment.
            // Falls through to the ordinary run/no-op handling below only
            // when that wasn't the case.
            if !collection::confirm_discovered_selection(state) {
                // A no-op with nothing loaded or nothing selected — there is
                // no request to run.
                if request_edit::request_is_selected(state) {
                    state.run_state = RunState::InFlight;
                    // A fresh run's text starts at the top, and its captures
                    // start masked again, regardless of a previous run's
                    // state.
                    state.response_scroll = 0;
                    state.reveal_captures = false;
                }
            }
        }
        // Scrolling/reveal route to whichever response panel is actually on
        // screen: the history overlay's own viewed entry while one is open,
        // the live run's `response_scroll`/`reveal_captures` otherwise.
        Message::ScrollResponseDown => match viewed_history_entry_mut(state) {
            Some(overlay) => {
                overlay.view_scroll = overlay.view_scroll.saturating_add(SCROLL_STEP_LINES);
            }
            None => {
                state.response_scroll = state.response_scroll.saturating_add(SCROLL_STEP_LINES);
            }
        },
        Message::ScrollResponseUp => match viewed_history_entry_mut(state) {
            Some(overlay) => {
                overlay.view_scroll = overlay.view_scroll.saturating_sub(SCROLL_STEP_LINES);
            }
            None => {
                state.response_scroll = state.response_scroll.saturating_sub(SCROLL_STEP_LINES);
            }
        },
        Message::ScrollResponseTop => match viewed_history_entry_mut(state) {
            Some(overlay) => overlay.view_scroll = 0,
            None => state.response_scroll = 0,
        },
        Message::ScrollResponseBottom => match viewed_history_entry_mut(state) {
            Some(overlay) => overlay.view_scroll = usize::MAX,
            None => state.response_scroll = usize::MAX,
        },
        Message::ToggleRevealCaptures => match viewed_history_entry_mut(state) {
            Some(overlay) => overlay.view_reveal_captures = !overlay.view_reveal_captures,
            None => state.reveal_captures = !state.reveal_captures,
        },
        Message::OpenHistoryOverlay => {
            if state.history_overlay.is_none() && state.environment_overlay.is_none() {
                state.history_overlay = Some(HistoryOverlay::default());
            }
        }
        Message::CloseHistoryOverlay => state.history_overlay = None,
        Message::ViewHistoryEntry => {
            let history_len = state.selected_history().len();
            if let Some(overlay) = &mut state.history_overlay {
                if overlay.viewing.is_none() && overlay.cursor < history_len {
                    overlay.viewing = Some(overlay.cursor);
                    overlay.view_scroll = 0;
                    overlay.view_reveal_captures = false;
                }
            }
        }
        Message::CloseHistoryEntryView => {
            if let Some(overlay) = &mut state.history_overlay {
                overlay.viewing = None;
            }
        }
        Message::ToggleHistoryEntryExpanded => {
            let history_len = state.selected_history().len();
            if let Some(overlay) = &mut state.history_overlay {
                if overlay.viewing.is_none() && overlay.cursor < history_len {
                    let cursor = overlay.cursor;
                    if !overlay.expanded.remove(&cursor) {
                        overlay.expanded.insert(cursor);
                    }
                }
            }
        }
        Message::AddRequest => request_edit::handle_add_request(state),
        Message::EnterEditMode => request_edit::handle_enter_edit_mode(state),
        Message::SaveEdit => request_edit::handle_save_edit(state),
        Message::CancelEdit => request_edit::handle_cancel_edit(state),
        Message::RequestDelete => request_edit::handle_request_delete(state),
        Message::CancelDelete => request_edit::handle_cancel_delete(state),
        Message::ConfirmDelete => request_edit::handle_confirm_delete(state),
        Message::EditFocusNext => {
            if let Some(edit) = &mut state.edit_mode {
                let has_body = edit.has_editable_body();
                edit.focus = edit.focus.next(
                    edit.headers.len(),
                    has_body,
                    edit.auth_field_order(),
                    edit.assertion_row_count(),
                    edit.capture_row_count(),
                );
            } else if let Some(env_edit) = &mut state.environment_edit {
                env_edit.focus_next();
            }
        }
        Message::EditFocusPrev => {
            if let Some(edit) = &mut state.edit_mode {
                let has_body = edit.has_editable_body();
                edit.focus = edit.focus.prev(
                    edit.headers.len(),
                    has_body,
                    edit.auth_field_order(),
                    edit.assertion_row_count(),
                    edit.capture_row_count(),
                );
            } else if let Some(env_edit) = &mut state.environment_edit {
                env_edit.focus_prev();
            }
        }
        Message::AddHeaderRow => request_edit::edit_state_mutate(state, EditState::add_header_row),
        Message::DeleteHeaderRow => {
            request_edit::edit_state_mutate(state, EditState::delete_focused_header_row)
        }
        Message::AddAssertionRow => {
            request_edit::edit_state_mutate(state, EditState::add_assertion_row)
        }
        Message::DeleteAssertionRow => {
            request_edit::edit_state_mutate(state, EditState::delete_focused_assertion_row)
        }
        Message::AddCaptureRow => {
            request_edit::edit_state_mutate(state, EditState::add_capture_row)
        }
        Message::DeleteCaptureRow => {
            request_edit::edit_state_mutate(state, EditState::delete_focused_capture_row)
        }
        // `EditInsertChar`/`EditBackspace`/`EditDelete`/`EditCursorLeft`/
        // `EditCursorRight` are the generic text-field messages both a
        // request edit session and an environment-variable edit session use
        // for their own focused field — the two are mutually exclusive, so
        // routing to whichever one is actually active, `edit_mode` first,
        // covers both without a second set of messages just for environment
        // variables. `EditCursorUp`/`EditCursorDown` stay request-only:
        // every environment-variable field is single-line.
        Message::EditInsertChar(ch) => {
            if state.edit_mode.is_some() {
                request_edit::edit_mutate(state, |field| field.insert_char(ch));
            } else {
                environment_edit::env_var_mutate(state, |field| field.insert_char(ch));
            }
        }
        Message::EditBackspace => {
            if state.edit_mode.is_some() {
                request_edit::edit_mutate(state, TextField::backspace);
            } else {
                environment_edit::env_var_mutate(state, TextField::backspace);
            }
        }
        Message::EditDelete => {
            if state.edit_mode.is_some() {
                request_edit::edit_mutate(state, TextField::delete);
            } else {
                environment_edit::env_var_mutate(state, TextField::delete);
            }
        }
        Message::EditCursorLeft => {
            if state.edit_mode.is_some() {
                request_edit::edit_move_or_toggle(state, false, TextField::move_left);
            } else {
                environment_edit::env_var_move(state, TextField::move_left);
            }
        }
        Message::EditCursorRight => {
            if state.edit_mode.is_some() {
                request_edit::edit_move_or_toggle(state, true, TextField::move_right);
            } else {
                environment_edit::env_var_move(state, TextField::move_right);
            }
        }
        Message::EditCursorUp => request_edit::edit_move(state, TextField::move_up),
        Message::EditCursorDown => request_edit::edit_move(state, TextField::move_down),
        Message::EnterEnvironmentEdit => environment_edit::handle_enter_environment_edit(state),
        Message::CancelEnvironmentEdit => environment_edit::handle_cancel_environment_edit(state),
        Message::SaveEnvironmentEdit => environment_edit::handle_save_environment_edit(state),
        Message::AddEnvVarRow => environment_edit::handle_add_env_var_row(state),
        Message::RequestDeleteEnvVarRow => {
            environment_edit::handle_request_delete_env_var_row(state)
        }
        Message::ConfirmDeleteEnvVarRow => {
            environment_edit::handle_confirm_delete_env_var_row(state)
        }
        Message::CancelDeleteEnvVarRow => environment_edit::handle_cancel_delete_env_var_row(state),
        // See the doc comment on `Message::Resize` — the redraw itself
        // comes from `terminal.draw` re-running `view` against the
        // already-resized backend on the loop's next iteration; there is no
        // state here for a resize to change.
        Message::Resize => {}
        // Every process-wide message — quitting, the shared spinner clock,
        // opening/switching/closing tabs, and routing a finished run back by
        // collection id — is handled by `update()` itself, which `return`s
        // before ever calling this function for one of them. This function
        // still has to be exhaustive over the whole `Message` enum, so this
        // one arm covers everything that structurally cannot reach here in
        // real operation.
        Message::Quit
        | Message::ConfirmQuit
        | Message::CancelQuit
        | Message::Tick
        | Message::OpenCollectionPrompt
        | Message::ConfirmOpenCollectionPath
        | Message::CancelOpenCollectionPrompt
        | Message::CollectionOpened { .. }
        | Message::NextCollection
        | Message::PreviousCollection
        | Message::CloseCollectionRequested
        | Message::ConfirmCloseCollection
        | Message::CancelCloseCollection
        | Message::RunCompleted { .. }
        | Message::OpenCheatsheet
        | Message::CloseCheatsheet => {}
    }
}

/// `dirty_requests` with every index shifted to still name the same request
/// after deleting the one at `deleted_index` — indices before it are
/// untouched, `deleted_index` itself is dropped, and every index after it
/// moves down by one to follow the request it pointed at through the
/// removal.
pub(crate) fn reindex_dirty_after_delete(
    dirty: &HashSet<usize>,
    deleted_index: usize,
) -> HashSet<usize> {
    dirty
        .iter()
        .filter_map(|&index| match index.cmp(&deleted_index) {
            std::cmp::Ordering::Less => Some(index),
            std::cmp::Ordering::Equal => None,
            std::cmp::Ordering::Greater => Some(index - 1),
        })
        .collect()
}

/// `run_history` (or `run_history_dropped`) with every key shifted to still
/// name the same request after deleting the one at `deleted_index` — the
/// exact same reindexing `reindex_dirty_after_delete` already does for
/// `dirty_requests`, generic over the map's value type so it covers both
/// `HashMap<usize, Vec<RunHistoryEntry>>` and `HashMap<usize, usize>` without
/// duplicating the index arithmetic.
pub(crate) fn reindex_history_after_delete<V>(
    history: HashMap<usize, V>,
    deleted_index: usize,
) -> HashMap<usize, V> {
    history
        .into_iter()
        .filter_map(|(index, value)| match index.cmp(&deleted_index) {
            std::cmp::Ordering::Less => Some((index, value)),
            std::cmp::Ordering::Equal => None,
            std::cmp::Ordering::Greater => Some((index - 1, value)),
        })
        .collect()
}

/// Records `outcome` as the selected request's newest history entry, then
/// trims that request's history back down to `RUN_HISTORY_CAP` if it grew
/// past it — the one place `Message::RunCompleted` writes into
/// `CollectionSession::run_history`. Every entry the trim drops is counted
/// into `run_history_dropped` for that same request, so the overlay can say
/// "N older runs were dropped" instead of the cap silently discarding data.
fn push_history_entry(session: &mut CollectionSession, outcome: crate::run_request::RunOutcome) {
    let LoadState::Loaded { selected, .. } = &session.load_state else {
        return;
    };
    let selected = *selected;
    let entries = session.run_history.entry(selected).or_default();
    entries.insert(
        0,
        RunHistoryEntry {
            completed_at: std::time::SystemTime::now(),
            outcome,
        },
    );
    if entries.len() > RUN_HISTORY_CAP {
        let dropped = entries.len() - RUN_HISTORY_CAP;
        entries.truncate(RUN_HISTORY_CAP);
        *session.run_history_dropped.entry(selected).or_default() += dropped;
    }
}

/// `state.history_overlay`'s own `view_scroll`/`view_reveal_captures`, but
/// only while it is actually showing one entry's full result
/// (`HistoryOverlay::viewing` is `Some`).
fn viewed_history_entry_mut(state: &mut CollectionSession) -> Option<&mut HistoryOverlay> {
    state
        .history_overlay
        .as_mut()
        .filter(|overlay| overlay.viewing.is_some())
}

/// Routes `SelectNext`/`SelectPrevious` to the overlay's cursor when it is
/// open, otherwise to the collection browser's selection. The history
/// browser's own list and the welcome screen's discovery picker are two more
/// such lists.
fn select(state: &mut CollectionSession, delta: isize) {
    if let Some(cursor) = &mut state.environment_overlay {
        *cursor = move_selection(*cursor, state.environments.len(), delta);
        return;
    }

    let history_len = state.selected_history().len();
    if let Some(overlay) = &mut state.history_overlay {
        if overlay.viewing.is_none() {
            overlay.cursor = move_selection(overlay.cursor, history_len, delta);
        }
        return;
    }

    if let LoadState::NoPathProvided { discovered, cursor } = &mut state.load_state {
        if !discovered.is_empty() {
            *cursor = move_selection(*cursor, discovered.len(), delta);
        }
        return;
    }

    if let LoadState::Loaded {
        document, selected, ..
    } = &mut state.load_state
    {
        let next = move_selection(*selected, document.requests().len(), delta);
        if next != *selected {
            state.run_state = RunState::Idle;
            state.response_scroll = 0;
            state.reveal_captures = false;
        }
        *selected = next;
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use sendra_core::{Document, Method};

    use crate::app::state::EditField;
    use crate::app::test_support::*;
    use crate::run_request::RunOutcome;

    use super::*;

    #[test]
    fn quit_message_sets_should_quit() {
        let mut state = AppState::default();
        assert!(!state.should_quit);

        update(&mut state, Message::Quit);

        assert!(state.should_quit);
    }

    #[test]
    fn tick_message_leaves_state_unchanged() {
        let mut state = AppState::default();

        update(&mut state, Message::Tick);

        assert!(!state.should_quit);
    }

    #[test]
    fn collection_loaded_ok_stores_document_with_selection_at_zero() {
        let mut state = AppState::default();
        let document = Document::from_yaml_str(VALID_COLLECTION).expect("valid test YAML");

        update(
            &mut state,
            Message::CollectionLoaded {
                base_dir: PathBuf::from("."),
                path: PathBuf::from("collection.yaml"),
                result: Box::new(Ok(document)),
            },
        );

        match &state.load_state {
            LoadState::Loaded {
                document, selected, ..
            } => {
                assert_eq!(document.requests().len(), 2);
                assert_eq!(*selected, 0);
            }
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        }
    }

    #[test]
    fn no_collection_path_message_sets_no_path_provided() {
        let mut state = AppState::default();

        update(&mut state, Message::NoCollectionPath);

        assert!(matches!(state.load_state, LoadState::NoPathProvided { .. }));
    }

    #[test]
    fn collection_loaded_err_stores_error() {
        let mut state = AppState::default();
        let error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");

        update(
            &mut state,
            Message::CollectionLoaded {
                base_dir: PathBuf::from("."),
                path: PathBuf::from("collection.yaml"),
                result: Box::new(Err(error)),
            },
        );

        assert!(matches!(state.load_state, LoadState::Failed(_)));
    }

    #[test]
    fn select_next_advances_selection() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);

        update(&mut state, Message::SelectNext);

        assert_eq!(selected(&state), 1);
    }

    #[test]
    fn select_previous_moves_selection_back() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::SelectNext);
        update(&mut state, Message::SelectNext);

        update(&mut state, Message::SelectPrevious);

        assert_eq!(selected(&state), 1);
    }

    #[test]
    fn select_next_wraps_from_last_to_first() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::SelectNext);
        update(&mut state, Message::SelectNext);
        assert_eq!(selected(&state), 2);

        update(&mut state, Message::SelectNext);

        assert_eq!(selected(&state), 0);
    }

    #[test]
    fn select_previous_wraps_from_first_to_last() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        assert_eq!(selected(&state), 0);

        update(&mut state, Message::SelectPrevious);

        assert_eq!(selected(&state), 2);
    }

    #[test]
    fn selection_messages_are_ignored_without_a_loaded_collection() {
        let mut state = AppState::default();

        update(&mut state, Message::SelectNext);
        update(&mut state, Message::SelectPrevious);

        assert!(matches!(state.load_state, LoadState::Loading));
    }

    #[test]
    fn environments_loaded_message_populates_state() {
        let mut state = AppState::default();

        update(
            &mut state,
            Message::EnvironmentsLoaded {
                environments: vec![named_environment("default", &[])],
                errors: Vec::new(),
            },
        );

        assert_eq!(state.environments.len(), 1);
        assert_eq!(state.environments[0].name, "default");
    }

    #[test]
    fn open_overlay_starts_cursor_at_active_environment() {
        let mut state = state_with_environments(&["default", "staging"]);
        state.active_environment = Some(1);

        update(&mut state, Message::OpenEnvironmentOverlay);

        assert_eq!(state.environment_overlay, Some(1));
    }

    #[test]
    fn overlay_navigation_moves_cursor_not_collection_selection() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        state.environments = ["default", "staging", "prod"]
            .into_iter()
            .map(|name| named_environment(name, &[]))
            .collect();
        update(&mut state, Message::OpenEnvironmentOverlay);

        update(&mut state, Message::SelectNext);

        assert_eq!(state.environment_overlay, Some(1));
        assert_eq!(selected(&state), 0, "collection selection must not move");
    }

    #[test]
    fn confirm_sets_active_environment_and_closes_overlay() {
        let mut state = state_with_environments(&["default", "staging"]);
        update(&mut state, Message::OpenEnvironmentOverlay);
        update(&mut state, Message::SelectNext);

        update(&mut state, Message::ConfirmEnvironmentSelection);

        assert_eq!(state.active_environment, Some(1));
        assert_eq!(state.environment_overlay, None);
    }

    #[test]
    fn cancel_closes_overlay_without_changing_active_environment() {
        let mut state = state_with_environments(&["default", "staging"]);
        state.active_environment = Some(0);
        update(&mut state, Message::OpenEnvironmentOverlay);
        update(&mut state, Message::SelectNext);
        assert_eq!(state.environment_overlay, Some(1));

        update(&mut state, Message::CloseEnvironmentOverlay);

        assert_eq!(state.environment_overlay, None);
        assert_eq!(
            state.active_environment,
            Some(0),
            "cancel must leave the previously active environment untouched"
        );
    }

    // --- run history: recording, browsing, the cap, isolation, reindexing --

    /// `RunRequested` then `RunCompleted(sample_outcome(status))` against
    /// whichever tab is active — the same two-message shape every other run
    /// test in this file already drives `update()` through, pulled out here
    /// only because the history tests below do it many times in a row with a
    /// different `status` each time.
    fn run_to_completion(state: &mut AppState, status: u16) {
        update(state, Message::RunRequested);
        let collection_id = state.active().id;
        update(
            state,
            Message::RunCompleted {
                collection_id,
                outcome: sample_outcome(status),
            },
        );
    }

    #[test]
    fn running_the_same_request_repeatedly_records_each_run_newest_first() {
        let mut state = loaded_state(VALID_COLLECTION);

        run_to_completion(&mut state, 200);
        run_to_completion(&mut state, 201);
        run_to_completion(&mut state, 202);

        let history = state.selected_history();
        assert_eq!(history.len(), 3, "one entry per run");
        let statuses: Vec<u16> = history
            .iter()
            .map(|entry| match &entry.outcome.result {
                Ok(response) => response.status,
                Err(_) => panic!("every sample_outcome here is Ok"),
            })
            .collect();
        assert_eq!(
            statuses,
            vec![202, 201, 200],
            "most recent run must be first, oldest last"
        );

        // `current_run()`/`RunState::Completed` must agree with history[0] —
        // see `CollectionSession::current_run`'s own doc comment on why
        // there is only ever this one copy of a run's data.
        assert_eq!(
            state.current_run().unwrap().result.as_ref().unwrap().status,
            202
        );
    }

    /// Running past `RUN_HISTORY_CAP` drops the oldest entry rather than
    /// growing without bound — see `RUN_HISTORY_CAP`'s own doc comment:
    /// generous for what browsing needs, bounded against unbounded response
    /// bodies accumulating over a long session.
    #[test]
    fn history_past_the_cap_drops_the_oldest_entry() {
        let mut state = loaded_state(VALID_COLLECTION);

        // Statuses 0..=RUN_HISTORY_CAP, i.e. RUN_HISTORY_CAP + 1 runs —
        // exactly one past the cap.
        for status in 0..=(RUN_HISTORY_CAP as u16) {
            run_to_completion(&mut state, status);
        }

        let history = state.selected_history();
        assert_eq!(
            history.len(),
            RUN_HISTORY_CAP,
            "history must never grow past the documented cap"
        );
        let newest_status = match &history[0].outcome.result {
            Ok(response) => response.status,
            Err(_) => panic!("sample_outcome is always Ok"),
        };
        assert_eq!(
            newest_status, RUN_HISTORY_CAP as u16,
            "the newest run (the last one sent) must still be entry 0"
        );
        let oldest_status = match &history[history.len() - 1].outcome.result {
            Ok(response) => response.status,
            Err(_) => panic!("sample_outcome is always Ok"),
        };
        assert_eq!(
            oldest_status, 1,
            "run 0 (the very first, now the oldest past the cap) must have been \
             dropped to make room, leaving run 1 as the oldest surviving entry"
        );
        assert_eq!(
            state.selected_history_dropped(),
            1,
            "exactly one entry (run 0) was evicted by the trim, and that must be \
             counted rather than discarded silently"
        );
    }

    /// A second run past the cap evicts a second entry — `run_history_dropped`
    /// accumulates across trims rather than only ever recording the first one.
    #[test]
    fn history_dropped_count_accumulates_across_multiple_trims() {
        let mut state = loaded_state(VALID_COLLECTION);

        for status in 0..=(RUN_HISTORY_CAP as u16) + 1 {
            run_to_completion(&mut state, status);
        }

        assert_eq!(
            state.selected_history_dropped(),
            2,
            "two runs (0 and 1) have now been evicted by the trim"
        );
    }

    /// `Message::ToggleHistoryEntryExpanded` toggles the entry at the
    /// overlay's own `cursor` in place, independent of which entry is
    /// selected before or after — the inline glance `render_history_overlay`
    /// shows without switching into `ViewHistoryEntry`'s full response panel.
    #[test]
    fn toggle_history_entry_expanded_toggles_the_entry_at_cursor() {
        let mut state = loaded_state(VALID_COLLECTION);
        run_to_completion(&mut state, 200);
        run_to_completion(&mut state, 500);
        update(&mut state, Message::OpenHistoryOverlay);

        update(&mut state, Message::ToggleHistoryEntryExpanded);
        assert!(state
            .history_overlay
            .as_ref()
            .unwrap()
            .expanded
            .contains(&0));

        update(&mut state, Message::SelectNext);
        update(&mut state, Message::ToggleHistoryEntryExpanded);
        assert!(state
            .history_overlay
            .as_ref()
            .unwrap()
            .expanded
            .contains(&1));
        assert!(
            state
                .history_overlay
                .as_ref()
                .unwrap()
                .expanded
                .contains(&0),
            "expanding a second row must not collapse the first"
        );

        update(&mut state, Message::ToggleHistoryEntryExpanded);
        assert!(
            !state
                .history_overlay
                .as_ref()
                .unwrap()
                .expanded
                .contains(&1),
            "toggling an already-expanded row collapses it"
        );
    }

    #[test]
    fn toggle_history_entry_expanded_is_a_no_op_while_viewing_an_entry() {
        let mut state = loaded_state(VALID_COLLECTION);
        run_to_completion(&mut state, 200);
        update(&mut state, Message::OpenHistoryOverlay);
        update(&mut state, Message::ViewHistoryEntry);

        update(&mut state, Message::ToggleHistoryEntryExpanded);

        assert!(state.history_overlay.as_ref().unwrap().expanded.is_empty());
    }

    #[test]
    fn opening_history_overlay_starts_the_cursor_at_the_most_recent_run() {
        let mut state = loaded_state(VALID_COLLECTION);
        run_to_completion(&mut state, 200);
        run_to_completion(&mut state, 500);

        update(&mut state, Message::OpenHistoryOverlay);

        let overlay = state.history_overlay.as_ref().expect("just opened");
        assert_eq!(overlay.cursor, 0);
        assert_eq!(overlay.viewing, None);
    }

    #[test]
    fn history_overlay_opens_even_with_no_runs_yet() {
        let mut state = loaded_state(VALID_COLLECTION);

        update(&mut state, Message::OpenHistoryOverlay);

        assert!(
            state.history_overlay.is_some(),
            "the overlay itself is what tells the user there's nothing yet, \
             not a refusal to open it at all"
        );
        assert!(state.selected_history().is_empty());
    }

    #[test]
    fn selecting_and_viewing_a_past_entry_shows_that_entrys_own_outcome() {
        let mut state = loaded_state(VALID_COLLECTION);
        run_to_completion(&mut state, 200);
        run_to_completion(&mut state, 404);

        update(&mut state, Message::OpenHistoryOverlay);
        // cursor starts at 0 (the 404 run, most recent) — move to 1 (the 200
        // run) with the overlay's own SelectNext, then view it.
        update(&mut state, Message::SelectNext);
        assert_eq!(state.history_overlay.as_ref().unwrap().cursor, 1);

        update(&mut state, Message::ViewHistoryEntry);

        let overlay = state.history_overlay.as_ref().unwrap();
        assert_eq!(overlay.viewing, Some(1));
        let viewed = &state.selected_history()[1];
        match &viewed.outcome.result {
            Ok(response) => assert_eq!(response.status, 200),
            Err(_) => panic!("expected the older, 200 run"),
        }
    }

    #[test]
    fn viewing_a_history_entry_scrolls_and_reveals_independently_of_the_live_run() {
        let mut state = loaded_state(VALID_COLLECTION);
        run_to_completion(&mut state, 200);
        run_to_completion(&mut state, 200);
        update(&mut state, Message::ScrollResponseDown);
        update(&mut state, Message::ToggleRevealCaptures);
        assert_eq!(state.response_scroll, SCROLL_STEP_LINES);
        assert!(state.reveal_captures);

        update(&mut state, Message::OpenHistoryOverlay);
        update(&mut state, Message::ViewHistoryEntry);
        update(&mut state, Message::ScrollResponseDown);
        update(&mut state, Message::ToggleRevealCaptures);

        let overlay = state.history_overlay.as_ref().unwrap();
        assert_eq!(overlay.view_scroll, SCROLL_STEP_LINES);
        assert!(overlay.view_reveal_captures);
        // The live run's own fields, still sitting under the overlay, must
        // be completely untouched by scrolling/revealing the viewed entry.
        assert_eq!(state.response_scroll, SCROLL_STEP_LINES);
        assert!(state.reveal_captures);
    }

    #[test]
    fn esc_backs_out_of_viewing_then_closes_the_overlay() {
        let mut state = loaded_state(VALID_COLLECTION);
        run_to_completion(&mut state, 200);

        update(&mut state, Message::OpenHistoryOverlay);
        update(&mut state, Message::ViewHistoryEntry);
        assert!(state.history_overlay.as_ref().unwrap().viewing.is_some());

        update(&mut state, Message::CloseHistoryEntryView);
        assert!(
            state.history_overlay.is_some(),
            "closing the viewed entry goes back to the list, not out of the overlay entirely"
        );
        assert_eq!(state.history_overlay.as_ref().unwrap().viewing, None);

        update(&mut state, Message::CloseHistoryOverlay);
        assert!(state.history_overlay.is_none());
    }

    /// Proof requirement: history in one tab must be completely unaffected
    /// by runs in another — the same isolation standard
    /// `each_tab_keeps_independent_selection_run_state_and_edit_mode_with_no_bleed_through`
    /// already proves for selection/`run_state`/edit mode.
    #[test]
    fn history_in_one_tab_does_not_bleed_into_another() {
        let mut state = loaded_state(VALID_COLLECTION); // tab 0
        run_to_completion(&mut state, 200);
        run_to_completion(&mut state, 200);
        assert_eq!(state.selected_history().len(), 2);

        open_second_collection(&mut state, VALID_COLLECTION); // tab 1
        assert!(
            state.selected_history().is_empty(),
            "a freshly opened tab must start with no history of its own"
        );
        run_to_completion(&mut state, 500);
        assert_eq!(state.selected_history().len(), 1);

        update(&mut state, Message::PreviousCollection);
        assert_eq!(
            state.selected_history().len(),
            2,
            "tab 0's own history must be exactly as it was left"
        );

        update(&mut state, Message::NextCollection);
        assert_eq!(state.selected_history().len(), 1);
    }

    /// Proof requirement: deleting a request reindexes history exactly like
    /// `dirty_requests` — see `dirty_requests_are_reindexed_after_a_delete`,
    /// the model this mirrors.
    #[test]
    fn history_is_reindexed_after_a_delete() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION); // "One"/"Two"/"Three"
        update(&mut state, Message::SelectNext); // "Two" (index 1)
        run_to_completion(&mut state, 200);
        update(&mut state, Message::SelectNext); // "Three" (index 2)
        run_to_completion(&mut state, 404);
        update(&mut state, Message::SelectNext); // wraps back to "One" (index 0)

        // Delete "One" (index 0) — "Two"'s history must follow it down from
        // index 1 to index 0, and "Three"'s from index 2 to index 1.
        update(&mut state, Message::RequestDelete);
        update(&mut state, Message::ConfirmDelete);

        // After deleting index 0, the old index-1 request ("Two") is now at
        // index 0, and the old index-2 request ("Three") is now at index 1.
        assert_eq!(
            state.run_history.get(&0).map(Vec::len),
            Some(1),
            "\"Two\"'s history must have followed it down to index 0"
        );
        assert_eq!(
            state.run_history.get(&1).map(Vec::len),
            Some(1),
            "\"Three\"'s history must have followed it down to index 1"
        );
        assert!(
            !state.run_history.contains_key(&2),
            "no request is left at the old highest index"
        );
    }

    #[test]
    fn history_overlay_is_a_no_op_while_editing_or_run_in_flight() {
        let mut state = loaded_state(VALID_COLLECTION);
        run_to_completion(&mut state, 200);

        update(&mut state, Message::EnterEditMode);
        update(&mut state, Message::OpenHistoryOverlay);
        assert!(
            state.history_overlay.is_none(),
            "must not open while editing"
        );
        update(&mut state, Message::CancelEdit);

        update(&mut state, Message::RunRequested);
        update(&mut state, Message::OpenHistoryOverlay);
        assert!(
            state.history_overlay.is_none(),
            "must not open while a run is in flight"
        );
    }

    #[test]
    fn enter_edit_mode_is_a_no_op_without_a_selected_request() {
        let mut state = AppState::default();

        update(&mut state, Message::EnterEditMode);

        assert!(state.edit_mode.is_none());
    }

    #[test]
    fn enter_edit_mode_seeds_the_working_copy_from_the_real_request() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);

        update(&mut state, Message::EnterEditMode);

        let edit = state.edit_mode.as_ref().expect("edit mode just entered");
        assert!(
            !edit.dirty,
            "entering edit mode alone must not mark the edit dirty"
        );
        assert_eq!(edit.method.value(), "GET");
        assert_eq!(edit.url.value(), "https://example.com");
        assert_eq!(edit.focus, EditField::Name);
        assert_eq!(
            edit.method_error, None,
            "the request's own method is always valid"
        );
        assert!(
            state.dirty_requests.is_empty(),
            "entering edit mode alone must not mark anything dirty"
        );
    }

    #[test]
    fn entering_and_cancelling_edit_mode_with_no_changes_is_a_no_op() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        let before_document = match &state.load_state {
            LoadState::Loaded { document, .. } => (**document).clone(),
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        };

        update(&mut state, Message::EnterEditMode);
        update(&mut state, Message::CancelEdit);

        assert!(state.edit_mode.is_none());
        assert!(state.dirty_requests.is_empty());
        match &state.load_state {
            LoadState::Loaded {
                document, selected, ..
            } => {
                assert_eq!(**document, before_document);
                assert_eq!(*selected, 0);
            }
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        }
    }

    #[test]
    fn cancel_edit_round_trips_state_exactly_after_editing_method_and_url() {
        // Full round-trip proof: enter edit mode, actually change both real
        // fields through the same messages a keystroke sends, cancel, and
        // check every piece of state an edit could plausibly have touched
        // — not just `edit_mode` (trivially `None` again either way) but
        // the dirty bookkeeping and the request's real `method`/`url`
        // inside the loaded document — is back to exactly what it was
        // before `EnterEditMode`.
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        let before_document = match &state.load_state {
            LoadState::Loaded { document, .. } => (**document).clone(),
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        };
        let before_selected = selected(&state);
        let before_active_environment = state.active_environment;
        let before_environment_overlay = state.environment_overlay;
        let before_response_scroll = state.response_scroll;
        let before_reveal_captures = state.reveal_captures;

        update(&mut state, Message::EnterEditMode);
        assert!(state.edit_mode.is_some());

        backspace_n(&mut state, "GET".len());
        type_into_focused_field(&mut state, "DELETE");
        update(&mut state, Message::EditFocusNext);
        type_into_focused_field(&mut state, "/changed");
        assert!(
            state.edit_mode.as_ref().expect("still editing").dirty,
            "a real edit must have marked the working copy dirty"
        );
        assert!(state.dirty_requests.contains(&before_selected));

        update(&mut state, Message::CancelEdit);

        assert_eq!(
            state.edit_mode, None,
            "cancel must leave edit mode entirely"
        );
        assert!(
            state.dirty_requests.is_empty(),
            "cancel must clear the dirty marker for the request that was being edited"
        );
        match &state.load_state {
            LoadState::Loaded {
                document, selected, ..
            } => {
                assert_eq!(
                    **document, before_document,
                    "cancel must not leave the loaded document's method/url changed, \
                     even though real edits were typed before cancelling"
                );
                assert_eq!(*selected, before_selected);
            }
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        }
        assert_eq!(state.active_environment, before_active_environment);
        assert_eq!(state.environment_overlay, before_environment_overlay);
        assert_eq!(state.response_scroll, before_response_scroll);
        assert_eq!(state.reveal_captures, before_reveal_captures);
    }

    #[test]
    fn save_edit_writes_the_new_method_and_url_into_the_loaded_document() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::EnterEditMode);
        update(&mut state, Message::EditFocusNext); // -> Method

        backspace_n(&mut state, "GET".len());
        type_into_focused_field(&mut state, "POST");
        update(&mut state, Message::EditFocusNext);
        backspace_n(&mut state, "https://example.com".len());
        type_into_focused_field(&mut state, "https://example.com/updated");

        update(&mut state, Message::SaveEdit);

        assert!(state.edit_mode.is_none(), "save must leave edit mode");
        assert!(
            state.dirty_requests.is_empty(),
            "save must clear the dirty marker for the request just saved"
        );
        // Re-inspect the request the same way re-selecting it in the
        // collection browser would read it — proving the new values are
        // really in `CollectionSession`'s in-memory request, not only in the
        // now-discarded `EditState`.
        let request = match &state.load_state {
            LoadState::Loaded {
                document, selected, ..
            } => &document.requests()[*selected],
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        };
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.url, "https://example.com/updated");
    }

    #[test]
    fn cannot_enter_edit_mode_while_the_environment_overlay_is_open() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::OpenEnvironmentOverlay);
        assert!(state.environment_overlay.is_some());

        update(&mut state, Message::EnterEditMode);

        assert!(
            state.edit_mode.is_none(),
            "edit mode must not open on top of the environment overlay"
        );
    }

    #[test]
    fn cannot_open_the_environment_overlay_while_editing() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::EnterEditMode);
        assert!(state.edit_mode.is_some());

        update(&mut state, Message::OpenEnvironmentOverlay);

        assert!(
            state.environment_overlay.is_none(),
            "the environment overlay must not open while editing"
        );
    }

    #[test]
    fn cannot_enter_edit_mode_while_a_run_is_in_flight() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::RunRequested);
        assert!(matches!(state.run_state, RunState::InFlight));

        update(&mut state, Message::EnterEditMode);

        assert!(
            state.edit_mode.is_none(),
            "edit mode must not open while a run is in flight"
        );
    }

    #[test]
    fn navigation_run_and_overlay_are_blocked_while_editing() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        state.environments = ["default", "staging"]
            .into_iter()
            .map(|name| named_environment(name, &[]))
            .collect();
        update(&mut state, Message::EnterEditMode);
        assert!(state.edit_mode.is_some());

        update(&mut state, Message::SelectNext);
        update(&mut state, Message::SelectPrevious);
        update(&mut state, Message::OpenEnvironmentOverlay);
        update(&mut state, Message::RunRequested);

        assert_eq!(selected(&state), 0, "selection must not move while editing");
        assert_eq!(state.environment_overlay, None);
        assert!(
            state.edit_mode.is_some(),
            "editing must still be active — none of the blocked messages should have ended it"
        );
        assert!(matches!(state.run_state, RunState::Idle));
    }

    #[test]
    fn quitting_while_a_run_is_in_flight_prompts_then_exits_on_confirm() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);

        update(&mut state, Message::Quit);
        assert!(
            !state.should_quit,
            "an in-flight run's result would be lost — this must prompt, not exit silently"
        );
        assert!(state.quit_confirm.is_some());

        update(&mut state, Message::ConfirmQuit);
        assert!(state.should_quit);
    }

    #[test]
    fn cancelling_the_quit_prompt_leaves_the_in_flight_run_untouched() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);

        update(&mut state, Message::Quit);
        update(&mut state, Message::CancelQuit);

        assert!(!state.should_quit);
        assert!(state.quit_confirm.is_none());
        assert!(
            matches!(state.run_state, RunState::InFlight),
            "cancelling quit must leave the in-flight run exactly as it was"
        );
    }

    #[test]
    fn pressing_the_quit_key_again_while_confirming_exits_immediately() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);

        update(&mut state, Message::Quit);
        assert!(state.quit_confirm.is_some());

        // The quit key pressed a second time (`q` or Ctrl+C, either one —
        // `main::translate_event` maps both to this same `Message::Quit`)
        // confirms rather than reopening the same prompt, the low-friction
        // "ask once" behavior `Message::Quit`'s own doc comment describes.
        update(&mut state, Message::Quit);

        assert!(state.should_quit);
    }

    #[test]
    fn quitting_with_nothing_unsaved_anywhere_exits_immediately_with_no_prompt() {
        let mut state = loaded_state(VALID_COLLECTION);

        update(&mut state, Message::Quit);

        assert!(state.should_quit);
        assert!(state.quit_confirm.is_none());
    }

    #[test]
    fn selecting_a_different_request_resets_run_state_and_scroll() {
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
        update(&mut state, Message::ScrollResponseDown);
        assert!(matches!(state.run_state, RunState::Completed));
        assert!(matches!(
            state.current_run(),
            Some(RunOutcome { result: Ok(_), .. })
        ));
        assert_eq!(state.response_scroll, SCROLL_STEP_LINES);

        update(&mut state, Message::SelectNext);

        assert!(
            matches!(state.run_state, RunState::Idle),
            "a previous run's result must not be attributed to the newly selected request"
        );
        assert_eq!(state.response_scroll, 0);
    }

    #[test]
    fn selecting_the_same_request_again_does_not_reset_run_state() {
        // A collection of one: `SelectNext`/`SelectPrevious` always land back
        // on the same request, so nothing about the result should be
        // disturbed — only an actual change of selection resets it.
        let mut state = loaded_state(VALID_COLLECTION);
        // Trim to a single request so every `SelectNext` is a no-op move.
        if let LoadState::Loaded { document, .. } = &mut state.load_state {
            let mut trimmed = Document::from_yaml_str(
                "name: test\nrequests:\n  - name: One\n    method: GET\n    url: https://example.com\n",
            )
            .unwrap();
            std::mem::swap(document.as_mut(), &mut trimmed);
        }
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

        assert!(matches!(state.run_state, RunState::Completed));
        assert!(matches!(
            state.current_run(),
            Some(RunOutcome { result: Ok(_), .. })
        ));
    }

    #[test]
    fn starting_a_new_run_resets_the_previous_scroll_position() {
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
        update(&mut state, Message::ScrollResponseDown);
        assert_eq!(state.response_scroll, SCROLL_STEP_LINES);

        update(&mut state, Message::RunRequested);

        assert_eq!(state.response_scroll, 0);
    }

    #[test]
    fn scroll_down_then_up_returns_to_the_top() {
        let mut state = AppState::default();

        update(&mut state, Message::ScrollResponseDown);
        assert_eq!(state.response_scroll, SCROLL_STEP_LINES);

        update(&mut state, Message::ScrollResponseUp);
        assert_eq!(state.response_scroll, 0);
    }

    #[test]
    fn scroll_up_from_the_top_saturates_at_zero() {
        let mut state = AppState::default();

        update(&mut state, Message::ScrollResponseUp);

        assert_eq!(state.response_scroll, 0);
    }

    #[test]
    fn scroll_top_jumps_straight_to_zero_from_anywhere() {
        let mut state = AppState::default();
        state.response_scroll = 5_000;

        update(&mut state, Message::ScrollResponseTop);

        assert_eq!(state.response_scroll, 0);
    }

    #[test]
    fn scroll_bottom_sets_scroll_past_any_real_content_for_render_time_clamping() {
        let mut state = AppState::default();

        update(&mut state, Message::ScrollResponseBottom);

        assert_eq!(
            state.response_scroll,
            usize::MAX,
            "End hands off the real clamp to render_response_panel, the same as any \
             other over-scroll"
        );
    }

    #[test]
    fn toggle_reveal_captures_flips_and_flips_back() {
        let mut state = AppState::default();
        assert!(!state.reveal_captures);

        update(&mut state, Message::ToggleRevealCaptures);
        assert!(state.reveal_captures);

        update(&mut state, Message::ToggleRevealCaptures);
        assert!(!state.reveal_captures);
    }

    #[test]
    fn reveal_captures_is_not_carried_over_to_the_next_run() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        update(&mut state, Message::ToggleRevealCaptures);
        assert!(state.reveal_captures);
        let collection_id = state.active().id;
        update(
            &mut state,
            Message::RunCompleted {
                collection_id,
                outcome: sample_outcome(200),
            },
        );
        assert!(
            state.reveal_captures,
            "revealing must survive until the run it was revealed for is replaced"
        );

        // A second run on the same request — proof this is not "persisted",
        // just held for the run that was on screen when it was revealed.
        update(&mut state, Message::RunRequested);

        assert!(
            !state.reveal_captures,
            "a fresh run must start with captures masked again, never auto-revealed"
        );
    }

    #[test]
    fn reveal_captures_resets_when_the_selection_changes() {
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
        update(&mut state, Message::ToggleRevealCaptures);
        assert!(state.reveal_captures);

        update(&mut state, Message::SelectNext);

        assert!(
            !state.reveal_captures,
            "moving to a different request must not carry a reveal over to it"
        );
    }

    #[test]
    fn environments_loaded_message_stores_errors_alongside_environments() {
        let mut state = AppState::default();
        let bad_error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");

        update(
            &mut state,
            Message::EnvironmentsLoaded {
                environments: vec![named_environment("default", &[])],
                errors: vec![("broken".to_string(), bad_error)],
            },
        );

        assert_eq!(state.environments.len(), 1);
        assert_eq!(state.environment_errors.len(), 1);
        assert_eq!(state.environment_errors[0].0, "broken");
    }

    /// A failed collection load must not be a dead end: quitting and
    /// opening the environment overlay — the two keys `status_help_text`
    /// advertises for this context — must still actually work.
    #[test]
    fn app_stays_responsive_after_a_collection_load_failure() {
        let mut state = AppState::default();
        let error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");
        update(
            &mut state,
            Message::CollectionLoaded {
                base_dir: PathBuf::from("."),
                path: PathBuf::from("collection.yaml"),
                result: Box::new(Err(error)),
            },
        );
        assert!(matches!(state.load_state, LoadState::Failed(_)));

        state.environments = vec![named_environment("default", &[])];
        update(&mut state, Message::OpenEnvironmentOverlay);
        assert_eq!(
            state.environment_overlay,
            Some(0),
            "the environment overlay must still open after a failed collection load"
        );

        update(&mut state, Message::Quit);
        assert!(
            state.should_quit,
            "quit must still work after a failed collection load"
        );
    }

    /// The same guarantee, for a run that itself failed (an unreachable
    /// host, say): navigation and re-running must both still work
    /// afterward, not just quit.
    #[test]
    fn app_stays_responsive_after_a_failed_run() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
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

        update(&mut state, Message::SelectNext);
        assert_eq!(
            selected(&state),
            1,
            "selection must still move after a failed run"
        );

        update(&mut state, Message::RunRequested);
        assert!(
            matches!(state.run_state, RunState::InFlight),
            "a request must still be runnable again after a previous run failed"
        );

        // The freshly-restarted run is itself now in flight and so counts as
        // something quitting would lose (`CollectionSession::is_dirty()`) —
        // quit must still *work*, just through the confirmation instead of
        // silently discarding it.
        update(&mut state, Message::Quit);
        assert!(!state.should_quit);
        update(&mut state, Message::ConfirmQuit);
        assert!(
            state.should_quit,
            "quit must still work (via its confirmation) after a failed run"
        );
    }
}
