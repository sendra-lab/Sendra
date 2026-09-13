//! The model half of sendra-tui's Elm-style architecture: `AppState` and
//! every type it is built from (`LoadState`, `RunState`, `CollectionSession`),
//! plus the `Message` enum every state transition flows through. No
//! transition logic lives here — see `super::update` for `update()` itself —
//! this module only defines what the state *is*.
//!
//! Split into submodules along the same lines the state itself naturally
//! divides: [`edit`] for a request's own edit session (`EditState` and
//! everything it's built from), [`environment_edit`] for an environment's
//! variables edit session — this file keeps only what is genuinely
//! process/session-wide (`AppState`, `CollectionSession`, `LoadState`,
//! `Message`, `RunState`) plus the handful of shared confirmation types
//! (`ConfirmPrompt` and its per-feature companions) every one of those
//! sessions builds a confirmation prompt from.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::SystemTime;

use sendra_core::{Document, Environment, SendraError};

use crate::run_request::RunOutcome;

mod edit;
mod environment_edit;

pub(crate) use edit::{
    non_empty, validate_assertion_value_text, validate_method_text, AssertionRow, AuthEdit,
    AuthField, BodyEdit, CaptureKind, CaptureRow, EditField, EditState, HeaderRow, JsonOperator,
    TextField,
};
pub(crate) use environment_edit::{EnvVarField, EnvironmentEditState};

/// One environment sendra-tui found in `.sendra/environments/`, loaded (not
/// just named) so the overlay can show its variables before it's picked.
#[derive(Debug, Clone)]
pub struct NamedEnvironment {
    pub name: String,
    pub environment: Environment,
}

/// All of one open collection's own state — one per tab in the multi-
/// collection TUI. This is what used to be the entire `AppState`, back when
/// the TUI could only ever have one collection open at a time; see
/// [`AppState`]'s own doc comment for why it split into "one `CollectionSession`
/// per open collection" plus a thin, genuinely process-wide wrapper around
/// `Vec<CollectionSession>`.
///
/// Every field here is scoped to exactly one collection: switching tabs
/// (`Message::NextCollection`/`PreviousCollection`) must never let one
/// session's selection, run result, edit session, dirty markers, or active
/// environment bleed into another's — the entire reason this type exists
/// separately from `AppState` at all, rather than as a `Vec` of some smaller
/// struct with the rest still flat. `should_quit`/`spinner_tick` are the only
/// two fields that stayed on `AppState` itself: a clean exit and a shared
/// animation clock are truly process-wide, not per-tab, concerns.
#[derive(Debug, Default)]
pub struct CollectionSession {
    /// Uniquely identifies this session for the lifetime of the process —
    /// assigned once, by `AppState::open_session`, and never reused even
    /// after the session is closed. Never a plain `Vec` index: closing an
    /// earlier tab shifts every later tab's index, which would silently
    /// misroute a `Message::RunCompleted` for an in-flight run that started
    /// before the close and completes after it (see that variant's own doc
    /// comment). `0` for the very first session `AppState::default` creates;
    /// tests that build a `CollectionSession` directly rather than through
    /// `AppState` never need to set this themselves; it defaults to `0` too,
    /// as if it were the first (and, in a single-session test, only) tab.
    pub id: u64,
    pub load_state: LoadState,
    /// Every environment discovered at startup, sorted by name.
    pub environments: Vec<NamedEnvironment>,
    /// Every environment file `main::load_environments` found but could not
    /// load — a name that exists in `.sendra/environments/` alongside
    /// whatever `Environment::from_path` said was wrong with it. Previously
    /// swallowed outright (`.ok()?` inside a `filter_map`, dropping both the
    /// name and the reason on the floor); now carried through the same
    /// `Message::EnvironmentsLoaded` as the environments that *did* load, so
    /// a malformed or unreadable file is a visible, in-app error — shown in
    /// the overlay via [`super::view::render_error`] — rather than a file
    /// that silently never appears in the list with no indication anything
    /// went wrong.
    pub environment_errors: Vec<(String, SendraError)>,
    /// Index into `environments` for the environment the detail pane
    /// resolves against, or `None` — the honest starting state until the
    /// user picks one in the overlay.
    pub active_environment: Option<usize>,
    /// `Some(cursor)` while the environment-picker overlay is open, `None`
    /// otherwise. The cursor is the overlay's own selection, separate from
    /// `active_environment`, so browsing the list and cancelling
    /// (`Message::CloseEnvironmentOverlay`) never touches what is active.
    pub environment_overlay: Option<usize>,
    /// The selected request's most recent run, if any has been started.
    pub run_state: RunState,
    /// How many lines into the response panel's text the view is scrolled —
    /// see `render_response_panel`. Reset to `0` whenever it would otherwise
    /// point at a different run's text: a fresh `RunRequested` and a
    /// changed request-list selection both reset it, in `update` and
    /// `select` respectively.
    pub response_scroll: usize,
    /// Whether captured values, and (see `view::format_resolved_request`)
    /// auth-derived header/query values in the read-only request preview,
    /// are shown in the clear rather than masked — `Message::
    /// ToggleRevealCaptures`, bound to `c`. One flag for both, not a second
    /// toggle: both are the same "sensitive value a screen-share or a
    /// terminal recording shouldn't casually expose while just browsing"
    /// concern, and a user who has already asked to see one kind of secret
    /// this session is not asking to keep the other hidden. Never affects
    /// edit mode, where auth values are always shown in the clear regardless
    /// — editing them is the whole point there, and there is nothing to
    /// browse-and-forget about a value you are actively typing. Starts
    /// `false` (masked) every time, is never written anywhere but this
    /// in-memory field, and is reset back to `false` on the same two events
    /// that reset `run_state` (a fresh `RunRequested`, a changed
    /// request-list selection) — see the doc comment on
    /// `format_capture_section` for why "never persisted, never
    /// auto-revealed on the next run" means resetting it there too, not only
    /// at process start.
    pub reveal_captures: bool,
    /// `Some(EditState)` while the request at `selected` (in
    /// `LoadState::Loaded`) is being edited, `None` while merely browsing.
    /// Entering and leaving flows through
    /// `Message::EnterEditMode`/`Message::SaveEdit`/`Message::CancelEdit`
    /// like every other state transition (see `update()`), never set
    /// directly from `main.rs` or anywhere outside this module's own
    /// `update()` — an Elm-style side-channel flag would break that
    /// single-source-of-truth guarantee.
    pub edit_mode: Option<EditState>,
    /// Indices into the current document's `requests()` that have unsaved
    /// edits — inserted by whatever future editing message mutates a
    /// request's working copy, removed by `Message::SaveEdit` and
    /// `Message::CancelEdit`. Lives here, not on `Document` itself: nothing
    /// in `sendra-core` exposes a mutable `Document`, and "which requests
    /// have unsaved TUI-local edits" is sendra-tui's own bookkeeping, not
    /// something a collection file format should have to represent.
    pub dirty_requests: HashSet<usize>,
    /// `Some(...)` while the currently open edit session is for a request
    /// `Message::AddRequest` just inserted and that has never been saved —
    /// `None` for an edit of a pre-existing request. What
    /// `Message::CancelEdit` needs to undo the insertion itself, not merely
    /// drop `edit_mode`: unlike every other edit (which never touches the
    /// real `Document` until `Message::SaveEdit`), adding a request has to
    /// mutate the loaded `Document` immediately — it has to actually exist,
    /// selected, for `EditState::new` to seed an edit session from and for
    /// the collection browser to show it — so cancelling has real work to
    /// undo here that it never has anywhere else. See
    /// `PendingNewRequest`'s own doc comment for why that undo restores the
    /// whole `Document` wholesale rather than trying to reverse the
    /// insertion in place.
    pub pending_new_request: Option<PendingNewRequest>,
    /// `Some(...)` while a delete confirmation prompt is open for the
    /// request at `DeleteConfirm::index` in the loaded document, `None`
    /// while merely browsing. Entering and leaving flows through
    /// `Message::RequestDelete`/`Message::ConfirmDelete`/`Message::CancelDelete`
    /// like every other state transition — see `update()`'s own guards,
    /// which treat this as exclusive with edit mode, the environment
    /// overlay and an in-flight run, the same way those are already
    /// exclusive with each other. Deliberately a minimal, single-purpose
    /// confirmation rather than a shared overlay component: a unified
    /// confirmation used by every destructive action in this batch is a
    /// later, separate concern.
    pub delete_confirm: Option<DeleteConfirm>,
    /// `Some(EnvironmentEditState)` while the environment overlay is showing
    /// an in-progress edit of one environment's variables, `None` while the
    /// overlay is merely browsing (or closed). See that type's own doc
    /// comment for why it is a sibling of `edit_mode`, not layered inside
    /// it. Entering and leaving flows through `Message::EnterEnvironmentEdit`/
    /// `Message::SaveEnvironmentEdit`/`Message::CancelEnvironmentEdit`, the
    /// same pattern every other editable state in this crate follows.
    pub environment_edit: Option<EnvironmentEditState>,
    /// Every past run of each request in this session, most recent entry
    /// first, keyed by that request's index into `document.requests()` —
    /// scoped to this one session exactly like every other field here, so
    /// history from a run in one tab can never appear while browsing another
    /// (see this struct's own doc comment on tab isolation).
    ///
    /// **In-memory only.** Never written to disk: gone the moment this
    /// process exits or this tab closes, by design — a run's response can
    /// hold arbitrary (and possibly sensitive) response bodies that were
    /// never asked to be persisted, unlike a request's own definition.
    ///
    /// **Capped per request at [`RUN_HISTORY_CAP`] entries**, oldest dropped
    /// first — see that constant's own doc comment for why. Written to only
    /// by `update::push_history_entry`, the one place `Message::RunCompleted`
    /// records a finished run.
    ///
    /// **Reindexed on delete, exactly like `dirty_requests`.** A request's
    /// index can shift when an earlier one is deleted
    /// (`Message::ConfirmDelete`), so this map is rewritten the same way
    /// `dirty_requests` already is — see `update::reindex_dirty_after_delete`
    /// and its history counterpart, `update::reindex_history_after_delete`.
    /// The deleted request's own history is dropped outright: there is no
    /// longer a request left for it to be about.
    pub run_history: HashMap<usize, Vec<RunHistoryEntry>>,
    /// How many of each request's own history entries have been evicted by
    /// the [`RUN_HISTORY_CAP`] trim in `update::push_history_entry` over the
    /// life of this session — not itself part of `run_history`, since an
    /// evicted entry carries no data worth keeping, only the fact that it
    /// existed. Surfaced by the history overlay ("N older runs were
    /// dropped") so hitting the cap is never silent. Reindexed on delete
    /// exactly like `run_history` itself (`update::reindex_history_after_delete`
    /// covers both in one pass), and, like `run_history`, dropped outright
    /// for a deleted request rather than carried forward to whichever index
    /// takes its place.
    pub run_history_dropped: HashMap<usize, usize>,
    /// `Some(...)` while the run-history browser is open for the currently
    /// selected request — a sibling of `environment_overlay`: opened and
    /// closed by `Message::OpenHistoryOverlay`/`Message::CloseHistoryOverlay`,
    /// and mutually exclusive with edit mode, the delete confirmation, the
    /// environment overlay/edit session, and an in-flight run, the same way
    /// every other modal in this crate is kept exclusive with every other
    /// (see `update()`'s own guards).
    pub history_overlay: Option<HistoryOverlay>,
}

