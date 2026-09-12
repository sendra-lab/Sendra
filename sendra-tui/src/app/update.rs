//! The reducer half of sendra-tui's Elm-style architecture: `update()`
//! itself, and the private helpers it alone calls. Every `AppState`
//! mutation in the whole crate happens through this one function — see its
//! own doc comment and `super::state::AppState::edit_mode`'s for why that
//! invariant matters.

use sendra_core::{Document, Request};

use super::state::{
    validate_assertion_value_text, validate_method_text, AppState, BodyEdit, EditField, EditState,
    LoadState, Message, RunState, TextField,
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
        Message::CollectionLoaded {
            base_dir,
            path,
            result,
        } => {
            state.load_state = match *result {
                Ok(document) => LoadState::Loaded {
                    document: Box::new(document),
                    selected: 0,
                    base_dir,
                    path,
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
            // computed on every keystroke the way `method_error` is. A fresh
            // attempt also clears any stale `save_error` from a previous
            // failed one — a validation failure this time around should show
            // its own message, not a leftover disk error from before.
            if let Some(edit) = &mut state.edit_mode {
                edit.body_error = validate_body_for_save(&edit.body);
                edit.save_error = None;
            }
            // Refuses to save — but, deliberately, does *not* clear
            // `edit_mode` — while `method_error`, `body_error`, or any
            // assertion row's `value_error` is set: invalid input must
            // never reach the loaded document, but the user must not be
            // locked out of fixing it, which dropping `edit_mode` here (as
            // `CancelEdit` does) would do by discarding what they typed.
            // Leaving edit mode active with the same bad text still in
            // place is what keeps the field "still editable afterward"
            // rather than stuck.
            let can_save = state.edit_mode.as_ref().is_some_and(|edit| {
                edit.method_error.is_none()
                    && edit.body_error.is_none()
                    && edit.assertions.iter().all(|row| row.value_error.is_none())
            });
            if can_save {
                // Everything the save needs, copied out of `state.load_state`
                // as owned values rather than matched by reference: the
                // candidate document built below has to be a full, separate
                // clone (see the comment on it) attempted against disk
                // *before* anything in `state` changes, so a failed write
                // leaves both the loaded document and `edit_mode` completely
                // untouched — the same "nothing happens until it's known to
                // work" guarantee `CancelEdit` already relies on for its own
                // "there is nothing to undo" claim.
                let loaded = match &state.load_state {
                    LoadState::Loaded {
                        document,
                        selected,
                        base_dir,
                        path,
                    } => Some((
                        (**document).clone(),
                        *selected,
                        base_dir.clone(),
                        path.clone(),
                    )),
                    LoadState::Loading | LoadState::NoPathProvided | LoadState::Failed(_) => None,
                };
                if let Some((mut candidate, selected, base_dir, path)) = loaded {
                    if let Some(request) = request_mut(&mut candidate, selected) {
                        // `edit_mode` is guaranteed `Some` here: `can_save`
                        // above is itself `Option::is_some_and`, so it can
                        // only be `true` when there is an edit to apply.
                        if let Some(edit) = &state.edit_mode {
                            apply_edit_to_request(request, edit);
                        }
                    }
                    match candidate.save_to_path(&path) {
                        Ok(()) => {
                            state.load_state = LoadState::Loaded {
                                document: Box::new(candidate),
                                selected,
                                base_dir,
                                path,
                            };
                            state.dirty_requests.remove(&selected);
                            state.edit_mode = None;
                        }
                        Err(error) => {
                            // The loaded document is still the pre-edit one —
                            // `candidate` was a clone, never written back —
                            // and `dirty_requests` still names this request,
                            // exactly as it should: the edit is neither lost
                            // nor silently marked clean over a save that
                            // never actually reached disk. `edit_mode` stays
                            // `Some` so the same typed changes are still
                            // there to retry, or to `CancelEdit` away.
                            if let Some(edit) = &mut state.edit_mode {
                                edit.save_error = Some(error.to_string());
                            }
                        }
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
                edit.focus = edit.focus.next(
                    edit.headers.len(),
                    has_body,
                    edit.auth_field_order(),
                    edit.assertion_row_count(),
                    edit.capture_row_count(),
                );
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
            }
        }
        Message::AddHeaderRow => edit_state_mutate(state, EditState::add_header_row),
        Message::DeleteHeaderRow => edit_state_mutate(state, EditState::delete_focused_header_row),
        Message::AddAssertionRow => edit_state_mutate(state, EditState::add_assertion_row),
        Message::DeleteAssertionRow => {
            edit_state_mutate(state, EditState::delete_focused_assertion_row)
        }
        Message::AddCaptureRow => edit_state_mutate(state, EditState::add_capture_row),
        Message::DeleteCaptureRow => {
            edit_state_mutate(state, EditState::delete_focused_capture_row)
        }
        Message::EditInsertChar(ch) => edit_mutate(state, |field| field.insert_char(ch)),
        Message::EditBackspace => edit_mutate(state, TextField::backspace),
        Message::EditDelete => edit_mutate(state, TextField::delete),
        Message::EditCursorLeft => edit_move_or_toggle(state, false, TextField::move_left),
        Message::EditCursorRight => edit_move_or_toggle(state, true, TextField::move_right),
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

/// Writes every field `EditState` can hold a working copy of — `method`,
/// `url`, `headers`, `body`, `auth`, `assertions`, `capture` — into `request`.
/// The one place this happens, shared between `Message::SaveEdit`'s real save
/// and (indirectly, via `EditState::to_request`) the live "resolved auth"
/// preview, so the two can never drift into applying the edit two different
/// ways.
fn apply_edit_to_request(request: &mut Request, edit: &EditState) {
    // `method_error` is confirmed `None` before this is ever called — see
    // `Message::SaveEdit`'s own `can_save` check — so this cannot fail;
    // matched rather than trusted blindly, so a bug in that invariant leaves
    // the request's method untouched instead of panicking.
    if let Ok(method) = validate_method_text(edit.method.value()) {
        request.method = method;
    }
    request.url = edit.url.value().to_string();
    request.headers = edit
        .headers
        .iter()
        .map(|row| (row.key.value().to_string(), row.value.value().to_string()))
        .collect();
    apply_body_edit(request, &edit.body);
    request.auth = edit.auth.to_auth();
    request.assertions = edit.to_assertions(request.assertions.as_ref());
    request.capture = edit.to_captures();
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
/// live validation on every keystroke. A no-op when edit mode is not active,
/// so every `Message::EditInsertChar`/`EditBackspace`/`EditDelete` arm can
/// call this unconditionally rather than each re-checking
/// `state.edit_mode.is_some()` itself. Also a no-op — nothing to mark dirty —
/// when focus is on a fixed-enum auth sub-field (`api_key.in`/
/// `oauth.grant_type`), which has no `TextField` for `focused_field_mut` to
/// hand back at all; typing/backspacing/deleting has nothing to do there.
fn edit_mutate(state: &mut AppState, mutate: impl FnOnce(&mut TextField)) {
    let Some(edit) = &mut state.edit_mode else {
        return;
    };
    let Some(field) = edit.focused_field_mut() else {
        return;
    };
    mutate(field);
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
    // Unlike `body_error`, an assertion row's `value_error` *is* recomputed
    // on every keystroke, the same as `method_error` — see
    // `AssertionRow::value_error`'s own doc comment for why: it is the same
    // "does this even parse as YAML" check a file load already enforces at
    // parse time, not the looser, evaluate-time operator-argument check a
    // mid-edit body deliberately defers.
    if let EditField::AssertionValue(index) = edit.focus {
        edit.assertions[index].value_error =
            validate_assertion_value_text(edit.assertions[index].value.value()).err();
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
/// `dirty_requests`/`method_error`. A no-op when focus is on a fixed-enum
/// auth sub-field, the same as `edit_mutate` — see `edit_move_or_toggle`,
/// which is what `Left`/`Right` actually call instead of this, for the field
/// where that would otherwise leave the key doing nothing at all.
fn edit_move(state: &mut AppState, mutate: impl FnOnce(&mut TextField)) {
    if let Some(edit) = &mut state.edit_mode {
        if let Some(field) = edit.focused_field_mut() {
            mutate(field);
        }
    }
}

/// `Left`/`Right` on the focused field: ordinarily cursor movement, exactly
/// like `edit_move` — but a handful of fields are fixed enums with no
/// `TextField`/cursor to move through at all (`api_key.in`/
/// `oauth.grant_type`, and an assertion row's `operator`/`negate` — see
/// `AuthField::ApiKeyLocation`/`OAuthGrantType` and `EditField::
/// AssertionOperator`/`AssertionNegate`), so while one of those has focus,
/// these same two keys instead toggle its value (see
/// `EditState::toggle_focused`) — `forward` is `true` for `Right`, `false`
/// for `Left`, which only actually changes anything for `operator` (an
/// 8-way cycle); every other toggle here is a plain two-value flip, where
/// direction makes no difference. Unlike ordinary cursor movement, a toggle
/// really is an edit — it changes what gets saved — so this marks `dirty`/
/// `dirty_requests` exactly the way `edit_mutate` does, not the way
/// `edit_move` deliberately doesn't.
fn edit_move_or_toggle(state: &mut AppState, forward: bool, mutate: impl FnOnce(&mut TextField)) {
    let Some(edit) = &mut state.edit_mode else {
        return;
    };
    if edit.toggle_focused(forward) {
        edit.dirty = true;
        if let LoadState::Loaded { selected, .. } = &state.load_state {
            state.dirty_requests.insert(*selected);
        }
        return;
    }
    if let Some(field) = edit.focused_field_mut() {
        mutate(field);
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

    use sendra_core::{
        ApiKeyLocation, Assertions, CaptureSource, Captures, Environment, Method, Response,
    };

    use super::super::state::{AuthEdit, AuthField, CaptureKind, JsonOperator};
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
                path: PathBuf::from("collection.yaml"),
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

    // --- Auth editing --------------------------------------------------------

    const REQUEST_WITH_BEARER_AUTH: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
    auth:
      bearer: old-token
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
        value: old-value
";

    const REQUEST_WITH_NO_AUTH: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
";

    #[test]
    fn enter_edit_mode_seeds_auth_from_the_real_request() {
        let mut state = loaded_state(REQUEST_WITH_BEARER_AUTH);

        update(&mut state, Message::EnterEditMode);

        let edit = state.edit_mode.as_ref().expect("edit mode just entered");
        match &edit.auth {
            AuthEdit::Bearer { token } => assert_eq!(token.value(), "old-token"),
            other => panic!("expected AuthEdit::Bearer, got {other:?}"),
        }
    }

    #[test]
    fn tab_moves_focus_from_body_into_the_bearer_token_field_and_wraps_to_method() {
        let mut state = loaded_state(REQUEST_WITH_BEARER_AUTH);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::Body;

        update(&mut state, Message::EditFocusNext);
        assert_eq!(
            state.edit_mode.as_ref().unwrap().focus,
            EditField::Auth(AuthField::BearerToken)
        );

        update(&mut state, Message::EditFocusNext);
        assert_eq!(
            state.edit_mode.as_ref().unwrap().focus,
            EditField::Method,
            "the last (only) auth field wraps back to Method"
        );
    }

    #[test]
    fn shift_tab_from_method_moves_into_the_last_auth_field() {
        let mut state = loaded_state(REQUEST_WITH_API_KEY_AUTH);
        update(&mut state, Message::EnterEditMode);

        update(&mut state, Message::EditFocusPrev);

        assert_eq!(
            state.edit_mode.as_ref().unwrap().focus,
            EditField::Auth(AuthField::ApiKeyLocation),
            "Shift+Tab from Method must land on the last auth field"
        );
    }

    #[test]
    fn typing_into_the_focused_bearer_token_field_marks_the_edit_dirty() {
        let mut state = loaded_state(REQUEST_WITH_BEARER_AUTH);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::Auth(AuthField::BearerToken);
        backspace_n(&mut state, "old-token".len());

        type_into_focused_field(&mut state, "new-token");

        let edit = state.edit_mode.as_ref().unwrap();
        match &edit.auth {
            AuthEdit::Bearer { token } => assert_eq!(token.value(), "new-token"),
            other => panic!("expected AuthEdit::Bearer, got {other:?}"),
        }
        assert!(edit.dirty);
        assert!(state.dirty_requests.contains(&0));
    }

    #[test]
    fn save_edit_writes_the_edited_bearer_token_into_the_loaded_document() {
        let mut state = loaded_state(REQUEST_WITH_BEARER_AUTH);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::Auth(AuthField::BearerToken);
        backspace_n(&mut state, "old-token".len());
        type_into_focused_field(&mut state, "new-token");

        update(&mut state, Message::SaveEdit);

        assert!(state.edit_mode.is_none());
        let request = saved_request(&state);
        let auth = request.auth.as_ref().expect("auth must still be set");
        assert_eq!(auth.bearer.as_deref(), Some("new-token"));
    }

    #[test]
    fn cancel_edit_discards_auth_changes() {
        let mut state = loaded_state(REQUEST_WITH_BEARER_AUTH);
        let before_document = match &state.load_state {
            LoadState::Loaded { document, .. } => (**document).clone(),
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        };
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::Auth(AuthField::BearerToken);
        backspace_n(&mut state, "old-token".len());
        type_into_focused_field(&mut state, "changed");

        update(&mut state, Message::CancelEdit);

        assert!(state.edit_mode.is_none());
        match &state.load_state {
            LoadState::Loaded { document, .. } => {
                assert_eq!(
                    **document, before_document,
                    "cancel must leave the real request's auth completely untouched"
                );
            }
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        }
    }

    #[test]
    fn left_right_toggle_the_api_key_location_instead_of_moving_a_cursor() {
        let mut state = loaded_state(REQUEST_WITH_API_KEY_AUTH);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::Auth(AuthField::ApiKeyLocation);

        update(&mut state, Message::EditCursorRight);

        let edit = state.edit_mode.as_ref().unwrap();
        match &edit.auth {
            AuthEdit::ApiKey { location, .. } => assert_eq!(*location, ApiKeyLocation::Query),
            other => panic!("expected AuthEdit::ApiKey, got {other:?}"),
        }
        assert!(edit.dirty, "toggling the location is a real edit");
        assert!(state.dirty_requests.contains(&0));

        update(&mut state, Message::EditCursorLeft);
        match &state.edit_mode.as_ref().unwrap().auth {
            AuthEdit::ApiKey { location, .. } => assert_eq!(*location, ApiKeyLocation::Header),
            other => panic!("expected AuthEdit::ApiKey, got {other:?}"),
        }
    }

    #[test]
    fn save_edit_writes_the_toggled_api_key_location_into_the_loaded_document() {
        let mut state = loaded_state(REQUEST_WITH_API_KEY_AUTH);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::Auth(AuthField::ApiKeyLocation);
        update(&mut state, Message::EditCursorRight);

        update(&mut state, Message::SaveEdit);

        let request = saved_request(&state);
        let api_key = request
            .auth
            .as_ref()
            .expect("auth must still be set")
            .api_key
            .as_ref()
            .expect("api_key must still be set");
        assert_eq!(api_key.r#in, ApiKeyLocation::Query);
        assert_eq!(api_key.name, "X-Api-Key");
        assert_eq!(api_key.value, "old-value");
    }

    #[test]
    fn typing_and_left_right_are_no_ops_when_the_request_has_no_auth() {
        let mut state = loaded_state(REQUEST_WITH_NO_AUTH);
        update(&mut state, Message::EnterEditMode);
        assert!(
            state
                .edit_mode
                .as_ref()
                .unwrap()
                .auth_field_order()
                .is_empty(),
            "a request with no auth: block has no auth fields to focus"
        );

        // Tab all the way around the whole focus cycle — Auth must never be
        // reachable, since there is nothing to focus.
        for _ in 0..6 {
            update(&mut state, Message::EditFocusNext);
            assert!(!matches!(
                state.edit_mode.as_ref().unwrap().focus,
                EditField::Auth(_)
            ));
        }

        update(&mut state, Message::SaveEdit);
        assert_eq!(saved_request(&state).auth, None);
    }

    /// Proof requirement: editing a request's own auth must be verified
    /// against `Request::resolve_auth`'s *actual* output, not just that the
    /// working-copy struct fields changed. This exercises the exact
    /// `EditState::to_request` + `Environment::apply`/`resolve_auth`
    /// pipeline `render_edit_pane`'s live preview reuses, proving a bearer
    /// token typed into the edit pane resolves to the real `Authorization`
    /// header sendra-core would actually send.
    #[test]
    fn edited_bearer_token_resolves_to_the_real_authorization_header() {
        let mut state = loaded_state(REQUEST_WITH_BEARER_AUTH);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::Auth(AuthField::BearerToken);
        backspace_n(&mut state, "old-token".len());
        type_into_focused_field(&mut state, "brand-new-token");

        let base_request = saved_request(&state).clone();
        let edit = state.edit_mode.as_ref().unwrap();
        let candidate = edit.to_request(&base_request);

        let resolved = candidate
            .resolve_auth()
            .expect("a bearer token always resolves");
        assert_eq!(
            resolved
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization")),
            Some(&(
                "Authorization".to_string(),
                "Bearer brand-new-token".to_string()
            )),
            "resolve_auth's real output must reflect the edited token, not just the struct field"
        );
    }

    /// Proof requirement: an environment-level `auth:` default must still
    /// only apply when the request's own auth is genuinely absent — editing
    /// (or clearing) a request's own auth must not change that precedence,
    /// verified through the same real `Environment::apply`/`resolve_auth`
    /// sendra-core itself uses to decide, not a TUI-side reimplementation of
    /// that rule.
    #[test]
    fn environment_default_auth_only_applies_when_the_edited_request_has_none() {
        use sendra_core::{Auth, Environment};

        let mut environment = Environment::default();
        environment.auth = Some(Auth {
            bearer: Some("env-default-token".to_string()),
            basic: None,
            api_key: None,
            oauth: None,
        });

        // The request's own bearer auth must win — the environment default
        // must never be applied on top of it.
        let mut state = loaded_state(REQUEST_WITH_BEARER_AUTH);
        update(&mut state, Message::EnterEditMode);
        let base_request = saved_request(&state).clone();
        let edit = state.edit_mode.as_ref().unwrap();
        let candidate = edit.to_request(&base_request);
        let resolved = environment
            .apply(&candidate)
            .and_then(|request| request.resolve_auth())
            .expect("resolves");
        assert_eq!(
            resolved
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization")),
            Some(&("Authorization".to_string(), "Bearer old-token".to_string())),
            "the request's own auth must win over the environment default"
        );

        // With no auth of its own (the request's auth: block removed), the
        // environment default must apply.
        let mut state = loaded_state(REQUEST_WITH_NO_AUTH);
        update(&mut state, Message::EnterEditMode);
        let base_request = saved_request(&state).clone();
        let edit = state.edit_mode.as_ref().unwrap();
        assert!(edit.auth_field_order().is_empty());
        let candidate = edit.to_request(&base_request);
        assert_eq!(candidate.auth, None);
        let resolved = environment
            .apply(&candidate)
            .and_then(|request| request.resolve_auth())
            .expect("resolves");
        assert_eq!(
            resolved
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization")),
            Some(&(
                "Authorization".to_string(),
                "Bearer env-default-token".to_string()
            )),
            "with no auth of its own, the environment default must apply"
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

    fn synthetic_response(status: u16, body: &str) -> Response {
        Response {
            status,
            status_text: String::new(),
            headers: Vec::new(),
            body: body.to_string(),
            elapsed: std::time::Duration::from_millis(1),
            redirects: Vec::new(),
        }
    }

    #[test]
    fn enter_edit_mode_seeds_assertion_rows_from_the_real_request() {
        let mut state = loaded_state(REQUEST_WITH_JSON_ASSERTION);

        update(&mut state, Message::EnterEditMode);

        let edit = state.edit_mode.as_ref().expect("edit mode just entered");
        assert_eq!(edit.assertions.len(), 1);
        assert_eq!(edit.assertions[0].path.value(), "$.status");
        assert_eq!(edit.assertions[0].value.value(), "ok");
    }

    #[test]
    fn add_assertion_row_appends_a_new_row_and_focuses_its_path() {
        let mut state = loaded_state(REQUEST_WITH_JSON_ASSERTION);
        update(&mut state, Message::EnterEditMode);

        update(&mut state, Message::AddAssertionRow);

        let edit = state.edit_mode.as_ref().unwrap();
        assert_eq!(edit.assertions.len(), 2);
        assert_eq!(edit.assertions[1].path.value(), "");
        assert_eq!(edit.focus, EditField::AssertionPath(1));
        assert!(edit.dirty);
        assert!(state.dirty_requests.contains(&0));
    }

    #[test]
    fn delete_assertion_row_removes_the_focused_one() {
        let mut state = loaded_state(REQUEST_WITH_JSON_ASSERTION);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::AssertionValue(0);

        update(&mut state, Message::DeleteAssertionRow);

        let edit = state.edit_mode.as_ref().unwrap();
        assert!(edit.assertions.is_empty());
        assert!(edit.dirty);
    }

    #[test]
    fn delete_assertion_row_is_a_no_op_while_focus_is_elsewhere() {
        let mut state = loaded_state(REQUEST_WITH_JSON_ASSERTION);
        update(&mut state, Message::EnterEditMode);
        assert_eq!(state.edit_mode.as_ref().unwrap().focus, EditField::Method);

        update(&mut state, Message::DeleteAssertionRow);

        assert_eq!(
            state.edit_mode.as_ref().unwrap().assertions.len(),
            1,
            "nothing should be deleted while focus is on method"
        );
    }

    #[test]
    fn tab_reaches_the_assertion_section_after_the_body_field() {
        let mut state = loaded_state(REQUEST_WITH_JSON_ASSERTION);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::Body;

        update(&mut state, Message::EditFocusNext);
        assert_eq!(
            state.edit_mode.as_ref().unwrap().focus,
            EditField::AssertionPath(0)
        );
        update(&mut state, Message::EditFocusNext);
        assert_eq!(
            state.edit_mode.as_ref().unwrap().focus,
            EditField::AssertionOperator(0)
        );
        update(&mut state, Message::EditFocusNext);
        assert_eq!(
            state.edit_mode.as_ref().unwrap().focus,
            EditField::AssertionValue(0)
        );
        update(&mut state, Message::EditFocusNext);
        assert_eq!(
            state.edit_mode.as_ref().unwrap().focus,
            EditField::AssertionNegate(0)
        );
        update(&mut state, Message::EditFocusNext);
        assert_eq!(
            state.edit_mode.as_ref().unwrap().focus,
            EditField::Method,
            "the last assertion row's negate flag wraps back to Method"
        );
    }

    #[test]
    fn typing_into_the_focused_path_field_edits_the_working_copy_and_marks_dirty() {
        let mut state = loaded_state(REQUEST_WITH_JSON_ASSERTION);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::AssertionPath(0);
        type_into_focused_field(&mut state, "x");

        let edit = state.edit_mode.as_ref().unwrap();
        assert_eq!(edit.assertions[0].path.value(), "$.statusx");
        assert!(edit.dirty);
        assert!(state.dirty_requests.contains(&0));
    }

    #[test]
    fn typing_a_malformed_value_sets_a_live_error_that_blocks_save() {
        let mut state = loaded_state(REQUEST_WITH_JSON_ASSERTION);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::AssertionValue(0);
        backspace_n(&mut state, "ok".len());
        type_into_focused_field(&mut state, "[a, b");

        assert!(
            state.edit_mode.as_ref().unwrap().assertions[0]
                .value_error
                .is_some(),
            "an unbalanced bracket is not valid YAML"
        );

        update(&mut state, Message::SaveEdit);
        assert!(
            state.edit_mode.is_some(),
            "SaveEdit must refuse to leave edit mode with a malformed assertion value"
        );

        // Still editable afterward — fix it and save again successfully.
        backspace_n(&mut state, "[a, b".len());
        type_into_focused_field(&mut state, "fixed");
        assert_eq!(
            state.edit_mode.as_ref().unwrap().assertions[0].value_error,
            None
        );
        update(&mut state, Message::SaveEdit);
        assert!(state.edit_mode.is_none(), "save must now succeed");
    }

    #[test]
    fn left_right_toggle_the_operator_and_the_negate_flag() {
        let mut state = loaded_state(REQUEST_WITH_JSON_ASSERTION);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::AssertionOperator(0);

        update(&mut state, Message::EditCursorRight);
        assert_eq!(
            state.edit_mode.as_ref().unwrap().assertions[0].operator,
            JsonOperator::GreaterThan
        );
        update(&mut state, Message::EditCursorLeft);
        assert_eq!(
            state.edit_mode.as_ref().unwrap().assertions[0].operator,
            JsonOperator::Equals
        );

        state.edit_mode.as_mut().unwrap().focus = EditField::AssertionNegate(0);
        update(&mut state, Message::EditCursorRight);
        assert!(state.edit_mode.as_ref().unwrap().assertions[0].negate);
    }

    #[test]
    fn save_edit_writes_added_edited_and_deleted_assertion_rows_into_the_loaded_document() {
        let mut state = loaded_state(REQUEST_WITH_JSON_ASSERTION);
        update(&mut state, Message::EnterEditMode);

        // Edit the existing row's value.
        state.edit_mode.as_mut().unwrap().focus = EditField::AssertionValue(0);
        backspace_n(&mut state, "ok".len());
        type_into_focused_field(&mut state, "ready");

        // Add a brand-new, negated row.
        update(&mut state, Message::AddAssertionRow);
        type_into_focused_field(&mut state, "$.count");
        update(&mut state, Message::EditFocusNext); // -> operator
        update(&mut state, Message::EditCursorRight); // -> GreaterThan
        update(&mut state, Message::EditFocusNext); // -> value
        type_into_focused_field(&mut state, "5");
        update(&mut state, Message::EditFocusNext); // -> negate
        update(&mut state, Message::EditCursorRight); // -> true

        update(&mut state, Message::SaveEdit);

        assert!(state.edit_mode.is_none());
        let request = saved_request(&state);
        let assertions = request
            .assertions
            .as_ref()
            .expect("assertions must remain set");
        assert_eq!(
            assertions.json.get("$.status"),
            Some(&serde_json::json!("ready")),
            "the edited row's new value must be saved"
        );
        assert_eq!(
            assertions.not.as_ref().unwrap().json.get("$.count"),
            Some(&serde_json::json!({"greater_than": 5})),
            "the added negated row must be saved under not.json in its real operator shape"
        );
    }

    #[test]
    fn cancel_edit_discards_every_assertion_change() {
        let mut state = loaded_state(REQUEST_WITH_JSON_ASSERTION);
        let before_document = match &state.load_state {
            LoadState::Loaded { document, .. } => (**document).clone(),
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        };
        update(&mut state, Message::EnterEditMode);
        update(&mut state, Message::AddAssertionRow);
        type_into_focused_field(&mut state, "$.new");
        state.edit_mode.as_mut().unwrap().focus = EditField::AssertionPath(0);
        update(&mut state, Message::DeleteAssertionRow);

        update(&mut state, Message::CancelEdit);

        assert!(state.edit_mode.is_none());
        match &state.load_state {
            LoadState::Loaded { document, .. } => {
                assert_eq!(
                    **document, before_document,
                    "cancel must leave the real request's assertions completely untouched"
                );
            }
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        }
    }

    #[test]
    fn a_request_with_no_assertions_offers_none_to_edit_and_save_adds_none() {
        let mut state = loaded_state(REQUEST_WITH_NO_ASSERTIONS);
        update(&mut state, Message::EnterEditMode);
        assert!(state.edit_mode.as_ref().unwrap().assertions.is_empty());

        update(&mut state, Message::SaveEdit);

        assert_eq!(saved_request(&state).assertions, None);
    }

    /// Proof requirement: adding, editing and deleting assertions must be
    /// verified against `Assertions::evaluate`'s *actual* output — a real
    /// pass/fail against a real response — not just that the working-copy
    /// struct fields changed. `run_request.rs` never gets touched: this
    /// calls `Assertions::evaluate` directly, as a plain library function,
    /// the same way `run_request::execute` itself does, without spawning a
    /// run or wiring the editor into it.
    #[test]
    fn edited_assertions_actually_change_what_assertions_evaluate_reports() {
        let mut state = loaded_state(REQUEST_WITH_JSON_ASSERTION);
        update(&mut state, Message::EnterEditMode);

        // Before editing: `$.status` must equal "ok" — passes against a
        // response whose body says so, fails otherwise.
        let request = saved_request(&state).clone();
        let original_assertions = request.assertions.clone().unwrap();
        let ok_response = synthetic_response(200, r#"{"status": "ok"}"#);
        let error_response = synthetic_response(200, r#"{"status": "error"}"#);
        assert!(original_assertions.evaluate(&ok_response).passed());
        assert!(!original_assertions.evaluate(&error_response).passed());

        // Edit it to expect "error" instead — a real, in-progress edit, run
        // through the exact same `to_assertions` `Message::SaveEdit` uses.
        state.edit_mode.as_mut().unwrap().focus = EditField::AssertionValue(0);
        backspace_n(&mut state, "ok".len());
        type_into_focused_field(&mut state, "error");
        let edit = state.edit_mode.as_ref().unwrap();
        let edited_assertions = edit.to_assertions(request.assertions.as_ref()).unwrap();

        // The real report must have flipped: what used to fail now passes,
        // and vice versa — proof the edit takes effect in evaluation, not
        // only in the struct.
        assert!(
            !edited_assertions.evaluate(&ok_response).passed(),
            "the old expectation must no longer hold"
        );
        assert!(
            edited_assertions.evaluate(&error_response).passed(),
            "the edited expectation must now hold"
        );

        // Now actually save it, and prove the exact same thing against the
        // request sitting in the loaded document afterward.
        update(&mut state, Message::SaveEdit);
        let saved_assertions = saved_request(&state).assertions.clone().unwrap();
        assert!(!saved_assertions.evaluate(&ok_response).passed());
        assert!(saved_assertions.evaluate(&error_response).passed());
    }

    /// The same proof, for deleting a row: once removed, `evaluate` must
    /// report nothing at all rather than a lingering pass — an empty report
    /// is a different, real outcome (`AssertionReport::is_empty`), not the
    /// same "passed" a check that still exists but happens to hold would
    /// report.
    #[test]
    fn deleting_an_assertion_row_and_saving_removes_it_from_what_evaluate_checks() {
        let mut state = loaded_state(REQUEST_WITH_JSON_ASSERTION);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::AssertionPath(0);

        update(&mut state, Message::DeleteAssertionRow);
        update(&mut state, Message::SaveEdit);

        assert_eq!(
            saved_request(&state).assertions,
            None,
            "deleting the only assertion must leave nothing behind"
        );
        // Confirmed the same way through evaluation itself, not only the
        // struct: an absent `assertions:` block is `Assertions::default()`,
        // which evaluates to an empty, vacuously-passing report.
        let report = Assertions::default().evaluate(&synthetic_response(200, "anything"));
        assert!(report.is_empty());
        assert!(report.passed());
    }

    /// The same proof, for adding a brand-new operator assertion: a real
    /// `greater_than` check that genuinely distinguishes a passing response
    /// from a failing one once evaluated for real.
    #[test]
    fn adding_a_greater_than_assertion_and_saving_actually_enforces_it() {
        let mut state = loaded_state(REQUEST_WITH_NO_ASSERTIONS);
        update(&mut state, Message::EnterEditMode);

        update(&mut state, Message::AddAssertionRow);
        type_into_focused_field(&mut state, "$.count");
        update(&mut state, Message::EditFocusNext); // -> operator
        update(&mut state, Message::EditCursorRight); // Equals -> GreaterThan
        update(&mut state, Message::EditFocusNext); // -> value
        type_into_focused_field(&mut state, "10");

        update(&mut state, Message::SaveEdit);

        let assertions = saved_request(&state).assertions.clone().unwrap();
        let passing = synthetic_response(200, r#"{"count": 20}"#);
        let failing = synthetic_response(200, r#"{"count": 5}"#);
        assert!(
            assertions.evaluate(&passing).passed(),
            "20 is genuinely greater than 10"
        );
        assert!(
            !assertions.evaluate(&failing).passed(),
            "5 is genuinely not greater than 10"
        );
    }

    // --- Capture editing -----------------------------------------------------

    const REQUEST_WITH_JSON_PATH_CAPTURE: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
    capture:
      auth_token: $.token
";

    const REQUEST_WITH_MIXED_CAPTURES: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
    capture:
      auth_token: $.token
      code:
        status: true
      trace:
        header: X-Trace-Id
";

    const REQUEST_WITH_NO_CAPTURE: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
";

    fn response_with_headers(status: u16, headers: &[(&str, &str)], body: &str) -> Response {
        Response {
            status,
            status_text: String::new(),
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            body: body.to_string(),
            elapsed: std::time::Duration::from_millis(1),
            redirects: Vec::new(),
        }
    }

    #[test]
    fn enter_edit_mode_seeds_capture_rows_from_the_real_request_in_name_order() {
        let mut state = loaded_state(REQUEST_WITH_MIXED_CAPTURES);

        update(&mut state, Message::EnterEditMode);

        let edit = state.edit_mode.as_ref().expect("edit mode just entered");
        assert_eq!(edit.captures.len(), 3);
        // `Captures::entries` iterates a `BTreeMap`, so seeding order is
        // alphabetical by name, the same deterministic order `AssertionRow`
        // seeding relies on for `Assertions::json`.
        assert_eq!(edit.captures[0].name.value(), "auth_token");
        assert_eq!(edit.captures[0].kind, CaptureKind::JsonPath);
        assert_eq!(edit.captures[0].value.value(), "$.token");

        assert_eq!(edit.captures[1].name.value(), "code");
        assert_eq!(edit.captures[1].kind, CaptureKind::Status);

        assert_eq!(edit.captures[2].name.value(), "trace");
        assert_eq!(edit.captures[2].kind, CaptureKind::Header);
        assert_eq!(edit.captures[2].value.value(), "X-Trace-Id");
    }

    #[test]
    fn add_capture_row_appends_a_new_row_and_focuses_its_name() {
        let mut state = loaded_state(REQUEST_WITH_JSON_PATH_CAPTURE);
        update(&mut state, Message::EnterEditMode);

        update(&mut state, Message::AddCaptureRow);

        let edit = state.edit_mode.as_ref().unwrap();
        assert_eq!(edit.captures.len(), 2);
        assert_eq!(edit.captures[1].name.value(), "");
        assert_eq!(edit.captures[1].kind, CaptureKind::JsonPath);
        assert_eq!(edit.focus, EditField::CaptureName(1));
        assert!(edit.dirty);
        assert!(state.dirty_requests.contains(&0));
    }

    #[test]
    fn delete_capture_row_removes_the_focused_one() {
        let mut state = loaded_state(REQUEST_WITH_JSON_PATH_CAPTURE);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::CaptureValue(0);

        update(&mut state, Message::DeleteCaptureRow);

        let edit = state.edit_mode.as_ref().unwrap();
        assert!(edit.captures.is_empty());
        assert!(edit.dirty);
    }

    #[test]
    fn delete_capture_row_is_a_no_op_while_focus_is_elsewhere() {
        let mut state = loaded_state(REQUEST_WITH_JSON_PATH_CAPTURE);
        update(&mut state, Message::EnterEditMode);
        assert_eq!(state.edit_mode.as_ref().unwrap().focus, EditField::Method);

        update(&mut state, Message::DeleteCaptureRow);

        assert_eq!(
            state.edit_mode.as_ref().unwrap().captures.len(),
            1,
            "nothing should be deleted while focus is on method"
        );
    }

    #[test]
    fn tab_reaches_the_capture_section_after_the_assertion_section() {
        let mut state = loaded_state(REQUEST_WITH_JSON_PATH_CAPTURE);
        update(&mut state, Message::EnterEditMode);
        // This request has no assertions, so the capture section follows
        // directly after Body (no auth on this request either).
        state.edit_mode.as_mut().unwrap().focus = EditField::Body;

        update(&mut state, Message::EditFocusNext);
        assert_eq!(
            state.edit_mode.as_ref().unwrap().focus,
            EditField::CaptureName(0)
        );
        update(&mut state, Message::EditFocusNext);
        assert_eq!(
            state.edit_mode.as_ref().unwrap().focus,
            EditField::CaptureKind(0)
        );
        update(&mut state, Message::EditFocusNext);
        assert_eq!(
            state.edit_mode.as_ref().unwrap().focus,
            EditField::CaptureValue(0)
        );
        update(&mut state, Message::EditFocusNext);
        assert_eq!(
            state.edit_mode.as_ref().unwrap().focus,
            EditField::Method,
            "the only capture row's value field wraps back to Method"
        );
    }

    #[test]
    fn typing_into_the_focused_name_field_edits_the_working_copy_and_marks_dirty() {
        let mut state = loaded_state(REQUEST_WITH_JSON_PATH_CAPTURE);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::CaptureName(0);
        type_into_focused_field(&mut state, "x");

        let edit = state.edit_mode.as_ref().unwrap();
        assert_eq!(edit.captures[0].name.value(), "auth_tokenx");
        assert!(edit.dirty);
        assert!(state.dirty_requests.contains(&0));
    }

    #[test]
    fn left_right_toggle_the_capture_kind() {
        let mut state = loaded_state(REQUEST_WITH_JSON_PATH_CAPTURE);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::CaptureKind(0);

        update(&mut state, Message::EditCursorRight);
        assert_eq!(
            state.edit_mode.as_ref().unwrap().captures[0].kind,
            CaptureKind::Header
        );
        update(&mut state, Message::EditCursorRight);
        assert_eq!(
            state.edit_mode.as_ref().unwrap().captures[0].kind,
            CaptureKind::Status
        );
        update(&mut state, Message::EditCursorLeft);
        assert_eq!(
            state.edit_mode.as_ref().unwrap().captures[0].kind,
            CaptureKind::Header
        );
        assert!(state.edit_mode.as_ref().unwrap().dirty);
    }

    #[test]
    fn save_edit_writes_added_and_edited_capture_rows_into_the_loaded_document() {
        let mut state = loaded_state(REQUEST_WITH_JSON_PATH_CAPTURE);
        update(&mut state, Message::EnterEditMode);

        // Edit the existing row's path.
        state.edit_mode.as_mut().unwrap().focus = EditField::CaptureValue(0);
        backspace_n(&mut state, "$.token".len());
        type_into_focused_field(&mut state, "$.session.token");

        // Add a brand-new header capture.
        update(&mut state, Message::AddCaptureRow);
        type_into_focused_field(&mut state, "trace");
        update(&mut state, Message::EditFocusNext); // -> kind
        update(&mut state, Message::EditCursorRight); // JsonPath -> Header
        update(&mut state, Message::EditFocusNext); // -> value
        type_into_focused_field(&mut state, "X-Trace-Id");

        update(&mut state, Message::SaveEdit);

        assert!(state.edit_mode.is_none());
        let request = saved_request(&state);
        let capture = request.capture.as_ref().expect("capture must remain set");
        assert_eq!(
            capture.entries().get("auth_token"),
            Some(&CaptureSource::JsonPath("$.session.token".to_string())),
            "the edited row's new path must be saved"
        );
        assert_eq!(
            capture.entries().get("trace"),
            Some(&CaptureSource::Header {
                header: "X-Trace-Id".to_string()
            }),
            "the added row must be saved as a real header capture"
        );
    }

    #[test]
    fn cancel_edit_discards_every_capture_change() {
        let mut state = loaded_state(REQUEST_WITH_JSON_PATH_CAPTURE);
        let before_document = match &state.load_state {
            LoadState::Loaded { document, .. } => (**document).clone(),
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        };
        update(&mut state, Message::EnterEditMode);
        update(&mut state, Message::AddCaptureRow);
        type_into_focused_field(&mut state, "new_var");
        state.edit_mode.as_mut().unwrap().focus = EditField::CaptureName(0);
        update(&mut state, Message::DeleteCaptureRow);

        update(&mut state, Message::CancelEdit);

        assert!(state.edit_mode.is_none());
        match &state.load_state {
            LoadState::Loaded { document, .. } => {
                assert_eq!(
                    **document, before_document,
                    "cancel must leave the real request's captures completely untouched"
                );
            }
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        }
    }

    #[test]
    fn a_request_with_no_capture_offers_none_to_edit_and_save_adds_none() {
        let mut state = loaded_state(REQUEST_WITH_NO_CAPTURE);
        update(&mut state, Message::EnterEditMode);
        assert!(state.edit_mode.as_ref().unwrap().captures.is_empty());

        update(&mut state, Message::SaveEdit);

        assert_eq!(saved_request(&state).capture, None);
    }

    /// Proof requirement: adding, editing and deleting captures must be
    /// verified against `Captures::evaluate`'s *actual* output — a real
    /// captured value (or failure) against a real response — not just that
    /// the working-copy struct fields changed. `run_request.rs` never gets
    /// touched: this calls `Captures::evaluate` directly, as a plain library
    /// function, the same way `run_request::execute` itself does, without
    /// spawning a run or wiring the editor into it. Mirrors
    /// `edited_assertions_actually_change_what_assertions_evaluate_reports`
    /// exactly, for `Captures` instead of `Assertions` — the one real
    /// difference is `Captures::evaluate`'s extra `&Environment` argument, an
    /// empty one here since nothing in this test collides with it.
    #[test]
    fn edited_captures_actually_change_what_captures_evaluate_reports() {
        let mut state = loaded_state(REQUEST_WITH_JSON_PATH_CAPTURE);
        update(&mut state, Message::EnterEditMode);
        let environment = Environment::default();

        // Before editing: `$.token` captures from a body that has one, and
        // fails (`NoMatch`) against a body that doesn't.
        let request = saved_request(&state).clone();
        let original_captures = request.capture.clone().unwrap();
        let has_token = response_with_headers(200, &[], r#"{"token": "abc123"}"#);
        let no_token = response_with_headers(200, &[], r#"{"nope": "x"}"#);
        assert!(original_captures
            .evaluate(&has_token, &environment)
            .passed());
        assert!(!original_captures.evaluate(&no_token, &environment).passed());

        // Edit it to read `$.nope` instead — a real, in-progress edit, run
        // through the exact same `to_captures` `Message::SaveEdit` uses.
        state.edit_mode.as_mut().unwrap().focus = EditField::CaptureValue(0);
        backspace_n(&mut state, "$.token".len());
        type_into_focused_field(&mut state, "$.nope");
        let edited_captures = state.edit_mode.as_ref().unwrap().to_captures().unwrap();

        // The real report must have flipped: what used to capture now fails,
        // and vice versa — proof the edit takes effect in evaluation, not
        // only in the struct.
        let report_against_has_token = edited_captures.evaluate(&has_token, &environment);
        assert!(
            !report_against_has_token.passed(),
            "the old path no longer matches anything in this body"
        );
        let report_against_no_token = edited_captures.evaluate(&no_token, &environment);
        assert!(
            report_against_no_token.passed(),
            "the edited path must now capture from this body"
        );
        assert_eq!(
            report_against_no_token.values().get("auth_token"),
            Some(&"x".to_string())
        );

        // Now actually save it, and prove the exact same thing against the
        // request sitting in the loaded document afterward.
        update(&mut state, Message::SaveEdit);
        let saved_captures = saved_request(&state).capture.clone().unwrap();
        assert!(!saved_captures.evaluate(&has_token, &environment).passed());
        assert!(saved_captures.evaluate(&no_token, &environment).passed());
    }

    /// The same proof, for deleting a row: once removed, `evaluate` must
    /// report nothing at all rather than a lingering capture — an empty
    /// report is a different, real outcome (`CaptureReport::is_empty`), not
    /// the same "passed" a capture that still exists but happens to succeed
    /// would report.
    #[test]
    fn deleting_a_capture_row_and_saving_removes_it_from_what_evaluate_checks() {
        let mut state = loaded_state(REQUEST_WITH_JSON_PATH_CAPTURE);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::CaptureName(0);

        update(&mut state, Message::DeleteCaptureRow);
        update(&mut state, Message::SaveEdit);

        assert_eq!(
            saved_request(&state).capture,
            None,
            "deleting the only capture must leave nothing behind"
        );
        // Confirmed the same way through evaluation itself, not only the
        // struct: an absent `capture:` block is `Captures::default()`, which
        // evaluates to an empty report that trivially "passed".
        let report = Captures::default().evaluate(
            &response_with_headers(200, &[], "anything"),
            &Environment::default(),
        );
        assert!(report.is_empty());
        assert!(report.passed());
    }

    /// The same proof, for adding a brand-new header capture: a real header
    /// value that genuinely gets pulled out of the response once evaluated
    /// for real, and a genuine `HeaderNotFound` failure when it isn't there.
    #[test]
    fn adding_a_header_capture_and_saving_actually_captures_it() {
        let mut state = loaded_state(REQUEST_WITH_NO_CAPTURE);
        update(&mut state, Message::EnterEditMode);

        update(&mut state, Message::AddCaptureRow);
        type_into_focused_field(&mut state, "trace");
        update(&mut state, Message::EditFocusNext); // -> kind
        update(&mut state, Message::EditCursorRight); // JsonPath -> Header
        update(&mut state, Message::EditFocusNext); // -> value
        type_into_focused_field(&mut state, "X-Trace-Id");

        update(&mut state, Message::SaveEdit);

        let capture = saved_request(&state).capture.clone().unwrap();
        let environment = Environment::default();
        let with_header = response_with_headers(200, &[("X-Trace-Id", "abc-123")], "{}");
        let without_header = response_with_headers(200, &[], "{}");
        let passing = capture.evaluate(&with_header, &environment);
        assert!(passing.passed(), "the header is genuinely present");
        assert_eq!(passing.values().get("trace"), Some(&"abc-123".to_string()));
        assert!(
            !capture.evaluate(&without_header, &environment).passed(),
            "with no such header, the capture genuinely fails"
        );
    }

    // --- Persisting to disk --------------------------------------------------

    /// Builds a `state` whose `LoadState::Loaded` really points at `path` on
    /// disk (unlike `loaded_state`'s own shared scratch file, this lets a
    /// test control exactly what's on disk before and after `SaveEdit`) —
    /// shared by every test below that needs to inspect real bytes on a real
    /// filesystem rather than just the in-memory `Document`.
    fn state_loaded_from(path: &std::path::Path) -> AppState {
        let mut state = AppState::default();
        let document = Document::from_path(path).expect("the fixture file must parse");
        update(
            &mut state,
            Message::CollectionLoaded {
                base_dir: path.parent().unwrap().to_path_buf(),
                path: path.to_path_buf(),
                result: Box::new(Ok(document)),
            },
        );
        state
    }

    /// Proof requirement: a saved edit must actually reach disk, not just the
    /// in-memory `Document` — verified by reloading the file with a brand-new
    /// `Document::from_path` call, the same thing a freshly started process
    /// would do, rather than reading anything still sitting in `state`.
    #[test]
    fn save_edit_actually_persists_to_disk_and_a_fresh_load_from_disk_sees_it() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = dir.path().join("collection.yaml");
        std::fs::write(&path, "method: GET\nurl: https://example.com\n").unwrap();

        let mut state = state_loaded_from(&path);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::Url;
        backspace_n(&mut state, "https://example.com".len());
        type_into_focused_field(&mut state, "https://example.com/changed");

        update(&mut state, Message::SaveEdit);

        assert!(
            state.edit_mode.is_none(),
            "a successful save must leave edit mode"
        );
        assert!(state.dirty_requests.is_empty());

        // The proof itself: a fresh `Document::from_path`, not `state`.
        let reloaded = Document::from_path(&path).expect("the saved file must exist and parse");
        assert_eq!(reloaded.requests()[0].url, "https://example.com/changed");
    }

    /// Formatting/comment-preservation policy, demonstrated live rather than
    /// only documented: saving reformats the whole file through
    /// `serde_yaml`'s own writer, so hand-written comments (and any custom
    /// spacing/quoting/key order) do not survive a save. This is the
    /// documented, deliberate choice `Document::to_yaml_string`'s own doc
    /// comment and `save_to_path`'s make: real YAML comment-preserving
    /// round-tripping is a much larger undertaking (it needs a CST-based
    /// writer, not `serde`), and reformatting the whole file is the honest,
    /// simple alternative — never silently assumed, always visible right
    /// here as a real `#`-stripped file on disk.
    #[test]
    fn saving_reformats_the_whole_file_and_drops_hand_written_comments() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = dir.path().join("collection.yaml");
        let original = "\
# This collection talks to the staging API -- do not point it at prod.
name: test
requests:
  - name: One  # the only request for now
    method: GET
    url: https://example.com
";
        std::fs::write(&path, original).unwrap();

        let mut state = state_loaded_from(&path);
        update(&mut state, Message::EnterEditMode);
        // Save with no changes at all: even an edit session that touched
        // nothing still rewrites the file through the same serializer, so
        // this isolates "saving reformats" from "saving also changed a
        // value".
        update(&mut state, Message::SaveEdit);

        assert!(
            state.edit_mode.is_none(),
            "an unmodified save must still succeed"
        );
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved.contains('#'),
            "comments are not preserved through a save — a real, visible \
             consequence of this policy, not just a claim about it: {saved}"
        );

        // The data itself is completely intact — only the formatting/comments
        // are gone.
        let reloaded = Document::from_path(&path).unwrap();
        assert_eq!(reloaded.requests()[0].name.as_deref(), Some("One"));
        assert_eq!(reloaded.requests()[0].url, "https://example.com");
    }

    /// Proof requirement: a failed write must never lose the edit, never
    /// silently mark it clean, and must show a real error — reusing
    /// `format_error`'s own inline rendering via `EditState::save_error`
    /// (see its doc comment), not a new, separate error-display path.
    /// Forces a real, deterministic failure the same way sendra-core's own
    /// `save_to_path` tests do: the destination is a directory, so the final
    /// rename genuinely fails on both POSIX and Windows.
    #[test]
    fn a_failed_disk_write_keeps_the_edit_dirty_and_surfaces_a_real_error_without_losing_it() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = dir.path().join("collection.yaml");
        std::fs::create_dir(&path).expect("a directory blocking the target path");

        let mut state = AppState::default();
        let document = Document::from_yaml_str("method: GET\nurl: https://example.com\n").unwrap();
        update(
            &mut state,
            Message::CollectionLoaded {
                base_dir: dir.path().to_path_buf(),
                path: path.clone(),
                result: Box::new(Ok(document)),
            },
        );

        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::Url;
        backspace_n(&mut state, "https://example.com".len());
        type_into_focused_field(&mut state, "https://example.com/changed");

        update(&mut state, Message::SaveEdit);

        assert!(
            state.edit_mode.is_some(),
            "a failed save must not discard the edit the way CancelEdit would"
        );
        assert!(
            state.dirty_requests.contains(&0),
            "a failed save must leave the request marked dirty, not silently clean"
        );
        let error = state
            .edit_mode
            .as_ref()
            .unwrap()
            .save_error
            .as_ref()
            .expect("a failed save must surface a real error message");
        assert!(!error.is_empty());

        // The typed edit itself is completely intact, ready to retry.
        assert_eq!(
            state.edit_mode.as_ref().unwrap().url.value(),
            "https://example.com/changed"
        );

        // The loaded document was never replaced by the failed candidate —
        // it still holds exactly what it did before this `SaveEdit` at all.
        match &state.load_state {
            LoadState::Loaded { document, .. } => {
                assert_eq!(document.requests()[0].url, "https://example.com");
            }
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        }

        // Removing the obstruction and retrying the exact same edit must now
        // succeed — proof the edit was never lost, only blocked.
        std::fs::remove_dir(&path).expect("removing the blocking directory");
        update(&mut state, Message::SaveEdit);

        assert!(
            state.edit_mode.is_none(),
            "retrying after the fix must succeed"
        );
        assert!(state.dirty_requests.is_empty());
        let reloaded =
            Document::from_path(&path).expect("the retried save must have written a real file");
        assert_eq!(reloaded.requests()[0].url, "https://example.com/changed");
    }

    /// A fresh `Message::SaveEdit` attempt clears a stale `save_error` from a
    /// previous failed one before deciding whether this attempt can even
    /// proceed — so a validation failure (an invalid method, say) on a retry
    /// shows *that* message, not a leftover disk error from before it was
    /// fixed.
    #[test]
    fn a_new_save_attempt_clears_a_stale_save_error_even_if_this_attempt_also_fails_validation() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = dir.path().join("collection.yaml");
        std::fs::create_dir(&path).expect("a directory blocking the target path");

        let mut state = AppState::default();
        let document = Document::from_yaml_str("method: GET\nurl: https://example.com\n").unwrap();
        update(
            &mut state,
            Message::CollectionLoaded {
                base_dir: dir.path().to_path_buf(),
                path: path.clone(),
                result: Box::new(Ok(document)),
            },
        );

        update(&mut state, Message::EnterEditMode);
        update(&mut state, Message::SaveEdit);
        assert!(
            state.edit_mode.as_ref().unwrap().save_error.is_some(),
            "the blocked write must have set a save error"
        );

        // Now also break validation, without fixing the blocked directory.
        state.edit_mode.as_mut().unwrap().focus = EditField::Method;
        type_into_focused_field(&mut state, "x");
        update(&mut state, Message::SaveEdit);

        assert!(
            state.edit_mode.as_ref().unwrap().method_error.is_some(),
            "an invalid method must still block this save attempt"
        );
        assert!(
            state.edit_mode.as_ref().unwrap().save_error.is_none(),
            "a fresh attempt must clear the stale disk error even though it also failed, so the \
             pane shows the current, real reason this save didn't go through"
        );
    }
}
