//! The model half of sendra-tui's Elm-style architecture: `AppState` and
//! every type it is built from (`LoadState`, `RunState`, edit mode's
//! `EditState`/`EditField`/`TextField`), plus the `Message` enum every state
//! transition flows through. No transition logic lives here — see
//! `super::update` for `update()` itself — this module only defines what the
//! state *is*.

use std::collections::HashSet;
use std::path::PathBuf;

use sendra_core::{Document, Environment, Method, Request, SendraError};

use crate::run_request::RunOutcome;

/// One environment sendra-tui found in `.sendra/environments/`, loaded (not
/// just named) so the overlay can show its variables before it's picked.
#[derive(Debug, Clone)]
pub struct NamedEnvironment {
    pub name: String,
    pub environment: Environment,
}

/// A single-line, cursor-addressable text buffer — the minimal text-input
/// primitive every field edit mode touches shares (method and URL today;
/// future fields like headers/auth/body can reuse it too), rather than each
/// hand-rolling its own insert/delete/cursor-movement logic.
///
/// A dependency like `tui-input` was considered and rejected: every field
/// this crate will ever edit is a single line of plain text with no
/// undo/redo, no multi-line, no IME composition, and no need for anything
/// beyond insert/backspace/delete/left/right — exactly what this type
/// covers in well under a hundred lines including its own tests. Pulling in
/// an external crate would trade a dependency (plus its own `Input`/event
/// API to learn and wire into `Message`/`update`, and its own opinions on
/// things like scrolling a field wider than its viewport, unneeded here)
/// for something this hand-rolls more simply and keeps fully under this
/// crate's own tests, the same reasoning `body_for_display`'s doc comment
/// gives for reimplementing rather than depending across a crate boundary.
/// Revisit if a later field genuinely needs more than this (multi-line
/// input, unicode grapheme-aware cursor movement instead of per-`char`,
/// undo).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextField {
    value: String,
    /// Byte offset into `value` — always on a `char` boundary, maintained by
    /// every method below rather than trusted from outside, since `value` is
    /// UTF-8 and an arbitrary byte offset could land mid-codepoint and panic
    /// on the next `insert`/`drain`.
    cursor: usize,
}

impl TextField {
    /// Starts with the cursor at the end — the natural place to resume
    /// typing a field that already has a value, matching how a browser's
    /// address bar or an ordinary GUI text field focuses.
    fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.len();
        Self { value, cursor }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    /// The cursor's position measured in `char`s rather than bytes — what a
    /// terminal column offset actually needs, since a multi-byte UTF-8
    /// character is still exactly one terminal cell wide for anything in the
    /// Basic Multilingual Plane this app is likely to see typed into a
    /// method or URL field.
    pub fn cursor_chars(&self) -> usize {
        self.value[..self.cursor].chars().count()
    }

    /// Visible to `super::update`, which is the one place outside this
    /// module that mutates a field in response to a keystroke — see
    /// `update::edit_mutate`/`edit_move`.
    pub(super) fn insert_char(&mut self, ch: char) {
        self.value.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    /// Deletes the character immediately before the cursor ("Backspace") — a
    /// no-op at the very start of the field.
    pub(super) fn backspace(&mut self) {
        if let Some(prev) = self.prev_char_boundary() {
            self.value.drain(prev..self.cursor);
            self.cursor = prev;
        }
    }

    /// Deletes the character the cursor sits on ("Delete") — a no-op at the
    /// very end of the field, where there is no character under the cursor.
    pub(super) fn delete(&mut self) {
        if let Some(next) = self.next_char_boundary() {
            self.value.drain(self.cursor..next);
        }
    }

    pub(super) fn move_left(&mut self) {
        if let Some(prev) = self.prev_char_boundary() {
            self.cursor = prev;
        }
    }

    pub(super) fn move_right(&mut self) {
        if let Some(next) = self.next_char_boundary() {
            self.cursor = next;
        }
    }

    fn prev_char_boundary(&self) -> Option<usize> {
        self.value[..self.cursor]
            .chars()
            .next_back()
            .map(|ch| self.cursor - ch.len_utf8())
    }

    fn next_char_boundary(&self) -> Option<usize> {
        self.value[self.cursor..]
            .chars()
            .next()
            .map(|ch| self.cursor + ch.len_utf8())
    }
}

/// Which of edit mode's fields `Message::EditFocusNext` (`Tab`) is currently
/// pointed at, and so which one `Message::EditInsertChar`/`EditBackspace`/etc.
/// act on. Two variants today (method, URL); if headers/auth/body ever gain
/// their own fields, this and `EditState::focused_field_mut` are exactly
/// where those new variants would slot in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EditField {
    #[default]
    Method,
    Url,
}

