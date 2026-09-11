//! The reducer half of sendra-tui's Elm-style architecture: `update()`
//! itself, and the private helpers it alone calls. Every `AppState`
//! mutation in the whole crate happens through this one function — see its
//! own doc comment and `super::state::AppState::edit_mode`'s for why that
//! invariant matters.

use sendra_core::{Document, Request};

use super::state::{
    validate_method_text, AppState, BodyEdit, EditField, EditState, LoadState, Message, RunState,
    TextField,
};

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

pub fn update(state: &mut AppState, msg: Message) {
    // While a run is in flight, every navigation/overlay/run message is
    // refused outright — the simplest correct behavior, and the one that
    // avoids the concurrent-state edge cases a second in-flight run, or a
    // selection change out from under one, would open up. `Quit`, `Tick` and
    // `RunCompleted` are exempt: quitting and the clock keep working
    // regardless, and `RunCompleted` is exactly the message that ends this
    // state, so blocking it would make the block permanent.
    if matches!(state.run_state, RunState::InFlight)
        && matches!(
            msg,
            Message::SelectNext
                | Message::SelectPrevious
                | Message::OpenEnvironmentOverlay
                | Message::CloseEnvironmentOverlay
                | Message::ConfirmEnvironmentSelection
                | Message::RunRequested
                | Message::EnterEditMode
        )
    {
        return;
    }

    // While the selected request is being edited, browsing/overlay/run
    // messages are refused the same way the InFlight guard above refuses
    // them — edit mode is exclusive with every other mode, not a state
    // layered on top of ordinary browsing (see the doc comment on
    // `AppState::edit_mode`). `SaveEdit`/`CancelEdit` are exempt: they are
    // exactly the messages that end this state, the same reason
    // `RunCompleted` is exempt from the InFlight guard. `Quit`/`Tick` keep
    // working for the same reason they always do.
    if state.edit_mode.is_some()
        && matches!(
            msg,
            Message::SelectNext
                | Message::SelectPrevious
                | Message::OpenEnvironmentOverlay
                | Message::CloseEnvironmentOverlay
                | Message::ConfirmEnvironmentSelection
                | Message::RunRequested
        )
    {
        return;
    }

    match msg {
        Message::Quit => state.should_quit = true,
        Message::Tick => state.spinner_tick = state.spinner_tick.wrapping_add(1),
        Message::NoCollectionPath => state.load_state = LoadState::NoPathProvided,
        Message::CollectionLoaded { base_dir, result } => {
            state.load_state = match *result {
                Ok(document) => LoadState::Loaded {
                    document: Box::new(document),
                    selected: 0,
                    base_dir,
                },
                Err(error) => LoadState::Failed(error),
            };
        }
        Message::EnvironmentsLoaded {
            environments,
            errors,
        } => {
            state.environments = environments;
            state.environment_errors = errors;
        }
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
            // A no-op with nothing loaded or nothing selected — there is no
            // request to run. `main` reads this same condition (a request
            // actually being selected) before deciding to spawn anything, so
            // the two checks have to agree: this one is what lets `main`
            // trust that "`run_state` became `InFlight`" means "there really
            // is a selected request to send".
            if request_is_selected(state) {
                state.run_state = RunState::InFlight;
                // A fresh run's text starts at the top, regardless of where
                // a previous run's was left scrolled, and its captures start
                // masked again regardless of whether the previous run's were
                // revealed — "never auto-revealed on the next run" applies
                // here, not only at startup.
                state.response_scroll = 0;
                state.reveal_captures = false;
            }
        }
        Message::RunCompleted(outcome) => {
            state.run_state = RunState::Completed(outcome);
        }
        Message::ScrollResponseDown => {
            state.response_scroll = state.response_scroll.saturating_add(SCROLL_STEP_LINES);
        }
        Message::ScrollResponseUp => {
            state.response_scroll = state.response_scroll.saturating_sub(SCROLL_STEP_LINES);
        }
        Message::ScrollResponseTop => state.response_scroll = 0,
        // See the doc comment on this variant: the real clamp happens in
        // `render_response_panel`, against whatever area is current at
        // render time, the same way an over-scroll from any other source
        // already does.
        Message::ScrollResponseBottom => state.response_scroll = usize::MAX,
        Message::ToggleRevealCaptures => state.reveal_captures = !state.reveal_captures,
        Message::EnterEditMode => {
            // Guarded here, not just left to the block above: the block
            // above only refuses messages while edit mode is *already*
            // active, so entering it needs its own check against the
            // overlay and InFlight — the two states edit mode is exclusive
            // with (see `AppState::edit_mode`'s doc comment).
            // `selected_request` both confirms there is something
            // to act on (the same check `RunRequested` uses) and hands over
            // the real `method`/`url` `EditState::new` seeds the working
            // copy from.
            if state.edit_mode.is_none()
                && state.environment_overlay.is_none()
                && !matches!(state.run_state, RunState::InFlight)
            {
                if let Some(request) = selected_request(state) {
                    state.edit_mode = Some(EditState::new(request));
                }
            }
        }
        Message::SaveEdit => {
            // Body validation happens exactly here, once per save attempt —
            // see `EditState::body_error`'s doc comment for why it is never
            // computed on every keystroke the way `method_error` is.
            if let Some(edit) = &mut state.edit_mode {
                edit.body_error = validate_body_for_save(&edit.body);
            }
            // Refuses to save — but, deliberately, does *not* clear
            // `edit_mode` — while `method_error` or `body_error` is set: an
            // invalid method or an invalid JSON body must never reach the
            // loaded document, but the user must not be locked out of
            // fixing either one, which dropping `edit_mode` here (as
            // `CancelEdit` does) would do by discarding what they typed.
            // Leaving edit mode active with the same bad text still in
            // place is what keeps the field "still editable afterward"
            // rather than stuck.
            let can_save = state
                .edit_mode
                .as_ref()
                .is_some_and(|edit| edit.method_error.is_none() && edit.body_error.is_none());
            if can_save {
                if let Some(edit) = state.edit_mode.take() {
                    if let LoadState::Loaded {
                        document, selected, ..
                    } = &mut state.load_state
                    {
                        if let Some(request) = request_mut(document, *selected) {
                            // `method_error` was already confirmed `None`
                            // above, so this cannot fail — matched rather
                            // than trusted blindly, so a bug in that
                            // invariant leaves the request's method
                            // untouched instead of panicking.
                            if let Ok(method) = validate_method_text(edit.method.value()) {
                                request.method = method;
                            }
                            request.url = edit.url.value().to_string();
                            request.headers = edit
                                .headers
                                .iter()
                                .map(|row| {
                                    (row.key.value().to_string(), row.value.value().to_string())
                                })
                                .collect();
                            apply_body_edit(request, &edit.body);
                        }
                        state.dirty_requests.remove(selected);
                    }
                }
            }
        }
        Message::CancelEdit => {
            // Dropping `edit_mode` here is the entire mechanism: `method`
            // and `url` only ever lived in that working copy (see
            // `EditState`'s own doc comment), never written into the loaded
            // document until `SaveEdit`, so there is nothing else to undo —
            // no snapshot to restore, because nothing real was ever changed.
            if state.edit_mode.take().is_some() {
                if let LoadState::Loaded { selected, .. } = &state.load_state {
                    state.dirty_requests.remove(selected);
                }
            }
        }
        Message::EditFocusNext => {
            if let Some(edit) = &mut state.edit_mode {
                let has_body = edit.has_editable_body();
                edit.focus = edit.focus.next(edit.headers.len(), has_body);
            }
        }
        Message::EditFocusPrev => {
            if let Some(edit) = &mut state.edit_mode {
                let has_body = edit.has_editable_body();
                edit.focus = edit.focus.prev(edit.headers.len(), has_body);
            }
        }
        Message::AddHeaderRow => edit_state_mutate(state, EditState::add_header_row),
        Message::DeleteHeaderRow => edit_state_mutate(state, EditState::delete_focused_header_row),
        Message::EditInsertChar(ch) => edit_mutate(state, |field| field.insert_char(ch)),
        Message::EditBackspace => edit_mutate(state, TextField::backspace),
        Message::EditDelete => edit_mutate(state, TextField::delete),
        Message::EditCursorLeft => edit_move(state, TextField::move_left),
        Message::EditCursorRight => edit_move(state, TextField::move_right),
        Message::EditCursorUp => edit_move(state, TextField::move_up),
        Message::EditCursorDown => edit_move(state, TextField::move_down),
        // See the doc comment on `Message::Resize` — the redraw itself
        // comes from `terminal.draw` re-running `view` against the
        // already-resized backend on the loop's next iteration; there is no
        // state here for a resize to change.
        Message::Resize => {}
    }
}

