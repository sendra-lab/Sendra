//! Message handlers for a request's own edit session:
//! `EnterEditMode`/`SaveEdit`/`CancelEdit`, `AddRequest`,
//! `RequestDelete`/`ConfirmDelete`/`CancelDelete`, plus every helper they
//! build on — including `try_save_edit`/`try_delete_request` — and the
//! shared `edit_mutate`/`edit_state_mutate`/`edit_move`/`edit_move_or_toggle`
//! machinery every focused-field message (`EditInsertChar`, `AddHeaderRow`,
//! ...) routes through while a request edit session is open.

use std::path::PathBuf;

use sendra_core::{Collection, Document, Request, SendraError};

use crate::app::state::{
    non_empty, validate_assertion_value_text, validate_method_text, BodyEdit, CollectionSession,
    ConfirmPrompt, DeleteConfirm, EditField, EditState, LoadState, PendingNewRequest, RunState,
    TextField,
};

use super::{reindex_dirty_after_delete, reindex_history_after_delete};

/// `Message::EnterEditMode`. A no-op (see `super::update`) unless a request
/// is actually selected, the environment overlay is closed, and no run is in
/// flight: edit mode is exclusive with those, the same way the environment
/// overlay and an in-flight run are already exclusive with each other and
/// with browsing.
pub(crate) fn handle_enter_edit_mode(state: &mut CollectionSession) {
    if state.edit_mode.is_none()
        && state.environment_overlay.is_none()
        && !matches!(state.run_state, RunState::InFlight)
    {
        if let Some(request) = selected_request(state) {
            state.edit_mode = Some(EditState::new(request));
        }
    }
}

/// `Message::SaveEdit`. Body validation happens exactly here, once per save
/// attempt — see `EditState::body_error`'s doc comment for why it is never
/// computed on every keystroke the way `method_error` is. A fresh attempt
/// also clears any stale `save_error` from a previous failed one.
pub(crate) fn handle_save_edit(state: &mut CollectionSession) {
    if let Some(edit) = &mut state.edit_mode {
        edit.body_error = validate_body_for_save(&edit.body);
        edit.save_error = None;
    }
    // Refuses to save — but, deliberately, does *not* clear `edit_mode` —
    // while `method_error`, `body_error`, or any assertion row's
    // `value_error` is set: invalid input must never reach the loaded
    // document, but the user must not be locked out of fixing it, which
    // dropping `edit_mode` here (as `CancelEdit` does) would do by
    // discarding what they typed.
    let can_save = state.edit_mode.as_ref().is_some_and(|edit| {
        edit.method_error.is_none()
            && edit.body_error.is_none()
            && edit.assertions.iter().all(|row| row.value_error.is_none())
    });
    if can_save {
        let attempt = state
            .edit_mode
            .as_ref()
            .and_then(|edit| try_save_edit(&state.load_state, edit));
        match attempt {
            Some(Ok(saved)) => {
                state.load_state = LoadState::Loaded {
                    document: Box::new(saved.document),
                    selected: saved.selected,
                    base_dir: saved.base_dir,
                    path: saved.path,
                };
                state.dirty_requests.remove(&saved.selected);
                state.edit_mode = None;
                // Whatever `Message::AddRequest` might have staged for
                // `CancelEdit` to undo no longer applies — the request (new
                // or not) is now genuinely saved, so there is nothing left
                // to roll back.
                state.pending_new_request = None;
            }
            Some(Err(error)) => {
                if let Some(edit) = &mut state.edit_mode {
                    edit.save_error = Some(error.to_string());
                }
            }
            None => {}
        }
    }
}

/// `Message::CancelEdit`. Dropping `edit_mode` here is the entire mechanism
/// for an edit of a pre-existing request: `method`/`url`/etc. only ever
/// lived in that working copy, never written into the loaded document until
/// `SaveEdit`, so there is nothing else to undo. `Message::AddRequest` is
/// the one case where something *was* already changed for real before
/// `SaveEdit` — see `PendingNewRequest`'s own doc comment — so undoing it
/// means restoring the whole `Document` from just before that insertion.
pub(crate) fn handle_cancel_edit(state: &mut CollectionSession) {
    if state.edit_mode.take().is_some() {
        match state.pending_new_request.take() {
            Some(pending) => {
                if let LoadState::Loaded {
                    document, selected, ..
                } = &mut state.load_state
                {
                    state.dirty_requests.remove(selected);
                    *document = pending.document_before;
                    *selected = pending.previous_selected;
                }
            }
            None => {
                if let LoadState::Loaded { selected, .. } = &state.load_state {
                    state.dirty_requests.remove(selected);
                }
            }
        }
    }
}

/// `Message::AddRequest`. Same guard `EnterEditMode` uses, for the same
/// reason: adding a request also immediately opens an edit session, so it is
/// exclusive with the overlay and an in-flight run the same way.
pub(crate) fn handle_add_request(state: &mut CollectionSession) {
    if state.edit_mode.is_none()
        && state.environment_overlay.is_none()
        && !matches!(state.run_state, RunState::InFlight)
    {
        if let LoadState::Loaded {
            document, selected, ..
        } = &mut state.load_state
        {
            // Snapshotted *before* the insertion — see `PendingNewRequest`'s
            // own doc comment for why `Message::CancelEdit` needs the whole
            // `Document` as it was, not just "the new request's index, to
            // remove".
            let document_before = Box::new((**document).clone());
            let previous_selected = *selected;

            let index = add_request_to_document(document);
            *selected = index;
            state.dirty_requests.insert(index);
            state.pending_new_request = Some(PendingNewRequest {
                previous_selected,
                document_before,
            });
            // A different request is now selected, the same reset
            // `select()` already applies when the selection moves.
            state.run_state = RunState::Idle;
            state.response_scroll = 0;
            state.reveal_captures = false;

            if let Some(new_request) = document.requests().get(index) {
                state.edit_mode = Some(EditState::new(new_request));
            }
        }
    }
}

