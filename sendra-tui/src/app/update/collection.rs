//! Message handlers for opening, closing, and loading collections — the
//! process-wide tab lifecycle: `OpenCollectionPrompt`/`ConfirmOpenCollectionPath`/
//! `CancelOpenCollectionPrompt`/`CollectionOpened`, `NextCollection`/
//! `PreviousCollection`, `CloseCollectionRequested`/`ConfirmCloseCollection`/
//! `CancelCloseCollection`, and the session-scoped `NoCollectionPath`/
//! `CollectionLoaded`/`EnvironmentsLoaded` that load the very first
//! collection into an already-open session.

use std::path::{Path, PathBuf};

use crate::app::discovery::discover_collections;
use crate::app::state::{
    AppState, CloseConfirm, CollectionSession, ConfirmPrompt, LoadState, OpenCollectionPromptState,
};

/// `Message::OpenCollectionPrompt`.
pub(crate) fn handle_open_collection_prompt(state: &mut AppState) {
    if state.open_collection_prompt.is_none()
        && state.close_confirm.is_none()
        && state.quit_confirm.is_none()
        && state.edit_mode.is_none()
        && state.environment_overlay.is_none()
        && state.delete_confirm.is_none()
        && !matches!(state.run_state, crate::app::state::RunState::InFlight)
    {
        state.open_collection_prompt = Some(OpenCollectionPromptState::default());
    }
}

/// `Message::CollectionOpened` — the result of a `Message::ConfirmOpenCollectionPath`
/// that `main::run`'s loop has already turned into a real load attempt.
/// `Ok(document)` appends a brand-new tab and switches to it; `Err(error)`
/// opens nothing and instead leaves the prompt open with the error set.
pub(crate) fn handle_collection_opened(
    state: &mut AppState,
    base_dir: std::path::PathBuf,
    path: std::path::PathBuf,
    result: Box<Result<sendra_core::Document, sendra_core::SendraError>>,
    environments: Vec<crate::app::state::NamedEnvironment>,
    environment_errors: Vec<(String, sendra_core::SendraError)>,
) {
    match *result {
        Ok(document) => {
            state.open_session(CollectionSession {
                load_state: LoadState::Loaded {
                    document: Box::new(document),
                    selected: 0,
                    base_dir,
                    path,
                },
                environments,
                environment_errors,
                ..CollectionSession::default()
            });
            state.open_collection_prompt = None;
        }
        Err(error) => {
            if let Some(prompt) = &mut state.open_collection_prompt {
                prompt.error = Some(error.to_string());
            }
        }
    }
}

/// `Message::NextCollection`: switches to the next tab, wrapping from the
/// last back to the first.
pub(crate) fn handle_next_collection(state: &mut AppState) {
    if state.close_confirm.is_none()
        && state.open_collection_prompt.is_none()
        && state.quit_confirm.is_none()
    {
        state.active_collection = (state.active_collection + 1) % state.collections.len();
    }
}

/// `Message::PreviousCollection`: the exact reverse of `NextCollection`.
pub(crate) fn handle_previous_collection(state: &mut AppState) {
    if state.close_confirm.is_none()
        && state.open_collection_prompt.is_none()
        && state.quit_confirm.is_none()
    {
        state.active_collection =
            (state.active_collection + state.collections.len() - 1) % state.collections.len();
    }
}

/// `Message::CloseCollectionRequested`. Deliberately does *not* also require
/// `edit_mode`/`environment_overlay`/`delete_confirm` to be `None`: closing a
/// tab that is mid-edit is exactly the case `CollectionSession::is_dirty()`
/// exists to catch and route through a confirmation, not a case to refuse
/// outright.
pub(crate) fn handle_close_collection_requested(state: &mut AppState) {
    if state.close_confirm.is_none()
        && state.open_collection_prompt.is_none()
        && state.quit_confirm.is_none()
    {
        if state.active().is_dirty() {
            let label = crate::app::view::collection_label(state.active());
            state.close_confirm = Some(CloseConfirm {
                collection_id: state.active().id,
                prompt: ConfirmPrompt::new(format!(
                    "Close '{label}'? Unsaved changes will be lost."
                )),
            });
        } else {
            close_session_by_id(state, state.active().id);
        }
    }
}

