use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use sendra_core::{
    AssertionReport, CaptureReport, Document, Environment, Request, Response, SendraError,
};

use crate::run_request::RunOutcome;

/// Body preview is capped rather than shown in full — scrolling through a
/// large body is issue 13's job; this just keeps a multi-megabyte body from
/// making every draw slower without ever crashing on one.
const MAX_BODY_PREVIEW_CHARS: usize = 2000;

/// One environment sendra-tui found in `.sendra/environments/`, loaded (not
/// just named) so the overlay can show its variables before it's picked.
#[derive(Debug, Clone)]
pub struct NamedEnvironment {
    pub name: String,
    pub environment: Environment,
}

/// The state of an in-progress edit of the selected request. Empty for now —
/// this issue lays down the enter/save/cancel/dirty scaffolding every real
/// editable field (method, URL, headers, ...) will hang off of starting at
/// issue 18, but adds none of them itself. `dirty` is the one thing that
/// already means something: it starts `false` and, once real editing
/// messages exist, they will flip it `true` the same way they mutate a
/// working copy of the field they touch — nothing here yet does, since there
/// is nothing to touch.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EditState {
    pub dirty: bool,
}

#[derive(Debug, Default)]
pub struct AppState {
    pub should_quit: bool,
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
    /// the overlay via [`render_error`] — rather than a file that silently
    /// never appears in the list with no indication anything went wrong.
    pub environment_errors: Vec<(String, SendraError)>,
    /// Index into `environments` for the environment the detail pane
    /// resolves against, or `None` — the honest starting state from issue 5
    /// — until the user picks one in the overlay.
    pub active_environment: Option<usize>,
    /// `Some(cursor)` while the environment-picker overlay is open, `None`
    /// otherwise. The cursor is the overlay's own selection, separate from
    /// `active_environment`, so browsing the list and cancelling
    /// (`Message::CloseEnvironmentOverlay`) never touches what is active.
    pub environment_overlay: Option<usize>,
    /// The selected request's most recent run, if any has been started.
    pub run_state: RunState,
    /// Advanced by one on every `Message::Tick` (roughly every 100ms — see
    /// `next_message` in `main.rs`), and read only to pick which spinner
    /// glyph `render_status_bar` draws while `run_state` is `InFlight`. Not
    /// meaningful on its own; it exists purely to make the spinner animate.
    pub spinner_tick: usize,
    /// How many lines into the response panel's text the view is scrolled —
    /// see `render_response_panel`. Reset to `0` whenever it would otherwise
    /// point at a different run's text: a fresh `RunRequested` and a
    /// changed request-list selection both reset it, in `update` and
    /// `select` respectively.
    pub response_scroll: usize,
    /// Whether captured values are shown in the clear rather than masked —
    /// `Message::ToggleRevealCaptures`, bound to `c`. Starts `false` (masked)
    /// every time, is never written anywhere but this in-memory field, and is
    /// reset back to `false` on the same two events that reset `run_state`
    /// (a fresh `RunRequested`, a changed request-list selection) — see the
    /// doc comment on `render_capture_section` for why "never persisted,
    /// never auto-revealed on the next run" means resetting it there too,
    /// not only at process start.
    pub reveal_captures: bool,
    /// `Some(EditState)` while the request at `selected` (in
    /// `LoadState::Loaded`) is being edited, `None` while merely browsing —
    /// V1's only mode. Entering and leaving flows through
    /// `Message::EnterEditMode`/`Message::SaveEdit`/`Message::CancelEdit`
    /// like every other state transition (see `update()`), never set
    /// directly from `main.rs` or anywhere outside this module's own
    /// `update()` — an Elm-style side-channel flag is exactly what this
    /// issue's own instructions rule out.
    pub edit_mode: Option<EditState>,
    /// Indices into the current document's `requests()` that have unsaved
    /// edits — inserted by whatever future editing message mutates a
    /// request's working copy, removed by `Message::SaveEdit` and
    /// `Message::CancelEdit`. Lives here, not on `Document` itself: nothing
    /// in `sendra-core` exposes a mutable `Document`, and "which requests
    /// have unsaved TUI-local edits" is sendra-tui's own bookkeeping, not
    /// something a collection file format should have to represent.
    pub dirty_requests: HashSet<usize>,
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
    },
    Failed(SendraError),
}

#[derive(Debug)]
pub enum Message {
    Quit,
    Tick,
    NoCollectionPath,
    CollectionLoaded {
        base_dir: PathBuf,
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
    /// the overlay's own cursor when it is open — the same two messages
    /// issue 4 already wired to arrows/j-k, routed by `update()` to whichever
    /// list is currently on screen rather than adding a second pair of
    /// navigation messages for the overlay.
    SelectNext,
    SelectPrevious,
    OpenEnvironmentOverlay,
    /// Cancel: closes the overlay without changing `active_environment`.
    CloseEnvironmentOverlay,
    /// Confirm: sets `active_environment` to the overlay's cursor, then closes it.
    ConfirmEnvironmentSelection,
    /// Enter or `r` on the selected request. A no-op — see [`update`] — when a
    /// run is already in flight or nothing is loaded/selected; otherwise
    /// moves `run_state` to `RunState::InFlight`, which is `main`'s cue to
    /// actually spawn the request via [`crate::run_request::spawn`].
    RunRequested,
    /// The spawned run finished, with the real `sendra-core` result —
    /// success or failure — plus the assertion/capture reports evaluated
    /// against it. Always accepted, even while another message would
    /// normally be blocked by an in-flight run, since this is the message
    /// that ends that state.
    RunCompleted(RunOutcome),
    /// PageDown on the response panel: scrolls its text down a few lines.
    /// A no-op whenever there is nothing showing that could scroll — see
    /// `render_response_panel`, which is the only place `response_scroll` is
    /// read and where the actual clamping against content length happens.
    ///
    /// **Keybinding model (issue 13):** arrow keys (and `j`/`k`) always move
    /// the request-list selection, full stop — never the response scroll,
    /// regardless of whether a response happens to be showing. The response
    /// panel's own scroll lives entirely on `PageUp`/`PageDown` (this
    /// variant and [`Message::ScrollResponseUp`]) plus `Home`/`End`
    /// ([`Message::ScrollResponseTop`]/[`Message::ScrollResponseBottom`]
    /// below). A focus-switch model (where arrows mean different things
    /// depending on whether the response panel currently has "focus") was
    /// considered and rejected: it would make the same physical key do two
    /// different things depending on state the help bar cannot fully convey
    /// at a glance, exactly the ambiguity this issue asks to avoid. Four
    /// keys with one meaning each, never two keys sharing a meaning that
    /// depends on invisible state, is the simpler and more predictable rule.
    /// `main::next_message` is the one place that turns these keys into
    /// messages; `status_help_text` is what tells the user which apply.
    ScrollResponseDown,
    /// PageUp on the response panel: scrolls its text up a few lines. See
    /// [`Message::ScrollResponseDown`].
    ScrollResponseUp,
    /// `Home` on the response panel: jumps straight to the top (line 0)
    /// rather than requiring repeated `PageUp` presses — the "finer/complete
    /// scrolling" half of issue 13 that doesn't collide with the
    /// request-list's own arrow-key bindings, since `Home`/`End` are not
    /// bound to anything else anywhere in the app. See
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
    /// `c`: flips `reveal_captures`. Masked values become visible, visible
    /// values become masked again — a toggle rather than a one-way reveal,
    /// so hiding them again does not need a second, differently-named key.
    ToggleRevealCaptures,
    /// Enters edit mode for the currently selected request — see
    /// `AppState::edit_mode`. A no-op (see [`update`]) unless a request is
    /// actually selected, the environment overlay is closed, and no run is
    /// in flight: edit mode is exclusive with those, the same way the
    /// environment overlay and an in-flight run are already exclusive with
    /// each other and with browsing.
    EnterEditMode,
    /// Ctrl+S while editing: commits the working edit and leaves edit mode.
    /// Nothing here has a real field to write back yet (see
    /// `AppState::edit_mode`'s own doc comment) — this issue proves the
    /// keybinding and the mode transition, not the content being saved.
    SaveEdit,
    /// Esc while editing: discards the working edit — whatever it changed —
    /// and leaves edit mode, restoring the exact state browsing was in
    /// before `EnterEditMode`. Distinct from `SaveEdit` only in that it
    /// clears `dirty_requests` for the edited index without ever having
    /// written anything back.
    CancelEdit,
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
#[derive(Debug, Default)]
pub enum RunState {
    #[default]
    Idle,
    InFlight,
    Completed(RunOutcome),
}

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
            // overlay and InFlight — the two states issue 17 decided edit
            // mode is exclusive with (see `AppState::edit_mode`'s doc
            // comment). `request_is_selected` is the same check
            // `RunRequested` already uses for "is there anything here to
            // act on".
            if state.edit_mode.is_none()
                && state.environment_overlay.is_none()
                && !matches!(state.run_state, RunState::InFlight)
                && request_is_selected(state)
            {
                state.edit_mode = Some(EditState::default());
            }
        }
        Message::SaveEdit => {
            if state.edit_mode.take().is_some() {
                if let LoadState::Loaded { selected, .. } = &state.load_state {
                    state.dirty_requests.remove(selected);
                }
            }
        }
        Message::CancelEdit => {
            if state.edit_mode.take().is_some() {
                if let LoadState::Loaded { selected, .. } = &state.load_state {
                    state.dirty_requests.remove(selected);
                }
            }
        }
        // See the doc comment on `Message::Resize` — the redraw itself
        // comes from `terminal.draw` re-running `view` against the
        // already-resized backend on the loop's next iteration; there is no
        // state here for a resize to change.
        Message::Resize => {}
    }
}