/// Lines moved per `PageUp`/`PageDown` on the response panel. Not tied to
/// the pane's actual height — a fixed step that comfortably outruns typical
/// pane heights is simplest, rather than full viewport-aware paging.
const SCROLL_STEP_LINES: usize = 10;

/// Whether the collection browser currently has a request selected — true
/// exactly when `load_state` is `Loaded` and `selected` indexes a real
/// request, which is always the case for a non-empty collection but not for
/// an empty one.
fn request_is_selected(state: &AppState) -> bool {
    matches!(
        &state.load_state,
        LoadState::Loaded { document, selected, .. } if document.requests().get(*selected).is_some()
    )
}

/// The currently selected request itself, when there is one — the same
/// condition `request_is_selected` checks, but handing back the `&Request`
/// `Message::EnterEditMode` needs to seed `EditState::new` from, instead of
/// just the bool.
fn selected_request(state: &AppState) -> Option<&Request> {
    match &state.load_state {
        LoadState::Loaded {
            document, selected, ..
        } => document.requests().get(*selected),
        _ => None,
    }
}

/// A mutable handle to the request at `index` in `document` — what
/// `Message::SaveEdit` writes the edited `method`/`url` into.
///
/// `sendra_core::Document`/`Collection` expose no `requests_mut()` method,
/// but every field involved (`Document`'s variants, `Collection.requests`)
/// is already `pub`, so matching on the variant and indexing its `Vec`
/// directly uses sendra-core's existing public surface rather than adding
/// a new public API to sendra-core just for this.
fn request_mut(document: &mut Document, index: usize) -> Option<&mut Request> {
    match document {
        Document::Single(request) => (index == 0).then_some(request),
        Document::Collection(collection) => collection.requests.get_mut(index),
    }
}