/// Cap on how many past runs [`CollectionSession::run_history`] keeps per
/// request, revisited and kept at 20: still generous for what browsing
/// actually needs (nobody scrolls back through more than a handful of past
/// runs while debugging a single request) while still bounding the real
/// memory cost of a long session that re-sends the same request many
/// times — nothing in sendra-core caps a response body's size, so that
/// history would otherwise accumulate in memory without limit. A
/// per-session, user-configurable cap was considered and rejected: nothing
/// in this crate persists settings across a process's lifetime, so a
/// configurable value would either need a new persistence mechanism just for
/// this one number or reset every restart anyway, for a knob real usage
/// doesn't show a need for. Once a request's history grows past this, its
/// oldest entries are dropped to make room for the newest — see
/// `update::push_history_entry`, which also counts every entry the trim
/// drops into `CollectionSession::run_history_dropped` so the history
/// overlay can say so rather than discarding data silently.
pub const RUN_HISTORY_CAP: usize = 20;

/// One past run of a request, kept in [`CollectionSession::run_history`] for
/// browsing after `RunState` has moved on to whatever the *next* run said.
/// Holds the exact same real [`RunOutcome`] `RunState::Completed` is backed
/// by for the current run, not a TUI-invented summary — see
/// `CollectionSession::current_run`'s own doc comment for why there is only
/// ever this one copy of it, not a second one duplicated onto `RunState`
/// itself. A historical entry renders through the exact same
/// `view::render_response_panel` a live one does, for the same reason: the
/// data is identical in shape, only "how long ago" differs.
#[derive(Debug)]
pub struct RunHistoryEntry {
    /// When this run's `Message::RunCompleted` was handled — wall-clock, so
    /// the history list can show how long ago a run happened, which the
    /// list's own position (most recent first) does not by itself convey.
    pub completed_at: SystemTime,
    pub outcome: RunOutcome,
}