/// Lines moved per `PageUp`/`PageDown` on the response panel. Not tied to
/// the pane's actual height — this is the minimal scroll the issue asks
/// for, not the full viewport-aware paging of issue 13 — so a fixed step
/// that comfortably outruns typical pane heights is simplest.
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

pub fn view(state: &AppState, frame: &mut Frame) {
    // The bottom row is reserved in every `LoadState`, not only `Loaded` —
    // an error state must not dead-end the app, and the help bar showing
    // which keys still work (at minimum quit, per `status_help_text`) is
    // exactly what makes that visible rather than assumed. See that
    // function's own doc comment for how it reads `load_state` to keep this
    // honest instead of always claiming nav/run apply.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(frame.area());

    match &state.load_state {
        LoadState::Loading => render_message(frame, rows[0], "Loading collection..."),
        LoadState::NoPathProvided => {
            render_message(frame, rows[0], "No collection path provided.");
        }
        LoadState::Failed(error) => {
            render_error(frame, rows[0], "Failed to load collection", error);
        }
        LoadState::Loaded {
            document,
            selected,
            base_dir,
        } => {
            let panes = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                .split(rows[0]);

            render_request_list(frame, panes[0], document, *selected, &state.dirty_requests);
            render_detail_pane(frame, panes[1], document, *selected, base_dir, state);
        }
    }

    render_status_bar(frame, rows[1], state);

    if let Some(cursor) = state.environment_overlay {
        render_environment_overlay(frame, state, cursor);
    }
}

fn render_message(frame: &mut Frame, area: Rect, text: &str) {
    frame.render_widget(Paragraph::new(text.to_string()), area);
}

/// One error, formatted the same way everywhere sendra-tui shows one: a
/// short heading naming *what* failed, then the real error's own `Display`
/// text, verbatim, on the line(s) under it — never paraphrased or
/// re-summarized. Every place with an error to show (a collection that
/// failed to load, an environment file that failed to load, a preview that
/// could not be resolved, a run that failed) builds its text through this,
/// so there is one error box style in the whole crate rather than three or
/// four ad hoc ones that happen to drift apart over time.
fn format_error(heading: &str, error: &impl std::fmt::Display) -> String {
    format!("⚠ {heading}\n{error}")
}

/// [`format_error`], rendered into `area` as a wrapped `Paragraph` — the
/// standalone-error half of the pair; [`format_error`] alone is what the
/// call sites that embed an error inside other text (the request preview,
/// the response panel) use instead, since those need the string, not a
/// widget of their own.
fn render_error(frame: &mut Frame, area: Rect, heading: &str, error: &impl std::fmt::Display) {
    frame.render_widget(
        Paragraph::new(format_error(heading, error)).wrap(Wrap { trim: false }),
        area,
    );
}

/// A `ListState` built fresh every frame, `offset` always starting at `0`,
/// deliberately — nothing in `AppState` tracks a scroll/viewport position
/// for this list. That is not a gap: ratatui's `List` widget computes its
/// own visible window from `state.selected` and `state.offset` on every
/// render (`ratatui_widgets::list::rendering::List::get_items_bounds`),
/// walking the offset forward or back as needed until the selected index
/// falls inside the area actually available — so handing it a fresh
/// `offset: 0` and the real `selected` index each frame reliably scrolls
/// the list to keep the selection visible, including the two directions a
/// hand-rolled viewport-window calculation would have to special-case
/// itself: scrolling down past the bottom of the current window, and
/// wrap-around (`move_selection` in this file) snapping straight from the
/// last item back to the first, or the first back to the last, which must
/// re-scroll the window to the opposite end in one step. Verified, not
/// assumed — see `collection_browser_scrolls_to_keep_selection_visible`.
fn render_request_list(
    frame: &mut Frame,
    area: Rect,
    document: &Document,
    selected: usize,
    dirty_requests: &HashSet<usize>,
) {
    let items: Vec<ListItem> = document
        .requests()
        .iter()
        .enumerate()
        .map(|(index, request)| {
            let name = request.name.as_deref().unwrap_or("(unnamed)");
            // `*` for unsaved edits, matching the same marker convention as
            // an editor's modified-buffer indicator — a leading space in the
            // ordinary case keeps every row's method column aligned rather
            // than shifting only dirty rows one character right.
            let marker = if dirty_requests.contains(&index) {
                "*"
            } else {
                " "
            };
            ListItem::new(format!("{marker}{} {name}", request.method))
        })
        .collect();

    let list = List::new(items).highlight_style(Style::new().add_modifier(Modifier::REVERSED));

    let mut list_state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, area, &mut list_state);
}

/// Once a run has completed for the currently selected request, this pane
/// shows the result **instead of** the request preview — replace, not a
/// toggle or a second tab. Two reasons: there is no natural third state to
/// toggle back and forth between once the answer to "what did this request
/// actually do" exists (the preview is only ever a stand-in for that
/// answer), and `RunState` already resets to `Idle` on every request-list
/// selection change (see its doc comment), so the preview reappears on its
/// own the moment it is relevant again — a toggle key would be one more
/// binding for a state that already un-does itself.
fn render_detail_pane(
    frame: &mut Frame,
    area: Rect,
    document: &Document,
    selected: usize,
    base_dir: &Path,
    state: &AppState,
) {
    let Some(request) = document.requests().get(selected) else {
        frame.render_widget(Paragraph::new("No request selected."), area);
        return;
    };

    if let RunState::Completed(outcome) = &state.run_state {
        render_response_panel(
            frame,
            area,
            outcome,
            state.response_scroll,
            state.reveal_captures,
        );
        return;
    }

    let active = active_environment(state);
    let environment = active.map_or_else(Environment::default, |named| named.environment.clone());

    let text = match resolve_preview(request, base_dir, &environment) {
        Ok(resolved) => format_resolved_request(&resolved),
        // Honest about what's actually active: with nothing selected yet
        // (issue 5's default), a `{{var}}` request surfaces the same
        // `VariableNotFound` core itself raises against an empty
        // environment; with one selected, the same error means that
        // environment specifically does not define the variable — either
        // way, the real sendra-core error is shown, never a faked value.
        Err(error) => match active {
            Some(named) => format_error(
                &format!(
                    "Could not resolve preview against environment '{}'",
                    named.name
                ),
                &error,
            ),
            None => format_error(
                "Could not resolve preview (no environment selected yet)",
                &error,
            ),
        },
    };

    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), area);
}