/// The one place body text is actually checked against JSON's grammar —
/// called only from `Message::SaveEdit`, never on every keystroke (see
/// `EditState::body_error`'s own doc comment for why). `None` covers three
/// cases that are all "nothing to reject": the body isn't in `json:` mode at
/// all (`is_json: false`), it's `Unsupported` (`body_file`/`form`/
/// `multipart` were never offered a text area to type invalid JSON into),
/// or the text is empty/blank — treated as "no body" by `apply_body_edit`
/// below, not as the empty string failing to parse as JSON (which it would,
/// since `""` is not valid JSON on its own).
fn validate_body_for_save(body: &BodyEdit) -> Option<String> {
    let BodyEdit::Editable {
        text,
        is_json: true,
    } = body
    else {
        return None;
    };
    if text.value().trim().is_empty() {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(text.value())
        .err()
        .map(|error| format!("Body is not valid JSON: {error}"))
}

/// Writes `body` back into `request`'s real `body`/`json` fields — the body
/// half of what `Message::SaveEdit` does for `method`/`url`/`headers`.
/// `Unsupported` bodies (`body_file`/`form`/`multipart`) are left completely
/// untouched: `EditState`/`BodyEdit` never held a working copy of them to
/// begin with (see `BodyEdit`'s own doc comment), so there is nothing here
/// to write back, the same way `CancelEdit` never had anything to undo for
/// `method`/`url`/`headers`.
fn apply_body_edit(request: &mut Request, body: &BodyEdit) {
    let BodyEdit::Editable { text, is_json } = body else {
        return;
    };
    if text.value().trim().is_empty() {
        request.body = None;
        request.json = None;
        return;
    }
    if *is_json {
        // `Message::SaveEdit` already ran this same text through
        // `validate_body_for_save` and refused to reach this call at all on
        // `Err` — matched rather than trusted blindly, so a bug in that
        // invariant leaves the request's body untouched instead of
        // panicking or saving something that never actually parsed.
        if let Ok(value) = serde_json::from_str(text.value()) {
            request.json = Some(value);
            request.body = None;
        }
    } else {
        request.body = Some(text.value().to_string());
        request.json = None;
    }
}

/// Applies `mutate` to whichever field `EditState::focus` currently points
/// at, then marks the edit (and the request being edited) dirty and, if the
/// method field is the one that just changed, recomputes `method_error` —
/// live validation on every keystroke. A no-op when edit mode
/// is not active, so every `Message::EditInsertChar`/`EditBackspace`/
/// `EditDelete` arm can call this unconditionally rather than each
/// re-checking `state.edit_mode.is_some()` itself.
fn edit_mutate(state: &mut AppState, mutate: impl FnOnce(&mut TextField)) {
    let Some(edit) = &mut state.edit_mode else {
        return;
    };
    mutate(edit.focused_field_mut());
    edit.dirty = true;
    if edit.focus == EditField::Method {
        edit.method_error = validate_method_text(edit.method.value()).err();
    }
    // `body_error` is deliberately not recomputed here the way `method_error`
    // just was — see its own doc comment for why body validation waits for
    // `Message::SaveEdit` — but a stale error from a previous failed save
    // must not keep showing once the user has started fixing it, so editing
    // the body clears it immediately rather than leaving it to look current
    // until the next save attempt.
    if edit.focus == EditField::Body {
        edit.body_error = None;
    }
    if let LoadState::Loaded { selected, .. } = &state.load_state {
        state.dirty_requests.insert(*selected);
    }
}

/// Like `edit_mutate`, but for a mutation to `EditState` itself rather than
/// to whichever `TextField` is focused — what `Message::AddHeaderRow`/
/// `DeleteHeaderRow` use, since adding or removing a whole row is still an
/// edit (marks `dirty`/`dirty_requests`) but isn't a `TextField` operation.
/// Unlike `edit_mutate`, this never touches `method_error`: neither
/// operation can change what `method` says.
fn edit_state_mutate(state: &mut AppState, mutate: impl FnOnce(&mut EditState)) {
    let Some(edit) = &mut state.edit_mode else {
        return;
    };
    mutate(edit);
    edit.dirty = true;
    if let LoadState::Loaded { selected, .. } = &state.load_state {
        state.dirty_requests.insert(*selected);
    }
}

/// Like `edit_mutate`, but for cursor movement: moving the cursor is not an
/// edit, so unlike `edit_mutate` this never sets `dirty` or touches
/// `dirty_requests`/`method_error`.
fn edit_move(state: &mut AppState, mutate: impl FnOnce(&mut TextField)) {
    if let Some(edit) = &mut state.edit_mode {
        mutate(edit.focused_field_mut());
    }
}

/// Routes `SelectNext`/`SelectPrevious` to the overlay's cursor when it is
/// open, otherwise to the collection browser's selection — see the doc
/// comment on those `Message` variants for why one pair serves both lists.
fn select(state: &mut AppState, delta: isize) {
    if let Some(cursor) = &mut state.environment_overlay {
        *cursor = move_selection(*cursor, state.environments.len(), delta);
        return;
    }

    if let LoadState::Loaded {
        document, selected, ..
    } = &mut state.load_state
    {
        let next = move_selection(*selected, document.requests().len(), delta);
        if next != *selected {
            // A different request's preview/response is about to show, so a
            // previous run's result — and its scroll position — belong to a
            // request no longer on screen. See the doc comment on
            // `RunState` for why this is what keeps `Completed` always
            // meaning "the currently selected request's result".
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

    use sendra_core::Method;

    use super::super::test_support::*;
    use super::super::view::status_help_text;
    use super::*;
    use crate::run_request::RunOutcome;

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
                result: Box::new(Ok(document)),
            },
        );

        match state.load_state {
            LoadState::Loaded {
                document, selected, ..
            } => {
                assert_eq!(document.requests().len(), 2);
                assert_eq!(selected, 0);
            }
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        }
    }

    #[test]
    fn no_collection_path_message_sets_no_path_provided() {
        let mut state = AppState::default();

        update(&mut state, Message::NoCollectionPath);

        assert!(matches!(state.load_state, LoadState::NoPathProvided));
    }

    #[test]
    fn collection_loaded_err_stores_error() {
        let mut state = AppState::default();
        let error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");

        update(
            &mut state,
            Message::CollectionLoaded {
                base_dir: PathBuf::from("."),
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

        update(&mut state, Message::RunCompleted(sample_outcome(200)));

        match state.run_state {
            RunState::Completed(RunOutcome {
                result: Ok(response),
                ..
            }) => assert_eq!(response.status, 200),
            other => panic!(
                "expected RunState::Completed(RunOutcome {{ result: Ok(_), .. }}), got {other:?}"
            ),
        }
    }

    #[test]
    fn run_completed_stores_the_real_error_in_run_state() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        let error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");

        update(&mut state, Message::RunCompleted(failed_outcome(error)));

        assert!(matches!(
            state.run_state,
            RunState::Completed(RunOutcome { result: Err(_), .. })
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

        update(&mut state, Message::RunCompleted(sample_outcome(204)));

        assert!(
            matches!(
                state.run_state,
                RunState::Completed(RunOutcome { result: Ok(_), .. })
            ),
            "RunCompleted must end the in-flight state even though it would \
             otherwise be blocked by it"
        );
    }

    #[test]
    fn navigation_works_again_once_a_run_has_completed() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::RunRequested);
        update(&mut state, Message::RunCompleted(sample_outcome(200)));

        update(&mut state, Message::SelectNext);

        assert_eq!(selected(&state), 1);
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
        assert_eq!(edit.focus, EditField::Method);
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
        // really in `AppState`'s in-memory request, not only in the
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
    fn invalid_method_shows_an_inline_error_blocks_save_and_stays_editable() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::EnterEditMode);

        backspace_n(&mut state, "GET".len());
        type_into_focused_field(&mut state, "FOOBAR");

        let edit = state.edit_mode.as_ref().expect("still editing");
        assert_eq!(edit.method.value(), "FOOBAR");
        assert!(
            edit.method_error.is_some(),
            "'FOOBAR' is not a real sendra_core::Method and must be flagged"
        );
        assert!(status_help_text(&state).contains("fix method to save"));

        // Attempting to save an invalid method must not crash, discard the
        // edit, or silently write a bogus method into the request — it is a
        // no-op that leaves the field exactly as typed.
        update(&mut state, Message::SaveEdit);
        assert!(
            state.edit_mode.is_some(),
            "SaveEdit must refuse to leave edit mode while the method is invalid"
        );
        assert_eq!(
            state.edit_mode.as_ref().unwrap().method.value(),
            "FOOBAR",
            "the invalid text must still be there — refusing to save must not clear it"
        );
        let unchanged_request = match &state.load_state {
            LoadState::Loaded {
                document, selected, ..
            } => &document.requests()[*selected],
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        };
        assert_eq!(
            unchanged_request.method,
            Method::Get,
            "the loaded document's real method must be untouched by a refused save"
        );

        // The field must still be editable afterward — not stuck — proven
        // by fixing it and saving again successfully.
        backspace_n(&mut state, "FOOBAR".len());
        type_into_focused_field(&mut state, "PUT");
        assert_eq!(
            state
                .edit_mode
                .as_ref()
                .expect("still editing")
                .method_error,
            None,
            "PUT is a real method again"
        );

        update(&mut state, Message::SaveEdit);
        assert!(
            state.edit_mode.is_none(),
            "save must now succeed with a valid method"
        );
        let saved_request = match &state.load_state {
            LoadState::Loaded {
                document, selected, ..
            } => &document.requests()[*selected],
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        };
        assert_eq!(saved_request.method, Method::Put);
    }

    #[test]
    fn method_validation_is_case_insensitive() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::EnterEditMode);

        backspace_n(&mut state, "GET".len());
        type_into_focused_field(&mut state, "post");

        assert_eq!(
            state
                .edit_mode
                .as_ref()
                .expect("still editing")
                .method_error,
            None,
            "lowercase 'post' must validate the same as 'POST'"
        );
    }

    #[test]
    fn dirty_marker_appears_in_the_collection_browser_and_disappears_on_cancel() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::EnterEditMode);
        type_into_focused_field(&mut state, "X");

        assert!(
            state.dirty_requests.contains(&0),
            "the request being edited (index 0) must be marked dirty"
        );
        assert!(
            status_help_text(&state).contains("unsaved changes"),
            "the help bar must surface the dirty edit"
        );

        update(&mut state, Message::CancelEdit);

        assert!(
            !state.dirty_requests.contains(&0),
            "cancel must remove the dirty marker"
        );
        assert!(!status_help_text(&state).contains("unsaved changes"));
    }

    // --- Header editing ----------------------------------------------------

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
    fn enter_edit_mode_seeds_header_rows_from_the_real_request() {
        let mut state = loaded_state(REQUEST_WITH_HEADERS);

        update(&mut state, Message::EnterEditMode);

        let edit = state.edit_mode.as_ref().expect("edit mode just entered");
        assert_eq!(edit.headers.len(), 2);
        assert_eq!(edit.headers[0].key.value(), "Accept");
        assert_eq!(edit.headers[0].value.value(), "application/json");
        assert_eq!(edit.headers[1].key.value(), "X-Env");
        assert_eq!(edit.headers[1].value.value(), "staging");
    }

    #[test]
    fn tab_moves_focus_from_url_into_the_first_header_rows_key_and_value() {
        let mut state = loaded_state(REQUEST_WITH_HEADERS);
        update(&mut state, Message::EnterEditMode);
        update(&mut state, Message::EditFocusNext); // Method -> Url

        update(&mut state, Message::EditFocusNext); // Url -> HeaderKey(0)
        assert_eq!(
            state.edit_mode.as_ref().unwrap().focus,
            EditField::HeaderKey(0)
        );

        update(&mut state, Message::EditFocusNext); // HeaderKey(0) -> HeaderValue(0)
        assert_eq!(
            state.edit_mode.as_ref().unwrap().focus,
            EditField::HeaderValue(0)
        );
    }

    #[test]
    fn shift_tab_moves_focus_backward_through_the_same_fields() {
        let mut state = loaded_state(REQUEST_WITH_HEADERS);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::HeaderKey(1);

        update(&mut state, Message::EditFocusPrev);
        assert_eq!(
            state.edit_mode.as_ref().unwrap().focus,
            EditField::HeaderValue(0)
        );

        update(&mut state, Message::EditFocusPrev);
        assert_eq!(
            state.edit_mode.as_ref().unwrap().focus,
            EditField::HeaderKey(0)
        );

        update(&mut state, Message::EditFocusPrev);
        assert_eq!(state.edit_mode.as_ref().unwrap().focus, EditField::Url);
    }

    #[test]
    fn tab_moves_from_the_last_header_values_field_into_the_body_field() {
        // `REQUEST_WITH_HEADERS` has no `body`/`json`/`body_file`/`form`/
        // `multipart` set, which `BodyEdit::new` treats as an editable
        // (empty) body — so `Body` is next in the focus cycle after the
        // last header row, not a wrap straight back to `Method`. See
        // `tab_wraps_from_the_body_field_back_to_method` for that wrap.
        let mut state = loaded_state(REQUEST_WITH_HEADERS);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::HeaderValue(1);

        update(&mut state, Message::EditFocusNext);

        assert_eq!(state.edit_mode.as_ref().unwrap().focus, EditField::Body);
    }

    #[test]
    fn tab_wraps_from_the_body_field_back_to_method() {
        let mut state = loaded_state(REQUEST_WITH_HEADERS);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::Body;

        update(&mut state, Message::EditFocusNext);

        assert_eq!(state.edit_mode.as_ref().unwrap().focus, EditField::Method);
    }

    #[test]
    fn typing_into_a_focused_header_field_edits_the_working_copy_and_marks_dirty() {
        let mut state = loaded_state(REQUEST_WITH_HEADERS);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::HeaderValue(0);
        backspace_n(&mut state, "application/json".len());

        type_into_focused_field(&mut state, "text/plain");

        let edit = state.edit_mode.as_ref().unwrap();
        assert_eq!(edit.headers[0].value.value(), "text/plain");
        assert!(edit.dirty);
        assert!(state.dirty_requests.contains(&0));
    }

    #[test]
    fn add_header_row_appends_a_new_row_and_focuses_its_key() {
        let mut state = loaded_state(REQUEST_WITH_HEADERS);
        update(&mut state, Message::EnterEditMode);

        update(&mut state, Message::AddHeaderRow);

        let edit = state.edit_mode.as_ref().unwrap();
        assert_eq!(edit.headers.len(), 3);
        assert_eq!(edit.headers[2].key.value(), "");
        assert_eq!(edit.focus, EditField::HeaderKey(2));
        assert!(edit.dirty);
    }

    #[test]
    fn delete_header_row_removes_it_and_shifts_focus_to_the_previous_row() {
        let mut state = loaded_state(REQUEST_WITH_HEADERS);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::HeaderValue(1); // X-Env

        update(&mut state, Message::DeleteHeaderRow);

        let edit = state.edit_mode.as_ref().unwrap();
        assert_eq!(edit.headers.len(), 1);
        assert_eq!(edit.headers[0].key.value(), "Accept");
        assert_eq!(edit.focus, EditField::HeaderKey(0));
        assert!(edit.dirty);
    }

    #[test]
    fn delete_header_row_is_a_no_op_while_focus_is_on_method_or_url() {
        let mut state = loaded_state(REQUEST_WITH_HEADERS);
        update(&mut state, Message::EnterEditMode);
        assert_eq!(state.edit_mode.as_ref().unwrap().focus, EditField::Method);

        update(&mut state, Message::DeleteHeaderRow);

        assert_eq!(
            state.edit_mode.as_ref().unwrap().headers.len(),
            2,
            "nothing should be deleted while focus is on method"
        );
    }

    #[test]
    fn save_edit_writes_added_edited_and_deleted_headers_into_the_loaded_document() {
        let mut state = loaded_state(REQUEST_WITH_HEADERS);
        update(&mut state, Message::EnterEditMode);

        // Delete "Accept" (index 0).
        state.edit_mode.as_mut().unwrap().focus = EditField::HeaderKey(0);
        update(&mut state, Message::DeleteHeaderRow);

        // Edit "X-Env"'s value (now index 0).
        state.edit_mode.as_mut().unwrap().focus = EditField::HeaderValue(0);
        backspace_n(&mut state, "staging".len());
        type_into_focused_field(&mut state, "production");

        // Add a brand-new row.
        update(&mut state, Message::AddHeaderRow);
        type_into_focused_field(&mut state, "X-New");
        update(&mut state, Message::EditFocusNext);
        type_into_focused_field(&mut state, "added");

        update(&mut state, Message::SaveEdit);

        assert!(state.edit_mode.is_none());
        let request = match &state.load_state {
            LoadState::Loaded {
                document, selected, ..
            } => &document.requests()[*selected],
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        };
        assert_eq!(
            request.headers,
            vec![
                ("X-Env".to_string(), "production".to_string()),
                ("X-New".to_string(), "added".to_string()),
            ],
            "the saved request's real headers must reflect the delete, edit and add \
             exactly, in order"
        );
    }

    #[test]
    fn cancel_edit_discards_every_header_change() {
        let mut state = loaded_state(REQUEST_WITH_HEADERS);
        let before_document = match &state.load_state {
            LoadState::Loaded { document, .. } => (**document).clone(),
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        };
        update(&mut state, Message::EnterEditMode);
        update(&mut state, Message::AddHeaderRow);
        type_into_focused_field(&mut state, "X-New");
        state.edit_mode.as_mut().unwrap().focus = EditField::HeaderKey(0);
        update(&mut state, Message::DeleteHeaderRow);

        update(&mut state, Message::CancelEdit);

        assert!(state.edit_mode.is_none());
        match &state.load_state {
            LoadState::Loaded { document, .. } => {
                assert_eq!(
                    **document, before_document,
                    "cancel must leave the real request's headers completely untouched"
                );
            }
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        }
    }

    // --- Body editing -------------------------------------------------------

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

    fn focus_body(state: &mut AppState) {
        state.edit_mode.as_mut().unwrap().focus = EditField::Body;
    }

    fn saved_request(state: &AppState) -> &Request {
        match &state.load_state {
            LoadState::Loaded {
                document, selected, ..
            } => &document.requests()[*selected],
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        }
    }

    /// The current length of the editable body's own text — used to clear a
    /// seeded body with exactly the right number of `Message::EditBackspace`s
    /// before typing a replacement, the same purpose `backspace_n` is always
    /// used for elsewhere in this file.
    fn body_text_len(state: &AppState) -> usize {
        match &state.edit_mode.as_ref().unwrap().body {
            BodyEdit::Editable { text, .. } => text.value().len(),
            other => panic!("expected BodyEdit::Editable, got {other:?}"),
        }
    }

    #[test]
    fn editing_and_saving_a_plain_text_body_reflects_in_the_real_request() {
        let mut state = loaded_state(REQUEST_WITH_PLAIN_BODY);
        update(&mut state, Message::EnterEditMode);
        focus_body(&mut state);
        backspace_n(&mut state, "hello world".len());

        type_into_focused_field(&mut state, "goodbye world");
        update(&mut state, Message::SaveEdit);

        assert!(state.edit_mode.is_none());
        let request = saved_request(&state);
        assert_eq!(request.body.as_deref(), Some("goodbye world"));
        assert_eq!(request.json, None);
    }

    #[test]
    fn editing_and_saving_a_valid_json_body_reflects_in_the_real_request() {
        let mut state = loaded_state(REQUEST_WITH_JSON_BODY);
        update(&mut state, Message::EnterEditMode);
        focus_body(&mut state);
        // Replace the whole pretty-printed `{ "name": "ada" }` with new JSON.
        let seeded_len = body_text_len(&state);
        backspace_n(&mut state, seeded_len);

        type_into_focused_field(&mut state, "{\"name\": \"grace\", \"active\": true}");
        update(&mut state, Message::SaveEdit);

        assert!(state.edit_mode.is_none());
        let request = saved_request(&state);
        assert_eq!(request.body, None, "a saved json body must clear `body:`");
        let json = request.json.as_ref().expect("json must be set");
        assert_eq!(json["name"], "grace");
        assert_eq!(json["active"], true);
    }

    #[test]
    fn invalid_json_body_blocks_save_shows_an_inline_error_and_keeps_the_typed_text() {
        let mut state = loaded_state(REQUEST_WITH_JSON_BODY);
        update(&mut state, Message::EnterEditMode);
        focus_body(&mut state);
        type_into_focused_field(&mut state, " this is not json {{{");

        update(&mut state, Message::SaveEdit);

        assert!(
            state.edit_mode.is_some(),
            "SaveEdit must refuse to leave edit mode with invalid JSON in a json body"
        );
        let edit = state.edit_mode.as_ref().unwrap();
        assert!(
            edit.body_error.is_some(),
            "an invalid JSON body must produce an inline error"
        );
        let text = match &edit.body {
            BodyEdit::Editable { text, .. } => text.value(),
            other => panic!("expected BodyEdit::Editable, got {other:?}"),
        };
        assert!(
            text.contains("this is not json"),
            "the invalid text the user typed must not be discarded: {text}"
        );
        // The real request must be untouched by the refused save.
        let request = saved_request(&state);
        assert_eq!(request.json.as_ref().unwrap()["name"], "ada");
    }

    #[test]
    fn fixing_invalid_json_and_saving_again_succeeds() {
        let mut state = loaded_state(REQUEST_WITH_JSON_BODY);
        update(&mut state, Message::EnterEditMode);
        focus_body(&mut state);
        let seeded_len = body_text_len(&state);
        backspace_n(&mut state, seeded_len);
        type_into_focused_field(&mut state, "not json");
        update(&mut state, Message::SaveEdit);
        assert!(state.edit_mode.as_ref().unwrap().body_error.is_some());

        backspace_n(&mut state, "not json".len());
        type_into_focused_field(&mut state, "{\"ok\": true}");
        update(&mut state, Message::SaveEdit);

        assert!(state.edit_mode.is_none(), "save must now succeed");
        assert_eq!(saved_request(&state).json.as_ref().unwrap()["ok"], true);
    }

    #[test]
    fn typing_into_the_body_clears_a_stale_error_immediately() {
        let mut state = loaded_state(REQUEST_WITH_JSON_BODY);
        update(&mut state, Message::EnterEditMode);
        focus_body(&mut state);
        type_into_focused_field(&mut state, "not json");
        update(&mut state, Message::SaveEdit);
        assert!(state.edit_mode.as_ref().unwrap().body_error.is_some());

        type_into_focused_field(&mut state, "x");

        assert_eq!(
            state.edit_mode.as_ref().unwrap().body_error,
            None,
            "a stale error must clear the moment the body is edited again, \
             not linger until the next save attempt"
        );
    }

    #[test]
    fn clearing_a_body_entirely_saves_as_no_body() {
        let mut state = loaded_state(REQUEST_WITH_PLAIN_BODY);
        update(&mut state, Message::EnterEditMode);
        focus_body(&mut state);
        backspace_n(&mut state, "hello world".len());

        update(&mut state, Message::SaveEdit);

        assert!(state.edit_mode.is_none());
        let request = saved_request(&state);
        assert_eq!(request.body, None);
        assert_eq!(request.json, None);
    }

    #[test]
    fn body_file_is_unsupported_focus_never_reaches_it_and_save_leaves_it_untouched() {
        let mut state = loaded_state(REQUEST_WITH_BODY_FILE);
        update(&mut state, Message::EnterEditMode);
        assert!(
            !state.edit_mode.as_ref().unwrap().has_editable_body(),
            "body_file must not offer an editable body field"
        );

        // Tab all the way around the whole focus cycle — Body must never be
        // reachable, since there is nothing editable to focus.
        for _ in 0..6 {
            update(&mut state, Message::EditFocusNext);
            assert_ne!(state.edit_mode.as_ref().unwrap().focus, EditField::Body);
        }

        update(&mut state, Message::SaveEdit);

        assert!(state.edit_mode.is_none());
        let request = saved_request(&state);
        assert_eq!(
            request.body_file.as_deref(),
            Some("./payload.json"),
            "an Unsupported body must be left completely untouched by save"
        );
    }

    #[test]
    fn form_body_is_unsupported_and_untouched_by_save() {
        let mut state = loaded_state(REQUEST_WITH_FORM_BODY);
        update(&mut state, Message::EnterEditMode);
        assert!(!state.edit_mode.as_ref().unwrap().has_editable_body());

        update(&mut state, Message::SaveEdit);

        assert!(state.edit_mode.is_none());
        let request = saved_request(&state);
        assert_eq!(
            request.form,
            vec![("username".to_string(), "ada".to_string())]
        );
    }

    #[test]
    fn cancel_edit_discards_body_changes_too() {
        let mut state = loaded_state(REQUEST_WITH_PLAIN_BODY);
        let before_document = match &state.load_state {
            LoadState::Loaded { document, .. } => (**document).clone(),
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        };
        update(&mut state, Message::EnterEditMode);
        focus_body(&mut state);
        backspace_n(&mut state, "hello world".len());
        type_into_focused_field(&mut state, "completely different");

        update(&mut state, Message::CancelEdit);

        assert!(state.edit_mode.is_none());
        match &state.load_state {
            LoadState::Loaded { document, .. } => {
                assert_eq!(**document, before_document);
            }
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        }
    }

    #[test]
    fn body_focused_reports_true_only_when_focus_is_on_the_body_field() {
        let mut state = loaded_state(REQUEST_WITH_PLAIN_BODY);
        assert!(!state.body_focused(), "not editing at all yet");

        update(&mut state, Message::EnterEditMode);
        assert!(!state.body_focused(), "focus starts on Method");

        focus_body(&mut state);
        assert!(state.body_focused());

        update(&mut state, Message::EditFocusNext);
        assert!(
            !state.body_focused(),
            "focus must have wrapped back to Method"
        );
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
    fn quit_still_works_while_a_run_is_in_flight() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);

        update(&mut state, Message::Quit);

        assert!(state.should_quit);
    }

    #[test]
    fn selecting_a_different_request_resets_run_state_and_scroll() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::RunRequested);
        update(&mut state, Message::RunCompleted(sample_outcome(200)));
        update(&mut state, Message::ScrollResponseDown);
        assert!(matches!(
            state.run_state,
            RunState::Completed(RunOutcome { result: Ok(_), .. })
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
        update(&mut state, Message::RunCompleted(sample_outcome(200)));

        update(&mut state, Message::SelectNext);

        assert!(matches!(
            state.run_state,
            RunState::Completed(RunOutcome { result: Ok(_), .. })
        ));
    }

    #[test]
    fn starting_a_new_run_resets_the_previous_scroll_position() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        update(&mut state, Message::RunCompleted(sample_outcome(200)));
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
        let mut state = AppState {
            response_scroll: 5_000,
            ..AppState::default()
        };

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
        update(&mut state, Message::RunCompleted(sample_outcome(200)));
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
        update(&mut state, Message::RunCompleted(sample_outcome(200)));
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
        update(&mut state, Message::RunCompleted(failed_outcome(error)));
        assert!(matches!(
            state.run_state,
            RunState::Completed(RunOutcome { result: Err(_), .. })
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

        update(&mut state, Message::Quit);
        assert!(state.should_quit, "quit must still work after a failed run");
    }
}