/// The run-history browser's own state, opened by `Message::OpenHistoryOverlay`
/// for whichever request is currently selected: a list of that request's past
/// runs (`CollectionSession::run_history`, read fresh each render rather than
/// copied in here), with `cursor` pointing at one of them, plus — once
/// `Message::ViewHistoryEntry` picks one — the full response-panel view of
/// that entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryOverlay {
    /// Index into the selected request's history entries (`0` = most recent)
    /// the list's own selection currently points at. Reset to `0` whenever
    /// the overlay opens; kept in bounds by `update::select`, the same way
    /// the collection browser's own `selected` already is.
    pub cursor: usize,
    /// `Some(index)` while showing that entry's full result instead of the
    /// list — a "replace, not a toggle" relationship one level in from the
    /// one `render_detail_pane`'s own doc comment describes between the
    /// request preview and a live run's response panel: the list is only
    /// ever a stand-in for "what did this past run actually show".
    pub viewing: Option<usize>,
    /// Scroll position for the entry named by `viewing`'s own response
    /// panel — a field of its own, not `CollectionSession::response_scroll`,
    /// since that field belongs to the *live* run and must not be disturbed
    /// by scrolling back through a past one. Reset to `0` whenever `viewing`
    /// changes.
    pub view_scroll: usize,
    /// Whether the viewed entry's captures are shown in the clear — a flag
    /// of its own for the same reason `view_scroll` is: independent of
    /// `CollectionSession::reveal_captures`, which is about the live run.
    /// Starts (and resets to) `false` whenever `viewing` changes, the same
    /// "never auto-revealed" rule `CollectionSession::reveal_captures` itself
    /// follows for a fresh run.
    pub view_reveal_captures: bool,
    /// Indices (into the same entry list `cursor`/`viewing` index into) of
    /// entries currently shown expanded in the list — a quick, in-place
    /// "status code plus a short assertion/capture summary" glance that
    /// stops short of `viewing`'s full response-panel switch. Toggled by
    /// `Message::ToggleHistoryEntryExpanded`, independent of `cursor` so more
    /// than one row can be left open at once while browsing. Not reset when
    /// `cursor` moves — only `Message::CloseHistoryOverlay` (via a fresh
    /// `HistoryOverlay::default()` next time the overlay opens) clears it.
    pub expanded: HashSet<usize>,
}

/// The one shared shape behind every yes/no confirmation in this crate —
/// deleting a request, deleting an environment variable, closing a tab, and
/// quitting with unsaved work — mirroring how `format_error` already gives
/// every *error* in the crate one shared rendering, rather than each feature
/// formatting its own. `message` is the question itself (built once, when
/// the prompt opens, from whatever real data it names — a request's name, a
/// tab's label, a count of dirty tabs — so `view::render_confirm_prompt`
/// never has to re-derive it, and can render this exact same type no matter
/// which destructive action it came from), and `error` is `Some` only after
/// a confirmed attempt actually failed to carry it out.
///
/// **Not itself a place to say *what* is pending confirmation or *how* to
/// carry it out.** Every caller pairs this with whatever it alone needs to
/// act on `Message::Confirm*` — `DeleteConfirm`'s own `index`, `CloseConfirm`'s
/// own `collection_id`, an env-var row's own index — since that part is
/// genuinely feature-specific in a way the confirmation UI itself is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmPrompt {
    pub message: String,
    /// `Some(message)` right after a confirmed attempt actually failed to
    /// carry out the action (e.g. `Message::ConfirmDelete` failing to write
    /// to disk) — the prompt stays open with this set so the failure is
    /// visible and the user can retry or cancel, the same way
    /// `EditState::save_error` keeps edit mode open on a failed `SaveEdit`
    /// rather than silently discarding the pending change. Never set by
    /// [`Self::new`]: every prompt starts with nothing to report yet.
    pub error: Option<String>,
}

impl ConfirmPrompt {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            error: None,
        }
    }
}

/// What `Message::RequestDelete` opens and `Message::ConfirmDelete`/
/// `Message::CancelDelete` close — which request is pending deletion (needed
/// to act on it), plus the shared confirmation UI itself.
#[derive(Debug, Clone)]
pub struct DeleteConfirm {
    /// Index into the loaded document's `requests()` — fixed for the life of
    /// this prompt; nothing else can change the selection while it is open
    /// (see `update()`'s browsing-message guard for `delete_confirm`).
    pub index: usize,
    pub prompt: ConfirmPrompt,
}

/// What `Message::CancelEdit` restores after `Message::AddRequest` is
/// cancelled without ever being saved.
///
/// `document_before` is the *entire* loaded `Document`, cloned the instant
/// before the new request was inserted — not just "the new request's index,
/// to remove". Restoring the whole thing wholesale is what correctly undoes
/// a `Document::Single` having been converted into a `Document::Collection`
/// along the way (see `update::add_request_to_document`): removing only the
/// newly added request would leave that conversion — and the synthesized
/// `name` it gave the original, single request — behind, which is not
/// "nothing happened", the same guarantee every other cancelled edit already
/// gets.
#[derive(Debug, Clone)]
pub struct PendingNewRequest {
    /// Where the collection browser's selection was before `AddRequest` ran
    /// — restored on cancel rather than left on the now-removed request's
    /// former index.
    pub previous_selected: usize,
    pub document_before: Box<Document>,
}

impl CollectionSession {
    /// Whether the body field specifically has focus right now — what
    /// `main::translate_event` needs to decide whether `Enter`/`Up`/`Down`
    /// mean "edit the body" (a newline, a line up/down) instead of their
    /// ordinary edit-mode meaning of nothing at all, which is what those
    /// keys do on every other, single-line field. `false` whenever edit
    /// mode isn't even active, the same as focus meaning nothing then.
    pub fn body_focused(&self) -> bool {
        self.edit_mode
            .as_ref()
            .is_some_and(|edit| edit.focus == EditField::Body)
    }

    /// Whether this session holds anything that closing it would lose —
    /// `Message::CloseCollectionRequested`'s own guard: a session with
    /// nothing at stake closes immediately, the same "don't ask a question
    /// with only one honest answer" reasoning `can_delete` already applies
    /// to a request that can't meaningfully be deleted. Every one of these
    /// is state closing the tab would silently discard with no way back:
    /// an edit in progress (`edit_mode`, whether or not it has actually been
    /// typed into yet — an open session is itself something to lose, not
    /// only a *dirty* one, since closing it is indistinguishable from
    /// `CancelEdit` the user never asked for), a request added but not yet
    /// saved (`dirty_requests`), a pending delete or environment-variable
    /// edit, or a run still in flight whose response would never be seen.
    pub fn is_dirty(&self) -> bool {
        self.edit_mode.is_some()
            || !self.dirty_requests.is_empty()
            || self.pending_new_request.is_some()
            || self.delete_confirm.is_some()
            || self.environment_edit.is_some()
            || matches!(self.run_state, RunState::InFlight)
    }