impl EditField {
    /// Visible to `super::update`'s `Message::EditFocusNext` arm.
    pub(super) fn next(self) -> Self {
        match self {
            EditField::Method => EditField::Url,
            EditField::Url => EditField::Method,
        }
    }
}

/// The state of an in-progress edit of the selected request: `method` and
/// `url` as working copies (`TextField`s) separate from the request itself.
/// `Message::SaveEdit` copies them back into the loaded document (see
/// `update`'s own `Message::SaveEdit` arm); `Message::CancelEdit` simply
/// drops this whole struct, which is what makes cancelling a full, provable
/// no-op no matter what was typed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EditState {
    pub dirty: bool,
    pub method: TextField,
    pub url: TextField,
    pub focus: EditField,
    /// `Some(message)` whenever `method`'s current text does not parse as a
    /// real `sendra_core::Method` — recomputed on every keystroke that
    /// touches `method` (see `edit_mutate` in `update`), not only at save
    /// time, so a bad method is never silently accepted *or* silently
    /// dropped: the field keeps exactly what was typed, and this is the
    /// message `render_edit_pane` shows alongside it. `Message::SaveEdit`
    /// refuses to save while this is `Some`, but never clears `edit_mode`
    /// over it — see that arm's own comment.
    pub method_error: Option<String>,
}

impl EditState {
    /// Visible to `super::update`'s `Message::EnterEditMode` arm, which is
    /// the only place outside this module allowed to start an edit.
    pub(super) fn new(request: &Request) -> Self {
        let method = TextField::new(request.method.as_str());
        let method_error = validate_method_text(method.value()).err();
        Self {
            dirty: false,
            method,
            url: TextField::new(request.url.clone()),
            focus: EditField::default(),
            method_error,
        }
    }

    /// Visible to `super::update`'s `edit_mutate`/`edit_move` helpers.
    pub(super) fn focused_field_mut(&mut self) -> &mut TextField {
        match self.focus {
            EditField::Method => &mut self.method,
            EditField::Url => &mut self.url,
        }
    }
}