/// `Message::RequestDelete`. Same guard `EnterEditMode`/`AddRequest` use,
/// for the same reason: deletion also opens a modal prompt, exclusive with
/// the overlay, edit mode and an in-flight run the same way.
pub(crate) fn handle_request_delete(state: &mut CollectionSession) {
    if state.edit_mode.is_none()
        && state.environment_overlay.is_none()
        && state.delete_confirm.is_none()
        && !matches!(state.run_state, RunState::InFlight)
    {
        if let LoadState::Loaded {
            document, selected, ..
        } = &state.load_state
        {
            if can_delete(document, *selected) {
                let name = document
                    .requests()
                    .get(*selected)
                    .and_then(|request| request.name.as_deref())
                    .unwrap_or("(unnamed)");
                state.delete_confirm = Some(DeleteConfirm {
                    index: *selected,
                    prompt: ConfirmPrompt::new(format!("Delete '{name}'? This cannot be undone.")),
                });
            }
        }
    }
}

/// `Message::CancelDelete`. Dropping `delete_confirm` is the entire
/// mechanism: nothing real is ever mutated before `ConfirmDelete` actually
/// runs, so there is nothing else to undo.
pub(crate) fn handle_cancel_delete(state: &mut CollectionSession) {
    state.delete_confirm = None;
}

/// `Message::ConfirmDelete`.
pub(crate) fn handle_confirm_delete(state: &mut CollectionSession) {
    if let Some(confirm) = &state.delete_confirm {
        let index = confirm.index;
        // `try_delete_request` attempts the write against a clone before
        // anything in `state` changes — the same "nothing happens until
        // it's known to work" shape `try_save_edit` uses.
        match try_delete_request(&state.load_state, index) {
            Some(Ok(saved)) => {
                state.load_state = LoadState::Loaded {
                    document: Box::new(saved.document),
                    selected: saved.selected,
                    base_dir: saved.base_dir,
                    path: saved.path,
                };
                state.dirty_requests = reindex_dirty_after_delete(&state.dirty_requests, index);
                state.run_history =
                    reindex_history_after_delete(std::mem::take(&mut state.run_history), index);
                state.run_history_dropped = reindex_history_after_delete(
                    std::mem::take(&mut state.run_history_dropped),
                    index,
                );
                state.delete_confirm = None;
                // A different (or no longer any) request is now selected,
                // the same reset `select()` already applies when the
                // selection moves.
                state.run_state = RunState::Idle;
                state.response_scroll = 0;
                state.reveal_captures = false;
            }
            Some(Err(error)) => {
                if let Some(confirm) = &mut state.delete_confirm {
                    confirm.prompt.error = Some(error.to_string());
                }
            }
            // The document changed shape out from under the prompt (or
            // deleting is no longer valid, e.g. down to the last request)
            // between `RequestDelete` and this — nothing to do but close it
            // rather than act on stale state.
            None => state.delete_confirm = None,
        }
    }
}

/// Whether the collection browser currently has a request selected — true
/// exactly when `load_state` is `Loaded` and `selected` indexes a real
/// request, which is always the case for a non-empty collection but not for
/// an empty one.
pub(crate) fn request_is_selected(state: &CollectionSession) -> bool {
    matches!(
        &state.load_state,
        LoadState::Loaded { document, selected, .. } if document.requests().get(*selected).is_some()
    )
}