    /// The selected request's history entries, most recent first — empty
    /// when nothing is loaded/selected or nothing has ever been run for it.
    /// Visible to `update`'s history-overlay message handling and `view`'s
    /// history overlay rendering, so both read the exact same slice rather
    /// than each re-deriving "which request, which entries" independently.
    pub fn selected_history(&self) -> &[RunHistoryEntry] {
        let LoadState::Loaded { selected, .. } = &self.load_state else {
            return &[];
        };
        self.run_history
            .get(selected)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// How many of the selected request's own history entries have been
    /// evicted by the [`RUN_HISTORY_CAP`] trim so far this session — `0`
    /// when nothing has ever been dropped, the ordinary case. See
    /// `CollectionSession::run_history_dropped`'s own doc comment for why
    /// this is tracked at all.
    pub fn selected_history_dropped(&self) -> usize {
        let LoadState::Loaded { selected, .. } = &self.load_state else {
            return 0;
        };
        self.run_history_dropped.get(selected).copied().unwrap_or(0)
    }

    /// The current run's outcome — always the selected request's own most
    /// recent history entry, since finishing a run and recording it as that
    /// request's newest history entry happen together, in
    /// `update::push_history_entry` (see `Message::RunCompleted`'s own
    /// handling). `RunState::Completed` deliberately carries no `RunOutcome`
    /// of its own for this reason: `run_history` is the one place a run's
    /// data lives, not two copies (`RunState`'s own and a history entry)
    /// that could quietly drift apart — and `RunOutcome` holding a
    /// `Result<Response, RunError>` (in turn wrapping `SendraError`, which is
    /// not `Clone`) rules out a second copy even if one were wanted.
    ///
    /// `None` while `run_state` isn't `Completed`. Also `None`, unreachable
    /// in ordinary operation, if a `Completed` run state ever outlived the
    /// history entry it should name — nothing else in this crate clears
    /// `run_history`, so this can only happen if that invariant is broken
    /// elsewhere.
    pub fn current_run(&self) -> Option<&RunOutcome> {
        if !matches!(self.run_state, RunState::Completed) {
            return None;
        }
        self.selected_history().first().map(|entry| &entry.outcome)
    }
}

/// The whole process's state: every open collection, which one is currently
/// on screen, and the handful of things that are genuinely process-wide
/// rather than scoped to any one collection.
///
/// **Why this split from the old, single-collection `AppState`.** Every
/// field that used to live flat on `AppState` — `load_state`, `run_state`,
/// `edit_mode`, `dirty_requests`, `delete_confirm`, the whole environment
/// picker/editor, `response_scroll`, `reveal_captures` — was really a fact
/// about *one collection*, not about the process. Opening a second
/// collection without this split would mean either forcing every open tab to
/// share one selection/run/edit session (impossible — they are genuinely
/// independent activities happening in different files) or bolting a second,
/// parallel copy of every one of those fields onto `AppState` by hand, which
/// does not scale past two tabs and invites exactly the "state bleeds
/// between tabs" bug this design has to rule out structurally. Moving every
/// one of those fields onto [`CollectionSession`] and holding a `Vec` of them
/// here instead makes "a session's own state cannot affect any other
/// session" a fact about the type (each is a separate value in the `Vec`),
/// not a rule every future message handler has to remember to uphold by
/// hand.
///
/// **The `Deref`/`DerefMut` below are load-bearing, not a shortcut.** They
/// make `state.load_state`, `state.edit_mode`, `state.dirty_requests`, and
/// every other field `CollectionSession` carries resolve through
/// `AppState::active`/`active_mut` to whichever session is currently on
/// screen — exactly the field access every `render_*` function and every
/// session-scoped `Message` arm already wrote before this issue, unchanged.
/// That is deliberate: the entire reducer/view surface this crate had before
/// multi-collection support is *about one collection*, and after this split
/// it is still about exactly one collection — the active one — with no
/// change to what it reads or writes. The only code that ever needs to look
/// past the active session is genuinely tab-aware: opening, closing, listing,
/// or switching between tabs (see `update()`'s own top-level dispatch, which
/// handles those messages itself, directly against `state.collections`,
/// before ever reaching a session-scoped one through `active_mut()`), and
/// `Message::RunCompleted`, which is tagged with the run's own collection id
/// specifically so it can reach a session that may no longer be the active
/// one (see that variant's own doc comment). Anything that reads or writes
/// through plain field access on an `&AppState`/`&mut AppState` is, by
/// construction, only ever touching the one session currently in view —
/// which is exactly the guarantee "no bleed-through between tabs" needs.
#[derive(Debug)]
pub struct AppState {
    pub should_quit: bool,
    /// Advanced by one on every `Message::Tick` (roughly every 100ms — see
    /// `next_message` in `main.rs`) — shared by every tab's spinner rather
    /// than one counter per session, since it carries no state worth keeping
    /// separate: it only ever drives which spinner glyph
    /// `render_status_bar` draws for whichever session is active, and two
    /// tabs both showing "in flight" would look identical either way.
    pub spinner_tick: usize,
    /// Every collection currently open, in the order its own tab is shown —
    /// never empty: closing the last remaining one resets it in place
    /// (`LoadState::NoPathProvided`, every other field back to its own
    /// default) rather than removing it, so `active_collection` is always a
    /// valid index and `Deref`/`DerefMut` below never have anything to fail
    /// on.
    pub collections: Vec<CollectionSession>,
    /// Index into `collections` for the tab currently on screen — what every
    /// session-scoped `Message`, and every `render_*` function that takes an
    /// `&AppState`, actually reads and writes through `Deref`/`DerefMut`
    /// (see this struct's own doc comment).
    pub active_collection: usize,
    /// The next id `open_session` hands out — see `CollectionSession::id`'s
    /// own doc comment for why a `Vec` index would be the wrong thing to tag
    /// an in-flight run's eventual `Message::RunCompleted` with.
    next_session_id: u64,
    /// `Some(...)` while the "open another collection" path-input prompt is
    /// on screen — a genuinely process-wide (not per-tab) piece of UI, since
    /// it exists to *create* a new tab, not to edit an existing one's own
    /// state. Modal in the same sense `delete_confirm`/`environment_edit`
    /// already are: `update()`'s own top-level guard refuses every
    /// session-scoped message while this is open, the same way a session's
    /// own modals refuse browsing messages within that session.
    pub open_collection_prompt: Option<OpenCollectionPromptState>,
    /// `Some(...)` while a confirmation is pending for closing the session
    /// named by `CloseConfirm::collection_id` — built on the same shared
    /// [`ConfirmPrompt`] every other destructive-action confirmation in this
    /// crate uses. Named by id rather than always meaning "the active
    /// collection": pinning it is what keeps a mid-confirmation tab switch
    /// (were one ever allowed — `update()`'s own guard in fact refuses
    /// tab-switching while this is open, the same way every other modal in
    /// this crate refuses navigation) from ever closing a different tab than
    /// the one the prompt was actually asking about.
    pub close_confirm: Option<CloseConfirm>,
    /// `Some(...)` while quitting is pending confirmation because at least
    /// one open tab has something `CollectionSession::is_dirty()` says would
    /// be lost — checked across *every* open tab, not just the active one
    /// (`Message::Quit`'s own arm in `update()`), since a run in flight or an
    /// edit left open in a tab the user has since switched away from is just
    /// as real a loss as one in the tab currently on screen. `None` — and so
    /// quitting with nothing at stake anywhere exits immediately, with no
    /// prompt — the instant every tab reports clean.
    pub quit_confirm: Option<ConfirmPrompt>,
}

/// What `Message::CloseCollectionRequested` opens and
/// `Message::ConfirmCloseCollection`/`Message::CancelCloseCollection` close —
/// which tab is pending closure, plus the shared confirmation UI itself.
#[derive(Debug, Clone)]
pub struct CloseConfirm {
    pub collection_id: u64,
    pub prompt: ConfirmPrompt,
}

/// What `Message::OpenCollectionPrompt` opens and
/// `Message::ConfirmOpenCollectionPath`/`Message::CancelOpenCollectionPrompt`
/// close: the path being typed, and (once a confirmed attempt to load it has
/// actually failed) whatever `Document::from_path` said was wrong with it.
#[derive(Debug, Clone, Default)]
pub struct OpenCollectionPromptState {
    pub path: TextField,
    /// `Some(message)` right after a load attempt failed — the prompt stays
    /// open with this set, the same "no-op that keeps you where you can
    /// retry or cancel" shape every other fallible confirm/save in this
    /// crate already has, rather than silently discarding the path that was
    /// typed or leaving the user with no idea why nothing happened.
    pub error: Option<String>,
}

impl Default for AppState {
    /// Hand-written rather than `#[derive(Default)]`: `collections` must
    /// start with exactly one session (see its own doc comment on "never
    /// empty"), not an empty `Vec`, which is what a derived `Default` would
    /// give it — `CollectionSession::default()`'s own `#[derive(Default)]`
    /// is exactly the empty-but-valid starting point (`LoadState::Loading`,
    /// nothing else set) `main::run` already fed straight into `update()`
    /// via `Message::CollectionLoaded`/`NoCollectionPath` before this issue,
    /// unchanged.
    fn default() -> Self {
        Self {
            should_quit: false,
            spinner_tick: 0,
            collections: vec![CollectionSession::default()],
            active_collection: 0,
            next_session_id: 1,
            open_collection_prompt: None,
            close_confirm: None,
            quit_confirm: None,
        }
    }
}

impl AppState {
    /// The session currently on screen — what every `Deref`/`DerefMut` call
    /// through `AppState` actually reaches. `collections`/`active_collection`
    /// together guarantee this never panics: `active_collection` is kept in
    /// `[0, collections.len())` by every piece of code that touches either
    /// (`open_session`, `close_active_session`, `Message::NextCollection`/
    /// `PreviousCollection`), and `collections` itself is never empty.
    pub fn active(&self) -> &CollectionSession {
        &self.collections[self.active_collection]
    }