/// `Message::ConfirmCloseCollection`.
pub(crate) fn handle_confirm_close_collection(state: &mut AppState) {
    if let Some(confirm) = state.close_confirm.take() {
        close_session_by_id(state, confirm.collection_id);
    }
}

/// `Message::CancelCloseCollection`.
pub(crate) fn handle_cancel_close_collection(state: &mut AppState) {
    state.close_confirm = None;
}

/// Closes the session named by `id`: removed from `collections` entirely
/// when it is not the last one open (with `active_collection` clamped back
/// into range); reset in place, never removed, when it is the only tab left.
/// Looks `id` up rather than assuming `active_collection` still names it.
pub(crate) fn close_session_by_id(state: &mut AppState, id: u64) {
    let Some(index) = state.collections.iter().position(|s| s.id == id) else {
        return;
    };
    if state.collections.len() > 1 {
        state.collections.remove(index);
        if state.active_collection >= state.collections.len() {
            state.active_collection = state.collections.len() - 1;
        }
    } else {
        state.collections[0] = CollectionSession {
            id,
            load_state: no_path_provided(),
            ..CollectionSession::default()
        };
    }
}

/// `Message::NoCollectionPath` (session-scoped).
pub(crate) fn handle_no_collection_path(state: &mut CollectionSession) {
    state.load_state = no_path_provided();
}

/// A fresh `LoadState::NoPathProvided`, re-running discovery against the
/// real current directory every time — both `handle_no_collection_path`
/// (startup with no path given) and `close_session_by_id`'s "closed the last
/// tab" reset go through this rather than each calling
/// `discover_collections` separately, so there is exactly one place that
/// decides what "nothing loaded, here's what's nearby" actually means.
/// `std::env::current_dir()` failing (the process's cwd deleted out from
/// under it, say) falls back to an empty candidate list — the plain welcome
/// message — rather than propagating an error over something this minor.
fn no_path_provided() -> LoadState {
    let discovered = std::env::current_dir()
        .map(|dir| discover_collections(&dir))
        .unwrap_or_default();
    LoadState::NoPathProvided {
        discovered,
        cursor: 0,
    }
}

/// `Message::RunRequested`, repurposed as "open the discovery picker's
/// highlighted candidate" while `LoadState::NoPathProvided` has any to offer
/// — see that variant's own doc comment on why discovery lives here, inside
/// `update()`, instead of being threaded in from `main.rs`. The same
/// constraint applies to *confirming* a pick: `main::translate_event` cannot
/// be taught a new key for it without editing `main.rs`, so this reuses
/// Enter/`r`, which already mean "confirm/act" everywhere else in this app
/// (running a request, confirming an environment selection, confirming a
/// delete) — a natural extension of what those keys already mean, for the
/// one `LoadState` where an actual run is structurally impossible anyway
/// (`request_edit::request_is_selected` is always `false` with nothing
/// loaded). Returns `true` when it actually consumed the message this way,
/// so `update_session`'s own `RunRequested` arm skips its ordinary handling
/// below it.
///
/// Loads the picked file through exactly the same `Document::from_path` +
/// `handle_collection_loaded` pair `Message::CollectionLoaded` itself uses
/// for the very first collection — reused, not duplicated — so a malformed
/// file picked from the discovery list surfaces the identical real
/// `SendraError` a bad path typed into the open-collection prompt would.
/// Environments are left exactly as they already are: `main::run` already
/// loaded them from this same current directory at startup (see
/// `main::load_environments`'s own call site), regardless of whether a path
/// was given, so there is nothing left to reload here.
pub(crate) fn confirm_discovered_selection(state: &mut CollectionSession) -> bool {
    let LoadState::NoPathProvided { discovered, cursor } = &state.load_state else {
        return false;
    };
    let Some(path) = discovered.get(*cursor).cloned() else {
        return false;
    };
    let base_dir = base_dir_of(&path);
    let result = sendra_core::Document::from_path(&path);
    handle_collection_loaded(state, base_dir, path, Box::new(result));
    true
}