/// The completed-run half of the detail pane: a real response's status,
/// headers and body, or the real `SendraError` that stopped it — pulled
/// straight from the `Response`/`SendraError` already sitting in
/// `RunState::Completed`, nothing recomputed or re-derived.
///
/// **Not wrapped, and scrolled by whole lines only.** The request preview
/// above wraps long lines because a request body is authored by the same
/// person reading it and rarely wide; a response body has no such
/// guarantee — a minified JSON payload is one line that can run to
/// thousands of characters — so wrapping it would make `response_scroll`'s
/// "line N" meaningless (a wrapped line renders as several visual rows,
/// and the count depends on pane width). Scrolling raw lines and letting a
/// too-wide one clip at the pane edge is the same trade-off `less` (without
/// `-S`) and most response viewers make, and it is what keeps the scroll
/// math in this function simple and always correct rather than an
/// approximation of ratatui's own wrapping.
///
/// The last line is always a footer — `Line a-b of n` plus the scroll keys
/// — never only shown once content overflows, so the pane never scrolls
/// silently: there is always something on screen saying whether there is
/// more, exactly the same posture `truncate_body` takes with its own
/// `[truncated to N characters]` marker.
fn render_response_panel(
    frame: &mut Frame,
    area: Rect,
    outcome: &RunOutcome,
    scroll: usize,
    reveal_captures: bool,
) {
    let text = format_run_result(outcome, reveal_captures);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let content_height = rows[0].height as usize;
    let max_scroll = total.saturating_sub(content_height.max(1));
    let scroll = scroll.min(max_scroll);

    let paragraph = Paragraph::new(text.clone()).scroll((scroll.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(paragraph, rows[0]);

    let last_visible = (scroll + content_height).min(total);
    let reveal_hint = if outcome.capture.is_empty() {
        String::new()
    } else if reveal_captures {
        "  |  c: hide captures".to_string()
    } else {
        "  |  c: reveal captures".to_string()
    };
    let footer = format!(
        "Line {}-{} of {total} — PgUp/PgDn to scroll{reveal_hint}",
        scroll.saturating_add(1).min(total.max(1)),
        last_visible,
    );
    frame.render_widget(
        Paragraph::new(footer).style(Style::new().add_modifier(Modifier::DIM)),
        rows[1],
    );
}

/// The text `render_response_panel` shows: on success, the response laid out
/// by `format_response`, followed by the assertion and capture sections
/// (`format_assertions`/`format_capture_section`) — always both, even when
/// their reports are empty, so "no assertions declared" reads distinctly
/// from either a passing or a failing assertion block, and likewise for
/// captures. On failure, [`format_error`] over the real
/// [`run_request::RunError`]'s own `Display` — the same wording
/// `sendra run` itself would print for the same `sendra_core::SendraError`
/// (`RunError::Core`), or, for the one failure that is not core's to report
/// (`RunError::RuntimeUnavailable` — see that variant's doc comment), the
/// honest reason the pipeline never even reached the network. Either way
/// there is no response to lay out and nothing was checked or captured, so
/// this is the whole of it: the real error, not a TUI-invented summary of
/// it and not two sections falsely claiming "no assertions declared" for a
/// request that may well have some.
fn format_run_result(outcome: &RunOutcome, reveal_captures: bool) -> String {
    match &outcome.result {
        Ok(response) => {
            let mut text = format_response(response);
            text.push('\n');
            text.push_str(&format_assertions(&outcome.assertions));
            text.push('\n');
            text.push_str(&format_capture_section(&outcome.capture, reveal_captures));
            text
        }
        Err(error) => format_error("Request failed", error),
    }
}

/// `assertions` heading, one line per check (`✓`/`✗` plus core's own
/// expectation/failure wording — [`sendra_core::AssertionResult`] renders
/// the words, this only lays them out), then a pass/fail count — the same
/// per-assertion granularity and the same wording as sendra-cli's own
/// `print_assertions` in `sendra-cli/src/output/human.rs`, reimplemented
/// (uncoloured) rather than imported for the same cross-crate reason
/// `body_for_display` is. **Declared distinctly from "declared and all
/// passed"**: an empty report — no `assertions:` block, or an empty one —
/// renders `no assertions declared` instead of silently matching the "0
/// failed" case a passing block would also produce, mirroring the
/// skipped-vs-passed distinction sendra-cli's own `--junit` output already
/// makes for the same report type.
fn format_assertions(report: &AssertionReport) -> String {
    if report.is_empty() {
        return "assertions\n  no assertions declared".to_string();
    }

    let mut lines = vec!["assertions".to_string()];
    for result in report.results() {
        match &result.failure {
            None => lines.push(format!("  ✓ {}", result.expectation)),
            Some(detail) => lines.push(format!("  ✗ {} — {detail}", result.expectation)),
        }
    }

    if report.passed() {
        lines.push(format!("  {} passed", report.passed_count()));
    } else {
        lines.push(format!(
            "  {} passed, {} failed",
            report.passed_count(),
            report.failed_count()
        ));
    }

    lines.join("\n")
}

/// A captured value that has not been revealed this session — the TUI's own
/// interactive counterpart to `--show-captures`'s default, not a value
/// sendra-cli itself ever prints to a terminal (its own `print_capture`
/// never shows the value at all, revealed or not; only `--json` carries it,
/// gated by that same flag). Same placeholder text `--json`'s
/// `REDACTED_CAPTURE_VALUE` uses (`sendra-cli/src/output/json.rs`), so the
/// two tools agree on what "hidden" is spelled as.
const REDACTED_CAPTURE_VALUE: &str = "<redacted>";

/// `capture` heading, one line per entry — a captured value shown as
/// [`REDACTED_CAPTURE_VALUE`] unless `reveal` is true, a failed entry's real
/// `CaptureFailure` message always shown regardless of `reveal` (a failure
/// never carried a value to begin with, so there is nothing to redact — the
/// same rule `--json`'s `CaptureRecord` follows: "failures is never
/// redacted") — then, like [`format_assertions`], `no captures declared`
/// for an empty report rather than looking like a capture block that
/// declared nothing wrong.
///
/// `reveal` is `AppState::reveal_captures` — session-only and reset on every
/// new run and every selection change (see its doc comment); nothing here
/// writes it anywhere durable, so a masked capture is masked again the next
/// time this function runs unless the user asks again.
fn format_capture_section(report: &CaptureReport, reveal: bool) -> String {
    if report.is_empty() {
        return "capture\n  no captures declared".to_string();
    }

    let mut lines = vec!["capture".to_string()];
    for result in report.results() {
        let from = format!("{} from `{}`", result.variable, result.path);
        match result.failure() {
            None => {
                let value = if reveal {
                    result.value().unwrap_or_default()
                } else {
                    REDACTED_CAPTURE_VALUE
                };
                lines.push(format!("  ✓ {from} = {value}"));
            }
            Some(failure) => lines.push(format!("  ✗ {from} — {failure}")),
        }
    }

    lines.join("\n")
}

/// Mirrors sendra-cli's own response layout
/// (`sendra-cli/src/output/human.rs::print_response`/`print_status_line`)
/// closely enough that the two are a direct side-by-side match for the same
/// response: status code, status text and elapsed time on one line, every
/// header in the order the response actually carried them, a blank line,
/// then the body. Uncoloured, unlike the CLI's terminal output — colour is
/// the one thing this deliberately does not reproduce, since ratatui styling
/// is a separate concern from the data being correct.
fn format_response(response: &Response) -> String {
    let mut lines = vec![format!(
        "{} {}  {} ms",
        response.status,
        response.status_text,
        response.elapsed.as_millis()
    )];

    for (name, value) in &response.headers {
        lines.push(format!("{name}: {value}"));
    }

    if !response.body.is_empty() {
        lines.push(String::new());
        lines.push(body_for_display(response));
    }

    lines.join("\n")
}

/// Pretty-prints the body when `Content-Type` claims JSON and it actually
/// parses as JSON, otherwise returns it unchanged — the same rule, sniffed
/// the same way, as sendra-cli's `body_for_display`/`claims_json` in
/// `output/human.rs`. Reimplemented here rather than imported, since
/// sendra-tui depends on sendra-core and not on sendra-cli, and this is
/// display formatting, not something sendra-core itself does or should do.
fn body_for_display(response: &Response) -> String {
    if !claims_json(&response.headers) {
        return response.body.clone();
    }

    match serde_json::from_str::<serde_json::Value>(&response.body) {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|_| response.body.clone()),
        Err(_) => response.body.clone(),
    }
}

/// Whether these response headers say the body is JSON — see the doc
/// comment on [`body_for_display`] for why this mirrors sendra-cli's own
/// `claims_json` instead of calling it.
fn claims_json(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .any(|(_, value)| {
            let media_type = value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            media_type == "application/json" || media_type.ends_with("+json")
        })
}

/// Cycled by `spinner_tick` while a run is in flight — an ordinary braille
/// spinner, no library, since ratatui ships no widget for one.
const SPINNER_FRAMES: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];

/// The one-line status/help bar under the panes: a status summary (in
/// flight, or what the last run did) where there is one, and — always — the
/// keybindings that actually do something right now.
///
/// Built entirely from [`status_help_text`], which is also what the tests
/// below exercise directly: this function's only job is handing that string
/// to a `Paragraph`.
fn render_status_bar(frame: &mut Frame, area: Rect, state: &AppState) {
    frame.render_widget(Paragraph::new(status_help_text(state)), area);
}

/// The bottom bar's full text — status where there is one, then the
/// keybindings currently live — chosen from exactly the state `update()`
/// itself branches on, so this can never say a key does something `update()`
/// would actually refuse, or omit one it would accept. Five contexts, in the
/// same priority order `update()`'s own InFlight/edit-mode guards and
/// `view()`'s own overlay-vs-response-panel-vs-preview dispatch already
/// imply:
///
/// 1. **The environment overlay is open** (`environment_overlay.is_some()`,
///    the same condition `view()` checks to draw it) — only the overlay's
///    own keys apply, checked first because the overlay is drawn on top of
///    everything else and is what has the user's attention, and because
///    nothing about `next_message` stops it opening over a `load_state`
///    that isn't `Loaded` (see the next context). Mutually exclusive with
///    edit mode below — `update()` refuses to open the overlay while
///    `edit_mode` is set, and refuses to enter edit mode while the overlay
///    is open — so these two contexts never need to be prioritized against
///    each other, only checked in some order.
/// 2. **The selected request is being edited** (`edit_mode.is_some()`) —
///    `update()`'s own edit-mode guard refuses every navigation/overlay/run
///    message while this holds, the same way the InFlight guard does for a
///    run, so only `Ctrl+S`/`Esc`/quit are genuinely live.
/// 3. **No collection is loaded** (`load_state` is `Loading`,
///    `NoPathProvided` or `Failed` — see `view()`'s own match on it): there
///    is no request list and nothing to run, so nav/run are not offered;
///    the environment overlay and quit are the only two keys that do
///    anything, and both keep working, which is the whole point of this
///    context existing — a failed collection load must not read as a dead
///    end.
/// 4. **A run is in flight** (`RunState::InFlight`) — `update()`'s own guard
///    at the top of the function refuses every navigation/overlay/run
///    message while this holds, so `q` (never blocked — see that guard's own
///    comment) is genuinely the only key left to advertise.
/// 5. **A run has completed** (`RunState::Completed`) — the response panel
///    is what `render_detail_pane` is showing (see its own doc comment), so
///    this is where the scroll keys and, when there is something to reveal,
///    the capture-reveal key belong; nav/run/env/quit are back too, since
///    the InFlight guard no longer applies.
/// 6. **Otherwise** (`RunState::Idle`, a collection is loaded) — the
///    ordinary collection browser, showing the request preview.
///
/// No key is added here that `update`/`main::next_message` do not already
/// bind — this only narrates keys issues 1-9 and 17 already wired.
fn status_help_text(state: &AppState) -> String {
    if state.environment_overlay.is_some() {
        return "↑/↓ nav  enter confirm  esc cancel  q quit".to_string();
    }

    if let Some(edit) = &state.edit_mode {
        let dirty = if edit.dirty { " (unsaved changes)" } else { "" };
        return format!("Editing{dirty}  |  ctrl+s save  esc cancel  q quit");
    }

    if !matches!(state.load_state, LoadState::Loaded { .. }) {
        return "e env  q quit".to_string();
    }

    match &state.run_state {
        RunState::Idle => "↑/↓ nav  enter/r run  i edit  e env  q quit".to_string(),
        RunState::InFlight => {
            let frame_char = SPINNER_FRAMES[state.spinner_tick % SPINNER_FRAMES.len()];
            format!("{frame_char} Running request...  |  q quit")
        }
        RunState::Completed(outcome) => {
            let status = match &outcome.result {
                Ok(response) => {
                    let assertions = if outcome.assertions.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " — {} passed, {} failed",
                            outcome.assertions.passed_count(),
                            outcome.assertions.failed_count()
                        )
                    };
                    format!("Done — {}{assertions}", response.status)
                }
                Err(error) => format!("Failed — {error}"),
            };
            let reveal = if outcome.capture.is_empty() {
                ""
            } else {
                "  c reveal/hide captures"
            };
            format!(
                "{status}  |  ↑/↓ nav  enter/r run again  PgUp/PgDn/Home/End scroll{reveal}  i edit  e env  q quit"
            )
        }
    }
}