    pub fn active_mut(&mut self) -> &mut CollectionSession {
        &mut self.collections[self.active_collection]
    }

    /// The session named by `id`, if it is still open — what
    /// `Message::RunCompleted` looks the run's own collection up by, since a
    /// tab may have been closed (or reordered — id, not position, is exactly
    /// what survives that) between the run starting and finishing.
    pub fn session_by_id_mut(&mut self, id: u64) -> Option<&mut CollectionSession> {
        self.collections.iter_mut().find(|session| session.id == id)
    }

    /// Appends `session` as a brand-new tab, assigns it the next id, and
    /// switches to it — the one place a `CollectionSession` is ever added to
    /// `collections`, so `next_session_id` can only ever move forward and
    /// every session's id is unique for the life of the process. Visible to
    /// `super::update`'s `Message::CollectionOpened` arm.
    pub(super) fn open_session(&mut self, mut session: CollectionSession) {
        session.id = self.next_session_id;
        self.next_session_id += 1;
        self.collections.push(session);
        self.active_collection = self.collections.len() - 1;
    }
}

impl std::ops::Deref for AppState {
    type Target = CollectionSession;

    fn deref(&self) -> &CollectionSession {
        self.active()
    }
}

impl std::ops::DerefMut for AppState {
    fn deref_mut(&mut self) -> &mut CollectionSession {
        self.active_mut()
    }
}

#[derive(Debug, Default)]
pub enum LoadState {
    #[default]
    Loading,
    NoPathProvided,
    Loaded {
        document: Box<Document>,
        selected: usize,
        /// Directory `body_file`/multipart paths in the resolved preview
        /// resolve relative to — the directory containing the collection's
        /// own YAML file, exactly as `Request::resolve_body` expects.
        base_dir: PathBuf,
        /// The collection's own YAML file — what `Message::SaveEdit` calls
        /// `Document::save_to_path` with. Distinct from `base_dir`, which is
        /// only ever the *directory* a request's own relative paths resolve
        /// against; this is the file itself, and the two serve entirely
        /// different callers (`Request::resolve_body` vs. `save_to_path`)
        /// even though one is always the other's parent.
        path: PathBuf,
    },
    Failed(SendraError),
}

#[derive(Debug)]
pub enum Message {
    /// `q`/Ctrl+C. Exits immediately when no open tab has anything
    /// `CollectionSession::is_dirty()` would call unsaved; otherwise opens
    /// `AppState::quit_confirm` instead of exiting — see that field's own
    /// doc comment. Sent again while that confirmation is already open (the
    /// quit key pressed a second time, `q` or Ctrl+C either one) it confirms
    /// and exits, the same low-friction "ask once, then get out of the way"
    /// behavior `Message::ConfirmQuit` gives the dedicated `y`/Enter key.
    Quit,
    /// `y`/Enter while `AppState::quit_confirm` is open: exits.
    ConfirmQuit,
    /// `n`/Esc while `AppState::quit_confirm` is open: closes the prompt
    /// without exiting, every open tab's state exactly as it was.
    CancelQuit,
    Tick,
    NoCollectionPath,
    CollectionLoaded {
        base_dir: PathBuf,
        /// The file `result` was loaded from (or attempted to be loaded
        /// from) — carried through regardless of whether `result` is `Ok` or
        /// `Err`, since `update()` only actually stores it in
        /// `LoadState::Loaded` on success, but the message itself is built
        /// before that outcome is known.
        path: PathBuf,
        result: Box<Result<Document, SendraError>>,
    },
    /// The environments `main::load_environments` found at startup — the
    /// ones that loaded, and, separately, the ones that were found but
    /// failed to load (see `AppState::environment_errors`).
    EnvironmentsLoaded {
        environments: Vec<NamedEnvironment>,
        errors: Vec<(String, SendraError)>,
    },
    /// Moves the collection-browser selection when the overlay is closed, or
    /// the overlay's own cursor when it is open — one pair of messages,
    /// bound to arrows/j-k, routed by `update()` to whichever list is
    /// currently on screen rather than adding a second pair of navigation
    /// messages for the overlay.
    SelectNext,
    SelectPrevious,
    OpenEnvironmentOverlay,
    /// Cancel: closes the overlay without changing `active_environment`.
    CloseEnvironmentOverlay,
    /// Confirm: sets `active_environment` to the overlay's cursor, then closes it.
    ConfirmEnvironmentSelection,
    /// Enter or `r` on the selected request. A no-op — see [`super::update::update`] — when a
    /// run is already in flight or nothing is loaded/selected; otherwise
    /// moves `run_state` to `RunState::InFlight`, which is `main`'s cue to
    /// actually spawn the request via [`crate::run_request::spawn`].
    RunRequested,
    /// The spawned run finished, with the real `sendra-core` result —
    /// success or failure — plus the assertion/capture reports evaluated
    /// against it. Always accepted, even while another message would
    /// normally be blocked by an in-flight run, since this is the message
    /// that ends that state.
    ///
    /// **Tagged with `collection_id`, not routed to "the active tab".** The
    /// spawn happens against whichever collection was active *when*
    /// `Message::RunRequested` fired, but the user is free to switch tabs
    /// (or open/close others) before the response comes back — the whole
    /// point of a run in one tab not blocking any other. `main::run`
    /// captures the spawning session's `CollectionSession::id` in the
    /// closure handed to `run_request::spawn`, and `update()` looks that id
    /// up via `AppState::session_by_id_mut` rather than assuming
    /// `active_collection` is still the same tab — a plain `Vec` index would
    /// silently misroute this (or land on the wrong tab entirely) the moment
    /// a tab closes or the user switches away and back in a different order.
    /// A session that has since been closed simply has nowhere to apply the
    /// result to; it is dropped rather than resurrecting a tab that no
    /// longer exists.
    RunCompleted {
        collection_id: u64,
        outcome: RunOutcome,
    },
    /// `h` while browsing: opens the run-history browser
    /// (`CollectionSession::history_overlay`) for the currently selected
    /// request. A no-op while it's already open, or while any other modal
    /// (edit mode, an overlay, a confirmation, an in-flight run) is already
    /// active — see `update()`'s own guards — consistent with every other
    /// mode-entry message in this crate. Opens even when the selected
    /// request has no history yet: the overlay itself says so, the same
    /// "say what isn't there yet, in the pane itself" rule the environment
    /// overlay already follows for zero discovered environments.
    OpenHistoryOverlay,
    /// Esc while the history browser's list has focus: closes it. While
    /// instead viewing one entry's full result (`HistoryOverlay::viewing` is
    /// `Some`), Esc means `CloseHistoryEntryView` (back to the list) —
    /// `main::translate_event` is what tells these two apart, the same way
    /// it already tells the environment overlay's own Esc apart from edit
    /// mode's.
    CloseHistoryOverlay,
    /// Enter on the history browser's list: shows the entry at
    /// `HistoryOverlay::cursor`'s full result in place of the list —
    /// `HistoryOverlay::viewing`'s own doc comment on why this is a
    /// replace, not a toggle.
    ViewHistoryEntry,
    /// Esc while viewing one entry's full result: back to the list,
    /// `HistoryOverlay::viewing` cleared. The exact reverse of
    /// `ViewHistoryEntry`.
    CloseHistoryEntryView,
    /// Space on the history browser's list: toggles whether the entry at
    /// `HistoryOverlay::cursor` shows its inline expanded detail (status
    /// code, assertion/capture summary) in place, without leaving the list —
    /// a lighter-weight look than `ViewHistoryEntry`'s full response-panel
    /// switch, for when a glance is all that's needed. A no-op when the list
    /// is empty; a no-op while `HistoryOverlay::viewing` is `Some`, since
    /// there's no list row to toggle in that view.
    ToggleHistoryEntryExpanded,
    /// `o` while browsing: opens the "open another collection" path-input
    /// prompt (`AppState::open_collection_prompt`). A no-op while any other
    /// modal (edit mode, an overlay, a confirmation) is already open,
    /// consistent with every other mode-entry message in this crate.
    OpenCollectionPrompt,
    /// Enter on the open-collection prompt. Never actually handled inside
    /// `update()` itself — `main::run`'s own loop intercepts this message
    /// before `update()` ever sees it, performs the real (synchronous)
    /// `Document::from_path` + environment discovery the exact same way
    /// `main::main` already does for the very first collection, and replaces
    /// it with a `Message::CollectionOpened` carrying the outcome. This
    /// keeps `update()` itself free of file I/O, the same separation
    /// `Message::RunRequested`/`RunCompleted` already draw for a request
    /// send — the reducer only ever sees results, never performs the I/O
    /// that produces them. Kept as a real, distinct `Message` (rather than
    /// building `CollectionOpened` straight from the keypress in
    /// `main::translate_event`) because that function has no access to
    /// `AppState` at all, by design — it only ever sees the raw key event
    /// plus a handful of booleans, which is what keeps it unit-testable
    /// without a real terminal. `update()` still gives this a real (empty)
    /// arm rather than refusing to compile it as unreachable, since a
    /// directly-constructed `update()` call in a test is exactly as valid a
    /// caller as `main::run`'s own loop.
    ConfirmOpenCollectionPath,
    /// Esc on the open-collection prompt: closes it without opening
    /// anything — nothing was ever created before this point, so there is
    /// nothing to undo.
    CancelOpenCollectionPrompt,
    /// The result of a `Message::ConfirmOpenCollectionPath` that
    /// `main::run`'s loop has already turned into a real load attempt (see
    /// that variant's own doc comment) — carries exactly what
    /// `Message::CollectionLoaded`/`EnvironmentsLoaded` together used to for
    /// the very first collection, bundled into one message since `main::run`
    /// now always produces both at once, for any collection, not just the
    /// first. `Ok(document)` appends a brand-new tab (via
    /// `AppState::open_session`) and switches to it; `Err(error)` opens
    /// nothing and instead leaves the prompt open with the error on
    /// `OpenCollectionPromptState::error`, the same "no-op that keeps you
    /// where you can retry or cancel" shape every other fallible save/confirm
    /// in this crate already has.
    CollectionOpened {
        base_dir: PathBuf,
        path: PathBuf,
        result: Box<Result<Document, SendraError>>,
        environments: Vec<NamedEnvironment>,
        environment_errors: Vec<(String, SendraError)>,
    },
    /// `]`: switches to the next tab, wrapping from the last back to the
    /// first — the same wrap-around `move_selection` already gives the
    /// request list and the environment overlay's own cursor. A no-op with
    /// only one tab open (wrapping to the same index changes nothing).
    NextCollection,
    /// `[`: the exact reverse of `NextCollection`.
    PreviousCollection,
    /// Ctrl+W: closes the active tab — immediately if
    /// `CollectionSession::is_dirty()` says there is nothing at stake, or
    /// through `AppState::close_confirm` otherwise. Closing the very last
    /// remaining tab does not shrink `collections` (see its own doc comment
    /// on why that must never happen) — it resets that one session in place
    /// instead, back to `LoadState::NoPathProvided` with every other field
    /// at its own default, the same blank slate a process started with no
    /// collection path at all already begins in.
    CloseCollectionRequested,
    /// `y`/Enter on the close-tab confirmation: actually closes the session
    /// `AppState::close_confirm` named.
    ConfirmCloseCollection,
    /// `n`/Esc on the close-tab confirmation: leaves every open tab exactly
    /// as it was.
    CancelCloseCollection,
    /// PageDown on the response panel: scrolls its text down a few lines.
    /// A no-op whenever there is nothing showing that could scroll — see
    /// `render_response_panel`, which is the only place `response_scroll` is
    /// read and where the actual clamping against content length happens.
    ///
    /// **Keybinding model:** arrow keys (and `j`/`k`) always move
    /// the request-list selection, full stop — never the response scroll,
    /// regardless of whether a response happens to be showing. The response
    /// panel's own scroll lives entirely on `PageUp`/`PageDown` (this
    /// variant and [`Message::ScrollResponseUp`]) plus `Home`/`End`
    /// ([`Message::ScrollResponseTop`]/[`Message::ScrollResponseBottom`]
    /// below). A focus-switch model (where arrows mean different things
    /// depending on whether the response panel currently has "focus") was
    /// considered and rejected: it would make the same physical key do two
    /// different things depending on state the help bar cannot fully convey
    /// at a glance, an ambiguity worth avoiding. Four
    /// keys with one meaning each, never two keys sharing a meaning that
    /// depends on invisible state, is the simpler and more predictable rule.
    /// `main::next_message` is the one place that turns these keys into
    /// messages; `status_help_text` is what tells the user which apply.
    ScrollResponseDown,
    /// PageUp on the response panel: scrolls its text up a few lines. See
    /// [`Message::ScrollResponseDown`].
    ScrollResponseUp,
    /// `Home` on the response panel: jumps straight to the top (line 0)
    /// rather than requiring repeated `PageUp` presses — chosen because it
    /// doesn't collide with the request-list's own arrow-key bindings,
    /// since `Home`/`End` are not bound to anything else anywhere in the
    /// app. See
    /// [`Message::ScrollResponseDown`] for the full keybinding model this is
    /// part of.
    ScrollResponseTop,
    /// `End` on the response panel: jumps to the last visible page rather
    /// than requiring repeated `PageDown` presses. Implemented by setting
    /// `response_scroll` to `usize::MAX` and letting `render_response_panel`
    /// clamp it against the real content height at render time — the same
    /// clamp every other scroll value already goes through, so this needs
    /// no separate "what's the last valid position" calculation here.
    ScrollResponseBottom,
    /// `c`: flips `reveal_captures`. Masked values — captured values and,
    /// while merely browsing, auth-derived header/query values in the
    /// request preview — become visible, visible values become masked
    /// again — a toggle rather than a one-way reveal, so hiding them again
    /// does not need a second, differently-named key.
    ToggleRevealCaptures,
    /// `n` while browsing: appends a brand-new, mostly-empty request to the
    /// loaded document, selects it, and immediately opens it in edit mode —
    /// the same `EditState`/`Message::SaveEdit`/`CancelEdit` machinery every
    /// other field edit already goes through, not a separate "create
    /// request" UI. A no-op under the same conditions `EnterEditMode` is:
    /// nothing loaded, the environment overlay open, or a run in flight.
    /// See `update::add_request_to_document` for what "mostly-empty" means
    /// and how `sendra_core::Document::Single` (which cannot itself hold a
    /// second request) is handled.
    AddRequest,
    /// Enters edit mode for the currently selected request — see
    /// `AppState::edit_mode`. A no-op (see [`super::update::update`]) unless a request is
    /// actually selected, the environment overlay is closed, and no run is
    /// in flight: edit mode is exclusive with those, the same way the
    /// environment overlay and an in-flight run are already exclusive with
    /// each other and with browsing.
    EnterEditMode,
    /// Ctrl+S while editing: commits `method`/`url` back into the loaded
    /// document (see `update`'s own arm) and leaves edit mode — a no-op that
    /// stays in edit mode, rather than one that discards the edit, when
    /// `method_error` is set: see that arm for why refusing to save invalid
    /// input is not the same as refusing the keystroke that produced it.
    SaveEdit,
    /// Esc while editing: discards the working edit — whatever it changed —
    /// and leaves edit mode, restoring the exact state browsing was in
    /// before `EnterEditMode`. Distinct from `SaveEdit` only in that it
    /// clears `dirty_requests` for the edited index without ever having
    /// written anything back.
    CancelEdit,
    /// `d` while browsing: opens a confirmation prompt (`AppState::
    /// delete_confirm`) to delete the currently selected request. A no-op
    /// under the same conditions `EnterEditMode`/`AddRequest` are (nothing
    /// loaded, the environment overlay open, edit mode active, a run in
    /// flight), plus one more that is specific to deletion: refused outright
    /// when deleting would leave the document with zero requests — see
    /// `update::can_delete`'s own doc comment for why that's never reachable
    /// rather than handled after the fact.
    RequestDelete,
    /// `y`/Enter on the delete confirmation prompt: removes the request
    /// `AppState::delete_confirm` names, persists the result via
    /// `Document::save_to_path`, and closes the prompt — unless the write
    /// fails, in which case the prompt stays open with
    /// `DeleteConfirm::error` set, the same "no-op that keeps you where you
    /// can retry or cancel" shape `Message::SaveEdit` already has for its own
    /// `save_error`.
    ConfirmDelete,
    /// `n`/Esc on the delete confirmation prompt: closes it without touching
    /// the loaded document at all — nothing was ever mutated before this
    /// point (see `update`'s own `Message::RequestDelete`/`ConfirmDelete`
    /// arms), so there is nothing to undo, the same "cancelling is a provable
    /// no-op" guarantee `Message::CancelEdit` already has for a pre-existing
    /// request's edit.
    CancelDelete,
    /// `i` while the environment overlay is open: opens an edit session
    /// (`AppState::environment_edit`) for whichever environment the
    /// overlay's own cursor is currently pointed at. A no-op if an edit
    /// session is already open or nothing is highlighted (no environments
    /// found at all).
    EnterEnvironmentEdit,
    /// Ctrl+S while editing an environment's variables: validates the
    /// working rows (see `update::validate_env_var_rows` — an empty or
    /// duplicated name refuses the save the same way an invalid `method`
    /// already does for a request), then writes them back to the
    /// environment's own file via `Environment::save_to_path` and leaves the
    /// edit session. A failed validation or a failed write leaves the
    /// session open with `EnvironmentEditState::save_error` set, the same
    /// "no-op that keeps you where you can retry" shape `Message::SaveEdit`
    /// already has.
    SaveEnvironmentEdit,
    /// Esc while editing an environment's variables: discards the working
    /// copy — nothing was ever written into `AppState::environments` before
    /// this point, so there is nothing else to undo, the same as
    /// `Message::CancelEdit` for a pre-existing request's edit.
    CancelEnvironmentEdit,
    /// Ctrl+N while editing an environment's variables: appends a new, empty
    /// row and focuses its name field. Mirrors `Message::AddHeaderRow`, kept
    /// as its own message rather than reused for both: the two operate on
    /// entirely different working copies (`EditState::headers` vs.
    /// `EnvironmentEditState::rows`), and conflating them would make one
    /// physical key's meaning depend on which of two mutually-exclusive
    /// modes happens to be open.
    AddEnvVarRow,
    /// Ctrl+D while editing an environment's variables: opens a minimal
    /// confirmation (`EnvironmentEditState::pending_delete`) for whichever
    /// row currently has focus — the actual removal is
    /// `Message::ConfirmDeleteEnvVarRow`. A no-op when nothing is focused.
    RequestDeleteEnvVarRow,
    /// `y`/Enter on the pending-row-delete confirmation: removes the row.
    ConfirmDeleteEnvVarRow,
    /// `n`/Esc on the pending-row-delete confirmation: leaves the row
    /// completely untouched.
    CancelDeleteEnvVarRow,
    /// `Tab` while editing: moves focus to the next field in order (see
    /// `EditField::next`) — method, URL, then each header row's key and
    /// value in turn, wrapping back to method.
    EditFocusNext,
    /// `Shift+Tab` while editing: the exact reverse of `EditFocusNext` (see
    /// `EditField::prev`).
    EditFocusPrev,
    /// A keybinding while editing (not a plain character, so it can't land
    /// inside whatever field is currently focused): appends a new, empty
    /// header row and focuses its key — see `EditState::add_header_row`.
    AddHeaderRow,
    /// A keybinding while editing: removes whichever header row is
    /// currently focused, a no-op if focus is on method or URL — see
    /// `EditState::delete_focused_header_row` for where focus lands
    /// afterward.
    DeleteHeaderRow,
    /// A keybinding while editing (`Ctrl+A`, not a plain character): appends
    /// a new, empty assertion row and focuses its path — see
    /// `EditState::add_assertion_row`. A separate binding from
    /// `AddHeaderRow`'s `Ctrl+N`, since both can be reachable in the same
    /// edit session.
    AddAssertionRow,
    /// A keybinding while editing (`Ctrl+X`): removes whichever assertion
    /// row is currently focused, a no-op everywhere else — see
    /// `EditState::delete_focused_assertion_row` for where focus lands
    /// afterward.
    DeleteAssertionRow,
    /// A keybinding while editing (`Ctrl+P`, not a plain character): appends
    /// a new, empty capture row and focuses its name — see
    /// `EditState::add_capture_row`. A separate binding from `AddHeaderRow`'s
    /// `Ctrl+N` and `AddAssertionRow`'s `Ctrl+A`, since all three kinds of
    /// row can be present in the same edit session.
    AddCaptureRow,
    /// A keybinding while editing (`Ctrl+K`): removes whichever capture row
    /// is currently focused, a no-op everywhere else — see
    /// `EditState::delete_focused_capture_row` for where focus lands
    /// afterward.
    DeleteCaptureRow,
    /// An ordinary printable character typed into whichever field is
    /// currently focused — inserted at the cursor via `TextField::insert_char`.
    EditInsertChar(char),
    /// `Backspace` on the focused field: deletes the character before the
    /// cursor.
    EditBackspace,
    /// `Delete` on the focused field: deletes the character under the
    /// cursor.
    EditDelete,
    /// `Left` on the focused field: ordinarily moves the cursor back one
    /// character, but on a fixed-enum sub-field (an auth sub-field, or an
    /// assertion row's operator/negate flag) toggles it backward instead —
    /// see `EditState::toggle_focused`/`super::update::edit_move_or_toggle`.
    EditCursorLeft,
    /// `Right` on the focused field: the mirror of `EditCursorLeft` — moves
    /// the cursor forward one character, or toggles a fixed-enum sub-field
    /// forward.
    EditCursorRight,
    /// `Up`, only bound while the body field is focused (see
    /// `main::translate_event`) — every other field is single-line, where
    /// vertical movement has no meaning. See `TextField::move_up`.
    EditCursorUp,
    /// `Down`, the mirror of `EditCursorUp`. See `TextField::move_down`.
    EditCursorDown,
    /// A crossterm `Event::Resize` reaching the translation layer in
    /// `main::next_message`. Carries no data and `update` treats it as a
    /// no-op: ratatui's `Terminal::draw` already calls `Terminal::autoresize`
    /// on every frame (see `ratatui_core::terminal::render`/`resize`), which
    /// re-queries the backend's real size, resizes its internal buffers and
    /// clears before the next render whenever that size changed — so the
    /// very next `terminal.draw(|frame| view(...))` call after a resize
    /// already lays out against the new size with no leftover cells from the
    /// old one. This variant exists only so a resize is a distinctly named
    /// event through the loop rather than silently falling into the
    /// catch-all `Message::Tick` arm in `next_message`, which would
    /// incorrectly advance the spinner on a resize alone.
    Resize,
}

/// What the selected request's most recent run did, if anything.
///
/// Holds the real `sendra_core::Response`/`SendraError`, `AssertionReport`
/// and `CaptureReport` a run produced (see [`RunOutcome`]), not a
/// TUI-invented summary — `render_response_panel` formats it, but nothing
/// about the data itself is reshaped, approximated or re-evaluated first.
///
/// **Always about the currently selected request.** Nothing here tracks
/// *which* request a completed run belongs to; instead, `select` resets this
/// back to `Idle` the moment the request-list selection actually moves, so
/// `Completed` can never be misread as an answer for a request other than
/// the one it was sent for.
///
/// **`Completed` carries no data of its own.** Earlier this held the run's
/// own `RunOutcome` directly (`Completed(RunOutcome)`); now that outcome
/// lives in `CollectionSession::run_history` instead — the request's newest
/// history entry — and `Completed` is only a marker that such an entry
/// exists. See `CollectionSession::current_run`'s own doc comment for the
/// full reasoning: chiefly that `RunOutcome` has no `Clone` to make a second
/// copy from (it holds a `Result<_, RunError>` wrapping `SendraError`), so
/// keeping the real value in exactly one place — history — rather than
/// duplicating it onto `RunState` too is both the simpler design and the
/// only one `RunOutcome`'s own type allows.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    #[default]
    Idle,
    InFlight,
    Completed,
}