/// The currently selected request itself, when there is one — the same
/// condition `request_is_selected` checks, but handing back the `&Request`
/// `Message::EnterEditMode` needs to seed `EditState::new` from, instead of
/// just the bool.
fn selected_request(state: &CollectionSession) -> Option<&Request> {
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

/// Whether the request at `index` in `document` can be deleted at all —
/// `Document::Single` never (there is no valid document to land on with
/// zero requests), and a `Document::Collection` only while it holds more
/// than one request (`Collection::validate` rejects an empty `requests`
/// list) and `index` is actually in range.
fn can_delete(document: &Document, index: usize) -> bool {
    match document {
        Document::Single(_) => false,
        Document::Collection(collection) => {
            collection.requests.len() > 1 && index < collection.requests.len()
        }
    }
}

/// Removes the request at `index` from `document` — a no-op for
/// `Document::Single` or an out-of-range `index`, both of which
/// `Message::ConfirmDelete` already refuses via `can_delete` before this is
/// ever called.
fn remove_request(document: &mut Document, index: usize) {
    if let Document::Collection(collection) = document {
        if index < collection.requests.len() {
            collection.requests.remove(index);
        }
    }
}

/// Where the collection browser's selection lands right after deleting the
/// request at `deleted_index`, given `new_len` — the document's request
/// count *after* the removal. Selects the previous request rather than the
/// one that slid into the deleted slot, or `0` when the first request was
/// the one deleted.
fn new_selection_after_delete(deleted_index: usize, new_len: usize) -> usize {
    if deleted_index == 0 {
        0
    } else {
        (deleted_index - 1).min(new_len.saturating_sub(1))
    }
}

/// Appends a brand-new request to `document` and returns its index —
/// visible to `Message::AddRequest`'s handler.
///
/// `sendra_core::Document::Single` holds exactly one `Request`, not a list,
/// so it cannot itself grow a second one: adding to a `Document::Single`
/// therefore first turns it into a `Document::Collection` holding both
/// requests (the original, given a synthesized `name` if it didn't already
/// have one, and the new one). A `Document::Collection` simply gets the new
/// request pushed onto its existing `requests`.
fn add_request_to_document(document: &mut Document) -> usize {
    let mut new_request = Request::from_yaml_str("method: GET\nurl: \"\"\n")
        .expect("a bare GET request with an empty url is always a valid Request");

    match document {
        Document::Single(existing) => {
            let mut converted = existing.clone();
            let existing_name = converted.name.clone().unwrap_or_else(|| converted.label());
            converted.name = Some(existing_name.clone());

            new_request.name = Some(unique_request_name(
                "New request",
                std::slice::from_ref(&existing_name),
            ));

            *document = Document::Collection(Collection {
                name: None,
                requests: vec![converted, new_request],
            });
            1
        }
        Document::Collection(collection) => {
            let existing_names: Vec<String> = collection
                .requests
                .iter()
                .filter_map(|request| request.name.clone())
                .collect();
            new_request.name = Some(unique_request_name("New request", &existing_names));
            collection.requests.push(new_request);
            collection.requests.len() - 1
        }
    }
}

/// `base`, or `base 2`/`base 3`/... — whichever is the first not already
/// present in `existing` — so a synthesized request name never collides with
/// one already in the collection.
fn unique_request_name(base: &str, existing: &[String]) -> String {
    if !existing.iter().any(|name| name == base) {
        return base.to_string();
    }
    let mut suffix = 2;
    loop {
        let candidate = format!("{base} {suffix}");
        if !existing.iter().any(|name| name == &candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

/// Writes every field `EditState` can hold a working copy of — `method`,
/// `url`, `headers`, `body`, `auth`, `assertions`, `capture` — into
/// `request`. The one place this happens, shared between `Message::SaveEdit`'s
/// real save and (indirectly, via `EditState::to_request`) the live
/// "resolved auth" preview.
fn apply_edit_to_request(request: &mut Request, edit: &EditState) {
    request.name = non_empty(edit.name.value());
    // `method_error` is confirmed `None` before this is ever called — see
    // `Message::SaveEdit`'s own `can_save` check — so this cannot fail;
    // matched rather than trusted blindly.
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
/// called only from `Message::SaveEdit`, never on every keystroke. `None`
/// covers three cases that are all "nothing to reject": the body isn't in
/// `json:` mode at all, it's `Unsupported`, or the text is empty/blank.
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
/// `Unsupported` bodies are left completely untouched.
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
        if let Ok(value) = serde_json::from_str(text.value()) {
            request.json = Some(value);
            request.body = None;
        }
    } else {
        request.body = Some(text.value().to_string());
        request.json = None;
    }
}

/// The result of a successful `Message::SaveEdit` write: the freshly saved
/// `Document` plus the `selected`/`base_dir`/`path` triple `LoadState::Loaded`
/// needs alongside it.
struct SavedEdit {
    document: Document,
    selected: usize,
    base_dir: PathBuf,
    path: PathBuf,
}

/// Builds the candidate `Document` `edit` describes — clone the loaded one,
/// apply `edit`'s fields onto the selected request via `apply_edit_to_request`
/// — and attempts to save it. Pure with respect to `AppState`: a failed write
/// is provably a no-op on the caller's own state, since the candidate here
/// was always a clone, never written back.
fn try_save_edit(
    load_state: &LoadState,
    edit: &EditState,
) -> Option<Result<SavedEdit, SendraError>> {
    let LoadState::Loaded {
        document,
        selected,
        base_dir,
        path,
    } = load_state
    else {
        return None;
    };
    let selected = *selected;
    let mut candidate = (**document).clone();
    if let Some(request) = request_mut(&mut candidate, selected) {
        apply_edit_to_request(request, edit);
    }

    Some(candidate.save_to_path(path).map(|()| SavedEdit {
        document: candidate,
        selected,
        base_dir: base_dir.clone(),
        path: path.clone(),
    }))
}

/// The result of a successful `Message::ConfirmDelete` write: the freshly
/// saved `Document` (with the request removed), the selection that should
/// land after it, plus the `base_dir`/`path` pair `LoadState::Loaded` needs
/// alongside them.
struct SavedDelete {
    document: Document,
    selected: usize,
    base_dir: PathBuf,
    path: PathBuf,
}

/// Builds the candidate `Document` with the request at `index` removed —
/// clone the loaded one, `remove_request` — and attempts to save it. Pure
/// with respect to `AppState`, the same shape and the same reason as
/// `try_save_edit` above.
fn try_delete_request(
    load_state: &LoadState,
    index: usize,
) -> Option<Result<SavedDelete, SendraError>> {
    let LoadState::Loaded {
        document,
        base_dir,
        path,
        ..
    } = load_state
    else {
        return None;
    };
    if !can_delete(document, index) {
        return None;
    }
    let mut candidate = (**document).clone();
    remove_request(&mut candidate, index);
    let selected = new_selection_after_delete(index, candidate.requests().len());

    Some(candidate.save_to_path(path).map(|()| SavedDelete {
        document: candidate,
        selected,
        base_dir: base_dir.clone(),
        path: path.clone(),
    }))
}

/// Applies `mutate` to whichever field `EditState::focus` currently points
/// at, then marks the edit (and the request being edited) dirty and, if the
/// method field is the one that just changed, recomputes `method_error` —
/// live validation on every keystroke. A no-op when edit mode is not active.
pub(crate) fn edit_mutate(state: &mut CollectionSession, mutate: impl FnOnce(&mut TextField)) {
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
    // just was — see its own doc comment — but a stale error from a previous
    // failed save must not keep showing once the user has started fixing it.
    if edit.focus == EditField::Body {
        edit.body_error = None;
    }
    // Unlike `body_error`, an assertion row's `value_error` *is* recomputed
    // on every keystroke, the same as `method_error`.
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
/// `DeleteHeaderRow` (and the assertion/capture row equivalents) use, since
/// adding or removing a whole row is still an edit but isn't a `TextField`
/// operation.
pub(crate) fn edit_state_mutate(
    state: &mut CollectionSession,
    mutate: impl FnOnce(&mut EditState),
) {
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
pub(crate) fn edit_move(state: &mut CollectionSession, mutate: impl FnOnce(&mut TextField)) {
    if let Some(edit) = &mut state.edit_mode {
        if let Some(field) = edit.focused_field_mut() {
            mutate(field);
        }
    }
}

/// `Left`/`Right` on the focused field: ordinarily cursor movement, exactly
/// like `edit_move` — but a handful of fields are fixed enums with no
/// `TextField`/cursor to move through at all, so while one of those has
/// focus, these same two keys instead toggle its value.
pub(crate) fn edit_move_or_toggle(
    state: &mut CollectionSession,
    forward: bool,
    mutate: impl FnOnce(&mut TextField),
) {
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

#[cfg(test)]
mod tests {
    use sendra_core::{
        ApiKeyLocation, Assertions, CaptureSource, Captures, Document, Environment, Method,
        Request, Response,
    };

    use crate::app::state::{AppState, AuthEdit, AuthField, CaptureKind, JsonOperator, Message};
    use crate::app::test_support::*;
    use crate::app::update::update;
    use crate::app::view::status_help_text;

    use super::*;

    // --- Naming a request -----------------------------------------------------

    #[test]
    fn enter_edit_mode_seeds_the_name_field_from_the_real_request() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);

        update(&mut state, Message::EnterEditMode);

        let edit = state.edit_mode.as_ref().unwrap();
        assert_eq!(edit.name.value(), "One");
        assert_eq!(edit.focus, EditField::Name, "Name is the default focus");
    }

    #[test]
    fn a_request_with_no_name_seeds_an_empty_name_field() {
        let mut state = loaded_state(REQUEST_WITHOUT_A_NAME);

        update(&mut state, Message::EnterEditMode);

        assert_eq!(state.edit_mode.as_ref().unwrap().name.value(), "");
    }

    #[test]
    fn typing_into_the_name_field_edits_the_working_copy_and_marks_dirty() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::EnterEditMode);

        type_into_focused_field(&mut state, " (renamed)");

        let edit = state.edit_mode.as_ref().unwrap();
        assert_eq!(edit.name.value(), "One (renamed)");
        assert!(edit.dirty);
        assert!(state.dirty_requests.contains(&0));
    }

    #[test]
    fn save_edit_writes_the_new_name_into_the_loaded_document() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::EnterEditMode);
        backspace_n(&mut state, "One".len());
        type_into_focused_field(&mut state, "Renamed");

        update(&mut state, Message::SaveEdit);

        assert!(
            state.edit_mode.is_none(),
            "a real, unique name must save cleanly"
        );
        assert_eq!(saved_request(&state).name.as_deref(), Some("Renamed"));
    }

    #[test]
    fn clearing_the_name_on_a_standalone_request_saves_as_no_name_at_all() {
        // `REQUEST_WITHOUT_A_NAME` is a `Document::Single`, where a name is
        // entirely optional (`Collection::validate`'s "every request must be
        // named" rule only applies inside a collection) — so an empty Name
        // field here must save as genuinely `None`, not as the request
        // having a name that happens to be the empty string.
        let mut state = loaded_state(REQUEST_WITHOUT_A_NAME);
        update(&mut state, Message::EnterEditMode);
        type_into_focused_field(&mut state, "a name, then cleared");
        backspace_n(&mut state, "a name, then cleared".len());
        assert_eq!(state.edit_mode.as_ref().unwrap().name.value(), "");

        update(&mut state, Message::SaveEdit);

        assert!(state.edit_mode.is_none());
        assert_eq!(saved_request(&state).name, None);
    }

    /// Proof requirement: renaming a request inside a real collection and
    /// saving must actually reach disk, verified with a fresh
    /// `Document::from_path` rather than anything still held in `state`.
    #[test]
    fn renaming_a_request_and_saving_persists_the_new_name_to_disk() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = dir.path().join("collection.yaml");
        std::fs::write(
            &path,
            "name: test\nrequests:\n  - name: One\n    method: GET\n    url: https://example.com\n",
        )
        .unwrap();

        let mut state = state_loaded_from(&path);
        update(&mut state, Message::EnterEditMode);
        backspace_n(&mut state, "One".len());
        type_into_focused_field(&mut state, "Renamed");
        update(&mut state, Message::SaveEdit);
        assert!(state.edit_mode.is_none());

        let reloaded = Document::from_path(&path).expect("the saved file must exist and parse");
        assert_eq!(reloaded.requests()[0].name.as_deref(), Some("Renamed"));
    }

    /// The rule this whole feature has to respect, proven live: clearing a
    /// request's name down to empty inside a collection must be refused at
    /// save time — reusing `Document::save_to_path`'s own `validate` check,
    /// which `Collection::validate` requires every request in a collection
    /// to have a name for — rather than silently writing a file that would
    /// fail to load the very next time anything opens it. Surfaced through
    /// the exact same `save_error`/`format_error` path a disk-level failure
    /// already uses; no separate "name is required" check was added to
    /// `EditState`/`EditField::Name` itself (see that variant's own doc
    /// comment for why).
    #[test]
    fn save_edit_refuses_to_leave_a_collection_request_unnamed() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::EnterEditMode);
        backspace_n(&mut state, "One".len());

        update(&mut state, Message::SaveEdit);

        assert!(
            state.edit_mode.is_some(),
            "an invalid save must not discard the edit"
        );
        assert!(state.dirty_requests.contains(&0));
        let error = state
            .edit_mode
            .as_ref()
            .unwrap()
            .save_error
            .as_ref()
            .expect("clearing the name of a request inside a collection must be refused");
        assert!(!error.is_empty());
        // The loaded document must still hold the original, valid name.
        assert_eq!(saved_request(&state).name.as_deref(), Some("One"));

        // Fixing it — giving it back a real name — must now save cleanly.
        type_into_focused_field(&mut state, "One Again");
        update(&mut state, Message::SaveEdit);
        assert!(state.edit_mode.is_none());
        assert_eq!(saved_request(&state).name.as_deref(), Some("One Again"));
    }

    #[test]
    fn invalid_method_shows_an_inline_error_blocks_save_and_stays_editable() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::EnterEditMode);
        update(&mut state, Message::EditFocusNext); // -> Method

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
        update(&mut state, Message::EditFocusNext); // Name -> Method
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
    fn tab_wraps_from_the_body_field_back_to_name() {
        let mut state = loaded_state(REQUEST_WITH_HEADERS);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::Body;

        update(&mut state, Message::EditFocusNext);

        assert_eq!(state.edit_mode.as_ref().unwrap().focus, EditField::Name);
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
        assert_eq!(state.edit_mode.as_ref().unwrap().focus, EditField::Name);

        update(&mut state, Message::DeleteHeaderRow);

        assert_eq!(
            state.edit_mode.as_ref().unwrap().headers.len(),
            2,
            "nothing should be deleted while focus is on name"
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

    fn focus_body(state: &mut CollectionSession) {
        state.edit_mode.as_mut().unwrap().focus = EditField::Body;
    }

    fn saved_request(state: &CollectionSession) -> &Request {
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
    fn body_text_len(state: &CollectionSession) -> usize {
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
    fn tab_moves_focus_from_body_into_the_bearer_token_field_and_wraps_to_name() {
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
            EditField::Name,
            "the last (only) auth field wraps back to Name"
        );
    }

    #[test]
    fn shift_tab_from_name_moves_into_the_last_auth_field() {
        let mut state = loaded_state(REQUEST_WITH_API_KEY_AUTH);
        update(&mut state, Message::EnterEditMode);

        update(&mut state, Message::EditFocusPrev);

        assert_eq!(
            state.edit_mode.as_ref().unwrap().focus,
            EditField::Auth(AuthField::ApiKeyLocation),
            "Shift+Tab from Name must land on the last auth field"
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
        assert_eq!(state.edit_mode.as_ref().unwrap().focus, EditField::Name);

        update(&mut state, Message::DeleteAssertionRow);

        assert_eq!(
            state.edit_mode.as_ref().unwrap().assertions.len(),
            1,
            "nothing should be deleted while focus is on name"
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
            EditField::Name,
            "the last assertion row's negate flag wraps back to Name"
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
        assert_eq!(state.edit_mode.as_ref().unwrap().focus, EditField::Name);

        update(&mut state, Message::DeleteCaptureRow);

        assert_eq!(
            state.edit_mode.as_ref().unwrap().captures.len(),
            1,
            "nothing should be deleted while focus is on name"
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
            EditField::Name,
            "the only capture row's value field wraps back to Name"
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

    // --- Adding a request -----------------------------------------------------

    const REQUEST_WITHOUT_A_NAME: &str = "method: GET\nurl: https://example.com\n";

    /// Proof requirement: adding to a `Document::Single` — investigated
    /// directly against `sendra_core::Document`'s own definition (`Single`
    /// holds exactly one `Request`, not a list — see `add_request_to_
    /// document`'s own doc comment) rather than assumed — must convert it
    /// into a real `Document::Collection` holding both requests, the
    /// original given a synthesized name since `Collection::validate`
    /// requires one.
    #[test]
    fn adding_a_request_to_a_document_single_converts_it_into_a_collection() {
        let mut state = loaded_state(REQUEST_WITHOUT_A_NAME);
        assert!(
            matches!(saved_document(&state), Document::Single(_)),
            "the fixture must start as a Single document for this test to mean anything"
        );

        update(&mut state, Message::AddRequest);

        match saved_document(&state) {
            Document::Collection(collection) => {
                assert_eq!(collection.requests.len(), 2);
                assert_eq!(
                    collection.requests[0].name.as_deref(),
                    Some("GET https://example.com"),
                    "the original, unnamed request must get a synthesized name (its own label)"
                );
                assert_eq!(collection.requests[0].url, "https://example.com");
                assert_eq!(collection.requests[1].name.as_deref(), Some("New request"));
                assert_eq!(collection.requests[1].method, Method::Get);
                assert_eq!(collection.requests[1].url, "");
            }
            other => panic!("expected Document::Collection after AddRequest, got {other:?}"),
        }
    }

    #[test]
    fn adding_a_request_to_an_existing_collection_appends_it() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);

        update(&mut state, Message::AddRequest);

        match saved_document(&state) {
            Document::Collection(collection) => {
                assert_eq!(collection.requests.len(), 4);
                assert_eq!(collection.requests[3].name.as_deref(), Some("New request"));
                assert_eq!(collection.names()[..3], ["One", "Two", "Three"]);
            }
            other => panic!("expected Document::Collection, got {other:?}"),
        }
    }

    #[test]
    fn adding_a_request_selects_it_and_opens_it_in_edit_mode_immediately() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);

        update(&mut state, Message::AddRequest);

        assert_eq!(
            selected(&state),
            3,
            "the new request must be the selected one"
        );
        let edit = state
            .edit_mode
            .as_ref()
            .expect("edit mode must open immediately");
        assert_eq!(edit.method.value(), "GET");
        assert_eq!(edit.url.value(), "");
        assert!(state.dirty_requests.contains(&3));
    }

    #[test]
    fn adding_a_request_twice_disambiguates_the_synthesized_names() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);

        update(&mut state, Message::AddRequest);
        update(&mut state, Message::SaveEdit); // commit the first before adding a second
        update(&mut state, Message::AddRequest);

        match saved_document(&state) {
            Document::Collection(collection) => {
                assert_eq!(
                    collection.names()[3..],
                    ["New request", "New request 2"],
                    "a second added request must not collide with the first's synthesized name"
                );
            }
            other => panic!("expected Document::Collection, got {other:?}"),
        }
    }

    #[test]
    fn cancelling_a_freshly_added_request_removes_it_and_restores_the_previous_selection() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::SelectNext); // select index 1 ("Two")
        assert_eq!(selected(&state), 1);

        update(&mut state, Message::AddRequest);
        assert_eq!(selected(&state), 3);

        update(&mut state, Message::CancelEdit);

        assert!(state.edit_mode.is_none());
        assert_eq!(
            selected(&state),
            1,
            "cancelling a fresh AddRequest must restore the selection it replaced"
        );
        match saved_document(&state) {
            Document::Collection(collection) => {
                assert_eq!(
                    collection.requests.len(),
                    3,
                    "the never-saved request must be gone entirely, not just deselected"
                );
                assert_eq!(collection.names(), ["One", "Two", "Three"]);
            }
            other => panic!("expected Document::Collection, got {other:?}"),
        }
        assert!(!state.dirty_requests.contains(&3));
    }

    /// The other half of the `Document::Single` proof: cancelling a request
    /// added to what started as a single-request file must undo the
    /// `Single` → `Collection` conversion too, not just remove the new
    /// request and leave the conversion (and the original's synthesized
    /// name) behind — see `PendingNewRequest`'s own doc comment for why a
    /// wholesale document restore, not an in-place removal, is what this
    /// takes.
    #[test]
    fn cancelling_a_request_added_to_a_document_single_restores_it_to_single() {
        let mut state = loaded_state(REQUEST_WITHOUT_A_NAME);

        update(&mut state, Message::AddRequest);
        assert!(matches!(saved_document(&state), Document::Collection(_)));

        update(&mut state, Message::CancelEdit);

        match saved_document(&state) {
            Document::Single(request) => {
                assert_eq!(
                    request.name, None,
                    "the original request's name must be untouched too"
                );
                assert_eq!(request.url, "https://example.com");
            }
            other => panic!("expected Document::Single restored, got {other:?}"),
        }
        assert_eq!(selected(&state), 0);
    }

    #[test]
    fn add_request_is_a_no_op_while_already_editing_or_overlay_open_or_run_in_flight() {
        let mut editing = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut editing, Message::EnterEditMode);
        update(&mut editing, Message::AddRequest);
        assert_eq!(
            editing.edit_mode.as_ref().unwrap().method.value(),
            "GET",
            "AddRequest must not replace an edit already in progress"
        );
        assert_eq!(saved_document(&editing).requests().len(), 3);

        let mut overlaid = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut overlaid, Message::OpenEnvironmentOverlay);
        update(&mut overlaid, Message::AddRequest);
        assert_eq!(saved_document(&overlaid).requests().len(), 3);
        assert!(overlaid.edit_mode.is_none());

        let mut running = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut running, Message::RunRequested);
        update(&mut running, Message::AddRequest);
        assert_eq!(saved_document(&running).requests().len(), 3);
        assert!(running.edit_mode.is_none());
    }

    /// Proof requirement, end to end: add a request, fill in its fields
    /// through the exact same editors issues 18-24 already built (method,
    /// URL, a header), save, and reload the collection with a brand-new
    /// `Document::from_path` — not anything still sitting in `state` — to
    /// confirm the new request genuinely reached disk with the fields it was
    /// given.
    #[test]
    fn adding_a_request_filling_it_in_and_saving_persists_it_to_disk() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = dir.path().join("collection.yaml");
        std::fs::write(
            &path,
            "name: test\nrequests:\n  - name: One\n    method: GET\n    url: https://example.com\n",
        )
        .unwrap();

        let mut state = state_loaded_from(&path);
        update(&mut state, Message::AddRequest);

        // Fill in the new request through the ordinary field editors — focus
        // starts on Name (`EditField::default`).
        update(&mut state, Message::EditFocusNext); // -> Method
        backspace_n(&mut state, "GET".len());
        type_into_focused_field(&mut state, "POST");
        update(&mut state, Message::EditFocusNext); // -> Url
        type_into_focused_field(&mut state, "https://example.com/new");
        update(&mut state, Message::AddHeaderRow);
        type_into_focused_field(&mut state, "X-Test");
        update(&mut state, Message::EditFocusNext); // -> header value
        type_into_focused_field(&mut state, "abc");

        update(&mut state, Message::SaveEdit);
        assert!(state.edit_mode.is_none(), "the save must succeed");

        // The proof itself: a fresh `Document::from_path`.
        let reloaded = Document::from_path(&path).expect("the saved file must exist and parse");
        let requests = reloaded.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].name.as_deref(), Some("One"));
        let new_request = &requests[1];
        assert_eq!(new_request.name.as_deref(), Some("New request"));
        assert_eq!(new_request.method, Method::Post);
        assert_eq!(new_request.url, "https://example.com/new");
        assert_eq!(
            new_request.header("X-Test"),
            Some("abc"),
            "the header added through the ordinary header editor must be saved too"
        );
    }

    /// The other end-to-end proof: a `Document::Single` file, a request
    /// added to it, filled in, saved — and a fresh reload must show a real
    /// `requests:` collection with both, on disk, not just in memory.
    #[test]
    fn adding_a_request_to_a_single_request_file_persists_as_a_real_collection_on_disk() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = dir.path().join("request.yaml");
        std::fs::write(&path, "method: GET\nurl: https://example.com\n").unwrap();

        let mut state = state_loaded_from(&path);
        update(&mut state, Message::AddRequest);
        update(&mut state, Message::EditFocusNext); // Name -> Method
        update(&mut state, Message::EditFocusNext); // Method -> Url
        type_into_focused_field(&mut state, "/new");
        update(&mut state, Message::SaveEdit);
        assert!(state.edit_mode.is_none());

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            on_disk.contains("requests:"),
            "a Document::Single file must become a real requests: collection on disk: {on_disk}"
        );

        let reloaded = Document::from_path(&path).expect("the saved file must exist and parse");
        match reloaded {
            Document::Collection(collection) => {
                assert_eq!(collection.requests.len(), 2);
                assert_eq!(collection.requests[0].url, "https://example.com");
                assert_eq!(collection.requests[1].url, "/new");
            }
            other => panic!("expected Document::Collection on disk, got {other:?}"),
        }
    }

    // --- Deleting a request -----------------------------------------------

    #[test]
    fn request_delete_opens_a_confirmation_prompt_and_touches_nothing_yet() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::SelectNext); // select index 1 ("Two")

        update(&mut state, Message::RequestDelete);

        let confirm = state
            .delete_confirm
            .as_ref()
            .expect("RequestDelete must open the confirmation prompt");
        assert_eq!(confirm.index, 1);
        assert!(confirm.prompt.error.is_none());
        // Nothing about the document or selection has moved yet — opening
        // the prompt is not itself a mutation.
        assert_eq!(saved_document(&state).requests().len(), 3);
        assert_eq!(selected(&state), 1);
    }

    /// Proof requirement: cancelling must leave the collection completely
    /// untouched — verified against real bytes on disk, not just the
    /// in-memory `Document`, since a partial or accidental write would still
    /// leave the in-memory state looking fine.
    #[test]
    fn cancelling_delete_leaves_the_file_byte_for_byte_unchanged_on_disk() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = dir.path().join("collection.yaml");
        std::fs::write(&path, THREE_REQUEST_COLLECTION).unwrap();
        let original_bytes = std::fs::read(&path).unwrap();

        let mut state = state_loaded_from(&path);
        update(&mut state, Message::SelectNext); // select index 1 ("Two")
        update(&mut state, Message::RequestDelete);
        assert!(state.delete_confirm.is_some());

        update(&mut state, Message::CancelDelete);

        assert!(state.delete_confirm.is_none());
        assert_eq!(saved_document(&state).requests().len(), 3);
        assert_eq!(
            selected(&state),
            1,
            "cancelling must not move the selection either"
        );
        let bytes_after = std::fs::read(&path).unwrap();
        assert_eq!(
            original_bytes, bytes_after,
            "cancelling a delete must never touch the file on disk"
        );
    }

    /// Proof requirement, end to end: confirm a delete, then reload the
    /// collection with a brand-new `Document::from_path` — not anything
    /// still sitting in `state` — to confirm the request is genuinely gone
    /// from disk and the other two are completely unaffected.
    #[test]
    fn confirming_delete_removes_the_request_and_a_reload_from_disk_shows_it_gone() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = dir.path().join("collection.yaml");
        std::fs::write(&path, THREE_REQUEST_COLLECTION).unwrap();

        let mut state = state_loaded_from(&path);
        update(&mut state, Message::SelectNext); // select index 1 ("Two")
        update(&mut state, Message::RequestDelete);

        update(&mut state, Message::ConfirmDelete);

        assert!(state.delete_confirm.is_none());
        assert_eq!(selected(&state), 0, "deleting selects the previous request");

        let reloaded = Document::from_path(&path).expect("the saved file must exist and parse");
        match reloaded {
            Document::Collection(collection) => {
                assert_eq!(collection.names(), ["One", "Three"]);
                assert_eq!(collection.requests[0].url, "https://example.com");
                assert_eq!(collection.requests[1].url, "https://example.com/three");
            }
            other => panic!("expected Document::Collection on disk, got {other:?}"),
        }
    }

    #[test]
    fn deleting_the_first_request_selects_index_zero_not_a_negative_previous() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = dir.path().join("collection.yaml");
        std::fs::write(&path, THREE_REQUEST_COLLECTION).unwrap();

        let mut state = state_loaded_from(&path);
        update(&mut state, Message::RequestDelete); // index 0 ("One")
        update(&mut state, Message::ConfirmDelete);

        assert_eq!(selected(&state), 0);
        let reloaded = Document::from_path(&path).unwrap();
        assert_eq!(reloaded.requests()[0].name.as_deref(), Some("Two"));
    }

    /// The investigation this issue asked for, demonstrated live:
    /// `Document::Single` cannot hold zero requests (there is no `Document`
    /// variant for an empty file — see `can_delete`'s own doc comment), so
    /// deletion is disabled outright rather than attempted and failed later.
    #[test]
    fn deleting_is_disabled_for_a_document_single() {
        let mut state = loaded_state("method: GET\nurl: https://example.com\n");
        assert!(matches!(saved_document(&state), Document::Single(_)));

        update(&mut state, Message::RequestDelete);

        assert!(
            state.delete_confirm.is_none(),
            "RequestDelete must be a no-op for a Document::Single"
        );
        assert!(matches!(saved_document(&state), Document::Single(_)));
    }

    /// The other half of the same investigation: `Collection::validate`
    /// rejects an empty `requests` list, so deleting a collection's last
    /// remaining request is refused the same way — the zero-requests-
    /// remaining state is therefore never actually reachable, handled
    /// without a crash by never letting it happen at all.
    #[test]
    fn deleting_the_last_remaining_request_in_a_collection_is_refused() {
        let mut state = loaded_state(
            "name: test\nrequests:\n  - name: Only\n    method: GET\n    url: https://example.com\n",
        );
        assert_eq!(saved_document(&state).requests().len(), 1);

        update(&mut state, Message::RequestDelete);

        assert!(
            state.delete_confirm.is_none(),
            "RequestDelete must be a no-op on a collection's last remaining request"
        );
        assert_eq!(saved_document(&state).requests().len(), 1);
    }

    /// A `Document::Collection` brought down to exactly one request *is*
    /// allowed — only zero is refused — and does not convert back into a
    /// `Document::Single` (see `can_delete`'s own doc comment for why that
    /// conversion is deliberately not mirrored from `add_request_to_document`).
    #[test]
    fn deleting_down_to_one_remaining_request_keeps_it_a_collection() {
        let mut state = loaded_state(
            "name: test\nrequests:\n  - name: One\n    method: GET\n    url: https://example.com\n  - name: Two\n    method: GET\n    url: https://example.com/two\n",
        );

        update(&mut state, Message::RequestDelete); // index 0 ("One")
        update(&mut state, Message::ConfirmDelete);

        match saved_document(&state) {
            Document::Collection(collection) => {
                assert_eq!(collection.names(), ["Two"]);
            }
            other => panic!(
                "a collection with one request remaining must stay a Collection, got {other:?}"
            ),
        }
    }

    /// Dirty markers must follow their request through the index shift a
    /// deletion causes — a marker on an index after the deleted one must
    /// move down by one to keep naming the same request; one on the deleted
    /// index itself must simply disappear.
    #[test]
    fn dirty_requests_are_reindexed_after_a_delete() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        state.dirty_requests.insert(2); // "Three" is marked dirty

        update(&mut state, Message::RequestDelete); // index 0 ("One")
        update(&mut state, Message::ConfirmDelete);

        let names: Vec<Option<&str>> = saved_document(&state)
            .requests()
            .iter()
            .map(|request| request.name.as_deref())
            .collect();
        assert_eq!(
            names,
            [Some("Two"), Some("Three")],
            "sanity: index 0 is really what got removed"
        );
        assert!(
            state.dirty_requests.contains(&1),
            "the dirty marker on the old index 2 (\"Three\") must follow it to its new index 1"
        );
        assert!(!state.dirty_requests.contains(&2));
    }

    #[test]
    fn request_delete_is_a_no_op_while_already_editing_or_overlay_open_or_run_in_flight() {
        let mut editing = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut editing, Message::EnterEditMode);
        update(&mut editing, Message::RequestDelete);
        assert!(editing.delete_confirm.is_none());

        let mut overlaid = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut overlaid, Message::OpenEnvironmentOverlay);
        update(&mut overlaid, Message::RequestDelete);
        assert!(overlaid.delete_confirm.is_none());

        let mut running = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut running, Message::RunRequested);
        update(&mut running, Message::RequestDelete);
        assert!(running.delete_confirm.is_none());
    }

    /// The same exclusivity as `navigation_run_and_overlay_are_blocked_
    /// while_editing`, checked for the delete confirmation prompt's own
    /// guard: while it is open, ordinary browsing/overlay/run/edit-entry
    /// messages must not slip through and act on state out from under it.
    #[test]
    fn navigation_and_edit_entry_are_blocked_while_the_delete_confirm_prompt_is_open() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::RequestDelete);
        assert!(state.delete_confirm.is_some());

        update(&mut state, Message::SelectNext);
        assert_eq!(selected(&state), 0, "selection must not move");

        update(&mut state, Message::EnterEditMode);
        assert!(state.edit_mode.is_none());

        update(&mut state, Message::AddRequest);
        assert_eq!(saved_document(&state).requests().len(), 3);

        update(&mut state, Message::OpenEnvironmentOverlay);
        assert!(state.environment_overlay.is_none());

        assert!(
            state.delete_confirm.is_some(),
            "the prompt itself must still be open throughout"
        );
    }

    /// Proof requirement: a failed write must never lose track of the
    /// pending delete or silently apply it — the prompt stays open with a
    /// real error, the same "no-op that keeps you where you can retry or
    /// cancel" shape `a_failed_disk_write_keeps_the_edit_dirty_and_surfaces_
    /// a_real_error_without_losing_it` already proves for `SaveEdit`.
    #[test]
    fn a_failed_disk_write_keeps_the_prompt_open_with_a_real_error_without_losing_anything() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = dir.path().join("collection.yaml");
        std::fs::write(&path, THREE_REQUEST_COLLECTION).unwrap();

        let mut state = state_loaded_from(&path);
        // Block the target path with a directory only *after* the initial
        // load, so `state_loaded_from` itself succeeds and only the delete's
        // own write fails.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).expect("a directory blocking the target path");

        update(&mut state, Message::RequestDelete);
        update(&mut state, Message::ConfirmDelete);

        assert!(
            state.delete_confirm.is_some(),
            "a failed write must not silently close the prompt"
        );
        let error = state
            .delete_confirm
            .as_ref()
            .unwrap()
            .prompt
            .error
            .as_ref()
            .expect("a failed write must surface a real error message");
        assert!(!error.is_empty());
        assert_eq!(
            saved_document(&state).requests().len(),
            3,
            "the loaded document must be untouched by the failed attempt"
        );

        // Removing the obstruction and retrying must now succeed.
        std::fs::remove_dir(&path).unwrap();
        update(&mut state, Message::ConfirmDelete);

        assert!(state.delete_confirm.is_none());
        let reloaded =
            Document::from_path(&path).expect("the retried delete must have written a real file");
        assert_eq!(reloaded.requests().len(), 2);
    }
}