/// The environment the detail pane resolves against and a run sends
/// against — the one `active_environment` names, or `None` before the user
/// has picked one. `pub` (not `fn`-private) because `main` needs the exact
/// same lookup to resolve a run's request against, rather than a second
/// implementation of "which environment is active" living outside `app.rs`.
pub fn active_environment(state: &AppState) -> Option<&NamedEnvironment> {
    state
        .active_environment
        .and_then(|index| state.environments.get(index))
}

/// The same substitution + auth/query/body resolution pipeline sendra-cli's
/// `--dry-run` runs before sending, reused directly rather than
/// reimplemented. Deliberately narrower than the CLI's full pipeline in two
/// ways, both because this is a preview, not a run: it skips OAuth token
/// acquisition (`Request::resolve_oauth`), a real network call with no place
/// in drawing a frame, and it skips `Config::apply`, since the TUI has no
/// project `Config` loaded anywhere yet — so the headers shown here are the
/// request's own, substituted, not the final wire-level set an actual run
/// would send after config defaults are layered on. `environment` is passed
/// in as a real, caller-chosen value rather than assumed here — an empty one
/// when nothing is active, matching issue 5, or the one the user picked in
/// the overlay — so any environment-level default (its own `auth` block
/// included) resolves exactly as `Environment::apply` already defines it.
fn resolve_preview(
    request: &Request,
    base_dir: &Path,
    environment: &Environment,
) -> Result<Request, SendraError> {
    environment
        .apply(request)
        .and_then(|request| request.resolve_auth())
        .and_then(|request| request.resolve_query())
        .and_then(|request| request.resolve_body(base_dir))
}

fn format_resolved_request(request: &Request) -> String {
    let mut lines = vec![
        format!("Method: {}", request.method),
        format!("URL:    {}", request.url),
        String::new(),
        "Headers:".to_string(),
    ];

    if request.headers.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        for (name, value) in &request.headers {
            lines.push(format!("  {name}: {value}"));
        }
    }

    lines.push(String::new());
    lines.push("Body:".to_string());
    match request.body.as_deref().filter(|body| !body.is_empty()) {
        Some(body) => {
            let (preview, truncated) = truncate_body(body);
            lines.push(preview);
            if truncated {
                lines.push(format!(
                    "... [truncated to {MAX_BODY_PREVIEW_CHARS} characters]"
                ));
            }
        }
        None => lines.push("  (none)".to_string()),
    }

    lines.join("\n")
}

fn truncate_body(body: &str) -> (String, bool) {
    if body.chars().count() <= MAX_BODY_PREVIEW_CHARS {
        (body.to_string(), false)
    } else {
        (body.chars().take(MAX_BODY_PREVIEW_CHARS).collect(), true)
    }
}

/// A rect roughly `percent_x`/`percent_y` of `area`, centered within it — the
/// standard ratatui popup-centering pattern, pure layout math with nothing
/// project-specific to reuse from elsewhere.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