/// Mirrors `main::base_dir` exactly (see that function's own doc comment on
/// why `body_file`/multipart paths resolve against a collection's own
/// directory) — duplicated rather than shared across the crate/binary
/// boundary, the same tradeoff that function's own doc comment already
/// makes for `sendra-cli`'s copy of the same three lines.
fn base_dir_of(path: &Path) -> PathBuf {
    path.parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `Message::CollectionLoaded` (session-scoped) — loading the very first
/// collection into an already-open session.
pub(crate) fn handle_collection_loaded(
    state: &mut CollectionSession,
    base_dir: std::path::PathBuf,
    path: std::path::PathBuf,
    result: Box<Result<sendra_core::Document, sendra_core::SendraError>>,
) {
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

/// `Message::EnvironmentsLoaded` (session-scoped).
pub(crate) fn handle_environments_loaded(
    state: &mut CollectionSession,
    environments: Vec<crate::app::state::NamedEnvironment>,
    errors: Vec<(String, sendra_core::SendraError)>,
) {
    state.environments = environments;
    state.environment_errors = errors;
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use sendra_core::Document;

    use crate::app::state::{EditField, Message, RunState};
    use crate::app::test_support::*;
    use crate::app::update::update;

    use super::*;

    // --- Multi-collection ---------------------------------------------------

    #[test]
    fn opening_a_second_collection_appends_a_new_tab_and_switches_to_it() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        let first_id = state.active().id;

        let (second_id, _dir, _path) = open_second_collection(&mut state, VALID_COLLECTION);

        assert_eq!(state.collections.len(), 2);
        assert_ne!(
            first_id, second_id,
            "every open collection must have its own, distinct id"
        );
        assert_eq!(
            state.active_collection, 1,
            "opening a collection switches straight to its new tab"
        );
        assert_eq!(saved_document(&state).requests().len(), 2);
    }

    #[test]
    fn a_failed_open_leaves_the_prompt_open_with_the_error_and_no_new_tab() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::OpenCollectionPrompt);
        assert!(state.open_collection_prompt.is_some());

        let error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");
        update(
            &mut state,
            Message::CollectionOpened {
                base_dir: PathBuf::from("."),
                path: PathBuf::from("bad.yaml"),
                result: Box::new(Err(error)),
                environments: Vec::new(),
                environment_errors: Vec::new(),
            },
        );

        assert_eq!(
            state.collections.len(),
            1,
            "a failed open must not create a new tab"
        );
        let prompt = state
            .open_collection_prompt
            .as_ref()
            .expect("the prompt must stay open so the path can be fixed and retried");
        assert!(prompt.error.is_some());
    }

    /// Proof requirement: two collections open at once, each with its own
    /// selection, `run_state` and edit session — switching between them must
    /// show exactly the state each one actually has, never the other's.
    #[test]
    fn each_tab_keeps_independent_selection_run_state_and_edit_mode_with_no_bleed_through() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION); // tab 0: "One"/"Two"/"Three"
        update(&mut state, Message::SelectNext); // tab 0 selects "Two" (index 1)

        let (_second_id, _dir, _path) = open_second_collection(&mut state, VALID_COLLECTION); // tab 1
        assert_eq!(state.active_collection, 1);
        assert_eq!(
            selected(&state),
            0,
            "a freshly opened tab starts at index 0"
        );

        // Run to completion in tab 1 only.
        update(&mut state, Message::RunRequested);
        let tab1_id = state.active().id;
        update(
            &mut state,
            Message::RunCompleted {
                collection_id: tab1_id,
                outcome: sample_outcome(200),
            },
        );
        assert!(matches!(state.run_state, RunState::Completed));

        // Start (but never finish) editing the selected request in tab 1.
        update(&mut state, Message::EnterEditMode);
        assert!(state.edit_mode.is_some());

        // Switch back to tab 0: its own selection must be exactly where it
        // was left, and neither the run nor the edit session from tab 1 must
        // have leaked across.
        update(&mut state, Message::PreviousCollection);
        assert_eq!(state.active_collection, 0);
        assert_eq!(
            selected(&state),
            1,
            "tab 0's own selection must be untouched by anything done in tab 1"
        );
        assert!(
            matches!(state.run_state, RunState::Idle),
            "tab 0 never ran anything — its run_state must still be Idle, not tab 1's Completed"
        );
        assert!(
            state.edit_mode.is_none(),
            "tab 0 was never put into edit mode — tab 1's own edit session must not appear here"
        );

        // Switching back to tab 1 must show its own state exactly as it was
        // left — the run result and the open edit session both still there.
        update(&mut state, Message::NextCollection);
        assert_eq!(state.active_collection, 1);
        assert!(matches!(state.run_state, RunState::Completed));
        assert!(state.edit_mode.is_some());
    }

    /// Proof requirement: quitting checks *every* open tab, not just the
    /// active one — an edit left open in a tab the user has since switched
    /// away from is just as real a loss as one in the tab on screen right
    /// now, so it must still prompt.
    #[test]
    fn quit_prompts_for_unsaved_edits_in_a_tab_other_than_the_active_one() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION); // tab 0
        update(&mut state, Message::EnterEditMode); // tab 0 now dirty
        assert!(state.edit_mode.is_some());

        open_second_collection(&mut state, VALID_COLLECTION); // tab 1, now active
        assert_eq!(state.active_collection, 1);
        assert!(
            !state.active().is_dirty(),
            "the active tab itself has nothing unsaved"
        );

        update(&mut state, Message::Quit);

        assert!(
            !state.should_quit,
            "tab 0's still-open edit session must still block quitting even \
             though tab 1, the active one, is completely clean"
        );
        assert!(state.quit_confirm.is_some());
    }

    /// Proof requirement: confirming the quit prompt discards everything and
    /// exits; cancelling returns to the app with every tab's edits intact,
    /// exactly where they were left.
    #[test]
    fn confirming_quit_exits_cancelling_preserves_every_tabs_edits() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION); // tab 0
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::Url;
        type_into_focused_field(&mut state, "/tab-0-unsaved-edit");
        assert!(state.edit_mode.as_ref().unwrap().dirty);

        open_second_collection(&mut state, VALID_COLLECTION); // tab 1
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::Url;
        type_into_focused_field(&mut state, "/tab-1-unsaved-edit");
        assert!(state.edit_mode.as_ref().unwrap().dirty);

        // Cancelling the prompt must leave both tabs' in-progress edits
        // completely untouched.
        update(&mut state, Message::Quit);
        update(&mut state, Message::CancelQuit);
        assert!(!state.should_quit);
        assert!(
            state.edit_mode.is_some(),
            "tab 1's own edit session must still be open after cancelling quit"
        );
        assert!(state
            .edit_mode
            .as_ref()
            .unwrap()
            .url
            .value()
            .ends_with("/tab-1-unsaved-edit"));
        update(&mut state, Message::PreviousCollection);
        assert_eq!(state.active_collection, 0);
        assert!(
            state.edit_mode.is_some(),
            "tab 0's own edit session must still be open too — cancelling \
             quit must not have touched any tab"
        );
        assert!(state
            .edit_mode
            .as_ref()
            .unwrap()
            .url
            .value()
            .ends_with("/tab-0-unsaved-edit"));

        // Confirming, on the other hand, discards everything and exits —
        // there is nothing left to check about the tabs' contents once the
        // whole process is ending, only that it actually ends.
        update(&mut state, Message::NextCollection);
        update(&mut state, Message::Quit);
        update(&mut state, Message::ConfirmQuit);
        assert!(state.should_quit);
    }

    /// Proof requirement: an unsaved edit in one tab must be completely
    /// unaffected by actions — including a real save — taken in another,
    /// verified against real bytes on disk for both files, not just
    /// in-memory state.
    #[test]
    fn saving_in_one_tab_does_not_affect_an_unsaved_edit_in_another() {
        let dir_a = tempfile::tempdir().expect("a temp dir for this test");
        let path_a = dir_a.path().join("a.yaml");
        std::fs::write(&path_a, "method: GET\nurl: https://example.com\n").unwrap();
        let mut state = state_loaded_from(&path_a);

        let (_id_b, _dir_b, path_b) = open_second_collection(
            &mut state,
            "name: test\nrequests:\n  - name: One\n    method: GET\n    url: https://example.com\n",
        );
        let original_b_bytes = std::fs::read(&path_b).unwrap();

        // Start editing tab B (the active one) without ever saving it.
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::Url;
        backspace_n(&mut state, "https://example.com".len());
        type_into_focused_field(&mut state, "https://example.com/unsaved-in-b");
        assert!(state.dirty_requests.contains(&0));

        // Switch to tab A and save a completely different edit there.
        update(&mut state, Message::PreviousCollection);
        assert_eq!(state.active_collection, 0);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().unwrap().focus = EditField::Url;
        backspace_n(&mut state, "https://example.com".len());
        type_into_focused_field(&mut state, "https://example.com/saved-in-a");
        update(&mut state, Message::SaveEdit);
        assert!(state.edit_mode.is_none(), "tab A's save must succeed");

        // Tab A's save reached disk...
        let reloaded_a = Document::from_path(&path_a).unwrap();
        assert_eq!(
            reloaded_a.requests()[0].url,
            "https://example.com/saved-in-a"
        );

        // ...and tab B's file on disk, and its still-open, still-unsaved
        // edit session in memory, are both completely untouched by it.
        let bytes_b_after = std::fs::read(&path_b).unwrap();
        assert_eq!(
            original_b_bytes, bytes_b_after,
            "saving in tab A must never touch tab B's file on disk"
        );
        update(&mut state, Message::NextCollection);
        assert_eq!(state.active_collection, 1);
        assert!(
            state.edit_mode.is_some(),
            "tab B's own in-progress edit must still be open"
        );
        assert_eq!(
            state.edit_mode.as_ref().unwrap().url.value(),
            "https://example.com/unsaved-in-b",
            "tab B's unsaved edit must still hold exactly what was typed into it"
        );
        assert!(state.dirty_requests.contains(&0));
    }

    #[test]
    fn next_and_previous_collection_wrap_and_are_exact_inverses() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        let (_id, _dir, _path) = open_second_collection(&mut state, VALID_COLLECTION);
        assert_eq!(state.active_collection, 1);

        update(&mut state, Message::NextCollection);
        assert_eq!(
            state.active_collection, 0,
            "must wrap from the last tab to the first"
        );

        update(&mut state, Message::PreviousCollection);
        assert_eq!(
            state.active_collection, 1,
            "must wrap back from the first to the last"
        );
    }

    #[test]
    fn closing_a_clean_tab_removes_it_immediately_with_no_confirmation() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        let (_id, _dir, _path) = open_second_collection(&mut state, VALID_COLLECTION);
        assert_eq!(state.collections.len(), 2);

        update(&mut state, Message::CloseCollectionRequested);

        assert!(
            state.close_confirm.is_none(),
            "a clean tab must close without ever opening a confirmation"
        );
        assert_eq!(state.collections.len(), 1);
        assert_eq!(
            saved_document(&state).requests().len(),
            3,
            "the remaining tab must be the other collection, untouched"
        );
    }

    #[test]
    fn closing_a_dirty_tab_requires_confirmation_and_cancelling_keeps_it_open() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        let (dirty_id, _dir, _path) = open_second_collection(&mut state, VALID_COLLECTION);
        update(&mut state, Message::EnterEditMode); // makes the active tab dirty

        update(&mut state, Message::CloseCollectionRequested);
        assert_eq!(
            state
                .close_confirm
                .as_ref()
                .map(|confirm| confirm.collection_id),
            Some(dirty_id),
            "a tab with an open edit session must ask before closing"
        );
        assert_eq!(state.collections.len(), 2, "nothing has closed yet");

        update(&mut state, Message::CancelCloseCollection);
        assert!(state.close_confirm.is_none());
        assert_eq!(state.collections.len(), 2);
        assert!(
            state.edit_mode.is_some(),
            "cancelling the close must leave the edit session exactly as it was"
        );
    }

    #[test]
    fn confirming_a_dirty_tab_close_actually_removes_it() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        open_second_collection(&mut state, VALID_COLLECTION);
        update(&mut state, Message::EnterEditMode);

        update(&mut state, Message::CloseCollectionRequested);
        update(&mut state, Message::ConfirmCloseCollection);

        assert!(state.close_confirm.is_none());
        assert_eq!(state.collections.len(), 1);
        assert_eq!(saved_document(&state).requests().len(), 3);
    }

    /// The one-tab edge case: closing the *only* open collection must not
    /// shrink `collections` to zero (see `AppState::collections`'s own doc
    /// comment on why that must never happen) — it resets that tab back to
    /// the same blank `NoPathProvided` state a process with no collection
    /// path at all already starts in.
    #[test]
    fn closing_the_last_remaining_tab_resets_it_instead_of_removing_it() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        assert_eq!(state.collections.len(), 1);

        update(&mut state, Message::CloseCollectionRequested);

        assert_eq!(
            state.collections.len(),
            1,
            "the Vec must never become empty"
        );
        assert!(matches!(state.load_state, LoadState::NoPathProvided { .. }));
        assert_eq!(state.active_collection, 0);
    }

    /// Proof requirement: a run started in one tab must route its result
    /// back to *that* tab specifically, even if the user has since switched
    /// to (or is actively using) a different one — see
    /// `Message::RunCompleted`'s own doc comment on why this is tagged by a
    /// stable id rather than trusting `active_collection` to still name the
    /// same tab.
    #[test]
    fn run_completed_reaches_its_own_tab_even_after_switching_away() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        let tab_a_id = state.active().id;
        update(&mut state, Message::RunRequested);
        assert!(matches!(state.run_state, RunState::InFlight));

        // Switch away before the run finishes — allowed precisely because a
        // run in one tab must never block browsing another.
        open_second_collection(&mut state, VALID_COLLECTION);
        assert_eq!(state.active_collection, 1);

        update(
            &mut state,
            Message::RunCompleted {
                collection_id: tab_a_id,
                outcome: sample_outcome(200),
            },
        );

        // Tab B (still active) is untouched...
        assert!(matches!(state.run_state, RunState::Idle));
        assert_eq!(
            state.active_collection, 1,
            "the result must not switch tabs either"
        );

        // ...and tab A picked up its own result, right where it was left.
        update(&mut state, Message::PreviousCollection);
        assert!(matches!(state.run_state, RunState::Completed));
    }

    #[test]
    fn run_completed_for_a_since_closed_tab_is_silently_dropped_not_misrouted() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        let closed_id = state.active().id;
        update(&mut state, Message::RunRequested);

        open_second_collection(&mut state, VALID_COLLECTION);
        // Close tab A (index 0) while its run is still notionally in
        // flight — an in-flight run is one of `is_dirty`'s own signals, so
        // this goes through (and confirms) the close prompt, same as any
        // other dirty tab — the tab is gone before the result ever arrives.
        update(&mut state, Message::PreviousCollection);
        update(&mut state, Message::CloseCollectionRequested);
        assert!(state.close_confirm.is_some());
        update(&mut state, Message::ConfirmCloseCollection);
        assert_eq!(state.collections.len(), 1);

        update(
            &mut state,
            Message::RunCompleted {
                collection_id: closed_id,
                outcome: sample_outcome(200),
            },
        );

        // Nothing to misroute onto — the remaining tab is untouched.
        assert_eq!(state.collections.len(), 1);
        assert!(matches!(state.run_state, RunState::Idle));
    }

    /// Each tab's environments are scoped to it: a newly opened collection
    /// discovers its *own* environments (passed in on
    /// `Message::CollectionOpened`, exactly as the event loop resolves them
    /// from that collection's own `base_dir` — see that variant's own doc
    /// comment) rather than inheriting whatever the first collection had.
    /// Picking an active environment in one tab must not touch the other's.
    #[test]
    fn each_tabs_environments_and_active_environment_are_independent() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        state.environments = vec![
            named_environment("prod", &[("base_url", "https://prod.example.com")]),
            named_environment("staging", &[("base_url", "https://staging.example.com")]),
        ];
        update(&mut state, Message::OpenEnvironmentOverlay);
        update(&mut state, Message::ConfirmEnvironmentSelection); // tab 0 -> "prod"
        assert_eq!(state.active_environment, Some(0));

        // Tab 1 opens with a completely different (here: empty) environment
        // list of its own — exactly what a collection opened from another
        // project's directory would have, since `main::run` discovers
        // environments per collection, not once for the whole process.
        open_second_collection(&mut state, VALID_COLLECTION);
        assert!(
            state.environments.is_empty(),
            "a newly opened tab must not inherit another tab's environments"
        );
        assert_eq!(state.active_environment, None);

        state.environments = vec![named_environment(
            "only-here",
            &[("base_url", "https://only-here.example.com")],
        )];
        update(&mut state, Message::OpenEnvironmentOverlay);
        update(&mut state, Message::ConfirmEnvironmentSelection); // tab 1 -> "only-here"
        assert_eq!(state.active_environment, Some(0));

        // Back to tab 0: its own environment list and its own choice of
        // active environment must both still be exactly what they were.
        update(&mut state, Message::PreviousCollection);
        assert_eq!(state.environments.len(), 2);
        assert_eq!(state.active_environment, Some(0));
        assert_eq!(state.environments[0].name, "prod");
    }

    // --- the welcome screen's discovery picker ------------------------------

    fn state_with_discovered(paths: Vec<PathBuf>) -> AppState {
        let mut state = AppState::default();
        state.load_state = LoadState::NoPathProvided {
            discovered: paths,
            cursor: 0,
        };
        state
    }

    #[test]
    fn confirming_a_discovered_valid_collection_loads_it() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = dir.path().join("found.yaml");
        std::fs::write(&path, VALID_COLLECTION).unwrap();
        let mut state = state_with_discovered(vec![path.clone()]);

        update(&mut state, Message::RunRequested);

        match &state.load_state {
            LoadState::Loaded {
                document, path: p, ..
            } => {
                assert_eq!(document.requests().len(), 2);
                assert_eq!(p, &path);
            }
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        }
    }

    /// The real parse error must still surface — discovering a file is not
    /// the same as vouching for its contents, which this must not paper
    /// over.
    #[test]
    fn confirming_a_discovered_malformed_collection_shows_the_real_error() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let path = dir.path().join("broken.yaml");
        std::fs::write(&path, MALFORMED_YAML).unwrap();
        let mut state = state_with_discovered(vec![path]);

        update(&mut state, Message::RunRequested);

        assert!(
            matches!(state.load_state, LoadState::Failed(_)),
            "a malformed discovered file must surface the real load error, \
             not be silently skipped or shown as an empty list entry"
        );
    }

    #[test]
    fn confirming_moves_the_cursor_first_then_opens_the_highlighted_entry() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let first = dir.path().join("a.yaml");
        let second = dir.path().join("b.yaml");
        std::fs::write(&first, VALID_COLLECTION).unwrap();
        std::fs::write(&second, THREE_REQUEST_COLLECTION).unwrap();
        let mut state = state_with_discovered(vec![first, second.clone()]);

        update(&mut state, Message::SelectNext);
        update(&mut state, Message::RunRequested);

        match &state.load_state {
            LoadState::Loaded { path, .. } => assert_eq!(path, &second),
            other => panic!("expected LoadState::Loaded, got {other:?}"),
        }
    }

    #[test]
    fn run_requested_is_a_harmless_no_op_when_nothing_was_discovered() {
        let mut state = state_with_discovered(Vec::new());

        update(&mut state, Message::RunRequested);

        assert!(matches!(state.load_state, LoadState::NoPathProvided { .. }));
    }

    #[test]
    fn selecting_up_and_down_wraps_through_the_discovered_list() {
        let dir = tempfile::tempdir().expect("a temp dir for this test");
        let paths: Vec<PathBuf> = ["a.yaml", "b.yaml", "c.yaml"]
            .iter()
            .map(|name| dir.path().join(name))
            .collect();
        let mut state = state_with_discovered(paths);

        update(&mut state, Message::SelectPrevious);

        match &state.load_state {
            LoadState::NoPathProvided { cursor, .. } => {
                assert_eq!(
                    *cursor, 2,
                    "moving previous from 0 must wrap to the last entry"
                )
            }
            other => panic!("expected LoadState::NoPathProvided, got {other:?}"),
        }
    }
}