/// Whether `text` parses as a real `sendra_core::Method` — reusing that
/// enum's own `Deserialize` impl (the exact check a collection YAML file's
/// `method:` field is already held to, via `serde_yaml`, a workspace
/// dependency sendra-core itself already uses for the same purpose) rather
/// than a hand-maintained list of method names living in sendra-tui that
/// could quietly drift from `Method`'s real, closed variant set.
///
/// Input is uppercased before checking: `Method`'s `Deserialize` is
/// case-sensitive (`#[serde(rename_all = "UPPERCASE")]`), matching the
/// convention every Sendra YAML file is written in, but a user typing into a
/// live text field is not writing YAML and has no reason to expect
/// `get`/`Get`/`GET` to mean three different things. Built as a
/// `serde_yaml::Value::String` and deserialized from that typed value,
/// rather than parsed from raw YAML text via `serde_yaml::from_str` — the
/// latter would run the *whole* YAML scalar grammar over whatever was typed,
/// which reinterprets some plain words as other types entirely (YAML 1.1
/// treats `no`/`yes`/`on`/`off` as booleans); going through a `Value`
/// already tagged as a string skips that grammar and checks only what this
/// function claims to check: is this string one of `Method`'s variants.
///
/// Visible to `super::update`, which calls this on every keystroke that
/// touches the method field and again at save time (see `edit_mutate` and
/// the `Message::SaveEdit` arm).
pub(super) fn validate_method_text(text: &str) -> Result<Method, String> {
    let candidate = serde_yaml::Value::String(text.trim().to_ascii_uppercase());
    serde_yaml::from_value::<Method>(candidate).map_err(|_| {
        format!(
            "'{text}' is not a valid HTTP method (GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS)"
        )
    })
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
    RunCompleted(RunOutcome),
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
    /// `c`: flips `reveal_captures`. Masked values become visible, visible
    /// values become masked again — a toggle rather than a one-way reveal,
    /// so hiding them again does not need a second, differently-named key.
    ToggleRevealCaptures,
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
    /// `Tab` while editing: moves focus to the other field (see
    /// `EditField::next`) — the only two exist today, so this simply
    /// alternates; a third field added later would only need
    /// `EditField::next` to grow, not this message.
    EditFocusNext,
    /// An ordinary printable character typed into whichever field is
    /// currently focused — inserted at the cursor via `TextField::insert_char`.
    EditInsertChar(char),
    /// `Backspace` on the focused field: deletes the character before the
    /// cursor.
    EditBackspace,
    /// `Delete` on the focused field: deletes the character under the
    /// cursor.
    EditDelete,
    /// `Left` on the focused field: moves the cursor back one character.
    EditCursorLeft,
    /// `Right` on the focused field: moves the cursor forward one character.
    EditCursorRight,
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- TextField: the shared text-input primitive --------------------------

    #[test]
    fn text_field_starts_with_the_cursor_at_the_end() {
        let field = TextField::new("GET");
        assert_eq!(field.value(), "GET");
        assert_eq!(field.cursor_chars(), 3);
    }

    #[test]
    fn insert_char_inserts_at_the_cursor_and_advances_it() {
        let mut field = TextField::new("GT");
        field.move_left(); // cursor between G and T
        field.insert_char('E');
        assert_eq!(field.value(), "GET");
        assert_eq!(field.cursor_chars(), 2);
    }

    #[test]
    fn backspace_removes_the_character_before_the_cursor() {
        let mut field = TextField::new("GET");
        field.backspace();
        assert_eq!(field.value(), "GE");
        assert_eq!(field.cursor_chars(), 2);
    }

    #[test]
    fn backspace_at_the_start_is_a_no_op() {
        let mut field = TextField::new("GET");
        field.move_left();
        field.move_left();
        field.move_left();
        assert_eq!(field.cursor_chars(), 0);
        field.backspace();
        assert_eq!(field.value(), "GET");
        assert_eq!(field.cursor_chars(), 0);
    }

    #[test]
    fn delete_removes_the_character_under_the_cursor() {
        let mut field = TextField::new("GET");
        field.move_left(); // cursor between E and T
        field.move_left(); // cursor between G and E
        field.delete();
        assert_eq!(field.value(), "GT");
        assert_eq!(field.cursor_chars(), 1);
    }

    #[test]
    fn delete_at_the_end_is_a_no_op() {
        let mut field = TextField::new("GET");
        field.delete();
        assert_eq!(field.value(), "GET");
    }

    #[test]
    fn cursor_movement_does_not_run_past_either_end() {
        let mut field = TextField::new("GO");
        field.move_right();
        assert_eq!(field.cursor_chars(), 2, "must not run past the end");
        field.move_left();
        field.move_left();
        field.move_left();
        assert_eq!(field.cursor_chars(), 0, "must not run past the start");
    }

    #[test]
    fn multi_byte_characters_are_never_split() {
        // "é" is two UTF-8 bytes but one `char` — insert/backspace/delete
        // and cursor movement must all treat it as one unit, or `value`
        // would stop being valid UTF-8 the moment a byte offset landed
        // mid-codepoint.
        let mut field = TextField::new("café");
        assert_eq!(field.cursor_chars(), 4);
        field.backspace();
        assert_eq!(field.value(), "caf");
        field.insert_char('é');
        assert_eq!(field.value(), "café");
        field.move_left();
        field.delete();
        assert_eq!(field.value(), "caf");
    }

    #[test]
    fn edit_field_next_alternates_between_method_and_url() {
        assert_eq!(EditField::Method.next(), EditField::Url);
        assert_eq!(EditField::Url.next(), EditField::Method);
    }

    // --- Method validation ----------------------------------------------------

    #[test]
    fn validate_method_text_accepts_every_real_method_case_insensitively() {
        for (text, expected) in [
            ("GET", Method::Get),
            ("post", Method::Post),
            ("Put", Method::Put),
            ("PATCH", Method::Patch),
            ("delete", Method::Delete),
            ("Head", Method::Head),
            ("OPTIONS", Method::Options),
        ] {
            assert_eq!(validate_method_text(text), Ok(expected));
        }
    }

    #[test]
    fn validate_method_text_rejects_a_made_up_method() {
        let error = validate_method_text("FOOBAR").expect_err("FOOBAR is not a real method");
        assert!(error.contains("FOOBAR"));
        assert!(error.contains("GET"), "the error should list real methods");
    }

    #[test]
    fn validate_method_text_does_not_coerce_yaml_boolean_words() {
        // A raw `serde_yaml::from_str` on "no"/"yes"/"on"/"off" would parse
        // as a YAML 1.1 boolean before ever reaching `Method`'s own
        // `Deserialize` — going through a `Value::String` instead (see
        // `validate_method_text`'s own doc comment) must sidestep that
        // entirely, and none of these are real methods regardless.
        for text in ["no", "yes", "on", "off"] {
            assert!(validate_method_text(text).is_err());
        }
    }
}