/// `area` shrunk by one cell on every side — where content goes inside a
/// bordered `Block` drawn over the same `area`.
fn inset(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

fn render_environment_overlay(frame: &mut Frame, state: &AppState, cursor: usize) {
    let area = centered_rect(70, 70, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title("Select environment — Enter to confirm, Esc to cancel"),
        area,
    );

    let inner = inset(area);

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

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_COLLECTION: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
  - name: Two
    method: GET
    url: https://example.com/two
";

    const THREE_REQUEST_COLLECTION: &str = "\
name: test
requests:
  - name: One
    method: GET
    url: https://example.com
  - name: Two
    method: GET
    url: https://example.com/two
  - name: Three
    method: GET
    url: https://example.com/three
";

    const MALFORMED_YAML: &str = "requests: [this is not valid yaml";

    /// A collection of `n` requests named `Request0`..`Request{n-1}` — used
    /// to build a collection taller than a small test terminal, to exercise
    /// the request list's own scrolling.
    fn many_request_collection(n: usize) -> String {
        let mut yaml = String::from("name: test\nrequests:\n");
        for i in 0..n {
            yaml.push_str(&format!(
                "  - name: Request{i}\n    method: GET\n    url: https://example.com/{i}\n"
            ));
        }
        yaml
    }

    fn loaded_state(yaml: &str) -> AppState {
        let mut state = AppState::default();
        let document = Document::from_yaml_str(yaml).expect("valid test YAML");
        update(
            &mut state,
            Message::CollectionLoaded {
                base_dir: PathBuf::from("."),
                result: Box::new(Ok(document)),
            },
        );
        state
    }

    fn named_environment(name: &str, variables: &[(&str, &str)]) -> NamedEnvironment {
        let mut environment = Environment::default();
        for (key, value) in variables {
            environment
                .variables
                .insert((*key).to_string(), (*value).to_string());
        }
        NamedEnvironment {
            name: name.to_string(),
            environment,
        }
    }

    fn state_with_environments(names: &[&str]) -> AppState {
        AppState {
            environments: names
                .iter()
                .map(|name| named_environment(name, &[]))
                .collect(),
            ..AppState::default()
        }
    }

    fn selected(state: &AppState) -> usize {
        match state.load_state {
            LoadState::Loaded { selected, .. } => selected,
            ref other => panic!("expected LoadState::Loaded, got {other:?}"),
        }
    }

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

    /// A collection with far more requests than fit on screen, driven
    /// through the real `update()` + `view()` exactly as a keypress would:
    /// scrolling down past the bottom of the visible window, scrolling back
    /// up, and the two wrap-around jumps (last-to-first, first-to-last) that
    /// `move_selection` produces — each must bring the newly selected
    /// request into view, never leave the highlight off-screen with nothing
    /// visibly selected.
    #[test]
    fn collection_browser_scrolls_to_keep_selection_visible() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        const REQUEST_COUNT: usize = 30;
        let mut state = loaded_state(&many_request_collection(REQUEST_COUNT));

        // 10 rows total: 9 for the panes, 1 for the status bar — so well
        // under half of the 30 requests can be on screen at once.
        let render = |state: &AppState| {
            let backend = TestBackend::new(60, 10);
            let mut terminal = Terminal::new(backend).expect("a test terminal builds");
            terminal
                .draw(|frame| view(state, frame))
                .expect("rendering must not panic");
            buffer_to_string(terminal.backend().buffer())
        };

        let top_screen = render(&state);
        assert!(
            top_screen.contains("Request0"),
            "the first request must be visible at the very top:\n{top_screen}"
        );
        assert!(
            !top_screen.contains("Request29"),
            "the last request must not already be visible before scrolling down to it:\n{top_screen}"
        );

        // Move well past the bottom of the initial visible window.
        for _ in 0..20 {
            update(&mut state, Message::SelectNext);
        }
        assert_eq!(selected(&state), 20);
        let scrolled_down_screen = render(&state);
        assert!(
            scrolled_down_screen.contains("Request20"),
            "the newly selected request must have scrolled into view:\n{scrolled_down_screen}"
        );
        assert!(
            !scrolled_down_screen.contains("Request0"),
            "the list must have actually scrolled — the far-away top item must no longer \
             be on screen:\n{scrolled_down_screen}"
        );

        // Wrap forward from somewhere in the middle straight past the last
        // item back to the first.
        for _ in 0..10 {
            update(&mut state, Message::SelectNext);
        }
        assert_eq!(
            selected(&state),
            0,
            "SelectNext must wrap from the last request to the first"
        );
        let wrapped_to_top_screen = render(&state);
        assert!(
            wrapped_to_top_screen.contains("Request0"),
            "wrapping to the first request must scroll the list back to the top:\n{wrapped_to_top_screen}"
        );
        assert!(
            !wrapped_to_top_screen.contains("Request29"),
            "the list must not still be showing the bottom after wrapping to the top:\n{wrapped_to_top_screen}"
        );

        // And the reverse wrap: from the first request straight back to the
        // last.
        update(&mut state, Message::SelectPrevious);
        assert_eq!(
            selected(&state),
            REQUEST_COUNT - 1,
            "SelectPrevious must wrap from the first request to the last"
        );
        let wrapped_to_bottom_screen = render(&state);
        assert!(
            wrapped_to_bottom_screen.contains("Request29"),
            "wrapping to the last request must scroll the list down to show it:\n{wrapped_to_bottom_screen}"
        );
        assert!(
            !wrapped_to_bottom_screen.contains("Request0"),
            "the list must not still be showing the top after wrapping to the bottom:\n{wrapped_to_bottom_screen}"
        );
    }

    #[test]
    fn resolve_preview_succeeds_for_a_request_with_no_placeholders() {
        let document = Document::from_yaml_str(
            "name: One\nmethod: GET\nurl: https://example.com\nheaders:\n  Accept: application/json\n",
        )
        .expect("valid single request");
        let request = &document.requests()[0];

        let resolved = resolve_preview(request, Path::new("."), &Environment::default())
            .expect("no placeholders to fail on");

        assert_eq!(resolved.url, "https://example.com");
    }

    #[test]
    fn resolve_preview_surfaces_variable_not_found_with_no_environment() {
        let document = Document::from_yaml_str(
            "name: One\nmethod: GET\nurl: https://example.com/{{user_id}}\n",
        )
        .expect("valid single request");
        let request = &document.requests()[0];

        let error = resolve_preview(request, Path::new("."), &Environment::default()).expect_err(
            "a placeholder with no active environment must surface a real resolution error",
        );

        assert!(matches!(error, SendraError::VariableNotFound { .. }));
    }

    #[test]
    fn resolve_preview_resolves_against_a_real_environment() {
        let document = Document::from_yaml_str(
            "name: One\nmethod: GET\nurl: https://example.com/{{user_id}}\n",
        )
        .expect("valid single request");
        let request = &document.requests()[0];
        let mut environment = Environment::default();
        environment
            .variables
            .insert("user_id".to_string(), "42".to_string());

        let resolved = resolve_preview(request, Path::new("."), &environment)
            .expect("the environment defines the variable the request needs");

        assert_eq!(resolved.url, "https://example.com/42");
    }

    #[test]
    fn body_under_the_cap_is_shown_in_full() {
        let (preview, truncated) = truncate_body("short body");
        assert_eq!(preview, "short body");
        assert!(!truncated);
    }

    #[test]
    fn body_over_the_cap_is_truncated_not_panicked_on() {
        let huge = "x".repeat(MAX_BODY_PREVIEW_CHARS * 3);

        let (preview, truncated) = truncate_body(&huge);

        assert_eq!(preview.chars().count(), MAX_BODY_PREVIEW_CHARS);
        assert!(truncated);
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

    fn sample_response(status: u16) -> Response {
        Response {
            status,
            status_text: "OK".to_string(),
            headers: Vec::new(),
            body: String::new(),
            elapsed: std::time::Duration::from_millis(1),
            redirects: Vec::new(),
        }
    }

    /// A successful `RunOutcome` with the given status and empty
    /// assertion/capture reports — the "no assertions/captures declared"
    /// case, which is what most `RunState`-plumbing tests below actually
    /// need; tests about assertions/captures themselves build their own.
    fn sample_outcome(status: u16) -> RunOutcome {
        RunOutcome {
            result: Ok(sample_response(status)),
            assertions: AssertionReport::default(),
            capture: CaptureReport::default(),
        }
    }

    fn failed_outcome(error: SendraError) -> RunOutcome {
        RunOutcome {
            result: Err(error.into()),
            assertions: AssertionReport::default(),
            capture: CaptureReport::default(),
        }
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

    // --- edit mode ----------------------------------------------------------

    #[test]
    fn enter_edit_mode_is_a_no_op_without_a_selected_request() {
        let mut state = AppState::default();

        update(&mut state, Message::EnterEditMode);

        assert!(state.edit_mode.is_none());
    }

    #[test]
    fn enter_edit_mode_starts_a_clean_edit_state() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);

        update(&mut state, Message::EnterEditMode);

        assert_eq!(state.edit_mode, Some(EditState::default()));
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
    fn cancel_edit_round_trips_state_exactly_after_a_placeholder_mutation() {
        // Full round-trip proof for issue 17: enter edit mode, mutate
        // something a real future editing message would mutate (there are
        // no real editable fields yet — see `AppState::edit_mode`'s own doc
        // comment — so this stands in for one), cancel, and check every
        // piece of state a real edit could plausibly have touched is back
        // to exactly what it was before `EnterEditMode`. Not just
        // `edit_mode` itself (trivially `None` again either way) but the
        // dirty bookkeeping and the untouched document/selection too — the
        // proof this issue's own instructions ask for that cancelling never
        // leaves `AppState` partially mutated.
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

        // The placeholder/test-only state change: directly flip the one
        // field `EditState` has, and mark the request dirty, exactly as a
        // real editing message (issue 18+) would once it exists — proving
        // the *cancel* mechanism works even when something really did
        // change, not only in the trivial no-change case above.
        state.edit_mode.as_mut().expect("just entered").dirty = true;
        state.dirty_requests.insert(before_selected);

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
                    "cancel must not leave the loaded document changed"
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
    fn save_edit_clears_edit_mode_and_the_dirty_marker() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().expect("just entered").dirty = true;
        state.dirty_requests.insert(selected(&state));

        update(&mut state, Message::SaveEdit);

        assert!(state.edit_mode.is_none());
        assert!(
            state.dirty_requests.is_empty(),
            "save must clear the dirty marker for the request just saved"
        );
    }

    #[test]
    fn dirty_marker_appears_in_the_collection_browser_and_disappears_on_cancel() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().expect("just entered").dirty = true;
        state.dirty_requests.insert(selected(&state));

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

    #[test]
    fn dirty_marker_renders_next_to_the_edited_request_in_the_collection_browser() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let render = |state: &AppState| {
            let backend = TestBackend::new(60, 10);
            let mut terminal = Terminal::new(backend).expect("a test terminal builds");
            terminal
                .draw(|frame| view(state, frame))
                .expect("rendering must not panic");
            buffer_to_string(terminal.backend().buffer())
        };

        let mut state = loaded_state(THREE_REQUEST_COLLECTION);
        let clean_screen = render(&state);
        assert!(
            !clean_screen.contains('*'),
            "nothing is dirty yet, so no marker should render:\n{clean_screen}"
        );

        update(&mut state, Message::EnterEditMode);
        state.edit_mode.as_mut().expect("just entered").dirty = true;
        state.dirty_requests.insert(0);
        let dirty_screen = render(&state);
        assert!(
            dirty_screen.contains("*GET One"),
            "the dirty request must show a marker in the collection browser:\n{dirty_screen}"
        );

        update(&mut state, Message::CancelEdit);
        let cancelled_screen = render(&state);
        assert!(
            !cancelled_screen.contains('*'),
            "cancelling must remove the marker again:\n{cancelled_screen}"
        );
    }

    #[test]
    fn help_bar_shows_editing_state_and_its_own_keys() {
        let mut state = loaded_state(THREE_REQUEST_COLLECTION);

        update(&mut state, Message::EnterEditMode);

        let text = status_help_text(&state);
        assert!(text.contains("Editing"));
        assert!(text.contains("ctrl+s save"));
        assert!(text.contains("esc cancel"));
        assert!(
            !text.contains("nav"),
            "browsing keys must not be advertised while editing"
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

    fn response_with(headers: &[(&str, &str)], body: &str) -> Response {
        Response {
            status: 201,
            status_text: "Created".to_string(),
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            body: body.to_string(),
            elapsed: std::time::Duration::from_millis(42),
            redirects: Vec::new(),
        }
    }

    #[test]
    fn format_response_matches_the_status_headers_body_layout() {
        let response = response_with(&[("X-Request-Id", "abc123")], "plain text body");

        let text = format_response(&response);

        assert_eq!(
            text,
            "201 Created  42 ms\nX-Request-Id: abc123\n\nplain text body"
        );
    }

    #[test]
    fn format_response_omits_the_body_section_when_the_body_is_empty() {
        let response = response_with(&[], "");

        let text = format_response(&response);

        assert_eq!(text, "201 Created  42 ms");
    }

    #[test]
    fn body_for_display_pretty_prints_a_json_content_type() {
        let response = response_with(
            &[("Content-Type", "application/json; charset=utf-8")],
            "{\"id\":1,\"name\":\"widget\"}",
        );

        let displayed = body_for_display(&response);

        assert_eq!(displayed, "{\n  \"id\": 1,\n  \"name\": \"widget\"\n}");
    }

    #[test]
    fn body_for_display_leaves_non_json_content_types_untouched() {
        let response = response_with(&[("Content-Type", "text/plain")], "{\"id\":1}");

        let displayed = body_for_display(&response);

        assert_eq!(
            displayed, "{\"id\":1}",
            "a non-JSON content type must not be re-formatted"
        );
    }

    #[test]
    fn body_for_display_leaves_malformed_json_untouched() {
        let response = response_with(&[("Content-Type", "application/json")], "not json");

        let displayed = body_for_display(&response);

        assert_eq!(
            displayed, "not json",
            "a JSON content type whose body does not actually parse must be shown verbatim, not dropped or panicked on"
        );
    }

    #[test]
    fn claims_json_matches_a_vendor_json_suffix() {
        assert!(claims_json(&[(
            "content-type".to_string(),
            "application/vnd.api+json".to_string()
        )]));
    }

    /// Parses a single request from `yaml` and evaluates its `assertions:`
    /// block against `response` via the real `Assertions::evaluate` —
    /// sendra-core's own machinery, the same thing `run_request::execute`
    /// calls, not a hand-built `AssertionReport`, which the type's private
    /// fields would not even allow from outside its own crate.
    fn evaluate_assertions(yaml: &str, response: &Response) -> AssertionReport {
        let request = Document::from_yaml_str(yaml)
            .expect("valid test request")
            .requests()[0]
            .clone();
        request
            .assertions
            .expect("the test YAML declares an `assertions:` block")
            .evaluate(response)
    }

    /// Same as [`evaluate_assertions`] for a `capture:` block, via the real
    /// `Captures::evaluate`.
    fn evaluate_capture(yaml: &str, response: &Response) -> CaptureReport {
        let request = Document::from_yaml_str(yaml)
            .expect("valid test request")
            .requests()[0]
            .clone();
        request
            .capture
            .expect("the test YAML declares a `capture:` block")
            .evaluate(response, &Environment::default())
    }

    #[test]
    fn format_assertions_marks_a_request_with_no_assertions_distinctly() {
        let text = format_assertions(&AssertionReport::default());

        assert_eq!(text, "assertions\n  no assertions declared");
    }

    #[test]
    fn format_assertions_shows_every_result_individually_with_pass_fail() {
        let response = response_with(&[], "");
        let report = evaluate_assertions(
            "method: GET\nurl: https://example.com\nassertions:\n  status: 201\n  status_in: [404]\n",
            &response,
        );

        let text = format_assertions(&report);

        assert!(
            text.contains("✓ status is 201"),
            "the passing assertion must be shown on its own line: {text}"
        );
        assert!(
            text.contains("✗ status is one of [404]"),
            "the failing assertion must be shown on its own line, not folded into an aggregate: {text}"
        );
        assert!(
            text.contains("1 passed, 1 failed"),
            "a mixed report must show both counts: {text}"
        );
        assert_ne!(
            text,
            format_assertions(&AssertionReport::default()),
            "a report with real (even if all-failing) results must not read the same as \
             \"no assertions declared\""
        );
    }

    #[test]
    fn format_assertions_all_passing_reads_differently_from_none_declared() {
        let response = response_with(&[], "");
        let report = evaluate_assertions(
            "method: GET\nurl: https://example.com\nassertions:\n  status: 201\n",
            &response,
        );

        let text = format_assertions(&report);

        assert!(text.contains("1 passed"));
        assert!(
            !text.contains("no assertions declared"),
            "an assertions block that all passed must not be confused with none being declared: {text}"
        );
    }

    #[test]
    fn format_capture_section_marks_a_request_with_no_captures_distinctly() {
        let text = format_capture_section(&CaptureReport::default(), true);

        assert_eq!(text, "capture\n  no captures declared");
    }

    #[test]
    fn format_capture_section_masks_the_value_by_default() {
        let response = response_with(&[("X-Token", "super-secret")], "");
        let report = evaluate_capture(
            "method: GET\nurl: https://example.com\ncapture:\n  token: {header: X-Token}\n",
            &response,
        );

        let path = report.results()[0].path.clone();
        let masked = format_capture_section(&report, false);

        assert!(
            masked.contains(&format!("token from `{path}` = {REDACTED_CAPTURE_VALUE}")),
            "the value must be masked by default: {masked}"
        );
        assert!(
            !masked.contains("super-secret"),
            "the real captured value must not appear when not revealed: {masked}"
        );
    }

    #[test]
    fn format_capture_section_reveals_the_value_when_asked() {
        let response = response_with(&[("X-Token", "super-secret")], "");
        let report = evaluate_capture(
            "method: GET\nurl: https://example.com\ncapture:\n  token: {header: X-Token}\n",
            &response,
        );

        let path = report.results()[0].path.clone();
        let revealed = format_capture_section(&report, true);

        assert!(
            revealed.contains(&format!("token from `{path}` = super-secret")),
            "the real value must appear once revealed: {revealed}"
        );
    }

    #[test]
    fn format_capture_section_never_masks_a_failure() {
        // No `X-Token` header in the response, so the capture fails — and a
        // failure never had a value to redact in the first place, the same
        // rule sendra-cli's own `--json` `CaptureRecord` follows.
        let response = response_with(&[], "");
        let report = evaluate_capture(
            "method: GET\nurl: https://example.com\ncapture:\n  token: {header: X-Token}\n",
            &response,
        );

        let path = report.results()[0].path.clone();
        let masked = format_capture_section(&report, false);
        let revealed = format_capture_section(&report, true);

        assert_eq!(
            masked, revealed,
            "a failed capture has no value, so masking it must not change its rendering"
        );
        assert!(
            !masked.contains(REDACTED_CAPTURE_VALUE),
            "a failure is shown as a failure, not as a redacted value: {masked}"
        );
        assert!(masked.contains(&format!("✗ token from `{path}`")));
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

    /// End-to-end proof, through the real `view()`: a captured value is
    /// masked on first render, the real value appears once
    /// `ToggleRevealCaptures` is applied, and a fresh run puts the mask back
    /// — the three states the proof requirement asks for, read back from the
    /// actual character buffer rather than from `format_capture_section` in
    /// isolation.
    #[test]
    fn view_masks_captures_by_default_reveals_on_toggle_and_remasks_on_a_new_run() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = loaded_state(VALID_COLLECTION);
        let response = response_with(&[("X-Token", "super-secret")], "");
        let capture = evaluate_capture(
            "method: GET\nurl: https://example.com\ncapture:\n  token: {header: X-Token}\n",
            &response,
        );
        let outcome = || RunOutcome {
            result: Ok(response.clone()),
            assertions: AssertionReport::default(),
            capture: capture.clone(),
        };

        let render = |state: &AppState| {
            let backend = TestBackend::new(100, 15);
            let mut terminal = Terminal::new(backend).expect("a test terminal builds");
            terminal
                .draw(|frame| view(state, frame))
                .expect("rendering must not panic");
            buffer_to_string(terminal.backend().buffer())
        };

        // A masked/revealed capture *line* is what's under test —
        // `= <redacted>` vs `= super-secret` — not whether "super-secret"
        // appears anywhere on screen at all: the raw `X-Token` response
        // header carries the same value and is shown in full regardless (by
        // design; see the doc comment on `format_capture_section`), so a
        // whole-screen search for the string would fail for the wrong
        // reason even when masking is working correctly.
        let capture_line_contains = |screen: &str, needle: &str| {
            screen
                .lines()
                .any(|line| line.contains("token from") && line.contains(needle))
        };

        update(&mut state, Message::RunRequested);
        update(&mut state, Message::RunCompleted(outcome()));
        let masked_screen = render(&state);
        assert!(
            capture_line_contains(&masked_screen, REDACTED_CAPTURE_VALUE),
            "captures must be masked by default:\n{masked_screen}"
        );
        assert!(
            !capture_line_contains(&masked_screen, "super-secret"),
            "the capture line must not show the real value by default:\n{masked_screen}"
        );

        update(&mut state, Message::ToggleRevealCaptures);
        let revealed_screen = render(&state);
        assert!(
            capture_line_contains(&revealed_screen, "super-secret"),
            "the real value must be on the capture line after the reveal keybinding:\n{revealed_screen}"
        );

        update(&mut state, Message::RunRequested);
        update(&mut state, Message::RunCompleted(outcome()));
        let next_run_screen = render(&state);
        assert!(
            capture_line_contains(&next_run_screen, REDACTED_CAPTURE_VALUE),
            "a new run must not carry the reveal over — masked again by default:\n{next_run_screen}"
        );
        assert!(
            !capture_line_contains(&next_run_screen, "super-secret"),
            "a new run's capture line must not still show the real value:\n{next_run_screen}"
        );
    }

    #[test]
    fn run_completed_err_renders_the_real_sendra_error_text() {
        let error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");
        let expected = error.to_string();

        let text = format_run_result(&failed_outcome(error), false);

        assert!(
            text.contains(&expected),
            "the panel must show the real SendraError text, got: {text}"
        );
    }

    /// A large body, drawn into a small area, must not overflow the pane,
    /// panic, or scroll past its own content — the actual `render_widget`
    /// call proves this rather than just the scroll-offset arithmetic
    /// (`format_response`/`format_run_result` above), since ratatui's own
    /// clipping is part of what makes this safe.
    #[test]
    fn a_large_body_renders_into_a_small_area_without_panicking() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let huge_body = (0..5000)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let outcome = RunOutcome {
            result: Ok(response_with(&[], &huge_body)),
            assertions: AssertionReport::default(),
            capture: CaptureReport::default(),
        };

        // Computed from the real formatted text rather than hand-counted,
        // so this stays correct however `format_run_result` lays out the
        // status/headers/body plus the assertions/capture sections below it.
        let total_lines = format_run_result(&outcome, false).lines().count();
        let backend = TestBackend::new(60, 5);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");

        // Scrolled absurdly far past the end of the content — proving the
        // clamp in `render_response_panel` (not just ratatui's own
        // clipping) keeps the footer's line numbers sane rather than
        // reporting a scroll position past `total`.
        terminal
            .draw(|frame| {
                render_response_panel(frame, frame.area(), &outcome, usize::MAX, false);
            })
            .expect("drawing a huge, over-scrolled body must not panic");

        let buffer = terminal.backend().buffer();
        let footer_row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, buffer.area.height - 1)].symbol())
            .collect();
        assert!(
            footer_row.contains(&format!("of {total_lines}")),
            "the footer must report the real total line count, got: {footer_row:?}"
        );
        assert!(
            !footer_row.contains(&format!("of {}", total_lines + 1)),
            "an over-scroll must not be reported as if it went past the real total"
        );
    }

    /// `Home`/`End` end to end, through the real `update()` + `view()`: `End`
    /// jumps straight to the bottom of a long body in one step (no repeated
    /// `PageDown`s needed), and `Home` from there jumps straight back to the
    /// top — the two keys this issue adds to round out response scrolling.
    #[test]
    fn home_and_end_jump_the_response_panel_to_the_real_top_and_bottom() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let body = (0..300)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let outcome = RunOutcome {
            result: Ok(response_with(&[], &body)),
            assertions: AssertionReport::default(),
            capture: CaptureReport::default(),
        };
        let total_lines = format_run_result(&outcome, false).lines().count();

        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        update(&mut state, Message::RunCompleted(outcome));

        let render = |state: &AppState| {
            let backend = TestBackend::new(60, 10);
            let mut terminal = Terminal::new(backend).expect("a test terminal builds");
            terminal
                .draw(|frame| view(state, frame))
                .expect("rendering must not panic");
            buffer_to_string(terminal.backend().buffer())
        };

        update(&mut state, Message::ScrollResponseBottom);
        let bottom_screen = render(&state);
        assert!(
            bottom_screen.contains(&format!("-{total_lines} of {total_lines}")),
            "End must jump straight to the real end of the body in one step, not \
             partway through it:\n{bottom_screen}"
        );

        update(&mut state, Message::ScrollResponseTop);
        let top_screen = render(&state);
        assert!(
            top_screen.contains("Line 1-"),
            "Home must jump straight back to the real top of the response:\n{top_screen}"
        );
        assert!(
            top_screen.contains("201 Created"),
            "the top of the response must show the status line:\n{top_screen}"
        );
    }

    /// Renders a completed successful run through the real `view()` — not
    /// just `format_response`/`render_response_panel` in isolation — and
    /// reads the actual character buffer back, so this is what the terminal
    /// would really show: proof the status, a header and the body all land
    /// on screen together, replacing the request preview as documented on
    /// `render_detail_pane`.
    #[test]
    fn view_renders_a_completed_success_as_a_real_response_panel() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        let response = response_with(&[("X-Request-Id", "abc123")], "hello world");
        update(
            &mut state,
            Message::RunCompleted(RunOutcome {
                result: Ok(response),
                assertions: AssertionReport::default(),
                capture: CaptureReport::default(),
            }),
        );

        let backend = TestBackend::new(100, 15);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        terminal
            .draw(|frame| view(&state, frame))
            .expect("rendering a completed run must not panic");

        let screen = buffer_to_string(terminal.backend().buffer());
        assert!(
            screen.contains("201 Created"),
            "the real status must be on screen:\n{screen}"
        );
        assert!(
            screen.contains("X-Request-Id: abc123"),
            "a real response header must be on screen:\n{screen}"
        );
        assert!(
            screen.contains("hello world"),
            "the real response body must be on screen:\n{screen}"
        );
    }

    /// Same as above for the failure path: a real `SendraError`'s message
    /// must appear on screen, not a generic "failed" placeholder.
    #[test]
    fn view_renders_a_completed_failure_with_the_real_error_message() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        let error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");
        let expected_message = error.to_string();
        update(&mut state, Message::RunCompleted(failed_outcome(error)));

        let backend = TestBackend::new(100, 15);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        terminal
            .draw(|frame| view(&state, frame))
            .expect("rendering a failed run must not panic");

        let screen = buffer_to_string(terminal.backend().buffer());
        assert!(
            screen.contains(&expected_message),
            "the real SendraError message must be on screen, not a generic \
             placeholder:\n{screen}\nexpected to find: {expected_message}"
        );
    }

    fn buffer_to_string(buffer: &ratatui::buffer::Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // --- status_help_text: the bar's single source of truth -------------

    #[test]
    fn help_bar_shows_browser_keys_when_idle() {
        let state = loaded_state(VALID_COLLECTION);

        let text = status_help_text(&state);

        assert!(text.contains("nav"), "{text}");
        assert!(text.contains("run"), "{text}");
        assert!(text.contains("env"), "{text}");
        assert!(text.contains('q'), "{text}");
        // Not yet relevant while idle: nothing has run, so nothing to
        // scroll or reveal.
        assert!(!text.contains("scroll"), "{text}");
        assert!(!text.contains("reveal"), "{text}");
    }

    #[test]
    fn help_bar_shows_only_overlay_keys_when_the_overlay_is_open() {
        let mut state = state_with_environments(&["default", "staging"]);
        update(&mut state, Message::OpenEnvironmentOverlay);

        let text = status_help_text(&state);

        assert!(text.contains("confirm"), "{text}");
        assert!(text.contains("cancel"), "{text}");
        assert!(text.contains('q'), "{text}");
        // The overlay owns the keyboard: the browser's own run/env keys
        // must not be advertised alongside it.
        assert!(!text.contains("run"), "{text}");
        assert!(!text.contains("env"), "{text}");
    }

    #[test]
    fn help_bar_shows_only_quit_while_a_run_is_in_flight() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        assert!(matches!(state.run_state, RunState::InFlight));

        let text = status_help_text(&state);

        assert!(text.contains('q'), "{text}");
        // Every one of these is genuinely refused right now by `update`'s
        // own InFlight guard, so none of them belongs in the hint.
        assert!(!text.contains("nav"), "{text}");
        assert!(!text.contains("run again"), "{text}");
        assert!(!text.contains("env"), "{text}");
        assert!(!text.contains("scroll"), "{text}");
    }

    #[test]
    fn help_bar_shows_scroll_and_nav_keys_once_a_run_has_completed() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        update(&mut state, Message::RunCompleted(sample_outcome(200)));

        let text = status_help_text(&state);

        assert!(text.contains("scroll"), "{text}");
        assert!(text.contains("nav"), "{text}");
        assert!(text.contains("run again"), "{text}");
        assert!(text.contains("env"), "{text}");
        assert!(text.contains('q'), "{text}");
    }

    #[test]
    fn help_bar_only_mentions_capture_reveal_when_there_is_something_to_reveal() {
        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        update(&mut state, Message::RunCompleted(sample_outcome(200)));
        let no_captures_text = status_help_text(&state);
        assert!(
            !no_captures_text.contains("reveal"),
            "a run with no `capture:` block has nothing to reveal: {no_captures_text}"
        );

        let response = response_with(&[("X-Token", "super-secret")], "");
        let capture = evaluate_capture(
            "method: GET\nurl: https://example.com\ncapture:\n  token: {header: X-Token}\n",
            &response,
        );
        update(&mut state, Message::RunRequested);
        update(
            &mut state,
            Message::RunCompleted(RunOutcome {
                result: Ok(response),
                assertions: AssertionReport::default(),
                capture,
            }),
        );

        let with_captures_text = status_help_text(&state);
        assert!(
            with_captures_text.contains("reveal"),
            "a run with a non-empty `capture:` block must advertise the reveal key: {with_captures_text}"
        );
    }

    /// The four contexts must not all read the same — the whole point of a
    /// *context-sensitive* bar, proven here as one assertion over the same
    /// four states the tests above exercise individually.
    #[test]
    fn help_bar_text_differs_across_all_four_contexts() {
        let idle = loaded_state(VALID_COLLECTION);

        let mut overlaid = state_with_environments(&["default"]);
        update(&mut overlaid, Message::OpenEnvironmentOverlay);

        let mut in_flight = loaded_state(VALID_COLLECTION);
        update(&mut in_flight, Message::RunRequested);

        let mut completed = loaded_state(VALID_COLLECTION);
        update(&mut completed, Message::RunRequested);
        update(&mut completed, Message::RunCompleted(sample_outcome(200)));

        let texts = [
            status_help_text(&idle),
            status_help_text(&overlaid),
            status_help_text(&in_flight),
            status_help_text(&completed),
        ];

        for (i, a) in texts.iter().enumerate() {
            for (j, b) in texts.iter().enumerate() {
                if i != j {
                    assert_ne!(
                        a, b,
                        "contexts {i} and {j} must show different help text, both got: {a:?}"
                    );
                }
            }
        }
    }

    /// End-to-end proof, through the real `view()`, that the bottom row
    /// actually changes as the bar's own state changes — not just that
    /// `status_help_text` returns different strings in isolation.
    #[test]
    fn view_renders_different_help_bar_text_across_contexts() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let render = |state: &AppState| {
            let backend = TestBackend::new(100, 15);
            let mut terminal = Terminal::new(backend).expect("a test terminal builds");
            terminal
                .draw(|frame| view(state, frame))
                .expect("rendering must not panic");
            buffer_to_string(terminal.backend().buffer())
        };

        let mut state = loaded_state(VALID_COLLECTION);
        let browsing_screen = render(&state);
        assert!(browsing_screen.contains("nav"));
        assert!(!browsing_screen.contains("scroll"));

        update(&mut state, Message::RunRequested);
        let in_flight_screen = render(&state);
        assert!(in_flight_screen.contains("Running request"));
        assert!(!in_flight_screen.contains("nav"));

        update(&mut state, Message::RunCompleted(sample_outcome(200)));
        let completed_screen = render(&state);
        assert!(completed_screen.contains("scroll"));
        assert!(completed_screen.contains("nav"));

        // Back to idle, then the fourth context: the environment overlay.
        // `state_with_environments` alone has no loaded collection, so this
        // goes through `loaded_state` plus environments instead, to prove
        // the bottom row still renders correctly underneath the popup.
        let mut overlaid = loaded_state(VALID_COLLECTION);
        overlaid.environments = vec![named_environment("default", &[])];
        update(&mut overlaid, Message::OpenEnvironmentOverlay);
        let overlay_screen = render(&overlaid);
        assert!(overlay_screen.contains("confirm"));
        assert!(overlay_screen.contains("cancel"));
        assert!(!overlay_screen.contains("run again"));

        assert_ne!(browsing_screen, in_flight_screen);
        assert_ne!(in_flight_screen, completed_screen);
        assert_ne!(browsing_screen, completed_screen);
        assert_ne!(browsing_screen, overlay_screen);
        assert_ne!(completed_screen, overlay_screen);
    }

    // --- error-handling audit (issue 11) ---------------------------------

    #[test]
    fn format_error_shows_a_heading_and_the_real_error_verbatim() {
        let error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");
        let expected = error.to_string();

        let text = format_error("Failed to load collection", &error);

        assert!(text.starts_with('⚠'), "{text}");
        assert!(text.contains("Failed to load collection"), "{text}");
        assert!(
            text.contains(&expected),
            "the real error text must appear verbatim: {text}"
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

    #[test]
    fn help_bar_offers_only_env_and_quit_with_no_collection_loaded() {
        for state in [
            AppState::default(),
            {
                let mut s = AppState::default();
                update(&mut s, Message::NoCollectionPath);
                s
            },
            {
                let mut s = AppState::default();
                let error =
                    Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");
                update(
                    &mut s,
                    Message::CollectionLoaded {
                        base_dir: PathBuf::from("."),
                        result: Box::new(Err(error)),
                    },
                );
                s
            },
        ] {
            let text = status_help_text(&state);
            assert!(text.contains("env"), "{text}");
            assert!(text.contains('q'), "{text}");
            assert!(
                !text.contains("nav") && !text.contains("run"),
                "no collection means nothing to navigate or run: {text}"
            );
        }
    }

    /// End-to-end proof, through the real `view()`, of the three triggers
    /// the audit asks for: a malformed collection, a malformed environment
    /// file, and — separately, in `run_request`'s own tests — an
    /// unreachable host. Each must show a readable message via the same
    /// [`format_error`] path, and the bar underneath must still say `q quit`
    /// works.
    #[test]
    fn view_shows_a_readable_error_for_a_malformed_collection_and_stays_responsive() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = AppState::default();
        let error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");
        let expected = error.to_string();
        update(
            &mut state,
            Message::CollectionLoaded {
                base_dir: PathBuf::from("."),
                result: Box::new(Err(error)),
            },
        );

        let backend = TestBackend::new(100, 15);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        terminal
            .draw(|frame| view(&state, frame))
            .expect("rendering a load failure must not panic");

        let screen = buffer_to_string(terminal.backend().buffer());
        assert!(
            screen.contains(&expected),
            "the real parse error must be readable on screen:\n{screen}"
        );
        assert!(
            screen.contains('q'),
            "the help bar must still show quit works:\n{screen}"
        );
    }

    #[test]
    fn view_shows_a_readable_error_for_a_malformed_environment_file() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = loaded_state(VALID_COLLECTION);
        let bad_error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");
        let expected = bad_error.to_string();
        update(
            &mut state,
            Message::EnvironmentsLoaded {
                environments: Vec::new(),
                errors: vec![("staging".to_string(), bad_error)],
            },
        );
        update(&mut state, Message::OpenEnvironmentOverlay);

        let backend = TestBackend::new(100, 15);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        terminal
            .draw(|frame| view(&state, frame))
            .expect("rendering an environment load failure must not panic");

        let screen = buffer_to_string(terminal.backend().buffer());
        assert!(
            screen.contains("staging"),
            "the broken environment's name must be visible:\n{screen}"
        );
        assert!(
            screen.contains(&expected),
            "the real environment error must be readable on screen:\n{screen}"
        );
    }

    #[test]
    fn run_error_display_covers_both_the_core_and_runtime_variants() {
        // `Core` displays as the wrapped `SendraError`'s own `Display` —
        // core's fixed wording for this variant, not the raw `io::Error`
        // `#[source]` alone does not interpolate into it.
        let core_error = crate::run_request::RunError::from(SendraError::CurrentDir(
            std::io::Error::other("boom"),
        ));
        assert_eq!(
            core_error.to_string(),
            SendraError::CurrentDir(std::io::Error::other("boom")).to_string()
        );

        let runtime_error =
            crate::run_request::RunError::RuntimeUnavailable(std::io::Error::other("no threads"));
        assert!(runtime_error.to_string().contains("no threads"));
        assert!(runtime_error.to_string().contains("runtime"));
    }

    // --- resize handling ---------------------------------------------

    /// The `Message::Resize` no-op, end to end: a resized backend, re-drawn
    /// through the real `view()` between two calls with no `update()` call
    /// for the resize itself in between (mirroring how `main::run` actually
    /// handles it — `Message::Resize` changes no state; the next
    /// `terminal.draw` picks up the new size on its own). Both a shrink and
    /// a subsequent grow are exercised on a completed run's response panel,
    /// scrolled deep into a long body, so this also proves `response_scroll`
    /// re-clamps against whichever area is current at render time rather
    /// than pointing past a now-gone taller viewport.
    #[test]
    fn resizing_between_renders_relayouts_without_panicking() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let body = (0..500)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let outcome = RunOutcome {
            result: Ok(response_with(&[], &body)),
            assertions: AssertionReport::default(),
            capture: CaptureReport::default(),
        };
        let total_lines = format_run_result(&outcome, false).lines().count();

        let mut state = loaded_state(VALID_COLLECTION);
        update(&mut state, Message::RunRequested);
        update(&mut state, Message::RunCompleted(outcome));
        // Deep enough to be well past what any of the sizes below can show,
        // so every draw below exercises the render-time clamp.
        state.response_scroll = 10_000;

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        terminal
            .draw(|frame| view(&state, frame))
            .expect("initial draw must not panic");

        // Shrink drastically — down to a sliver of a terminal — and redraw.
        terminal.backend_mut().resize(20, 6);
        terminal
            .draw(|frame| view(&state, frame))
            .expect("drawing after a drastic shrink must not panic");
        let shrunk = terminal.backend().buffer();
        assert_eq!(
            shrunk.area,
            Rect::new(0, 0, 20, 6),
            "the buffer must track the new, smaller size exactly — no leftover \
             cells from the previous 100x30 frame"
        );
        let shrunk_screen = buffer_to_string(shrunk);
        // At 20 columns wide the footer's `of {total}` tail is clipped off
        // screen, so what actually proves the clamp worked is the line
        // range itself ending at (not past) the real total.
        assert!(
            shrunk_screen.contains(&format!("-{total_lines}")),
            "the footer's visible range must end exactly at the real total \
             line count ({total_lines}) once scrolled past the end, not \
             beyond it or blank:\n{shrunk_screen}"
        );

        // Grow back past the original size and redraw again.
        terminal.backend_mut().resize(150, 40);
        terminal
            .draw(|frame| view(&state, frame))
            .expect("drawing after growing must not panic");
        let grown = terminal.backend().buffer();
        assert_eq!(
            grown.area,
            Rect::new(0, 0, 150, 40),
            "the buffer must track the new, larger size exactly"
        );
        let grown_screen = buffer_to_string(grown);
        assert!(
            grown_screen.contains("line 4"),
            "the grown frame must show real body content, not a blank pane \
             left over from the shrunk size:\n{grown_screen}"
        );
    }

    /// Every top-level `view()` state — loading, a failed load, the request
    /// browser, a completed response panel, and the environment overlay on
    /// top of it — drawn into a terminal shrunk to a single-digit size, the
    /// smallest a real terminal resize could plausibly produce. Content is
    /// necessarily cramped or clipped; nothing may panic.
    #[test]
    fn every_screen_survives_a_very_small_terminal_size() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let draw = |state: &AppState, width: u16, height: u16| {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("a test terminal builds");
            terminal
                .draw(|frame| view(state, frame))
                .unwrap_or_else(|err| panic!("{width}x{height} draw must not panic: {err}"));
        };

        let sizes: [(u16, u16); 4] = [(1, 1), (2, 1), (1, 2), (4, 3)];

        for (width, height) in sizes {
            draw(&AppState::default(), width, height);

            let mut browsing = loaded_state(THREE_REQUEST_COLLECTION);
            draw(&browsing, width, height);

            update(&mut browsing, Message::RunRequested);
            update(&mut browsing, Message::RunCompleted(sample_outcome(200)));
            browsing.response_scroll = 9_999;
            draw(&browsing, width, height);

            let mut overlaid = loaded_state(VALID_COLLECTION);
            overlaid.environments = vec![
                named_environment("default", &[("token", "abc")]),
                named_environment("staging", &[]),
            ];
            update(&mut overlaid, Message::OpenEnvironmentOverlay);
            draw(&overlaid, width, height);

            let mut failed = AppState::default();
            let error = Document::from_yaml_str(MALFORMED_YAML).expect_err("malformed test YAML");
            update(
                &mut failed,
                Message::CollectionLoaded {
                    base_dir: PathBuf::from("."),
                    result: Box::new(Err(error)),
                },
            );
            draw(&failed, width, height);
        }
    }

    /// The response panel's own scroll clamp (`render_response_panel`),
    /// isolated from `view()`: a scroll offset that was valid for a tall
    /// area must not be trusted after the area shrinks — it must be
    /// re-clamped against the *current* area on every draw, never carried
    /// over from a previous, larger one.
    #[test]
    fn response_panel_scroll_reclamps_to_a_shrunk_area() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let body = (0..200)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let outcome = RunOutcome {
            result: Ok(response_with(&[], &body)),
            assertions: AssertionReport::default(),
            capture: CaptureReport::default(),
        };
        let total_lines = format_run_result(&outcome, false).lines().count();

        let backend = TestBackend::new(80, 40);
        let mut terminal = Terminal::new(backend).expect("a test terminal builds");
        // A scroll position that was in range for an 80x40 area but is
        // nowhere close to the top of the much shorter area used below.
        let deep_scroll = total_lines - 5;

        terminal
            .draw(|frame| {
                render_response_panel(frame, frame.area(), &outcome, deep_scroll, false);
            })
            .expect("initial draw must not panic");

        terminal.backend_mut().resize(80, 4);
        terminal
            .draw(|frame| {
                render_response_panel(frame, frame.area(), &outcome, deep_scroll, false);
            })
            .expect("drawing the same stale scroll offset into a shrunk area must not panic");

        let screen = buffer_to_string(terminal.backend().buffer());
        assert!(
            !screen.trim().is_empty(),
            "a re-clamped scroll must still show real content, not a blank pane:\n{screen}"
        );
        assert!(
            screen.contains(&format!("of {total_lines}")),
            "the footer must report the real total even after the area shrank:\n{screen}"
        );
    }
}
